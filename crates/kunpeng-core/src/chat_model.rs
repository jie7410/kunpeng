//! 模型调用抽象。Agent 依赖这个 trait，而不是具体的 provider 实现。
//!
//! 核心层不知道 HTTP、厂商协议或适配器；具体实现由 `kunpeng-provider` 和
//! `kunpeng-adapter-*` 提供，测试可以注入脚本化模型。

use crate::types::{ChatRequest, ChatResponse, StreamControl, StreamDelta};
use crate::{Error, Result};
use async_trait::async_trait;

#[async_trait]
pub trait ChatModel: Send + Sync {
    async fn chat(&self, req: &ChatRequest<'_>) -> Result<ChatResponse>;

    /// 默认回退到非流式：新增模型只实现 `chat` 也能被 Agent 使用。
    ///
    /// 回调借用增量，避免默认回退时为了发事件而 clone 完整内容。思考内容先于
    /// 正文发出，与真实流式的顺序一致；回调返回 `Stop` 时返回 `Error::Cancelled`。
    async fn chat_stream(
        &self,
        req: &ChatRequest<'_>,
        on_delta: &mut (dyn for<'a> FnMut(StreamDelta<'a>) -> StreamControl + Send),
    ) -> Result<ChatResponse> {
        let response = self.chat(req).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatResponse, StreamControl, StreamDelta};

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
            .chat_stream(&request, &mut |delta| {
                deltas.push(match delta {
                    StreamDelta::Text(text) => format!("text:{text}"),
                    StreamDelta::Thinking(text) => format!("thinking:{text}"),
                });
                StreamControl::Continue
            })
            .await
            .unwrap();

        assert_eq!(response.content.as_deref(), Some("hello"));
        assert_eq!(
            deltas,
            vec!["thinking:think".to_string(), "text:hello".to_string()]
        );
    }

    #[tokio::test]
    async fn chat_stream_stops_when_consumer_requests_it() {
        let request = ChatRequest::new(&[], &[]);
        let result = ChatOnlyModel
            .chat_stream(&request, &mut |_| StreamControl::Stop)
            .await;
        assert!(matches!(result, Err(Error::Cancelled)));
    }
}
