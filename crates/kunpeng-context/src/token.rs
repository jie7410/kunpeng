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
                ..
            } => {
                content
                    .as_deref()
                    .map(|text| self.count_text(text))
                    .unwrap_or(0)
                    + thinking
                        .as_deref()
                        .map(|text| self.count_text(text))
                        .unwrap_or(0)
                    + tool_calls
                        .iter()
                        .map(|call| {
                            self.count_text(&call.name)
                                + self.count_text(&call.arguments.to_string())
                        })
                        .sum::<usize>()
                    + citations
                        .iter()
                        .map(|citation| self.count_text(&citation.to_string()))
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
}
