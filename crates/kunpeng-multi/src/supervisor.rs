use crate::{AgentBus, AgentEnvelope, AgentId, AgentPayload};
use async_trait::async_trait;
use kunpeng_agents::Agent;
use kunpeng_core::types::Message;
use kunpeng_core::Result;
use kunpeng_runtime::AgentHooks;
use std::{collections::HashMap, sync::Arc};

/// Supervisor 管理的 Worker。Worker 可以包 Agent，也可以是路由/测试实现。
#[async_trait]
pub trait Worker: Send {
    fn id(&self) -> &AgentId;

    async fn handle(&mut self, task: String, hooks: &AgentHooks<'_>) -> Result<String>;
}

/// 把 `Agent` 包装成 Worker。任务作为 user 消息发送，返回最后一条 assistant 文本。
pub struct AgentWorker {
    id: AgentId,
    agent: Agent,
}

impl AgentWorker {
    pub fn new(id: impl Into<AgentId>, agent: Agent) -> Self {
        Self {
            id: id.into(),
            agent,
        }
    }
}

#[async_trait]
impl Worker for AgentWorker {
    fn id(&self) -> &AgentId {
        &self.id
    }

    async fn handle(&mut self, task: String, hooks: &AgentHooks<'_>) -> Result<String> {
        self.agent.send(Message::user(task), hooks).await?;
        self.agent
            .history()
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant {
                    content: Some(content),
                    ..
                } => Some(content.clone()),
                _ => None,
            })
            .ok_or_else(|| kunpeng_core::Error::Other("Worker 没有返回 assistant 文本".into()))
    }
}

/// Supervisor 负责注册 Worker、接收消息、执行 Task/Handoff 并回传 Result。
///
/// 它是消息驱动而不是共享 `Arc<Mutex<Agent>>`：每个 Worker 独立持有 Agent 与
/// Session，通过 AgentBus 通信。
pub struct Supervisor {
    bus: Arc<dyn AgentBus>,
    workers: HashMap<AgentId, Box<dyn Worker>>,
}

impl Supervisor {
    pub fn new(bus: Arc<dyn AgentBus>) -> Self {
        Self {
            bus,
            workers: HashMap::new(),
        }
    }

    pub fn bus(&self) -> &Arc<dyn AgentBus> {
        &self.bus
    }

    pub async fn register<W: Worker + 'static>(&mut self, worker: W) -> Result<()> {
        let id = worker.id().clone();
        if self.workers.contains_key(&id) {
            return Err(kunpeng_core::Error::InvalidConfig(format!(
                "Worker {id} 已注册"
            )));
        }
        self.bus.register(id.clone()).await?;
        self.workers.insert(id, Box::new(worker));
        Ok(())
    }

    pub async fn send_task(
        &self,
        from: impl Into<AgentId>,
        to: impl Into<AgentId>,
        task: impl Into<String>,
    ) -> Result<()> {
        self.bus.send(AgentEnvelope::task(from, to, task)).await
    }

    /// 处理目标 Agent 的一条消息。Task/Handoff 会交给 Worker，并回传 Result。
    /// 返回被消费的 envelope，便于调用方记录日志。
    pub async fn step(
        &mut self,
        agent: &AgentId,
        hooks: &AgentHooks<'_>,
    ) -> Result<Option<AgentEnvelope>> {
        let Some(envelope) = self.bus.recv(agent).await? else {
            return Ok(None);
        };

        let task = match &envelope.payload {
            AgentPayload::Task { task } => Some(task.clone()),
            AgentPayload::Handoff { task, .. } => Some(task.clone()),
            AgentPayload::Text { .. } | AgentPayload::Result { .. } => None,
        };

        if let Some(task) = task {
            let worker = self
                .workers
                .get_mut(agent)
                .ok_or_else(|| kunpeng_core::Error::Other(format!("Worker {agent} 未注册")))?;
            let content = worker.handle(task, hooks).await?;
            let reply = AgentEnvelope::result(
                agent.clone(),
                envelope.from.clone(),
                content,
                envelope.correlation_id.clone(),
            );
            self.bus.send(reply).await?;
        }
        Ok(Some(envelope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentPayload, LocalBus};

    struct EchoWorker {
        id: AgentId,
    }

    #[async_trait]
    impl Worker for EchoWorker {
        fn id(&self) -> &AgentId {
            &self.id
        }

        async fn handle(&mut self, task: String, _hooks: &AgentHooks<'_>) -> Result<String> {
            Ok(format!("echo: {task}"))
        }
    }

    #[tokio::test]
    async fn supervisor_routes_task_and_result() {
        let bus = Arc::new(LocalBus::new());
        let mut supervisor = Supervisor::new(bus.clone());
        supervisor
            .register(EchoWorker {
                id: AgentId::new("worker"),
            })
            .await
            .unwrap();
        supervisor
            .bus()
            .register(AgentId::new("client"))
            .await
            .unwrap();

        supervisor
            .send_task("client", "worker", "hello")
            .await
            .unwrap();
        let consumed = supervisor
            .step(&AgentId::new("worker"), &AgentHooks::default())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(consumed.payload, AgentPayload::Task { .. }));

        let reply = supervisor
            .bus()
            .recv(&AgentId::new("client"))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            reply.payload,
            AgentPayload::Result { ref content } if content == "echo: hello"
        ));
    }
}
