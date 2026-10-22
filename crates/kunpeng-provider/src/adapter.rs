//! Provider 与具体协议之间的最小接口，以及各协议共用的 HTTP 工具。
//!
//! 这里不持有配置或状态，请求上下文按引用传入，目的是让协议适配器可以在测试里
//! 用 mock 替换，也避免每个请求复制 base_url/api_key。

use async_trait::async_trait;
use kunpeng_core::error::{Error, ParseResponseError, Result};
use kunpeng_core::types::{ChatRequest, ChatResponse, StreamControl, StreamDelta};
use kunpeng_core::HttpErrorKind;
use reqwest::header::{HeaderMap, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{RequestBuilder, Response, StatusCode};
use std::time::Duration;

/// 把 reqwest 传输错误收敛成 core 的结构化错误，同时保留 source。
pub fn http_error(error: reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        HttpErrorKind::Timeout
    } else if error.is_connect() {
        HttpErrorKind::Connect
    } else if error.is_body() {
        HttpErrorKind::Body
    } else if error.is_request() {
        HttpErrorKind::Request
    } else {
        HttpErrorKind::Other
    };
    Error::http_with_source(kind, error.to_string(), error)
}

/// 单次请求的共享上下文。
///
/// 字段都是借用：`Provider` 生命周期覆盖整个请求，没必要为每次调用克隆字符串。
pub struct CallContext<'a> {
    pub http: &'a reqwest::Client,
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub model: &'a str,
    pub max_retries: u32,
}

/// 协议适配器 API。
///
/// 每个厂商实现一个 `Adapter`，同时提供元数据（名称、别名、默认地址/模型）和
/// 协议翻译。`AdapterRegistry` 负责按名称找到实现；`Provider` 只依赖这个 trait，
/// 不认识任何具体厂商。
///
/// 用 `async_trait` 是因为原生 async fn trait 目前不能直接作为 `dyn Adapter`
/// 使用；一次 Box 分配相对于秒级 LLM 请求可以忽略。`req` 按引用传入，适配器只能
/// 读取历史，不能修改 Agent 的状态。
#[async_trait]
pub trait Adapter: Send + Sync {
    /// 规范名称，例如 `openai`、`anthropic`。
    fn name(&self) -> &'static str;

    /// 额外别名。别名和 `name` 共享同一套默认值/适配器实例。
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// 没有显式 base URL 时使用；返回空串表示必须由配置提供。
    fn default_base_url(&self) -> &'static str {
        ""
    }

    /// 没有显式模型名时使用；返回空串表示必须由配置提供。
    fn default_model(&self) -> &'static str {
        ""
    }

    async fn chat(&self, ctx: &CallContext<'_>, req: &ChatRequest<'_>) -> Result<ChatResponse>;

    /// 流式调用。默认实现退回非流式，并把完整文本作为单个增量发出去，
    /// 这样新增 provider 可以先只实现 `chat`，Agent 不需要知道它是否支持 SSE。
    ///
    /// 回调区分正文和思考内容；工具调用的参数分片由适配器自行累积，最终放进
    /// `ChatResponse.tool_calls`。UI 不应把增量当成最终消息，收到
    /// `AgentEvent::Assistant` 后再以完整内容为准。
    async fn chat_stream(
        &self,
        ctx: &CallContext<'_>,
        req: &ChatRequest<'_>,
        on_delta: &mut (dyn for<'a> FnMut(StreamDelta<'a>) -> StreamControl + Send),
    ) -> Result<ChatResponse> {
        let response = self.chat(ctx, req).await?;
        if let Some(thinking) = &response.thinking {
            if on_delta(StreamDelta::Thinking(thinking)) == StreamControl::Stop {
                return Err(Error::Cancelled);
            }
        }
        if let Some(content) = &response.content {
            if on_delta(StreamDelta::Text(content)) == StreamControl::Stop {
                return Err(Error::Cancelled);
            }
        }
        Ok(response)
    }
}

/// 一条 SSE 事件。`event` 对应 `event:` 字段，`data` 是拼接后的 `data:` 内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// 增量 SSE 解码器。
///
/// 网络 chunk 不按行切分，可能把一行 JSON、甚至一个 UTF-8 字符切成两半，所以
/// 这里保留原始字节缓冲，只按 `\n` 取完整行。行内的 `\r` 和 `data:` 后可选空格
/// 按 SSE 规范处理。
#[derive(Default)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
    event: Option<String>,
    data: String,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一个网络 chunk，返回其中已经完整的事件。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=index).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            self.process_line(line.as_ref(), &mut events);
        }
        events
    }

    /// 流结束时调用。部分服务端最后一帧没有空行，需要把残留行补处理掉。
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let mut line = std::mem::take(&mut self.buffer);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            let mut events = Vec::new();
            self.process_line(line.as_ref(), &mut events);
            if let Some(event) = events.into_iter().next() {
                return Some(event);
            }
        }
        self.dispatch()
    }

    fn process_line(&mut self, line: &str, events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if let Some(event) = self.dispatch() {
                events.push(event);
            }
            return;
        }
        // 注释行，例如 `: keep-alive`。
        if line.starts_with(':') {
            return;
        }
        let Some((field, value)) = line.split_once(':') else {
            return;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            // id/retry 等字段目前用不到；未知字段按规范忽略。
            _ => {}
        }
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() {
            // 只有 event 没有 data 的事件不完整，直接丢弃，避免把心跳当数据。
            self.event = None;
            return None;
        }
        Some(SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data),
        })
    }
}

/// 判断响应是否是 SSE。
///
/// 有些兼容端点在 `stream=true` 时仍返回普通 JSON；如果直接按 SSE 解码，会得到
/// 空回复。header 缺失时按 SSE 处理，避免误伤正常流式服务。
pub fn is_event_stream(response: &Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase().contains("text/event-stream"))
        .unwrap_or(true)
}

/// 逐块读取响应体并解码 SSE 事件。
///
/// 用 `Response::chunk()` 而不是引入额外 stream 依赖；LLM SSE 的吞吐量远低于
/// 每次 chunk 的调度成本。
pub async fn consume_sse(
    mut response: Response,
    mut on_event: impl FnMut(SseEvent) -> Result<()>,
) -> Result<()> {
    let mut decoder = SseDecoder::new();
    while let Some(chunk) = response.chunk().await.map_err(http_error)? {
        for event in decoder.push(&chunk) {
            on_event(event)?;
        }
    }
    if let Some(event) = decoder.finish() {
        on_event(event)?;
    }
    Ok(())
}

/// 对 408/429/5xx 做有上限的重试。
///
/// `make` 每次尝试都会重新构造请求。调用方通常在里面 `.json(&body)`，所以 body
/// 会在每次重试时重新序列化——这是有意为之：RequestBuilder 的 body 不能跨请求
/// 复用，且序列化成本远低于一次网络往返。
///
/// 退避从 500ms 起翻倍，最多按 2^5 计算；服务端给了 `Retry-After` 时优先用它。
/// 注意 Client 的总 timeout 是按单次尝试计算的，因此最坏总耗时约为
/// `timeout * (max_retries + 1)`。
pub async fn send_with_retry(
    max_retries: u32,
    mut make: impl FnMut() -> RequestBuilder + Send,
) -> Result<Response> {
    let mut attempt = 0u32;
    loop {
        let response = make().send().await.map_err(http_error)?;
        if !is_retryable(response.status()) || attempt >= max_retries {
            return Ok(response);
        }

        let delay = retry_after(response.headers()).unwrap_or_else(|| {
            Duration::from_millis(500u64.saturating_mul(1u64 << attempt.min(5)))
        });
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

/// 只重试明确表示“稍后再试”的状态码。401/403/404/422 重试没有意义，
/// 400 通常是请求体或工具 schema 的问题，重试只会重复计费。
fn is_retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

/// 只解析整数秒形式。HTTP-date 形式在 LLM 网关里很少见，解析失败时回落到指数退避
/// 更简单；上限 60 秒，避免服务端给出一个让 UI 看起来卡死的值。
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(seconds.min(60)))
}

/// 非 2xx 统一成 `Api` 或 `RateLimited`。body 截断到 1KiB：有些网关会把整个
/// HTML 错误页塞回来，原样放进错误链会让日志和 UI 难以阅读。
pub fn api_error(
    provider: &'static str,
    status: StatusCode,
    body: String,
    retry_after: Option<Duration>,
) -> Error {
    Error::api(
        provider,
        status.as_u16(),
        truncate(&body, 1024),
        retry_after,
    )
}

/// 解析响应体。
///
/// 不直接用 `response.json()`，因为解析失败时拿不到原始 body；把片段放进错误里，
/// 才能区分“模型返回了非预期 JSON”和“代理返回了登录页”。
pub fn parse_response<T: serde::de::DeserializeOwned>(
    provider: &'static str,
    body: &str,
) -> Result<T> {
    serde_json::from_str(body).map_err(|source| {
        Error::ParseResponse(Box::new(ParseResponseError::new(
            provider,
            truncate(body, 1024),
            source,
        )))
    })
}

/// 按 UTF-8 字符边界截断。直接 `&s[..max]` 可能切在中文/emoji 中间而 panic。
pub fn truncate(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_event_split_across_chunks() {
        let mut decoder = SseDecoder::new();
        assert!(decoder
            .push(b"event: content_block_delta\ndata: {\"a\":")
            .is_empty());

        let events = decoder.push(b"1}\n\n");
        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("content_block_delta".into()),
                data: r#"{"a":1}"#.into(),
            }]
        );
    }

    #[test]
    fn handles_crlf_comments_and_multiline_data() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(b": keep-alive\r\ndata: first\r\ndata: second\r\n\r\n");
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "first\nsecond".into(),
            }]
        );
    }

    #[test]
    fn finish_flushes_event_without_trailing_blank_line() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: tail").is_empty());
        assert_eq!(
            decoder.finish(),
            Some(SseEvent {
                event: None,
                data: "tail".into(),
            })
        );
    }

    #[test]
    fn split_utf8_is_not_corrupted() {
        let mut decoder = SseDecoder::new();
        let payload = "data: 北京\n\n".as_bytes();
        let split = payload
            .windows(3)
            .position(|window| window == "北".as_bytes())
            .unwrap()
            + 1;
        assert!(decoder.push(&payload[..split]).is_empty());
        let events = decoder.push(&payload[split..]);
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "北京".into(),
            }]
        );
    }

    #[test]
    fn maps_429_to_rate_limited() {
        let error = api_error(
            "openai",
            StatusCode::TOO_MANY_REQUESTS,
            "slow down".into(),
            Some(Duration::from_secs(1)),
        );
        assert!(matches!(
            error,
            Error::RateLimited {
                provider: "openai",
                retry_after: Some(_),
                ..
            }
        ));
    }
}
