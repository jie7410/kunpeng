//! 关联分析工具：多源时间线与双会话差异。

use async_trait::async_trait;
use kunpeng_core::types::ToolDefinition;
use kunpeng_core::Result;
use std::sync::Arc;

use crate::analysis::{cross, diff};
use crate::capability::LogSessionStore;

/// `log_cross_timeline`：多个会话按时间戳归并成统一时间线。
pub struct CrossTimelineTool {
    sessions: Arc<dyn LogSessionStore>,
}

impl CrossTimelineTool {
    pub fn new(sessions: Arc<dyn LogSessionStore>) -> Self {
        Self { sessions }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for CrossTimelineTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "log_cross_timeline".into(),
            description: "把多个日志会话按时间戳归并成一条统一时间线（多源对齐：如 main + events、\
                          问题机 + 正常机）。要求各会话时间戳格式一致（thread-time 或 epoch，\
                          混用会报错）。可用 keyword 过滤，limit 控制返回条数。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "sessions": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "会话 id 列表；缺省使用全部已打开会话"
                    },
                    "keyword": {"type": "string", "description": "整行子串过滤（可选）"},
                    "limit": {"type": "integer", "description": "返回条数上限，默认 400"}
                }
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let requested: Vec<String> = match arguments.get("sessions") {
            None | Some(serde_json::Value::Null) => vec![],
            Some(serde_json::Value::Array(items)) => {
                let mut ids = Vec::new();
                for item in items {
                    ids.push(
                        item.as_str()
                            .ok_or_else(|| {
                                super::invalid_tool(
                                    "log_cross_timeline",
                                    "sessions 数组元素必须是字符串",
                                )
                            })?
                            .to_string(),
                    );
                }
                ids
            }
            Some(_) => {
                return Err(super::invalid_tool(
                    "log_cross_timeline",
                    "sessions 必须是字符串数组",
                ))
            }
        };

        let all = self.sessions.list().await;
        let selected: Vec<_> = if requested.is_empty() {
            all
        } else {
            all.into_iter()
                .filter(|meta| requested.contains(&meta.id))
                .collect()
        };
        if selected.len() < 2 {
            return Err(super::invalid_tool(
                "log_cross_timeline",
                "至少需要两个会话才能对齐；缺省参数要求已打开 ≥2 个会话",
            ));
        }

        let mut labeled: Vec<(String, Arc<Vec<crate::types::LogLine>>)> = Vec::new();
        for meta in &selected {
            let lines = self.sessions.snapshot(&meta.id).await?;
            labeled.push((meta.label.clone(), lines));
        }
        let borrowed: Vec<(String, &[crate::types::LogLine])> = labeled
            .iter()
            .map(|(label, lines)| (label.clone(), lines.as_slice()))
            .collect();

        let keyword = super::optional_str(&arguments, "log_cross_timeline", "keyword")?;
        let limit = super::optional_u32(&arguments, "log_cross_timeline", "limit")?
            .unwrap_or(400)
            .clamp(1, 2000) as usize;
        let timeline = cross::cross_timeline(&borrowed, keyword.as_deref(), limit)?;

        let entries_json: Vec<serde_json::Value> = timeline
            .entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "session": entry.session,
                    "line_no": entry.line_no,
                    "timestamp": entry.timestamp,
                    "raw": entry.raw,
                })
            })
            .collect();
        Ok(serde_json::json!({
            "format": format!("{:?}", timeline.format).to_lowercase(),
            "sessions": selected.iter().map(|meta| serde_json::json!({
                "id": meta.id, "label": meta.label, "line_count": meta.line_count,
            })).collect::<Vec<_>>(),
            "shown": entries_json.len(),
            "omitted": timeline.omitted,
            "entries": entries_json,
        })
        .to_string())
    }
}

/// `log_diff`：双会话模板差异对比。
pub struct LogDiffTool {
    sessions: Arc<dyn LogSessionStore>,
}

impl LogDiffTool {
    pub fn new(sessions: Arc<dyn LogSessionStore>) -> Self {
        Self { sessions }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for LogDiffTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "log_diff".into(),
            description: "对比两个日志会话（baseline 基准 vs target 目标，如 正常机 vs 问题机、\
                          修复前 vs 修复后）：以模板为签名，输出只在目标出现的新模式、\
                          只在基准消失的模式、以及两侧频次剧变的模式。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "baseline": {"type": "string", "description": "基准会话 id"},
                    "target": {"type": "string", "description": "目标会话 id"}
                },
                "required": ["baseline", "target"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let baseline = super::required_str(&arguments, "log_diff", "baseline")?;
        let target = super::required_str(&arguments, "log_diff", "target")?;
        if baseline == target {
            return Err(super::invalid_tool(
                "log_diff",
                "baseline 与 target 不能是同一会话",
            ));
        }

        let base_lines = self.sessions.snapshot(&baseline).await?;
        let target_lines = self.sessions.snapshot(&target).await?;
        let report = diff::diff_lines(&base_lines, &target_lines);

        let changed_json: Vec<serde_json::Value> = report
            .changed
            .iter()
            .map(|delta| {
                serde_json::json!({
                    "template": delta.template,
                    "baseline": delta.baseline,
                    "target": delta.target,
                    "delta": delta.delta,
                })
            })
            .collect();
        Ok(serde_json::json!({
            "baseline": baseline,
            "target": target,
            "new_in_target": report.new_in_target,
            "missing_in_target": report.missing_in_target,
            "changed": changed_json,
        })
        .to_string())
    }
}
