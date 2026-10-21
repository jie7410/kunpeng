//! 多智能体通信：AgentId、消息信封、总线、Worker 和 Supervisor。
//!
//! 核心不假设进程内或远程总线；`AgentBus` 是 trait，`LocalBus` 是 tokio mpsc
//! 实现。每个 Agent 独立持有自己的 Session，通过消息而不是共享可变状态通信。

mod bus;
mod supervisor;

pub use bus::{AgentBus, LocalBus};

use async_trait::async_trait;
use kunpeng_core::Result;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

pub use supervisor::{AgentWorker, Supervisor, Worker};

/// 远程 Agent 协议的最小边界。
///
/// A2A、ANP 等协议只负责把内核信封发到远端并返回协议响应；它们不实现本地
/// mailbox，也不要求运行时知道协议细节。需要接收远端异步消息时，应在适配器
/// 内部把结果投递回 `AgentBus`，而不是让 core 认识具体协议。
#[async_trait]
pub trait AgentProtocol: Send + Sync {
    async fn send_envelope(&self, envelope: &AgentEnvelope) -> Result<serde_json::Value>;
}

static NEXT_ENVELOPE: AtomicU64 = AtomicU64::new(1);

fn next_envelope_id() -> String {
    format!("env-{}", NEXT_ENVELOPE.fetch_add(1, Ordering::Relaxed))
}

/// Agent 的逻辑 id。只要求唯一和可序列化，不绑定线程或进程。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AgentId(String);

impl AgentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AgentId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for AgentId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// 多智能体消息负载。`Handoff` 表示把任务转交另一个 Agent，并带上原因。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentPayload {
    Text { content: String },
    Task { task: String },
    Result { content: String },
    Handoff { reason: String, task: String },
}

/// 总线信封。`correlation_id` 用于把 Result 关联回原始 Task/Handoff。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentEnvelope {
    pub id: String,
    pub from: AgentId,
    pub to: AgentId,
    pub correlation_id: String,
    pub payload: AgentPayload,
}

impl AgentEnvelope {
    /// 通用构造器，供自定义协议 payload 使用。
    pub fn new(from: impl Into<AgentId>, to: impl Into<AgentId>, payload: AgentPayload) -> Self {
        let id = next_envelope_id();
        Self {
            correlation_id: id.clone(),
            id,
            from: from.into(),
            to: to.into(),
            payload,
        }
    }

    pub fn task(from: impl Into<AgentId>, to: impl Into<AgentId>, task: impl Into<String>) -> Self {
        let id = next_envelope_id();
        Self {
            correlation_id: id.clone(),
            id,
            from: from.into(),
            to: to.into(),
            payload: AgentPayload::Task { task: task.into() },
        }
    }

    pub fn result(
        from: impl Into<AgentId>,
        to: impl Into<AgentId>,
        content: impl Into<String>,
        correlation_id: impl Into<String>,
    ) -> Self {
        Self {
            id: next_envelope_id(),
            from: from.into(),
            to: to.into(),
            correlation_id: correlation_id.into(),
            payload: AgentPayload::Result {
                content: content.into(),
            },
        }
    }

    pub fn handoff(
        from: impl Into<AgentId>,
        to: impl Into<AgentId>,
        reason: impl Into<String>,
        task: impl Into<String>,
    ) -> Self {
        let id = next_envelope_id();
        Self {
            correlation_id: id.clone(),
            id,
            from: from.into(),
            to: to.into(),
            payload: AgentPayload::Handoff {
                reason: reason.into(),
                task: task.into(),
            },
        }
    }

    pub fn text(
        from: impl Into<AgentId>,
        to: impl Into<AgentId>,
        content: impl Into<String>,
    ) -> Self {
        let id = next_envelope_id();
        Self {
            correlation_id: id.clone(),
            id,
            from: from.into(),
            to: to.into(),
            payload: AgentPayload::Text {
                content: content.into(),
            },
        }
    }
}
