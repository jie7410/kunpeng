use crate::Tool;
use async_trait::async_trait;
use kunpeng_core::types::ToolDefinition;
use kunpeng_core::Result;
use std::time::{SystemTime, UNIX_EPOCH};

/// 返回当前 Unix 时间戳。无依赖、无副作用，适合做最小内置工具。
pub struct TimeTool;

#[async_trait]
impl Tool for TimeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "current_time".into(),
            description: "获取当前 Unix 时间戳（秒）".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    async fn call(&self, _arguments: serde_json::Value) -> Result<String> {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        Ok(format!("unix_seconds: {seconds}"))
    }
}
