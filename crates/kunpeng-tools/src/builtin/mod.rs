//! 内置工具。默认启用 `tools-builtin` feature。
//!
//! 内置工具都带明确边界：fs 限制沙箱目录，http 限制 host allowlist，time
//! 无副作用。shell 等高风险工具应放在独立 feature 中，默认关闭。

mod fs_read;
mod http_fetch;
mod time;

pub use fs_read::FsReadTool;
pub use http_fetch::HttpFetchTool;
pub use time::TimeTool;
