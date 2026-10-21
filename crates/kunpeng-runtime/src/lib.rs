//! Runtime 层：一次 Agent 运行所需的配置、上下文、事件、审批和取消。
//!
//! 具体策略（ReAct、Plan-Execute 等）在 `agents` 层；这里只提供它们共享的
//! 运行基础设施。

use async_trait::async_trait;
use kunpeng_core::types::{
    ProviderOptions, ServerToolUse, ThinkingConfig, TokenBudget, ToolCall, ToolChoice, Usage,
};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Notify;

/// 一次运行的配置。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RunConfig {
    /// 最多发起多少次模型请求，不是“工具轮数”。
    pub max_turns: usize,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stream: bool,
    /// 仅 Anthropic 使用；None 表示不开启 extended thinking。
    pub thinking: Option<ThinkingConfig>,
    /// Anthropic 服务端工具定义，只传给 API。
    pub server_tools: Vec<serde_json::Value>,
    pub tool_choice: Option<ToolChoice>,
    pub stop_sequences: Vec<String>,
    pub provider_options: ProviderOptions,
    /// 等待 UI 审批的超时；None 表示一直等待。
    pub approval_timeout: Option<Duration>,
    /// 单个工具的硬超时；None 表示不限制。
    pub tool_timeout: Option<Duration>,
    /// 历史裁剪预算；None 表示不裁剪。
    pub context_budget: Option<TokenBudget>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            max_turns: 8,
            temperature: None,
            max_tokens: None,
            stream: true,
            thinking: None,
            server_tools: Vec::new(),
            tool_choice: None,
            stop_sequences: Vec::new(),
            provider_options: ProviderOptions::default(),
            approval_timeout: Some(Duration::from_secs(300)),
            tool_timeout: Some(Duration::from_secs(60)),
            context_budget: None,
        }
    }
}

impl RunConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    pub fn with_thinking(mut self, thinking: ThinkingConfig) -> Self {
        self.thinking = Some(thinking);
        self
    }

    pub fn with_server_tools(mut self, server_tools: Vec<serde_json::Value>) -> Self {
        self.server_tools = server_tools;
        self
    }

    pub fn with_tool_choice(mut self, tool_choice: ToolChoice) -> Self {
        self.tool_choice = Some(tool_choice);
        self
    }

    pub fn with_stop_sequences(mut self, stop_sequences: Vec<String>) -> Self {
        self.stop_sequences = stop_sequences;
        self
    }

    pub fn with_provider_options(mut self, provider_options: ProviderOptions) -> Self {
        self.provider_options = provider_options;
        self
    }

    pub fn with_approval_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.approval_timeout = timeout;
        self
    }

    pub fn with_tool_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.tool_timeout = timeout;
        self
    }

    pub fn with_context_budget(mut self, budget: Option<TokenBudget>) -> Self {
        self.context_budget = budget;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StopReason {
    /// 模型返回了不再需要工具的最终回复。
    Completed,
    /// 达到 `max_turns`。
    MaxTurns,
    /// 模型因 token 上限停止。
    Length,
    /// 内容被安全策略过滤。
    ContentFilter,
    /// 调用方主动取消。
    Cancelled,
}

/// 工具审批结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    Deny(String),
}

/// 异步工具审批器。UI 可以 await oneshot/IPC 的用户决定。
#[async_trait]
pub trait ToolApprover: Send + Sync {
    async fn approve(&self, call: &ToolCall) -> ToolDecision;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolOutcome {
    Success { content: String },
    Error { message: String },
    Denied { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolExecution {
    pub id: String,
    pub name: String,
    pub outcome: ToolOutcome,
}

/// UI 无关的事件流。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentEvent {
    AssistantDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    Assistant {
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
        usage: Option<Usage>,
        finish_reason: Option<kunpeng_core::FinishReason>,
        thinking: Option<String>,
        citations: Vec<serde_json::Value>,
        server_tool_uses: Vec<ServerToolUse>,
    },
    ToolStarted {
        call: ToolCall,
    },
    ToolFinished {
        execution: ToolExecution,
    },
    Finished {
        stop_reason: StopReason,
    },
    Error {
        message: String,
    },
}

/// 调用方注入的观察/审批钩子。事件回调同步，审批异步。
#[derive(Default)]
#[non_exhaustive]
pub struct AgentHooks<'a> {
    pub on_event: Option<&'a (dyn Fn(AgentEvent) + Send + Sync)>,
    pub approve: Option<&'a dyn ToolApprover>,
}

impl<'a> AgentHooks<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_event(mut self, on_event: &'a (dyn Fn(AgentEvent) + Send + Sync)) -> Self {
        self.on_event = Some(on_event);
        self
    }

    pub fn with_approver(mut self, approve: &'a dyn ToolApprover) -> Self {
        self.approve = Some(approve);
        self
    }
}

/// 取消令牌。用 AtomicBool 存状态，Notify 唤醒等待者。
#[derive(Clone, Default)]
pub struct CancellationToken {
    inner: Arc<CancelInner>,
}

#[derive(Default)]
struct CancelInner {
    flag: AtomicBool,
    notify: Notify,
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.flag.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.flag.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.inner.flag.store(false, Ordering::SeqCst);
    }

    /// 取消后完成；注册 waiter 前后各检查一次 flag，避免丢失通知。
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

pub fn emit(hooks: &AgentHooks<'_>, event: AgentEvent) {
    if let Some(on_event) = hooks.on_event {
        on_event(event);
    }
}
