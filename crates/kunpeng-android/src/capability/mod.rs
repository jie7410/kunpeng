//! 能力接缝：工具层唯一依赖的抽象。
//!
//! 每个 trait 对应一类可替换的外部能力：
//!
//! | trait | 默认离线实现 | 计划中的升级实现 |
//! |---|---|---|
//! | [`LogProvider`] | `builtin::StaticLogProvider` | `AdbLogcatProvider`（实时设备日志）|
//! | [`LogParser`] | `builtin::JavaCrashParser` / `AnrParser` | tombstone、watchdog 解析器 |
//! | [`CodeIndex`] | `builtin::FsCodeIndex`（文件系统扫描）| tree-sitter 符号索引、ripgrep |
//! | [`DeviceConnector`] | `builtin::UnavailableDeviceConnector` | `AdbConnector`（spawn adb）|
//!
//! 工具（[`crate::tools`]）只持有 `Arc<dyn Trait>`，实现替换通过
//! [`crate::env::AndroidEnvBuilder`] 注入，工具与 Agent 不感知变化。

pub mod code;
pub mod device;
pub mod log;
pub mod parser;
pub mod session;

pub use code::CodeIndex;
pub use device::{DeviceConnector, DeviceLogProvider};
pub use log::LogProvider;
pub use parser::{CompositeLogParser, LogParser};
pub use session::LogSessionStore;
