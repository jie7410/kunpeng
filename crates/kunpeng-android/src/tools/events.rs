//! 语义分析工具：事件解码与启动时间线。

use async_trait::async_trait;
use kunpeng_core::types::ToolDefinition;
use kunpeng_core::Result;
use std::sync::Arc;

use crate::analysis::{boot, events};
use crate::capability::LogSessionStore;

/// `android_event_log_decode`：解码 events buffer 事件。
pub struct EventDecodeTool {
    sessions: Arc<dyn LogSessionStore>,
}

impl EventDecodeTool {
    pub fn new(sessions: Arc<dyn LogSessionStore>) -> Self {
        Self { sessions }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for EventDecodeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "android_event_log_decode".into(),
            description:
                "解码日志会话中的 event log 行（EVENT: [payload] 形式，来自 events buffer）：\
                          已知事件（am_proc_start/am_anr/am_kill/boot_progress_* 等）按字段命名，\
                          未知事件保留原始值。可用 name 子串过滤（如 am_、boot_progress_）。"
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "会话 id，缺省活动会话"},
                    "name": {"type": "string", "description": "事件名子串过滤"},
                    "limit": {"type": "integer", "description": "返回条数上限，默认 200"}
                }
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let (_, lines) =
            super::resolve_session(&self.sessions, &arguments, "android_event_log_decode").await?;
        let name = super::optional_str(&arguments, "android_event_log_decode", "name")?;
        let limit = super::optional_u32(&arguments, "android_event_log_decode", "limit")?
            .unwrap_or(200)
            .clamp(1, 1000) as usize;

        let all = events::decode_events(&lines, name.as_deref());
        if all.is_empty() {
            return Ok(serde_json::json!({
                "events": [],
                "total": 0,
                "hint": "未发现 event log 行（形如 TAG: [payload] 的 events buffer 输出）"
            })
            .to_string());
        }
        let total = all.len();
        let events_json: Vec<serde_json::Value> = all
            .iter()
            .take(limit)
            .map(|event| {
                // 字段名与值按位置配对；未知字段名留空。
                let fields = event.fields;
                let pairs: Vec<serde_json::Value> = event
                    .values
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        serde_json::json!({
                            "name": fields.and_then(|f| f.get(index)).copied(),
                            "value": value,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "line_no": event.line_no,
                    "timestamp": event.timestamp,
                    "event": event.name,
                    "description": event.description,
                    "fields": pairs,
                })
            })
            .collect();
        Ok(serde_json::json!({
            "total": total,
            "shown": events_json.len(),
            "events": events_json,
        })
        .to_string())
    }
}

/// `android_boot_timeline`：启动里程碑与卡点。
pub struct BootTimelineTool {
    sessions: Arc<dyn LogSessionStore>,
}

impl BootTimelineTool {
    pub fn new(sessions: Arc<dyn LogSessionStore>) -> Self {
        Self { sessions }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for BootTimelineTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "android_boot_timeline".into(),
            description: "从会话提取启动时间线：boot_progress_* / 开机动画等里程碑\
                          （自内核启动的 elapsedMs + 相邻间隔）、间隔最大的卡点段、\
                          main buffer 中的启动关键词标记。需要包含 events buffer 的日志。"
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
            super::resolve_session(&self.sessions, &arguments, "android_boot_timeline").await?;
        let timeline = boot::boot_timeline(&lines);
        if timeline.milestones.is_empty() {
            return Ok(serde_json::json!({
                "milestones": [],
                "hint": "未发现启动里程碑事件；日志需要包含 events buffer（boot_progress_*）"
            })
            .to_string());
        }
        let milestones_json: Vec<serde_json::Value> = timeline
            .milestones
            .iter()
            .map(|milestone| {
                serde_json::json!({
                    "name": milestone.name,
                    "elapsed_ms": milestone.elapsed_ms,
                    "delta_from_previous_ms": milestone.delta_from_previous_ms,
                    "line_no": milestone.line_no,
                })
            })
            .collect();
        let gaps_json: Vec<serde_json::Value> = timeline
            .slowest_gaps
            .iter()
            .map(|(span, delta)| serde_json::json!({"gap": span, "ms": delta}))
            .collect();
        let markers_json: Vec<serde_json::Value> = timeline
            .wall_markers
            .iter()
            .map(|marker| {
                serde_json::json!({
                    "line_no": marker.line_no,
                    "timestamp": marker.timestamp,
                    "raw": marker.raw,
                })
            })
            .collect();
        Ok(serde_json::json!({
            "milestones": milestones_json,
            "slowest_gaps": gaps_json,
            "wall_markers": markers_json,
        })
        .to_string())
    }
}
