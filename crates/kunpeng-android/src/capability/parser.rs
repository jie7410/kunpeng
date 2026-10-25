//! 日志解析接缝：原始日志文本 → 结构化崩溃报告。
//!
//! 解析是纯 CPU 同步函数，不做 IO；一次输入可能包含多份崩溃（例如混合了
//! main/crash buffer 的导出日志），返回值是数组。

use crate::types::CrashReport;

/// 单一崩溃格式的解析器。实现应当是确定性的：同一输入永远产生同一输出，
/// 这样崩溃指纹（`CrashReport::signature`）才能用于聚合去重。
pub trait LogParser: Send + Sync {
    /// 解析器名称，用于诊断与审计（例如说明哪个解析器产出了报告）。
    fn name(&self) -> &'static str;

    /// 从任意日志文本中识别本解析器负责的崩溃格式；没有命中返回空数组。
    fn parse(&self, input: &str) -> Vec<CrashReport>;
}

/// 解析器组合：按注册顺序依次尝试，结果合并。
///
/// 后期新增 tombstone/watchdog 解析器时，通过
/// [`crate::env::AndroidEnvBuilder::add_parser`] 追加，无需改工具层。
#[derive(Default)]
pub struct CompositeLogParser {
    parsers: Vec<Box<dyn LogParser>>,
}

impl CompositeLogParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_parser(mut self, parser: Box<dyn LogParser>) -> Self {
        self.parsers.push(parser);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.parsers.is_empty()
    }

    /// 已注册解析器名称，用于诊断“为什么没解析出来”。
    pub fn names(&self) -> Vec<&'static str> {
        self.parsers.iter().map(|parser| parser.name()).collect()
    }
}

impl LogParser for CompositeLogParser {
    fn name(&self) -> &'static str {
        "composite"
    }

    fn parse(&self, input: &str) -> Vec<CrashReport> {
        self.parsers
            .iter()
            .flat_map(|parser| parser.parse(input))
            .collect()
    }
}
