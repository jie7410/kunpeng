use crate::token::{ApproxTokenCounter, TokenCounter};
use async_trait::async_trait;
use kunpeng_core::types::Message;
use kunpeng_core::{Result, TokenBudget};
use std::{borrow::Cow, sync::Arc};

/// 把会话历史转换成这一次请求实际发送的消息。
///
/// 返回 `Cow`：没有裁剪时直接借用原历史，避免每轮复制整段会话。
#[async_trait]
pub trait ContextBuilder: Send + Sync {
    async fn build<'a>(
        &self,
        history: &'a [Message],
        budget: Option<TokenBudget>,
    ) -> Result<Cow<'a, [Message]>>;
}

/// 按“轮次”从旧到新裁剪历史。
///
/// 一轮从 User 开始，包含后续 Assistant/Tool 消息。裁剪时整轮丢弃，避免把
/// assistant 的 tool_calls 和对应 tool_result 拆开。system 消息始终保留，
/// 最新一轮即使超出预算也保留，否则请求会失去当前问题。
#[derive(Clone)]
pub struct SlidingWindowContext {
    counter: Arc<dyn TokenCounter>,
}

impl Default for SlidingWindowContext {
    fn default() -> Self {
        Self::new(Arc::new(ApproxTokenCounter))
    }
}

impl SlidingWindowContext {
    pub fn new(counter: Arc<dyn TokenCounter>) -> Self {
        Self { counter }
    }

    fn trim<'a>(&self, history: &'a [Message], budget: Option<TokenBudget>) -> Cow<'a, [Message]> {
        let Some(budget) = budget else {
            return Cow::Borrowed(history);
        };
        if budget.max_prompt_tokens == 0 {
            return Cow::Borrowed(history);
        }

        let mut keep = vec![false; history.len()];
        let mut used = 0;
        for (index, message) in history.iter().enumerate() {
            if matches!(message, Message::System { .. }) {
                keep[index] = true;
                used += self.counter.count_message(message);
            }
        }

        let groups = turn_groups(history);
        if groups.is_empty() {
            return Cow::Owned(
                history
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| keep[*index])
                    .map(|(_, message)| message.clone())
                    .collect(),
            );
        }

        let last_group = groups.len() - 1;
        for (index, group) in groups.iter().enumerate().rev() {
            let tokens: usize = group
                .iter()
                .map(|&message_index| self.counter.count_message(&history[message_index]))
                .sum();

            // 最新一轮必须保留；更旧的轮次只有在预算允许时才加入。
            if index == last_group || used + tokens <= budget.max_prompt_tokens {
                for &message_index in group {
                    keep[message_index] = true;
                }
                used += tokens;
            } else {
                break;
            }
        }

        Cow::Owned(
            history
                .iter()
                .enumerate()
                .filter(|(index, _)| keep[*index])
                .map(|(_, message)| message.clone())
                .collect(),
        )
    }
}

#[async_trait]
impl ContextBuilder for SlidingWindowContext {
    async fn build<'a>(
        &self,
        history: &'a [Message],
        budget: Option<TokenBudget>,
    ) -> Result<Cow<'a, [Message]>> {
        Ok(self.trim(history, budget))
    }
}

/// 按 User 消息切分轮次。孤立的 Assistant/Tool（例如从历史中间恢复）也会
/// 形成一个独立轮次，保证裁剪时不会漏掉工具结果。
fn turn_groups(history: &[Message]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (index, message) in history.iter().enumerate() {
        if matches!(message, Message::System { .. }) {
            continue;
        }
        if matches!(message, Message::User { .. }) || groups.is_empty() {
            groups.push(vec![index]);
        } else if let Some(group) = groups.last_mut() {
            group.push(index);
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use kunpeng_core::types::{ContentPart, ToolCall};

    fn history() -> Vec<Message> {
        vec![
            Message::system("system"),
            Message::user("old"),
            Message::assistant("old answer"),
            Message::user("new"),
            Message::assistant("new answer"),
        ]
    }

    #[tokio::test]
    async fn no_budget_keeps_everything() {
        let context = SlidingWindowContext::default();
        let history = history();
        let result = context.build(&history, None).await.unwrap();
        assert_eq!(result.len(), 5);
    }

    #[tokio::test]
    async fn trims_old_turns_and_keeps_system() {
        let context = SlidingWindowContext::default();
        let history = history();
        let result = context
            .build(&history, Some(TokenBudget::new(20)))
            .await
            .unwrap();
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0], Message::System { .. }));
        assert!(matches!(&result[1], Message::User { .. }));
        assert!(matches!(&result[2], Message::Assistant { .. }));
    }

    #[tokio::test]
    async fn keeps_tool_call_and_result_together() {
        let history = vec![
            Message::system("system"),
            Message::user("question"),
            Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({}),
                }],
                thinking: None,
                citations: vec![],
                server_tool_uses: vec![],
                provider_data: None,
            },
            Message::tool_result("call-1", "result", false),
        ];

        let context = SlidingWindowContext::default();
        let result = context
            .build(&history, Some(TokenBudget::new(1)))
            .await
            .unwrap();

        assert_eq!(result.len(), 4);
    }

    #[tokio::test]
    async fn multimodal_parts_still_count() {
        let history = vec![Message::user_parts(vec![ContentPart::Text {
            text: "hello".into(),
        }])];
        let context = SlidingWindowContext::default();
        let result = context
            .build(&history, Some(TokenBudget::new(1)))
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
    }
}
