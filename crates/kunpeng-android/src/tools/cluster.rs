//! 模板聚类工具。

use async_trait::async_trait;
use kunpeng_core::types::ToolDefinition;
use kunpeng_core::Result;
use std::sync::Arc;

use crate::analysis::templates;
use crate::capability::LogSessionStore;

/// `log_template_cluster`：message 数字/十六进制泛化后聚类，输出高频模板。
///
/// 海量日志的“第二眼”：先看统计（log_stats），再看模板分布，
/// 高频模板中的异常级别行往往是反复失败的重试。
pub struct TemplateClusterTool {
    sessions: Arc<dyn LogSessionStore>,
}

impl TemplateClusterTool {
    pub fn new(sessions: Arc<dyn LogSessionStore>) -> Self {
        Self { sessions }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for TemplateClusterTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "log_template_cluster".into(),
            description: "把日志按模板聚类（message 中的数字→<N>、十六进制→<H>），\
                          输出 top 高频模板：模板、次数、首末行号、样例。\
                          用于海量日志降噪与发现重复模式。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string", "description": "会话 id，缺省活动会话"},
                    "top": {"type": "integer", "description": "返回模板数上限，默认 30"},
                    "min_count": {"type": "integer", "description": "模板最少出现次数，默认 2"}
                }
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let (_, lines) =
            super::resolve_session(&self.sessions, &arguments, "log_template_cluster").await?;
        let top = super::optional_u32(&arguments, "log_template_cluster", "top")?
            .unwrap_or(30)
            .clamp(1, 100) as usize;
        let min_count =
            super::optional_u32(&arguments, "log_template_cluster", "min_count")?.unwrap_or(2);
        let stats = templates::cluster(&lines, top, min_count as usize);
        let templates_json: Vec<serde_json::Value> = stats
            .iter()
            .map(|stat| {
                serde_json::json!({
                    "template": stat.template,
                    "count": stat.count,
                    "first_line": stat.first_line,
                    "last_line": stat.last_line,
                    "sample": stat.sample,
                })
            })
            .collect();
        Ok(serde_json::json!({ "templates": templates_json }).to_string())
    }
}
