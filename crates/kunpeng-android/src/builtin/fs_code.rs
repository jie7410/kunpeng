//! 文件系统代码索引：目录遍历 + 子串匹配 + 行区间读取。
//!
//! 刻意保持简单（同步语义、无正则、无缓存）：先支撑“堆栈帧 → 源码行”的
//! 定位链路；大仓库性能问题出现时，用 ripgrep/tree-sitter 实现替换本类型。

use async_trait::async_trait;
use kunpeng_core::{Error, Result};
use std::path::{Path, PathBuf};

use crate::capability::CodeIndex;
use crate::types::{CodeHit, CodeQuery, CodeSnippet};

/// 跳过的目录名。`build/`、`.gradle/` 是 Android 工程的典型产物目录，
/// 检索命中它们只会引入噪声。
const SKIPPED_DIRS: [&str; 6] = [
    ".git",
    "build",
    ".gradle",
    ".idea",
    "node_modules",
    "target",
];

/// 单文件读取上限。超过按非文本文件跳过，避免把 so/aar 拖进内存。
const MAX_FILE_BYTES: u64 = 1024 * 1024;

pub struct FsCodeIndex {
    root: PathBuf,
}

impl FsCodeIndex {
    /// 根目录在构造时规范化，后续所有路径以它为沙箱边界。
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = std::fs::canonicalize(root.as_ref()).map_err(|error| {
            Error::InvalidConfig(format!(
                "代码根目录无法规范化 {}: {error}",
                root.as_ref().display()
            ))
        })?;
        Ok(Self { root })
    }

    fn resolve(&self, path: &str) -> Result<PathBuf> {
        let candidate = self.root.join(path);
        // canonicalize 是一次快速路径解析，不值得为此引入 spawn_blocking。
        let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
            Error::InvalidConfig(format!("路径无法解析 {}: {error}", candidate.display()))
        })?;
        if !canonical.starts_with(&self.root) {
            return Err(Error::InvalidConfig(format!(
                "路径越界：{path}（沙箱根 {}）",
                self.root.display()
            )));
        }
        Ok(canonical)
    }
}

/// `file_glob` 语义：`*.kt` 按扩展名匹配，其余按路径子串匹配。
fn matches_glob(relative: &str, glob: &str) -> bool {
    if let Some(ext) = glob.strip_prefix("*.") {
        relative.ends_with(&format!(".{ext}"))
    } else {
        relative.contains(glob)
    }
}

#[async_trait]
impl CodeIndex for FsCodeIndex {
    async fn search(&self, query: &CodeQuery) -> Result<Vec<CodeHit>> {
        let max_results = query.max_results.min(200);
        let mut hits = Vec::new();
        let mut stack = vec![self.root.clone()];

        while let Some(dir) = stack.pop() {
            let mut entries = match tokio::fs::read_dir(&dir).await {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let Ok(file_type) = entry.file_type().await else {
                    continue;
                };
                if file_type.is_dir() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if !SKIPPED_DIRS.contains(&name) {
                            stack.push(path);
                        }
                    }
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let relative = match path.strip_prefix(&self.root) {
                    Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
                    Err(_) => continue,
                };
                if let Some(glob) = &query.file_glob {
                    if !matches_glob(&relative, glob) {
                        continue;
                    }
                }
                if tokio::fs::metadata(&path).await?.len() > MAX_FILE_BYTES {
                    continue;
                }
                let Ok(text) = tokio::fs::read_to_string(&path).await else {
                    continue; // 二进制或非 UTF-8
                };
                for (index, line) in text.lines().enumerate() {
                    if line.contains(&query.pattern) {
                        hits.push(CodeHit {
                            path: relative.clone(),
                            line_number: index as u32 + 1,
                            line_text: line.trim().to_string(),
                        });
                        if hits.len() >= max_results {
                            return Ok(hits);
                        }
                    }
                }
            }
        }
        Ok(hits)
    }

    async fn read_range(&self, path: &str, start_line: u32, end_line: u32) -> Result<CodeSnippet> {
        if start_line == 0 || end_line < start_line {
            return Err(Error::InvalidTool {
                name: "android_code_read".into(),
                reason: format!("行区间不合法：{start_line}-{end_line}（1-based 闭区间）"),
            });
        }
        let full = self.resolve(path)?;
        let text = tokio::fs::read_to_string(&full).await.map_err(|error| {
            Error::InvalidConfig(format!("文件无法读取 {}: {error}", full.display()))
        })?;

        let start = start_line as usize;
        let end = (end_line as usize).min(text.lines().count());
        let content: Vec<&str> = text
            .lines()
            .skip(start - 1)
            .take(end.saturating_sub(start - 1))
            .collect();
        if content.is_empty() {
            return Err(Error::InvalidTool {
                name: "android_code_read".into(),
                reason: format!("{path} 第 {start_line} 行超出文件末尾"),
            });
        }
        Ok(CodeSnippet {
            path: path.to_string(),
            start_line: start as u32,
            end_line: end as u32,
            content: content.join("\n"),
        })
    }

    fn describe(&self) -> String {
        format!("源码目录 {}", self.root.display())
    }
}
