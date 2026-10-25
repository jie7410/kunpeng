//! 代码工具：检索 + 行区间读取。

use async_trait::async_trait;
use kunpeng_core::types::ToolDefinition;
use kunpeng_core::Result;
use std::sync::Arc;

use crate::capability::CodeIndex;
use crate::types::CodeQuery;

/// `android_code_search`：在代码库中按模式检索。
pub struct CodeSearchTool {
    index: Arc<dyn CodeIndex>,
}

impl CodeSearchTool {
    pub fn new(index: Arc<dyn CodeIndex>) -> Self {
        Self { index }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for CodeSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "android_code_search".into(),
            description: format!(
                "在 Android 工程源码中按文本模式检索（大小写敏感子串）。索引：{}。\
                 返回 文件路径:行号 与该行内容；配合 android_code_read 读取上下文。",
                self.index.describe()
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "检索文本，如类名、方法名"},
                    "file_glob": {"type": "string", "description": "过滤：`*.kt` 按扩展名，其余按路径子串"},
                    "max_results": {"type": "integer", "description": "命中上限，默认 50"}
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let pattern = super::required_str(&arguments, "android_code_search", "pattern")?;
        let mut query = CodeQuery::new(pattern);
        if let Some(file_glob) =
            super::optional_str(&arguments, "android_code_search", "file_glob")?
        {
            query = query.with_file_glob(file_glob);
        }
        if let Some(max_results) =
            super::optional_u32(&arguments, "android_code_search", "max_results")?
        {
            query = query.with_max_results(max_results as usize);
        }

        let hits = self.index.search(&query).await?;
        Ok(serde_json::json!({
            "hits": hits,
            "truncated": hits.len() >= query.max_results,
        })
        .to_string())
    }
}

/// `android_code_read`：读取源码的行区间（1-based 闭区间）。
pub struct CodeReadTool {
    index: Arc<dyn CodeIndex>,
}

impl CodeReadTool {
    pub fn new(index: Arc<dyn CodeIndex>) -> Self {
        Self { index }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for CodeReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "android_code_read".into(),
            description: "读取源码文件的指定行区间（1-based 闭区间）。\
                          行号来自 android_code_search 或崩溃堆栈帧（file:line）。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "相对代码根目录的路径"},
                    "start_line": {"type": "integer", "description": "起始行（含），默认 1"},
                    "end_line": {"type": "integer", "description": "结束行（含），默认 start_line + 120"}
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let path = super::required_str(&arguments, "android_code_read", "path")?;
        let start_line =
            super::optional_u32(&arguments, "android_code_read", "start_line")?.unwrap_or(1);
        // 只给起始行时，默认向后取一屏上下文。
        let end_line = super::optional_u32(&arguments, "android_code_read", "end_line")?
            .unwrap_or_else(|| start_line.saturating_add(120));

        let snippet = self.index.read_range(&path, start_line, end_line).await?;
        Ok(format!(
            "# {}:{}-{}\n{}",
            snippet.path, snippet.start_line, snippet.end_line, snippet.content
        ))
    }
}
