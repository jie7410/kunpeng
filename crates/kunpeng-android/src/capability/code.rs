//! 代码检索接缝。
//!
//! 当前查询语义是“子串匹配”；实现升级为正则/符号级检索时，`CodeQuery`
//! 的字段语义随实现变化，但 trait 与工具签名保持不变。

use async_trait::async_trait;
use kunpeng_core::Result;

use crate::types::{CodeHit, CodeQuery, CodeSnippet};

/// 在代码库中检索符号/文本，并按行区间读取源码。
///
/// 实现必须做根目录沙箱校验（canonicalize + 前缀比对），拒绝 `../` 越界，
/// 与 `kunpeng-tools` 内置 `fs_read` 的约定一致。
#[async_trait]
pub trait CodeIndex: Send + Sync {
    /// 返回命中列表，按实现自定义顺序（文件系统实现为目录遍历序）。
    async fn search(&self, query: &CodeQuery) -> Result<Vec<CodeHit>>;

    /// 读取 `path` 的 `[start_line, end_line]`（1-based 闭区间）。
    /// `end_line` 超出文件末尾时截到实际末行；`path` 越界返回错误。
    async fn read_range(&self, path: &str, start_line: u32, end_line: u32) -> Result<CodeSnippet>;

    /// 人可读的索引描述，进入 system prompt 与审计日志。
    fn describe(&self) -> String;
}
