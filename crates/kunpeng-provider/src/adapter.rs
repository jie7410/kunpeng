//! Provider 与具体协议之间的最小接口，以及各协议共用的请求工具。
//!
//! 这里不持有配置或状态，请求上下文按引用传入，目的是让协议适配器可以在测试里
//! 用 mock 替换，也避免每个请求复制 base_url/api_key。
//!
//! 通用 HTTP 能力（reqwest 错误映射、SSE 解码、限时 body 读取）在 `kunpeng-http`
//! 里，这里 re-export 是让协议适配器只需要依赖 provider 一个上游。

use async_trait::async_trait;
use kunpeng_core::error::{Error, ParseResponseError, Result};
use kunpeng_core::types::{ChatRequest, ChatResponse, StreamControl, StreamDelta};
pub use kunpeng_http::{consume_sse, http_error, is_event_stream, read_body};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{RequestBuilder, Response, StatusCode};
use std::time::Duration;

/// 单次请求的共享上下文。
///
/// 字段都是借用：`Provider` 生命周期覆盖整个请求，没必要为每次调用克隆字符串。
pub struct CallContext<'a> {
    pub http: &'a reqwest::Client,
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub model: &'a str,
    /// 单次尝试各等待阶段的上限：等待响应头（含建连）、读取非流式 body、
    /// 流式响应相邻 chunk 的间隔。不是整次请求的总时长——长流式生成可以合法
    /// 超过任何总上限，只有某一阶段停滞才应中断。
    pub timeout: Duration,
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

/// 对 408/429/5xx 做有上限的重试。
///
/// `make` 每次尝试都会重新构造请求。调用方通常在里面 `.json(&body)`，所以 body
/// 会在每次重试时重新序列化——这是有意为之：RequestBuilder 的 body 不能跨请求
/// 复用，且序列化成本远低于一次网络往返。
///
/// `timeout` 限制的是单次尝试从发起到收到响应头的时间（含建连）。非流式 body
/// 读取由调用方用 `read_body` 限制，流式由 `consume_sse` 的空闲超时限制——不能
/// 在这里设整次请求的总上限，否则长流式生成会被误杀。传输错误和超时不重试，
/// 只有明确表示“稍后再试”的状态码才重试。
///
/// 退避从 500ms 起翻倍，最多按 2^5 计算；服务端给了 `Retry-After` 时优先用它。
pub async fn send_with_retry(
    max_retries: u32,
    timeout: Duration,
    mut make: impl FnMut() -> RequestBuilder + Send,
) -> Result<Response> {
    let mut attempt = 0u32;
    loop {
        let response = match tokio::time::timeout(timeout, make().send()).await {
            Ok(result) => result.map_err(http_error)?,
            Err(_) => return Err(Error::timeout("等待响应头", timeout)),
        };
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
