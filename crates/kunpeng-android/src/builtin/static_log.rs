//! 内存/文件静态日志源。

use async_trait::async_trait;
use kunpeng_core::{Error, Result};

use crate::capability::LogProvider;
use crate::types::{LogLine, LogQuery, LogSlice};

/// 构造时一次性载入全部日志，之后 `fetch` 只做内存过滤。
///
/// 适合离线分析场景（用户粘贴的崩溃日志、导出的 logcat 文件）。日志量
/// 大到内存放不下时，实现分块/索引版 `LogProvider` 替换。
pub struct StaticLogProvider {
    label: String,
    lines: Vec<LogLine>,
}

impl StaticLogProvider {
    pub fn from_text(label: impl Into<String>, text: &str) -> Self {
        Self {
            label: label.into(),
            lines: text.lines().map(LogLine::from_raw).collect(),
        }
    }

    /// 整文件读入后构建。文件不存在立即报错，不推迟到第一次 fetch。
    pub async fn from_file(
        label: impl Into<String>,
        path: impl AsRef<std::path::Path>,
    ) -> Result<Self> {
        let text = tokio::fs::read_to_string(path.as_ref())
            .await
            .map_err(|error| {
                Error::InvalidConfig(format!(
                    "日志文件无法读取 {}: {error}",
                    path.as_ref().display()
                ))
            })?;
        Ok(Self::from_text(label, &text))
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
}

#[async_trait]
impl LogProvider for StaticLogProvider {
    async fn fetch(&self, query: &LogQuery) -> Result<LogSlice> {
        let keyword = query
            .keyword
            .as_ref()
            .map(|keyword| keyword.to_ascii_lowercase());
        let tag = query.tag.as_ref().map(|tag| tag.to_ascii_lowercase());

        let mut lines = Vec::new();
        let mut truncated = false;
        for line in &self.lines {
            if let Some(keyword) = &keyword {
                if !line.raw.to_ascii_lowercase().contains(keyword.as_str()) {
                    continue;
                }
            }
            if let Some(tag) = &tag {
                let Some(actual) = &line.tag else {
                    continue;
                };
                if !actual.to_ascii_lowercase().contains(tag.as_str()) {
                    continue;
                }
            }
            if let Some(min_level) = query.min_level {
                // 解析不出级别的行保留：过滤的本意是降噪，不是丢弃无格式行。
                if let Some(level) = line.level {
                    if level.rank() < min_level.rank() {
                        continue;
                    }
                }
            }
            if let Some(pid) = query.pid {
                if line.pid != Some(pid) {
                    continue;
                }
            }
            if lines.len() >= query.max_lines {
                truncated = true;
                break;
            }
            lines.push(line.clone());
        }

        Ok(LogSlice::new(lines, truncated))
    }

    fn describe(&self) -> String {
        format!("静态日志 {}（{} 行）", self.label, self.lines.len())
    }
}
