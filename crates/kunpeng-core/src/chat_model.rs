//! 模型调用抽象。Agent 依赖这个 trait，而不是具体的 provider 实现。
//!
//! 核心层不知道 HTTP、厂商协议或适配器；具体实现由 `kunpeng-provider` 和
//! `kunpeng-adapter-*` 提供，测试可以注入脚本化模型。

use crate::types::{ChatRequest, ChatResponse, StreamDelta};
use crate::Result;
use async_trait::async_trait;

#[async_trait]
pub trait ChatModel: Send + Sync {
    async fn chat(&self, req: &ChatRequest<'_>) -> Result<ChatResponse>;

    /// 默认回退到非流式：新增模型只实现 `chat` 也能被 Agent 使用。
    async fn chat_stream(
        &self,
        req: &ChatRequest<'_>,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<ChatResponse> {
        let response = self.chat(req).await?;
        if let Some(content) = &response.content {
            on_delta(StreamDelta::Text(content.clone()));
        }
        if let Some(thinking) = &response.thinking {
            on_delta(StreamDelta::Thinking(thinking.clone()));
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatResponse, StreamDelta};

    struct ChatOnlyModel;

    #[async_trait]
    impl ChatModel for ChatOnlyModel {
        async fn chat(&self, _req: &ChatRequest<'_>) -> Result<ChatResponse> {
            let mut response = ChatResponse::new("mock");
            response.content = Some("hello".into());
            response.thinking = Some("think".into());
            Ok(response)
        }
    }

    #[tokio::test]
    async fn chat_stream_defaults_to_chat() {
        let request = ChatRequest::new(&[], &[]);
        let mut deltas = Vec::new();
        let response = ChatOnlyModel
            .chat_stream(&request, &mut |delta| deltas.push(delta))
            .await
            .unwrap();

        assert_eq!(response.content.as_deref(), Some("hello"));
        assert_eq!(
            deltas,
            vec![
                StreamDelta::Text("hello".into()),
                StreamDelta::Thinking("think".into())
            ]
        );
    }
}
