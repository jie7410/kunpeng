use crate::Tool;
use async_trait::async_trait;
use kunpeng_core::error::{Error, Result};
use kunpeng_core::types::ToolDefinition;
use std::path::{Path, PathBuf};

/// 读取沙箱目录内的文本文件。
///
/// 通过规范化路径 + `starts_with(root)` 防止 `../` 越界；只读取普通文件。
pub struct FsReadTool {
    root: PathBuf,
    max_bytes: usize,
}

impl FsReadTool {
    pub fn new(root: impl AsRef<Path>, max_bytes: usize) -> Result<Self> {
        let root = std::fs::canonicalize(root.as_ref()).map_err(|error| {
            Error::InvalidConfig(format!(
                "fs_read root 无法规范化 {}: {error}",
                root.as_ref().display()
            ))
        })?;
        Ok(Self { root, max_bytes })
    }
}

#[async_trait]
impl Tool for FsReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "fs_read".into(),
            description: "读取沙箱目录内的文本文件".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "相对于沙箱根目录的文件路径"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let path = arguments
            .get("path")
            .and_then(|value| value.as_str())
            .ok_or_else(|| Error::Other("fs_read 缺少 path".into()))?;
        let candidate = self.root.join(path);
        let canonical = tokio::fs::canonicalize(&candidate).await.map_err(|error| {
            Error::Other(format!("fs_read 无法访问 {}: {error}", candidate.display()))
        })?;

        if !canonical.starts_with(&self.root) {
            return Err(Error::Other("fs_read 拒绝访问沙箱外路径".into()));
        }
        let metadata = tokio::fs::metadata(&canonical).await?;
        if !metadata.is_file() {
            return Err(Error::Other("fs_read 只能读取普通文件".into()));
        }

        let bytes = tokio::fs::read(&canonical).await?;
        let bytes = &bytes[..bytes.len().min(self.max_bytes)];
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "kunpeng-fs-{}-{stamp}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn reads_file_inside_root() {
        let root = temp_dir();
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        let tool = FsReadTool::new(&root, 1024).unwrap();
        let output = tool
            .call(serde_json::json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert_eq!(output, "hello");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejects_path_outside_root() {
        let root = temp_dir();
        let tool = FsReadTool::new(&root, 1024).unwrap();
        let result = tool
            .call(serde_json::json!({"path": "../outside.txt"}))
            .await;
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
