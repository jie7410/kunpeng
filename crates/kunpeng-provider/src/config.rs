//! 配置层：适配器解析、默认值补齐、构建 `Model`。
//!
//! 这里不读取环境变量。CLI、Tauri、测试或服务端入口各自负责把外部配置映射成
//! `LlmConfig`；核心库只处理已经验证过的值。
//!
//! 协议默认地址/模型由 `Adapter` 自己声明；`LlmConfig` 不认识任何具体厂商。

use crate::{model::Model, provider::Provider, registry::AdapterRegistry};
use kunpeng_core::error::{Error, Result};
use std::{fmt, sync::Arc};

#[derive(Clone)]
#[non_exhaustive]
pub struct LlmConfig {
    /// 协议名或别名，例如 `openai`、`anthropic`、`zhipu`。
    pub protocol: String,
    /// 为空时使用适配器声明的默认地址。兼容端点可以由适配器返回空串。
    pub base_url: Option<String>,
    pub api_key: String,
    /// 为空时使用适配器声明的默认模型；兼容端点可以由适配器返回空串。
    pub model: Option<String>,
    /// 单次 HTTP 请求的总超时（连接 + 读完 body）。
    pub timeout_secs: u64,
    /// 429/408/5xx 的最大重试次数。
    pub max_retries: u32,
}

// 不能 derive Debug：默认实现会把 api_key 原样打印出来。panic、日志或 UI 的
// 调试面板一旦格式化配置就会泄露密钥。
impl fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LlmConfig")
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("api_key", &"***")
            .field("model", &self.model)
            .field("timeout_secs", &self.timeout_secs)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

impl LlmConfig {
    /// 程序化构造。默认值保持保守：120 秒超时、2 次重试。
    pub fn new(protocol: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            protocol: protocol.into().trim().to_string(),
            base_url: None,
            api_key: api_key.into().trim().to_string(),
            model: None,
            timeout_secs: 120,
            max_retries: 2,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// 从注册表解析适配器，补齐默认地址/模型，构造共享的 Provider 和 Model。
    pub fn build(&self, registry: &AdapterRegistry) -> Result<Model> {
        let configured_protocol = self.protocol.trim();
        let protocol_name = if configured_protocol.is_empty() {
            "openai"
        } else {
            configured_protocol
        };
        let adapter = registry.resolve(protocol_name)?;

        let base_url = self
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| adapter.default_base_url().to_string());
        if base_url.is_empty() {
            return Err(Error::InvalidConfig(format!(
                "adapter {} 没有默认 base_url，请显式设置",
                adapter.name()
            )));
        }

        let model_name = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| adapter.default_model().to_string());
        if model_name.is_empty() {
            return Err(Error::InvalidConfig(format!(
                "adapter {} 没有默认模型，请显式设置",
                adapter.name()
            )));
        }

        // Provider 用 Arc 共享：同一个连接池可以挂多个 Model，后续如果要做
        // “同一网关下切换模型”，不需要重新建 Client。
        let provider = Arc::new(
            Provider::new(base_url, self.api_key.clone(), adapter, self.timeout_secs)?
                .with_retries(self.max_retries),
        );
        Ok(Model::new(provider, model_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{Adapter, CallContext};
    use async_trait::async_trait;
    use kunpeng_core::types::{ChatRequest, ChatResponse};

    struct TestAdapter;

    #[async_trait]
    impl Adapter for TestAdapter {
        fn name(&self) -> &'static str {
            "test"
        }

        fn default_base_url(&self) -> &'static str {
            "https://example.invalid"
        }

        fn default_model(&self) -> &'static str {
            "test-model"
        }

        async fn chat(
            &self,
            _ctx: &CallContext<'_>,
            _req: &ChatRequest<'_>,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse::new("test-model"))
        }
    }

    struct BareAdapter;

    #[async_trait]
    impl Adapter for BareAdapter {
        fn name(&self) -> &'static str {
            "bare"
        }

        async fn chat(
            &self,
            _ctx: &CallContext<'_>,
            _req: &ChatRequest<'_>,
        ) -> Result<ChatResponse> {
            Err(Error::Other("unused".into()))
        }
    }

    fn registry() -> AdapterRegistry {
        let mut registry = AdapterRegistry::new();
        registry.register(TestAdapter).unwrap();
        registry.register(BareAdapter).unwrap();
        registry
    }

    #[test]
    fn debug_redacts_api_key() {
        let config = LlmConfig::new("test", "super-secret");
        let debug = format!("{config:?}");
        assert!(!debug.contains("super-secret"));
        assert!(debug.contains("***"));
    }

    #[test]
    fn adapter_defaults_are_used() {
        let model = LlmConfig::new("test", "key").build(&registry()).unwrap();
        assert_eq!(model.name(), "test-model");
    }

    #[test]
    fn empty_api_key_is_rejected() {
        let error = LlmConfig::new("test", "   ")
            .build(&registry())
            .unwrap_err();
        assert!(matches!(error, Error::InvalidConfig(_)));
    }

    #[test]
    fn adapter_without_defaults_requires_explicit_values() {
        let error = LlmConfig::new("bare", "key")
            .build(&registry())
            .unwrap_err();
        assert!(error.to_string().contains("base_url"));
    }

    #[test]
    fn unknown_adapter_is_reported() {
        let error = LlmConfig::new("missing", "key")
            .build(&registry())
            .unwrap_err();
        assert!(matches!(error, Error::UnsupportedProvider(_)));
    }
}
