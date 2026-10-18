//! Provider 与具体协议之间的最小接口，以及各协议共用的 HTTP 工具。
//!
//! 这里不持有配置或状态，请求上下文按引用传入，目的是让协议适配器可以在测试里
//! 用 mock 替换，也避免每个请求复制 base_url/api_key。

use crate::llm::error::{Error, Result};
use crate::llm::types::{ChatRequest, ChatResponse};
use async_trait::async_trait;
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
    pub max_retries: u32,
}

/// 协议适配器。
///
/// 用 `async_trait` 是因为原生 async fn trait 目前不能直接作为 `dyn Protocol`
/// 使用；一次 Box 分配相对于秒级 LLM 请求可以忽略。`req` 按引用传入，适配器只能
/// 读取历史，不能修改 Agent 的状态。
#[async_trait]
pub trait Protocol: Send + Sync {
    async fn chat(&self, ctx: &CallContext<'_>, req: &ChatRequest<'_>) -> Result<ChatResponse>;
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
pub(crate) async fn send_with_retry(
    max_retries: u32,
    mut make: impl FnMut() -> RequestBuilder + Send,
) -> Result<Response> {
    let mut attempt = 0u32;
    loop {
        let response = make().send().await?;
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
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(seconds.min(60)))
}

/// 非 2xx 统一成 `Error::Api`。body 截断到 1KiB：有些网关会把整个 HTML 错误页
/// 塞回来，原样放进错误链会让日志和 UI 难以阅读。
pub(crate) fn api_error(status: StatusCode, body: String) -> Error {
    Error::Api {
        status: status.as_u16(),
        body: truncate(&body, 1024),
    }
}

/// 解析响应体。
///
/// 不直接用 `response.json()`，因为解析失败时拿不到原始 body；把片段放进错误里，
/// 才能区分“模型返回了非预期 JSON”和“代理返回了登录页”。
pub(crate) fn parse_response<T: serde::de::DeserializeOwned>(
    provider: &'static str,
    body: &str,
) -> Result<T> {
    serde_json::from_str(body).map_err(|source| Error::ParseResponse {
        provider,
        body: truncate(body, 1024),
        source,
    })
}

/// 按 UTF-8 字符边界截断。直接 `&s[..max]` 可能切在中文/emoji 中间而 panic。
pub(crate) fn truncate(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}
