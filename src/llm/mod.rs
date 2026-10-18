//! LLM 客户端抽象层。
//!
//! 分层：
//! - `config`：环境变量 / 程序化配置。
//! - `provider`：连接层 + 协议适配（OpenAI / Anthropic）。
//! - `model`：绑定 Provider + 模型名 + 默认参数。
//! - `agent`：有状态工具调用循环、事件、审批、取消。
//! - `tool`：工具定义与注册表。
//! - `types`：跨 provider 的语义类型。
//! - `error`：统一错误。
//!
//! 依赖方向只能向下：`agent` 依赖 `model`/`tool`/`types`，`provider` 依赖
//! `types`/`error`，`types` 不依赖任何上层。UI 只依赖本模块公开的 API，不直接
//! 使用 provider 内部结构。

pub mod agent;
pub mod config;
pub mod error;
pub mod model;
pub mod provider;
pub mod tool;
pub mod types;

pub use agent::{
    Agent, AgentConfig, AgentEvent, AgentHooks, CancellationToken, StopReason, ToolDecision,
    ToolExecution, ToolOutcome,
};
pub use config::LlmConfig;
pub use error::{Error, Result};
pub use model::{Model, ModelDefault};
pub use provider::Provider;
pub use tool::{Tool, ToolRegistry};
pub use types::{
    ChatRequest, ChatResponse, FinishReason, Message, ToolCall, ToolDefinition, Usage,
};
