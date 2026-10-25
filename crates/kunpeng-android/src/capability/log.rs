//! 日志来源接缝。

use async_trait::async_trait;
use kunpeng_core::Result;

use crate::types::{LogQuery, LogSlice};

/// 按条件拉取一段日志。
///
/// 离线模式读内存/文件；在线模式实现此 trait 对接 `adb logcat`（拉取式，如
/// `adb -s <serial> logcat -d`，配合过滤参数）。持续性订阅（实时跟踪）语义
/// 不同，后续以独立 trait 扩展，不塞进 `fetch`。
#[async_trait]
pub trait LogProvider: Send + Sync {
    /// 返回命中行；超过 `max_lines` 时置 `truncated`，由调用方决定如何收窄。
    async fn fetch(&self, query: &LogQuery) -> Result<LogSlice>;

    /// 人可读的来源描述，进入 system prompt 与审计日志，例如
    /// `"静态日志 crash-20261025.txt（1.2 万行）"`、`"设备 emulator-5554 logcat"`。
    fn describe(&self) -> String;
}
