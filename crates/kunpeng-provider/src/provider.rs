//! 连接层：一个 `Provider` 固定 base URL、API key、协议适配器和重试策略。
//!
//! 协议差异全部收敛在 `Adapter` 实现里；这里只负责复用 HTTP Client、
//! 校验配置、构造借用上下文。`Provider` 本身无状态，可被多个 `Model` 共享。

use crate::adapter::{http_error, Adapter, CallContext};
use kunpeng_core::error::{Error, Result};
use kunpeng_core::types::{ChatRequest, ChatResponse, StreamDelta};
use std::{sync::Arc, time::Duration};

pub struct Provider {
    /// 复用一个 Client，连接池和 TLS 会话才能跨请求复用。
    http: reqwest::Client,
    /// 已去掉尾部 `/`，协议层直接拼路径。
    base_url: String,
    api_key: String,
    adapter: Arc<dyn Adapter>,
    max_retries: u32,
}

impl Provider {
    /// 构造时做完全部静态校验，避免请求发出后才发现 URL 或 timeout 不合法。
    ///
    /// `timeout_secs` 是整次 HTTP 请求的上限（连接 + 发送 + 读完 body），不是空闲
    /// 超时。流式响应如果以后要接，需要单独的 idle timeout，不能直接复用这个值。
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        adapter: Arc<dyn Adapter>,
        timeout_secs: u64,
    ) -> Result<Self> {
        if timeout_secs == 0 {
            return Err(Error::InvalidConfig("timeout_secs 必须大于 0".into()));
        }

        let base_url = normalize_base_url(base_url.into());
        if base_url.is_empty() {
            return Err(Error::InvalidConfig("base_url 不能为空".into()));
        }
        reqwest::Url::parse(&base_url)
            .map_err(|e| Error::InvalidConfig(format!("base_url 无效: {e}")))?;

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(http_error)?;
        let api_key = api_key.into().trim().to_string();
        if api_key.is_empty() {
            return Err(Error::InvalidConfig("api_key 不能为空".into()));
        }
        Ok(Self {
            http,
            base_url,
            api_key,
            adapter,
            max_retries: 2,
        })
    }

    /// 重试次数上限单独限制到 10。配置错误时宁可少重试，也不要把一次 UI 操作
    /// 挂在一个理论上的长退避链上。
    pub fn with_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries.min(10);
        self
    }

    pub(crate) async fn chat(&self, model: &str, req: &ChatRequest<'_>) -> Result<ChatResponse> {
        let ctx = self.context(model);
        self.adapter.chat(&ctx, req).await
    }

    pub(crate) async fn chat_stream(
        &self,
        model: &str,
        req: &ChatRequest<'_>,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<ChatResponse> {
        let ctx = self.context(model);
        self.adapter.chat_stream(&ctx, req, on_delta).await
    }

    /// 构造借用上下文。base_url/api_key 只读，不复制；model 由调用方传入，
    /// 因为同一个 Provider 可以被多个 Model 共用。
    fn context<'a>(&'a self, model: &'a str) -> CallContext<'a> {
        CallContext {
            http: &self.http,
            base_url: &self.base_url,
            api_key: &self.api_key,
            model,
            max_retries: self.max_retries,
        }
    }
}

/// 只去掉首尾空白和尾部 `/`。不在 URL 层面做路径归一化，因为兼容端点的前缀
/// 差异很大（`/v1`、`/api/paas/v4` 等），猜错比不猜更糟。
fn normalize_base_url(value: String) -> String {
    value.trim().trim_end_matches('/').to_string()
}
