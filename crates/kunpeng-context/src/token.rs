use kunpeng_core::types::{Content, ContentPart, Message};

/// Token 计数器。实现可以替换为 provider 的精确 tokenizer。
pub trait TokenCounter: Send + Sync {
    fn count_text(&self, text: &str) -> usize;
    fn count_message(&self, message: &Message) -> usize;

    fn count_messages(&self, messages: &[Message]) -> usize {
        messages
            .iter()
            .map(|message| self.count_message(message))
            .sum()
    }
}

/// 粗略计数器：4 个字符约 1 token。图片按固定 1000 token 估算。
///
/// 它不追求精确，只用于决定“是否该裁剪历史”。真正的 token 超限仍由 provider
/// 返回错误，后续可以加精确 tokenizer。
#[derive(Debug, Clone, Copy, Default)]
pub struct ApproxTokenCounter;

const MESSAGE_OVERHEAD: usize = 4;
const IMAGE_TOKENS: usize = 1000;

impl TokenCounter for ApproxTokenCounter {
    fn count_text(&self, text: &str) -> usize {
        text.chars().count().div_ceil(4)
    }

    fn count_message(&self, message: &Message) -> usize {
        let tokens = match message {
            Message::System { content }
            | Message::User {
                content: Content::Text(content),
            } => self.count_text(content),
            Message::User {
                content: Content::Parts(parts),
            } => parts
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } => self.count_text(text),
                    ContentPart::Image { .. } => IMAGE_TOKENS,
                    ContentPart::Other { data, .. } => self.count_text(&data.to_string()),
                    _ => 0,
                })
                .sum(),
            Message::Assistant {
                content,
                tool_calls,
                thinking,
                citations,
                server_tool_uses,
                provider_data,
                ..
            } => {
                // provider_data 优先：它是 provider 的原始 block 数组，比展示用的
                // thinking 字段更完整（含 signature、redacted_thinking 等回传时
                // 必须占预算的内容）。没有 provider_data 时才单独计 thinking，
                // 避免两处对同一段思考文本重复计数。
                let thinking_tokens = match provider_data {
                    Some(data) => self.count_text(&data.data.to_string()),
                    None => thinking
                        .as_deref()
                        .map(|text| self.count_text(text))
                        .unwrap_or(0),
                };
                content
                    .as_deref()
                    .map(|text| self.count_text(text))
                    .unwrap_or(0)
                    + thinking_tokens
                    + tool_calls
                        .iter()
                        .map(|call| {
                            self.count_text(&call.id)
                                + self.count_text(&call.name)
                                + self.count_text(&call.arguments.to_string())
                        })
                        .sum::<usize>()
                    + citations
                        .iter()
                        .map(|citation| self.count_text(&citation.to_string()))
                        .sum::<usize>()
                    + server_tool_uses
                        .iter()
                        .map(|tool| self.count_text(&tool.input.to_string()))
                        .sum::<usize>()
            }
            Message::Tool {
                tool_call_id,
                content,
                ..
            } => self.count_text(tool_call_id) + self.count_text(content),
            _ => 0,
        };
        tokens + MESSAGE_OVERHEAD
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_text_and_images() {
        let counter = ApproxTokenCounter;
        assert_eq!(counter.count_text("abcd"), 1);
        assert_eq!(counter.count_text("abcde"), 2);
        let image = Message::user_parts(vec![ContentPart::Image {
            source: kunpeng_core::types::ImageSource::Url {
                url: "https://example.com/a.png".into(),
            },
            detail: None,
        }]);
        assert!(counter.count_message(&image) >= IMAGE_TOKENS);
    }

    #[test]
    fn provider_data_counts_toward_budget() {
        use kunpeng_core::types::ProviderData;

        let counter = ApproxTokenCounter;
        let base = |provider_data| Message::Assistant {
            content: Some("答案".into()),
            tool_calls: vec![],
            thinking: Some("想一想".into()),
            citations: vec![],
            server_tool_uses: vec![],
            provider_data,
        };
        // provider_data 序列化后的体积（含 signature 等）比展示字段更大，
        // 计数必须体现出来，否则长会话会系统性低估用量。
        let without = counter.count_message(&base(None));
        let with = counter.count_message(&base(Some(ProviderData::new(
            "anthropic",
            serde_json::json!([
                {"type": "thinking", "thinking": "想一想", "signature": "sig-0123456789"},
                {"type": "redacted_thinking", "data": "opaque-redacted-block"}
            ]),
        ))));
        assert!(with > without);
    }
}
