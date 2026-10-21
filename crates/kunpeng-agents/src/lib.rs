//! Agent 实现层。

pub mod react;

pub use react::{Agent, ReActAgent};

use async_trait::async_trait;
use kunpeng_context::ContextBuilder;
use kunpeng_core::types::Message;
use kunpeng_core::{ChatModel, Result};
use kunpeng_runtime::{AgentHooks, CancellationToken, RunConfig, StopReason};
use kunpeng_tools::ToolRegistry;

/// 一次策略运行可用的共享上下文。
///
/// 这是策略层与运行基础设施的边界：runtime 只定义配置/事件/取消，策略层把
/// 模型、工具和上下文构建器组装进来。
pub struct RunContext<'a> {
    pub model: &'a dyn ChatModel,
    pub tools: &'a ToolRegistry,
    pub context: &'a dyn ContextBuilder,
    pub config: &'a RunConfig,
    pub hooks: &'a AgentHooks<'a>,
    pub cancel: CancellationToken,
}

/// 可插拔的 Agent 策略。
///
/// `Agent` 门面持有模型、工具、上下文和取消令牌；策略只负责“如何推进历史”。
/// 这样 ReAct、Plan-Execute、Router 等范式可以共享同一套基础设施。
#[async_trait]
pub trait AgentStrategy: Send {
    fn history(&self) -> &[Message];

    fn history_mut(&mut self) -> &mut Vec<Message>;

    async fn run(&mut self, ctx: &RunContext<'_>) -> Result<StopReason>;
}
