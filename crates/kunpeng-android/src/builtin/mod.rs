//! 离线默认实现。
//!
//! 这些实现刻意简单：能支撑“离线崩溃分诊”跑通端到端链路，但性能与覆盖面
//! 不是目标（文件扫描、子串匹配、仅 Java/ANR 两种格式）。生产实现按
//! [`crate::capability`] 的 trait 整体替换，不要在默认实现上叠加特性。
//!
//! 尚未提供默认实现的解析器（按 trait 追加即可）：
//! - native tombstone（`/data/tombstones`，signal + backtrace + so 偏移）
//! - watchdog 重启日志

mod anr_parser;
mod fs_code;
mod java_parser;
mod offline_device;
mod sessions;
mod static_log;

pub use anr_parser::AnrParser;
pub use fs_code::FsCodeIndex;
pub use java_parser::JavaCrashParser;
pub use offline_device::UnavailableDeviceConnector;
pub use sessions::InMemorySessions;
pub use static_log::StaticLogProvider;

/// 剥离 logcat 行前缀，返回正文。兼容两种常见格式：
/// - `E/Tag( 1234): message`（带 PID 括号）
/// - `TIME PID TID E Tag: message`（thread-time）
///
/// 识别 thread-time 时要求 tag 前存在单字符级别 token，避免把已是正文的
/// 行（如 `Process: com.x`）再剥一层。无法识别时返回原行。
pub(crate) fn strip_logcat_prefix(line: &str) -> &str {
    if let Some(index) = line.find("): ") {
        if line[..index].contains('(') {
            return &line[index + 3..];
        }
    }
    if let Some(index) = line.find(": ") {
        let head = &line[..index];
        let mut tokens = head.rsplit(' ');
        let tag = tokens.next().unwrap_or_default();
        let tag_ok = !tag.is_empty()
            && tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-' | '$'));
        let level_ok = tokens
            .next()
            .is_some_and(|level| level.len() == 1 && "VDIWEF".contains(level));
        if tag_ok && level_ok {
            return &line[index + 2..];
        }
    }
    line.trim()
}
