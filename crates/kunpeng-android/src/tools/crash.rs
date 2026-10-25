//! 崩溃工具：确定性文本解析 + 会话级事件目录。

use async_trait::async_trait;
use kunpeng_core::types::ToolDefinition;
use kunpeng_core::Result;
use std::sync::Arc;

use crate::analysis::catalog::{crash_catalog, group_by_signature};
use crate::capability::{CompositeLogParser, LogParser, LogSessionStore};

/// `android_log_parse`：把原始日志文本解析成结构化崩溃报告。
///
/// 作用于调用参数里的文本片段（模型从 grep/read 结果中摘取，或用户粘贴）；
/// 扫描整个会话用 `android_crashes`。
pub struct LogParseTool {
    parser: Arc<CompositeLogParser>,
}

impl LogParseTool {
    pub fn new(parser: Arc<CompositeLogParser>) -> Self {
        Self { parser }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for LogParseTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "android_log_parse".into(),
            description: "解析 Android 日志文本片段，提取结构化崩溃报告（Java 异常堆栈、ANR）。\
                          返回崩溃类别、异常类型、调用栈（标注应用帧与源码位置）、\
                          Caused by 链与崩溃指纹 signature。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "log_text": {
                        "type": "string",
                        "description": "原始日志文本片段"
                    }
                },
                "required": ["log_text"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let log_text = super::required_str(&arguments, "android_log_parse", "log_text")?;
        let reports = self.parser.parse(&log_text);
        if reports.is_empty() {
            return Ok(serde_json::json!({
                "reports": [],
                "hint": format!(
                    "未识别到崩溃格式。已注册解析器：{}",
                    self.parser.names().join(", ")
                )
            })
            .to_string());
        }
        let reports_json: Vec<serde_json::Value> = reports
            .iter()
            .map(|report| {
                serde_json::json!({
                    "kind": report.kind.as_str(),
                    "signature": report.signature(),
                    "process": report.process,
                    "pid": report.pid,
                    "thread": report.thread,
                    "exception": report.exception,
                    "message": report.message,
                    "caused_by": report.causes,
                    "app_frames": report.app_frames().iter().map(|f| f.to_string()).collect::<Vec<_>>(),
                    "frames": report.frames,
                    "raw_excerpt": report.raw_excerpt,
                })
            })
            .collect();
        Ok(serde_json::json!({ "reports": reports_json }).to_string())
    }
}

/// `android_crashes`：扫描会话产出崩溃事件目录，按 signature 聚合。
pub struct CrashCatalogTool {
    sessions: Arc<dyn LogSessionStore>,
    parser: Arc<CompositeLogParser>,
}

impl CrashCatalogTool {
    pub fn new(sessions: Arc<dyn LogSessionStore>, parser: Arc<CompositeLogParser>) -> Self {
        Self { sessions, parser }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for CrashCatalogTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "android_crashes".into(),
            description: "扫描整个日志会话，输出崩溃/ANR 事件目录并按 signature 聚合：\
                          每组含崩溃指纹、出现次数、首次行号、最新报告（应用帧、异常、进程）。\
                          从目录定位到行号后，用 log_grep context 视图看崩溃前后上下文。"
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
            super::resolve_session(&self.sessions, &arguments, "android_crashes").await?;
        let entries = crash_catalog(&lines, self.parser.as_ref());
        if entries.is_empty() {
            return Ok(serde_json::json!({
                "crashes": [],
                "total": 0,
                "hint": "会话中未发现崩溃/ANR 标记（FATAL EXCEPTION / ANR in）"
            })
            .to_string());
        }
        let groups = group_by_signature(entries);
        let total: usize = groups.iter().map(|group| group.count).sum();
        let crashes_json: Vec<serde_json::Value> = groups
            .iter()
            .map(|group| {
                serde_json::json!({
                    "signature": group.signature,
                    "count": group.count,
                    "first_line": group.first_line,
                    "kind": group.latest.kind.as_str(),
                    "process": group.latest.process,
                    "thread": group.latest.thread,
                    "exception": group.latest.exception,
                    "message": group.latest.message,
                    "caused_by": group.latest.causes,
                    "app_frames": group.latest.app_frames().iter().map(|f| f.to_string()).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(serde_json::json!({
            "total": total,
            "groups": crashes_json,
        })
        .to_string())
    }
}
