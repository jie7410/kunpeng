//! 确定性分析函数：纯 CPU 计算，输入 `&[LogLine]`，无 IO、无状态。
//!
//! 工具层只是参数解析 + 调这里的函数 + 组 JSON。大日志需要索引/分块时，
//! 这里整体替换为查询引擎 trait，工具签名与提示词不变（对应 capability
//! 层 `LogSessionStore` 的演进路线）。

pub mod catalog;
pub mod grep;
pub mod stats;
pub mod templates;
