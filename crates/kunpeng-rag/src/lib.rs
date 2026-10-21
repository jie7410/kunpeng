use async_trait::async_trait;
use kunpeng_context::{ContextBuilder, TokenBudget};
use kunpeng_core::types::{Content, ContentPart, Message};
use kunpeng_core::Result;
use kunpeng_memory::Memory;
use std::{borrow::Cow, sync::Arc};

/// 在基础上下文之上注入长期记忆。
///
/// 只取最后一条 user 消息作为检索 query，把命中的记忆拼成一条 system 消息，
/// 插在已有 system 之后。记忆为空时直接返回基础上下文，不额外分配。
pub struct RagContextBuilder {
    base: Arc<dyn ContextBuilder>,
    memory: Arc<dyn Memory>,
    limit: usize,
    max_chars: usize,
}

impl RagContextBuilder {
    pub fn new(base: Arc<dyn ContextBuilder>, memory: Arc<dyn Memory>) -> Self {
        Self {
            base,
            memory,
            limit: 4,
            max_chars: 2000,
        }
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars;
        self
    }
}

#[async_trait]
impl ContextBuilder for RagContextBuilder {
    async fn build<'a>(
        &self,
        history: &'a [Message],
        budget: Option<TokenBudget>,
    ) -> Result<Cow<'a, [Message]>> {
        let base = self.base.build(history, budget).await?;
        let Some(query) = latest_user_text(base.as_ref()) else {
            return Ok(base);
        };

        let items = self.memory.recall(&query, self.limit).await?;
        if items.is_empty() {
            return Ok(base);
        }

        let mut rendered = String::new();
        for item in items {
            let text = item.text.trim();
            if text.is_empty() {
                continue;
            }
            let projected = rendered.chars().count() + text.chars().count();
            if projected > self.max_chars {
                break;
            }
            rendered.push_str("- ");
            rendered.push_str(text);
            rendered.push('\n');
        }
        if rendered.trim().is_empty() {
            return Ok(base);
        }

        let mut messages = base.into_owned();
        // 保留已有 system 在最前，记忆放在其后，正文 user 之前。
        let index = messages
            .iter()
            .take_while(|message| matches!(message, Message::System { .. }))
            .count();
        messages.insert(index, Message::system(format!("相关记忆：\n{rendered}")));
        Ok(Cow::Owned(messages))
    }
}

fn latest_user_text(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| match message {
        Message::User { content } => {
            let text = content_text(content);
            (!text.trim().is_empty()).then_some(text)
        }
        _ => None,
    })
}

fn content_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kunpeng_context::SlidingWindowContext;
    use kunpeng_memory::MemoryItem;

    struct StaticMemory {
        items: Vec<MemoryItem>,
        query: std::sync::Mutex<Option<String>>,
    }

    #[async_trait]
    impl Memory for StaticMemory {
        async fn remember(&self, _item: MemoryItem) -> Result<()> {
            Ok(())
        }

        async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>> {
            *self.query.lock().unwrap() = Some(query.to_string());
            Ok(self.items.iter().take(limit).cloned().collect())
        }
    }

    #[tokio::test]
    async fn injects_memory_after_system_messages() {
        let memory = Arc::new(StaticMemory {
            items: vec![MemoryItem::new("用户喜欢 Rust")],
            query: std::sync::Mutex::new(None),
        });
        let builder =
            RagContextBuilder::new(Arc::new(SlidingWindowContext::default()), memory.clone());
        let history = vec![Message::system("system"), Message::user("我学过什么？")];

        let result = builder.build(&history, None).await.unwrap();
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0], Message::System { .. }));
        assert!(
            matches!(&result[1], Message::System { content } if content.contains("用户喜欢 Rust"))
        );
        assert_eq!(
            memory.query.lock().unwrap().as_deref(),
            Some("我学过什么？")
        );
    }

    #[tokio::test]
    async fn empty_memory_returns_base() {
        let memory = Arc::new(StaticMemory {
            items: vec![],
            query: std::sync::Mutex::new(None),
        });
        let builder = RagContextBuilder::new(Arc::new(SlidingWindowContext::default()), memory);
        let history = vec![Message::user("hi")];
        let result = builder.build(&history, None).await.unwrap();
        assert_eq!(result.len(), 1);
    }
}
