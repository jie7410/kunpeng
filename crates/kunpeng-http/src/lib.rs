//! 通用 HTTP 传输工具：reqwest 错误映射、SSE 增量解码和限时 body 读取。
//!
//! 这些能力同时被三类使用方需要：provider 的协议适配器、A2A/ANP 远程协议客户端、
//! 内置 http 工具。放在独立 crate 是为了不让 a2a/anp 反向依赖 LLM provider 层，
//! 也避免同一份 reqwest 错误映射 / SSE 解码器被复制多份后各自漂移。
//!
//! `kunpeng-core` 保持不依赖 HTTP；这里只依赖 core 的 `Error` / `HttpErrorKind`，
//! 不引入任何 provider 或 Agent 概念。

use kunpeng_core::error::{Error, Result};
use kunpeng_core::HttpErrorKind;
use reqwest::header::CONTENT_TYPE;
use reqwest::Response;
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

/// 读取完整响应体，超过 `timeout` 判定为超时。
///
/// 不用 Client 级或请求级总超时：它们覆盖整个响应体读取，会误杀合法的长流式
/// 生成。这里只给“已收到响应头之后”的 body 读取单独限时。
pub async fn read_body(response: Response, timeout: Duration) -> Result<String> {
    match tokio::time::timeout(timeout, response.text()).await {
        Ok(result) => result.map_err(http_error),
        Err(_) => Err(Error::timeout("响应体读取", timeout)),
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
///
/// 行处理按“先扫描全部行边界、再一次性截断已消费前缀”的方式进行；每行单独
/// `drain` 会反复搬移剩余缓冲，行数多时退化为 O(n²)。
#[derive(Default)]
pub struct SseDecoder {
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
        let mut consumed = 0usize;
        let mut start = 0usize;
        while let Some(offset) = self.buffer[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset;
            let mut line_end = end;
            if line_end > start && self.buffer[line_end - 1] == b'\r' {
                line_end -= 1;
            }
            // 行内容借用 buffer，而 event/data 是独立字段，字段级借用不冲突。
            let line = String::from_utf8_lossy(&self.buffer[start..line_end]);
            process_line(&mut self.event, &mut self.data, line.as_ref(), &mut events);
            start = end + 1;
            consumed = start;
        }
        if consumed > 0 {
            self.buffer.drain(..consumed);
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
            let line = String::from_utf8_lossy(&line).into_owned();
            let mut events = Vec::new();
            process_line(&mut self.event, &mut self.data, &line, &mut events);
            if let Some(event) = events.into_iter().next() {
                return Some(event);
            }
        }
        dispatch(&mut self.event, &mut self.data)
    }
}

/// 处理一行。拆成自由函数并按字段传引用，`push` 才能同时借用 buffer 与
/// 可变借用 event/data（同一 struct 的不同字段）。
fn process_line(
    event: &mut Option<String>,
    data: &mut String,
    line: &str,
    events: &mut Vec<SseEvent>,
) {
    if line.is_empty() {
        if let Some(event) = dispatch(event, data) {
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
        "event" => *event = Some(value.to_string()),
        "data" => {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value);
        }
        // id/retry 等字段目前用不到；未知字段按规范忽略。
        _ => {}
    }
}

fn dispatch(event: &mut Option<String>, data: &mut String) -> Option<SseEvent> {
    if data.is_empty() {
        // 只有 event 没有 data 的事件不完整，直接丢弃，避免把心跳当数据。
        *event = None;
        return None;
    }
    Some(SseEvent {
        event: event.take(),
        data: std::mem::take(data),
    })
}

/// 逐块读取响应体并解码 SSE 事件。
///
/// `idle` 是相邻 chunk 之间的最大等待时间。流式响应的总时长不设上限——一次长
/// 生成可能远超任何合理的总超时——但服务端长时间不发数据会被判定为挂起并中断。
///
/// 用 `Response::chunk()` 而不是引入额外 stream 依赖；LLM SSE 的吞吐量远低于
/// 每次 chunk 的调度成本。
pub async fn consume_sse(
    mut response: Response,
    idle: Duration,
    mut on_event: impl FnMut(SseEvent) -> Result<()>,
) -> Result<()> {
    let mut decoder = SseDecoder::new();
    loop {
        let chunk = match tokio::time::timeout(idle, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => break,
            Ok(Err(error)) => return Err(http_error(error)),
            Err(_) => return Err(Error::timeout("SSE 流式数据", idle)),
        };
        for event in decoder.push(&chunk) {
            on_event(event)?;
        }
    }
    if let Some(event) = decoder.finish() {
        on_event(event)?;
    }
    Ok(())
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
    fn many_lines_in_one_chunk_are_linear() {
        // 一个 chunk 里带很多行：行边界扫描只前进不回退，验证解码结果完整。
        let mut payload = Vec::new();
        for index in 0..100 {
            payload.extend_from_slice(format!("data: line-{index}\n\n").as_bytes());
        }
        let mut decoder = SseDecoder::new();
        let events = decoder.push(&payload);
        assert_eq!(events.len(), 100);
        assert_eq!(events[99].data, "line-99");
        assert!(decoder.finish().is_none());
    }

    #[tokio::test]
    async fn consume_sse_times_out_when_stream_stalls() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // 先读掉请求，再发送一个事件后保持连接静默。
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            let head =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(b"data: {\"a\":1}\n\n").unwrap();
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(500));
        });

        let response = reqwest::Client::new()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap();
        let mut seen = Vec::new();
        let result = consume_sse(response, Duration::from_millis(50), |event| {
            seen.push(event);
            Ok(())
        })
        .await;

        assert!(matches!(
            result,
            Err(Error::Http {
                kind: HttpErrorKind::Timeout,
                ..
            })
        ));
        assert_eq!(seen.len(), 1);
        let _ = server.join();
    }
}
