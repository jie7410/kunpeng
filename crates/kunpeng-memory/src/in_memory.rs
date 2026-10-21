use crate::{Memory, MemoryItem, SessionStore};
use async_trait::async_trait;
use kunpeng_core::types::Message;
use kunpeng_core::Result;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        RwLock,
    },
};

/// 轻量内存记忆：关键词/子串评分，适合本地开发和小规模数据。
///
/// 它不是语义检索；需要向量检索时实现 `Memory` 并接入 `RagContextBuilder`。
pub struct InMemoryMemory {
    items: RwLock<Vec<MemoryItem>>,
    next_id: AtomicU64,
}

impl Default for InMemoryMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryMemory {
    pub fn new() -> Self {
        Self {
            items: RwLock::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn len(&self) -> usize {
        self.items.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl Memory for InMemoryMemory {
    async fn remember(&self, mut item: MemoryItem) -> Result<()> {
        if item.id.is_empty() {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            item.id = format!("memory-{id}");
        }
        self.items.write().unwrap().push(item);
        Ok(())
    }

    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>> {
        let query = query.trim().to_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        let mut scored: Vec<(usize, MemoryItem)> = self
            .items
            .read()
            .unwrap()
            .iter()
            .cloned()
            .map(|item| {
                let text = item.text.to_lowercase();
                let mut score = 0;
                if !query.is_empty() && text.contains(&query) {
                    score += 10;
                }
                for term in &terms {
                    score += text.matches(term).count();
                }
                (score, item)
            })
            .collect();

        // 同分时保持插入顺序，结果稳定。
        scored.sort_by(|(left_score, left), (right_score, right)| {
            right_score
                .cmp(left_score)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(scored
            .into_iter()
            .take(limit)
            .map(|(_, item)| item)
            .collect())
    }
}

/// 进程内会话存储，适合测试和单实例 CLI；生产可以替换成 SQLite/Redis。
#[derive(Default)]
pub struct InMemorySessionStore {
    sessions: RwLock<HashMap<String, Vec<Message>>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    async fn load(&self, session_id: &str) -> Result<Vec<Message>> {
        Ok(self
            .sessions
            .read()
            .unwrap()
            .get(session_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn save(&self, session_id: &str, messages: &[Message]) -> Result<()> {
        self.sessions
            .write()
            .unwrap()
            .insert(session_id.to_string(), messages.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kunpeng_core::types::{ContentPart, Message};

    #[tokio::test]
    async fn memory_assigns_ids_and_recalls_ranked_items() {
        let memory = InMemoryMemory::new();
        memory
            .remember(MemoryItem::new("Rust 所有权和借用"))
            .await
            .unwrap();
        memory
            .remember(MemoryItem::new("Python 列表"))
            .await
            .unwrap();

        let results = memory.recall("Rust 借用", 5).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "memory-1");
        assert!(results[0].text.contains("Rust"));
    }

    #[tokio::test]
    async fn memory_recall_respects_limit() {
        let memory = InMemoryMemory::new();
        memory.remember(MemoryItem::new("a")).await.unwrap();
        memory.remember(MemoryItem::new("b")).await.unwrap();
        assert_eq!(memory.recall("", 1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn session_store_round_trips_messages() {
        let store = InMemorySessionStore::new();
        let messages = vec![
            Message::system("system"),
            Message::user_parts(vec![ContentPart::Text {
                text: "hello".into(),
            }]),
        ];
        store.save("s1", &messages).await.unwrap();
        assert_eq!(store.load("s1").await.unwrap(), messages);
        assert!(store.load("missing").await.unwrap().is_empty());
    }
}
