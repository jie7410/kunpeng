use crate::{AgentEnvelope, AgentId};
use async_trait::async_trait;
use kunpeng_core::error::{Error, Result};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use tokio::sync::{mpsc, Mutex};

/// 多智能体消息总线。实现可以替换成 NATS/Redis/进程间 IPC。
#[async_trait]
pub trait AgentBus: Send + Sync {
    /// 注册逻辑 Agent。远程总线可以 no-op 或映射到远端 endpoint。
    async fn register(&self, _id: AgentId) -> Result<()> {
        Ok(())
    }

    async fn send(&self, envelope: AgentEnvelope) -> Result<()>;
    async fn recv(&self, agent: &AgentId) -> Result<Option<AgentEnvelope>>;
}

/// 进程内总线。每个 Agent 一个无界 mpsc mailbox。
#[derive(Default)]
pub struct LocalBus {
    senders: RwLock<HashMap<AgentId, mpsc::UnboundedSender<AgentEnvelope>>>,
    receivers: RwLock<HashMap<AgentId, Arc<Mutex<mpsc::UnboundedReceiver<AgentEnvelope>>>>>,
}

impl LocalBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// 为 Agent 创建 mailbox。重复注册会报错，避免消息静默落到旧 mailbox。
    pub fn register(&self, id: impl Into<AgentId>) -> Result<()> {
        let id = id.into();
        let mut senders = self.senders.write().unwrap();
        if senders.contains_key(&id) {
            return Err(Error::InvalidConfig(format!("Agent {id} 已注册")));
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        senders.insert(id.clone(), sender);
        self.receivers
            .write()
            .unwrap()
            .insert(id, Arc::new(Mutex::new(receiver)));
        Ok(())
    }
}

#[async_trait]
impl AgentBus for LocalBus {
    async fn register(&self, id: AgentId) -> Result<()> {
        LocalBus::register(self, id)
    }

    async fn send(&self, envelope: AgentEnvelope) -> Result<()> {
        let sender = self
            .senders
            .read()
            .unwrap()
            .get(&envelope.to)
            .cloned()
            .ok_or_else(|| Error::Other(format!("Agent {} 未注册", envelope.to)))?;
        sender
            .send(envelope)
            .map_err(|_| Error::Other("Agent mailbox 已关闭".into()))
    }

    async fn recv(&self, agent: &AgentId) -> Result<Option<AgentEnvelope>> {
        let receiver = self
            .receivers
            .read()
            .unwrap()
            .get(agent)
            .cloned()
            .ok_or_else(|| Error::Other(format!("Agent {agent} 未注册")))?;
        let mut receiver = receiver.lock().await;
        Ok(receiver.recv().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentPayload;

    #[tokio::test]
    async fn routes_messages_between_agents() {
        let bus = LocalBus::new();
        bus.register("a").unwrap();
        bus.register("b").unwrap();

        let envelope = AgentEnvelope::task("a", "b", "do it");
        let correlation = envelope.correlation_id.clone();
        bus.send(envelope).await.unwrap();

        let received = bus.recv(&AgentId::new("b")).await.unwrap().unwrap();
        assert_eq!(received.from, AgentId::new("a"));
        assert_eq!(received.correlation_id, correlation);
        assert!(matches!(received.payload, AgentPayload::Task { .. }));
    }

    #[tokio::test]
    async fn rejects_duplicate_and_unknown_agents() {
        let bus = LocalBus::new();
        bus.register("a").unwrap();
        assert!(bus.register("a").is_err());
        let envelope = AgentEnvelope::task("a", "missing", "x");
        assert!(bus.send(envelope).await.is_err());
    }
}
