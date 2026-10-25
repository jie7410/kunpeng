//! 性能信号工具。

use async_trait::async_trait;
use kunpeng_core::types::ToolDefinition;
use kunpeng_core::Result;
use std::sync::Arc;

use crate::analysis::perf;
use crate::capability::LogSessionStore;

/// `android_perf_signals`：扫描会话中的性能信号。
pub struct PerfSignalsTool {
    sessions: Arc<dyn LogSessionStore>,
}

impl PerfSignalsTool {
    pub fn new(sessions: Arc<dyn LogSessionStore>) -> Self {
        Self { sessions }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for PerfSignalsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "android_perf_signals".into(),
            description: "扫描日志会话中的性能信号并按类聚合：应用启动耗时（Displayed +Ns Nms）、\
                          主线程掉帧（Choreographer Skipped N frames）、StrictMode 违规、\
                          GC 压力、低内存查杀（LMK）。是代码优化场景的运行期证据来源。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "会话 id，缺省活动会话"}
                }
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let (_, lines) =
            super::resolve_session(&self.sessions, &arguments, "android_perf_signals").await?;
        let groups = perf::perf_signals(&lines);
        if groups.is_empty() {
            return Ok(serde_json::json!({
                "signals": [],
                "hint": "未发现已知性能信号（Displayed/掉帧/StrictMode/GC/LMK）"
            })
            .to_string());
        }
        let signals_json: Vec<serde_json::Value> = groups
            .iter()
            .map(|group| {
                serde_json::json!({
                    "kind": group.kind,
                    "description": group.description,
                    "count": group.count,
                    "entries": group.entries.iter().map(|entry| serde_json::json!({
                        "line_no": entry.line_no,
                        "process": entry.process,
                        "value_ms": entry.value_ms,
                        "raw": entry.raw,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(serde_json::json!({ "signals": signals_json }).to_string())
    }
}
