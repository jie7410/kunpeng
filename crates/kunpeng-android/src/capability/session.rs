//! 日志会话接缝：把“一份日志”抽象成可打开、检索、对比的会话。
//!
//! 工具层的 log_* 家族（open/sessions/stats/grep/read/template_cluster/diff）
//! 都通过 [`LogSessionStore`] 取数据，而不是各自持有日志文本。这样：
//! - 离线：`InMemorySessions` 一次性载入文件/粘贴文本；
//! - 在线（规划中）：`adb logcat -d` 导出成会话后走同一套工具；持续性
//!   tail 订阅语义不同，后续以独立 trait 扩展，不塞进这里。
//!
//! v1 行数据整体驻留内存（`snapshot` 返回 `Arc<Vec<LogLine>>` 零拷贝共享）；
//! 大日志需要分块/索引时实现本 trait 替换，工具与提示词不变。

use async_trait::async_trait;
use kunpeng_core::Result;
use std::sync::Arc;

use crate::types::{LogLine, LogSessionMeta};

/// 会话集合：打开/关闭/切换 + 只读快照访问。
#[async_trait]
pub trait LogSessionStore: Send + Sync {
    /// 从粘贴文本建立会话。大段文本建议走 `open_file`，避免撑爆工具参数。
    async fn open_text(&self, label: &str, text: &str) -> Result<LogSessionMeta>;

    /// 从沙箱内的相对路径读文件建立会话；实现必须做根目录越界校验。
    async fn open_file(&self, label: &str, path: &str) -> Result<LogSessionMeta>;

    /// 按打开顺序返回全部会话元数据。
    async fn list(&self) -> Vec<LogSessionMeta>;

    /// 当前活动会话；未打开任何会话时为 `None`。`session` 参数缺省的工具
    /// 都作用于活动会话。
    async fn active(&self) -> Option<LogSessionMeta>;

    async fn set_active(&self, id: &str) -> Result<()>;

    /// 修改会话标签（对比分析前给两份日志起可读名字很有用）。
    async fn rename(&self, id: &str, label: &str) -> Result<()>;

    async fn close(&self, id: &str) -> Result<()>;

    /// 行数据只读快照，`line_no` 已填好原始行号。
    async fn snapshot(&self, id: &str) -> Result<Arc<Vec<LogLine>>>;
}
