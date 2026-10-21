//! 工具系统：工具抽象与注册表。
//!
//! 工具执行策略（审批、超时、限流）留在 runtime/agent 层；内置工具后续放在
//! `tools::builtin`，用 feature flag 隔离。

mod policy;
mod registry;

pub use policy::ToolPolicy;
pub use registry::{Tool, ToolRegistry};

use async_trait::async_trait;
use kunpeng_core::Result;
use std::sync::Arc;

/// 动态工具来源，例如 MCP server。实现返回一组工具，由注册表批量注册。
#[async_trait]
pub trait ToolProvider: Send + Sync {
    async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>>;
}

#[cfg(feature = "tools-builtin")]
pub mod builtin;
