use crate::llm::provider::Provider;
use crate::llm::types::{ChatRequest, ChatResponse};
use crate::llm::Result;
use std::{fmt, sync::Arc};

/// 模型级默认值。请求里的同名字段优先；请求留空时才回落到这里。
///
/// 放在 `Model` 而不是 `Provider`，因为同一个网关下不同模型的 token 上限、
/// 默认温度可能不同。
#[derive(Debug, Clone, Default)]
pub struct ModelDefault {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

/// Provider + 模型名 + 默认参数。克隆成本只有一次 `Arc` 计数和一次 String。
///
/// `Model` 不持有会话状态，可以被多个 `Agent` / 多个并发请求共享。
#[derive(Clone)]
pub struct Model {
    provider: Arc<Provider>,
    name: String,
    defaults: ModelDefault,
}

// 手写 Debug 而不是 derive：`Provider` 里有 API key 和 HTTP Client，不适合展开。
impl fmt::Debug for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Model")
            .field("name", &self.name)
            .field("defaults", &self.defaults)
            .finish_non_exhaustive()
    }
}

impl Model {
    pub fn new(provider: Arc<Provider>, name: impl Into<String>) -> Self {
        Self {
            provider,
            name: name.into(),
            defaults: ModelDefault::default(),
        }
    }

    pub fn with_defaults(mut self, defaults: ModelDefault) -> Self {
        self.defaults = defaults;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// 补齐默认参数后发请求。
    ///
    /// 这里构造一个新的 `ChatRequest` 而不是改调用方传进来的值：请求是借用的，
    /// 没法原地修改；构造的副本只有两个 `Option` 被替换，消息和工具仍是切片引用。
    pub async fn chat(&self, req: &ChatRequest<'_>) -> Result<ChatResponse> {
        let req = ChatRequest {
            temperature: req.temperature.or(self.defaults.temperature),
            max_tokens: req.max_tokens.or(self.defaults.max_tokens),
            ..*req
        };
        self.provider.chat(&self.name, &req).await
    }

    /// 流式调用。默认参数补齐逻辑与非流式一致；文本增量通过回调交给 Agent/UI。
    pub async fn chat_stream(
        &self,
        req: &ChatRequest<'_>,
        on_delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<ChatResponse> {
        let req = ChatRequest {
            temperature: req.temperature.or(self.defaults.temperature),
            max_tokens: req.max_tokens.or(self.defaults.max_tokens),
            ..*req
        };
        self.provider.chat_stream(&self.name, &req, on_delta).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::error::Error;
    use crate::llm::provider::protocol::{CallContext, Protocol};
    use crate::llm::types::ChatRequest;
    use async_trait::async_trait;

    struct UnusedProtocol;

    #[async_trait]
    impl Protocol for UnusedProtocol {
        async fn chat(
            &self,
            _ctx: &CallContext<'_>,
            _req: &ChatRequest<'_>,
        ) -> Result<ChatResponse> {
            Err(Error::Other("unused".into()))
        }
    }

    #[test]
    fn name_does_not_recurse() {
        let provider = Arc::new(
            Provider::new(
                "https://example.invalid",
                "key",
                Arc::new(UnusedProtocol),
                5,
            )
            .unwrap(),
        );
        let model = Model::new(provider, "test-model");
        assert_eq!(model.name(), "test-model");
    }
}
