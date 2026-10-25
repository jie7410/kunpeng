//! 内存会话实现：全量载入 + 行号标注。

use async_trait::async_trait;
use kunpeng_core::{Error, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::capability::LogSessionStore;
use crate::types::{LogLine, LogSessionMeta};

struct SessionEntry {
    meta: LogSessionMeta,
    lines: Arc<Vec<LogLine>>,
}

#[derive(Default)]
struct Inner {
    order: Vec<String>,
    sessions: HashMap<String, SessionEntry>,
    active: Option<String>,
    next_id: u32,
}

/// 全部会话驻留内存。`root` 是 `open_file` 的沙箱根（构造时规范化）；
/// `None` 时拒绝文件来源。
///
/// 临界区都是纯内存操作，用 `std::sync::Mutex` 而不是异步锁，这样 builder
/// 可以同步 `preload`，trait 的 async 方法也只是薄包装。
pub struct InMemorySessions {
    root: Option<PathBuf>,
    inner: Mutex<Inner>,
}

impl InMemorySessions {
    pub fn new(root: Option<impl AsRef<Path>>) -> Result<Self> {
        let root = root
            .map(|root| {
                std::fs::canonicalize(root.as_ref()).map_err(|error| {
                    Error::InvalidConfig(format!(
                        "会话沙箱根无法规范化 {}: {error}",
                        root.as_ref().display()
                    ))
                })
            })
            .transpose()?;
        Ok(Self {
            root,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// 预置一个会话并设为活动（builder 组装离线环境时使用），返回会话 id。
    pub fn preload(&self, label: &str, text: &str) -> Result<String> {
        let label = if label.trim().is_empty() {
            "粘贴文本"
        } else {
            label
        };
        let meta = self.insert(label, "粘贴文本".into(), text)?;
        self.inner.lock().unwrap().active = Some(meta.id.clone());
        Ok(meta.id)
    }

    fn load(text: &str) -> Arc<Vec<LogLine>> {
        Arc::new(
            text.lines()
                .enumerate()
                .map(|(index, raw)| LogLine::parse_numbered(raw, index as u32 + 1))
                .collect(),
        )
    }

    fn meta_of(id: &str, label: &str, source: &str, lines: &[LogLine]) -> LogSessionMeta {
        let mut meta = LogSessionMeta::new(id, label, source, lines.len());
        let timestamps: Vec<&String> = lines
            .iter()
            .filter_map(|line| line.timestamp.as_ref())
            .collect();
        if let (Some(first), Some(last)) = (timestamps.first(), timestamps.last()) {
            meta.time_range = Some(((*first).clone(), (*last).clone()));
        }
        meta
    }

    fn insert(&self, label: &str, source: String, text: &str) -> Result<LogSessionMeta> {
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = format!("s{}", inner.next_id);
        let lines = Self::load(text);
        let meta = Self::meta_of(&id, label, &source, &lines);
        inner.order.push(id.clone());
        inner.sessions.insert(
            id.clone(),
            SessionEntry {
                meta: meta.clone(),
                lines,
            },
        );
        Ok(meta)
    }

    fn resolve(&self, path: &str) -> Result<PathBuf> {
        let root = self.root.as_ref().ok_or_else(|| {
            Error::InvalidConfig("会话存储未配置文件沙箱根，仅支持文本来源".into())
        })?;
        let candidate = root.join(path);
        // canonicalize 是一次快速路径解析，不值得为此引入 spawn_blocking。
        let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
            Error::InvalidConfig(format!("路径无法解析 {}: {error}", candidate.display()))
        })?;
        if !canonical.starts_with(root) {
            return Err(Error::InvalidConfig(format!(
                "路径越界：{path}（沙箱根 {}）",
                root.display()
            )));
        }
        Ok(canonical)
    }
}

fn not_found(id: &str) -> Error {
    Error::InvalidTool {
        name: "log session".into(),
        reason: format!("会话不存在：{id}"),
    }
}

#[async_trait]
impl LogSessionStore for InMemorySessions {
    async fn open_text(&self, label: &str, text: &str) -> Result<LogSessionMeta> {
        let label = if label.trim().is_empty() {
            "粘贴文本"
        } else {
            label
        };
        self.insert(label, "粘贴文本".into(), text)
    }

    async fn open_file(&self, label: &str, path: &str) -> Result<LogSessionMeta> {
        let full = self.resolve(path)?;
        let text = tokio::fs::read_to_string(&full).await.map_err(|error| {
            Error::InvalidConfig(format!("日志文件无法读取 {}: {error}", full.display()))
        })?;
        let label = if label.trim().is_empty() {
            path.to_string()
        } else {
            label.to_string()
        };
        let source = format!("file: {path}");
        self.insert(&label, source, &text)
    }

    async fn list(&self) -> Vec<LogSessionMeta> {
        let inner = self.inner.lock().unwrap();
        inner
            .order
            .iter()
            .filter_map(|id| inner.sessions.get(id).map(|entry| entry.meta.clone()))
            .collect()
    }

    async fn active(&self) -> Option<LogSessionMeta> {
        let inner = self.inner.lock().unwrap();
        inner
            .active
            .as_ref()
            .and_then(|id| inner.sessions.get(id).map(|entry| entry.meta.clone()))
    }

    async fn set_active(&self, id: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.sessions.contains_key(id) {
            return Err(not_found(id));
        }
        inner.active = Some(id.to_string());
        Ok(())
    }

    async fn rename(&self, id: &str, label: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .sessions
            .get_mut(id)
            .ok_or_else(|| not_found(id))?
            .meta
            .label = label.to_string();
        Ok(())
    }

    async fn close(&self, id: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.sessions.remove(id).is_none() {
            return Err(not_found(id));
        }
        inner.order.retain(|existing| existing != id);
        // 关闭活动会话时回退到剩余的第一个，避免“活动会话悬空”。
        if inner.active.as_deref() == Some(id) {
            inner.active = inner.order.first().cloned();
        }
        Ok(())
    }

    async fn snapshot(&self, id: &str) -> Result<Arc<Vec<LogLine>>> {
        let inner = self.inner.lock().unwrap();
        inner
            .sessions
            .get(id)
            .map(|entry| entry.lines.clone())
            .ok_or_else(|| not_found(id))
    }
}
