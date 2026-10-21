//! 记忆与会话存储。
//!
//! 这一层只定义存储/检索契约和轻量内存实现；向量数据库、嵌入模型放在后续的
//! 可选 feature 中，不进入 core。

mod in_memory;

pub use in_memory::{InMemoryMemory, InMemorySessionStore};

use async_trait::async_trait;
use kunpeng_core::types::Message;
use kunpeng_core::Result;
use serde::{Deserialize, Serialize};

/// 一条长期记忆。`metadata` 留给来源、标签、时间戳等扩展字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MemoryItem {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

impl MemoryItem {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            text: text.into(),
            metadata: serde_json::Map::new(),
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn with_metadata(mut self, metadata: serde_json::Map<String, serde_json::Value>) -> Self {
        self.metadata = metadata;
        self
    }
}

/// 长期记忆接口。实现可以是关键词、向量库或外部服务。
#[async_trait]
pub trait Memory: Send + Sync {
    async fn remember(&self, item: MemoryItem) -> Result<()>;

    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>>;
}

/// 会话历史持久化。Agent 通过它加载/保存 `Vec<Message>`。
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn load(&self, session_id: &str) -> Result<Vec<Message>>;

    async fn save(&self, session_id: &str, messages: &[Message]) -> Result<()>;
}
