//! 工具级策略：执行超时和输出上限。

use std::time::Duration;

/// 单个工具的策略。未设置的项回落到 `RunConfig` 或全局默认。
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ToolPolicy {
    /// 覆盖 Agent 的全局工具超时。
    pub timeout: Option<Duration>,
    /// 工具输出超过该字节数时截断，并附加截断标记。
    pub max_output_bytes: Option<usize>,
}

impl ToolPolicy {
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = Some(max_output_bytes);
        self
    }
}
