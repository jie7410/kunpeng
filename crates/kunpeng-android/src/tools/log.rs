//! 日志会话工具：open / sessions / stats / grep / read。

use async_trait::async_trait;
use kunpeng_core::types::ToolDefinition;
use kunpeng_core::Result;
use std::sync::Arc;

use crate::analysis::grep::{
    context_windows, count_by, grep_indices, ContextWindow, CountBy, GrepFilter,
};
use crate::analysis::stats;
use crate::capability::LogSessionStore;
use crate::types::{LogLevel, LogLine};

/// `log_open`：从文本或文件建立日志会话，并返回结构概览。
///
/// 大段日志优先走 `path`（沙箱内相对路径）；`text` 适合短片段。
/// 打开后自动成为活动会话。
pub struct LogOpenTool {
    sessions: Arc<dyn LogSessionStore>,
}

impl LogOpenTool {
    pub fn new(sessions: Arc<dyn LogSessionStore>) -> Self {
        Self { sessions }
    }

    fn overview(lines: &[LogLine]) -> serde_json::Value {
        let stats = stats::stats(lines, 5);
        serde_json::json!({
            "time_range": stats.time_range,
            "buffers": stats.buffer_markers,
            "by_level": stats.by_level,
            "top_tags": stats.by_tag,
            "crash_marker_lines": stats.crash_marker_lines,
            "unparseable_lines": stats.unparseable_lines,
        })
    }
}

#[async_trait]
impl kunpeng_tools::Tool for LogOpenTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "log_open".into(),
            description: "打开一份日志建立会话（path 与 text 二选一），返回会话元数据与结构概览：\
                          时间范围、buffer、级别分布、top tag、崩溃标记行。\
                          打开的会话成为活动会话，供 log_grep/log_read 等使用。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "沙箱内日志文件相对路径（大日志优先）"},
                    "text": {"type": "string", "description": "直接粘贴的日志文本（短片段）"},
                    "label": {"type": "string", "description": "会话标签，缺省按来源命名"}
                }
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let path = super::optional_str(&arguments, "log_open", "path")?;
        let text = super::optional_str(&arguments, "log_open", "text")?;
        let label = super::optional_str(&arguments, "log_open", "label")?.unwrap_or_default();

        let meta = match (path, text) {
            (Some(path), _) => self.sessions.open_file(&label, &path).await?,
            (None, Some(text)) => self.sessions.open_text(&label, &text).await?,
            (None, None) => {
                return Err(super::invalid_tool("log_open", "path 与 text 必须提供其一"))
            }
        };
        self.sessions.set_active(&meta.id).await?;
        let lines = self.sessions.snapshot(&meta.id).await?;
        Ok(serde_json::json!({
            "session": meta,
            "overview": Self::overview(&lines),
        })
        .to_string())
    }
}

/// `log_sessions`：会话内务——list / use / close / rename。
pub struct LogSessionsTool {
    sessions: Arc<dyn LogSessionStore>,
}

impl LogSessionsTool {
    pub fn new(sessions: Arc<dyn LogSessionStore>) -> Self {
        Self { sessions }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for LogSessionsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "log_sessions".into(),
            description: "日志会话内务：action=list 列出全部会话（含活动标记）；\
                          use=切换活动会话；close=关闭；rename=改标签。\
                          多份日志对比分析（如问题机 vs 正常机）前先给两份日志起可读标签。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "use", "close", "rename"]},
                    "session": {"type": "string", "description": "目标会话 id（use/close/rename 必填）"},
                    "label": {"type": "string", "description": "rename 的新标签"}
                },
                "required": ["action"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let action = super::required_str(&arguments, "log_sessions", "action")?;
        let session = super::optional_str(&arguments, "log_sessions", "session")?;
        match action.as_str() {
            "list" => {
                let active = self.sessions.active().await.map(|meta| meta.id);
                let sessions: Vec<serde_json::Value> = self
                    .sessions
                    .list()
                    .await
                    .into_iter()
                    .map(|meta| {
                        serde_json::json!({
                            "id": meta.id,
                            "label": meta.label,
                            "source": meta.source,
                            "line_count": meta.line_count,
                            "time_range": meta.time_range,
                            "active": active.as_deref() == Some(meta.id.as_str()),
                        })
                    })
                    .collect();
                Ok(serde_json::json!({ "sessions": sessions }).to_string())
            }
            "use" => {
                let id = session
                    .ok_or_else(|| super::invalid_tool("log_sessions", "use 需要 session 参数"))?;
                self.sessions.set_active(&id).await?;
                Ok(format!("活动会话已切换为 {id}"))
            }
            "close" => {
                let id = session.ok_or_else(|| {
                    super::invalid_tool("log_sessions", "close 需要 session 参数")
                })?;
                self.sessions.close(&id).await?;
                Ok(format!("会话 {id} 已关闭"))
            }
            "rename" => {
                let id = session.ok_or_else(|| {
                    super::invalid_tool("log_sessions", "rename 需要 session 参数")
                })?;
                let label = super::required_str(&arguments, "log_sessions", "label")?;
                self.sessions.rename(&id, &label).await?;
                Ok(format!("会话 {id} 标签已改为 {label}"))
            }
            other => Err(super::invalid_tool(
                "log_sessions",
                &format!("未知 action：{other}，可用 list/use/close/rename"),
            )),
        }
    }
}

/// `log_stats`：会话全貌统计。
pub struct LogStatsTool {
    sessions: Arc<dyn LogSessionStore>,
}

impl LogStatsTool {
    pub fn new(sessions: Arc<dyn LogSessionStore>) -> Self {
        Self { sessions }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for LogStatsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "log_stats".into(),
            description: "日志会话全貌统计：总行数、级别/Tag/PID 聚合 top-N、时间范围、\
                          buffer 组成、崩溃标记行号。分析前先看统计，再决定检索方向。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "会话 id，缺省活动会话"},
                    "top": {"type": "integer", "description": "聚合条目上限，默认 20"}
                }
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let (_, lines) = super::resolve_session(&self.sessions, &arguments, "log_stats").await?;
        let top = super::optional_u32(&arguments, "log_stats", "top")?
            .unwrap_or(20)
            .clamp(1, 100) as usize;
        let stats = stats::stats(&lines, top);
        Ok(serde_json::json!({
            "total_lines": stats.total_lines,
            "unparseable_lines": stats.unparseable_lines,
            "by_level": stats.by_level,
            "by_tag": stats.by_tag,
            "by_pid": stats.by_pid,
            "time_range": stats.time_range,
            "buffer_markers": stats.buffer_markers,
            "crash_marker_lines": stats.crash_marker_lines,
        })
        .to_string())
    }
}

/// `log_grep`：多维检索 + 聚合视图，检索主力。
pub struct LogGrepTool {
    sessions: Arc<dyn LogSessionStore>,
}

impl LogGrepTool {
    pub fn new(sessions: Arc<dyn LogSessionStore>) -> Self {
        Self { sessions }
    }

    fn filter_from(arguments: &serde_json::Value) -> Result<GrepFilter> {
        let min_level = match super::optional_str(arguments, "log_grep", "min_level")? {
            None => None,
            Some(level) => Some(LogLevel::parse(&level).ok_or_else(|| {
                super::invalid_tool(
                    "log_grep",
                    &format!("min_level 无法识别：{level}，可用值 V/D/I/W/E/F"),
                )
            })?),
        };
        Ok(GrepFilter {
            keyword: super::optional_str(arguments, "log_grep", "keyword")?,
            tag: super::optional_str(arguments, "log_grep", "tag")?,
            min_level,
            pid: super::optional_u32(arguments, "log_grep", "pid")?,
            time_from: super::optional_str(arguments, "log_grep", "time_from")?,
            time_to: super::optional_str(arguments, "log_grep", "time_to")?,
        })
    }

    fn window_json(lines: &[LogLine], window: &ContextWindow) -> serde_json::Value {
        let lines_json: Vec<serde_json::Value> = lines[window.start..=window.end]
            .iter()
            .enumerate()
            .map(|(offset, line)| {
                serde_json::json!({
                    "line_no": line.line_no,
                    "raw": line.raw,
                    "hit": window.hits.contains(&(window.start + offset)),
                })
            })
            .collect();
        serde_json::json!({
            "start_line": lines[window.start].line_no,
            "end_line": lines[window.end].line_no,
            "lines": lines_json,
        })
    }
}

#[async_trait]
impl kunpeng_tools::Tool for LogGrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "log_grep".into(),
            description: "日志多维检索（条件 AND）：keyword 整行子串、tag、pid、min_level、\
                          time_from/time_to（时间戳字典序闭区间，如 \"10-25 14:23\"）。\
                          view=lines（默认，offset/limit 分页）；count_by_tag/count_by_level/\
                          count_by_pid（聚合 top）；context（命中行前后各 before/after 行，\
                          命中行标 hit=true）。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "会话 id，缺省活动会话"},
                    "view": {"type": "string", "enum": ["lines", "count_by_tag", "count_by_level", "count_by_pid", "context"]},
                    "keyword": {"type": "string"},
                    "tag": {"type": "string"},
                    "pid": {"type": "integer"},
                    "min_level": {"type": "string", "description": "V/D/I/W/E/F"},
                    "time_from": {"type": "string"},
                    "time_to": {"type": "string"},
                    "offset": {"type": "integer", "description": "lines 视图偏移，默认 0"},
                    "limit": {"type": "integer", "description": "lines 视图行数上限，默认 200"},
                    "before": {"type": "integer", "description": "context 视图前置行数，默认 3"},
                    "after": {"type": "integer", "description": "context 视图后置行数，默认 8"}
                }
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let (_, lines) = super::resolve_session(&self.sessions, &arguments, "log_grep").await?;
        let filter = Self::filter_from(&arguments)?;
        let view =
            super::optional_str(&arguments, "log_grep", "view")?.unwrap_or_else(|| "lines".into());
        let hits = grep_indices(&lines, &filter);

        match view.as_str() {
            "lines" => {
                let offset =
                    super::optional_u32(&arguments, "log_grep", "offset")?.unwrap_or(0) as usize;
                let limit = super::optional_u32(&arguments, "log_grep", "limit")?
                    .unwrap_or(200)
                    .clamp(1, 1000) as usize;
                let shown: Vec<serde_json::Value> = hits
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|&index| {
                        let line = &lines[index];
                        serde_json::json!({"line_no": line.line_no, "raw": line.raw})
                    })
                    .collect();
                Ok(serde_json::json!({
                    "total_matches": hits.len(),
                    "offset": offset,
                    "shown": shown.len(),
                    "has_more": offset + shown.len() < hits.len(),
                    "lines": shown,
                })
                .to_string())
            }
            "count_by_tag" | "count_by_level" | "count_by_pid" => {
                let dimension = match view.as_str() {
                    "count_by_tag" => CountBy::Tag,
                    "count_by_level" => CountBy::Level,
                    _ => CountBy::Pid,
                };
                let counts = count_by(&lines, &hits, dimension, 20);
                Ok(serde_json::json!({
                    "total_matches": hits.len(),
                    "counts": counts,
                })
                .to_string())
            }
            "context" => {
                let before =
                    super::optional_u32(&arguments, "log_grep", "before")?.unwrap_or(3) as usize;
                let after =
                    super::optional_u32(&arguments, "log_grep", "after")?.unwrap_or(8) as usize;
                let windows = context_windows(lines.len(), &hits, before, after, 20);
                let windows_json: Vec<serde_json::Value> =
                    windows.iter().map(|window| Self::window_json(&lines, window)).collect();
                Ok(serde_json::json!({
                    "total_matches": hits.len(),
                    "windows": windows_json,
                })
                .to_string())
            }
            other => Err(super::invalid_tool(
                "log_grep",
                &format!(
                    "未知 view：{other}，可用 lines/count_by_tag/count_by_level/count_by_pid/context"
                ),
            )),
        }
    }
}

/// `log_read`：按原始行号读取整段原文（含上下文阅读、核对引用证据）。
pub struct LogReadTool {
    sessions: Arc<dyn LogSessionStore>,
}

impl LogReadTool {
    pub fn new(sessions: Arc<dyn LogSessionStore>) -> Self {
        Self { sessions }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for LogReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "log_read".into(),
            description: "按原始行号读取日志原文：start_line（1-based）起连续 count 行\
                          （默认 100，上限 500）。行号来自 log_grep/log_stats/android_crashes 的结果。".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "会话 id，缺省活动会话"},
                    "start_line": {"type": "integer", "description": "起始行号（1-based）"},
                    "count": {"type": "integer", "description": "行数，默认 100"}
                },
                "required": ["start_line"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let (meta, lines) = super::resolve_session(&self.sessions, &arguments, "log_read").await?;
        let start_line = super::optional_u32(&arguments, "log_read", "start_line")?
            .ok_or_else(|| super::invalid_tool("log_read", "缺少必填参数 start_line"))?;
        let count = super::optional_u32(&arguments, "log_read", "count")?
            .unwrap_or(100)
            .clamp(1, 500);

        if start_line == 0 || start_line as usize > lines.len() {
            return Err(super::invalid_tool(
                "log_read",
                &format!("start_line {start_line} 超出范围（1..={}）", lines.len()),
            ));
        }
        let start_index = start_line as usize - 1;
        let end_index = (start_index + count as usize).min(lines.len());
        let shown: Vec<serde_json::Value> = lines[start_index..end_index]
            .iter()
            .map(|line| serde_json::json!({"line_no": line.line_no, "raw": line.raw}))
            .collect();
        Ok(serde_json::json!({
            "session": meta.id,
            "start_line": start_line,
            "shown": shown.len(),
            "next_line": if end_index < lines.len() {
                serde_json::json!(end_index + 1)
            } else {
                serde_json::Value::Null
            },
            "lines": shown,
        })
        .to_string())
    }
}
