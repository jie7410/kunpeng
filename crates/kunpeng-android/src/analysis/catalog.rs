//! 崩溃事件目录：按标记行切块，交给解析器，产出带原始行号的目录。

use crate::capability::LogParser;
use crate::types::{CrashReport, LogLine};

/// 一个崩溃块的行数上限：块以空行结束，超过仍未结束按截断处理。
const MAX_BLOCK_LINES: usize = 300;

/// 崩溃/ANR 标记行。tombstone/watchdog 解析器接入后在此追加标记。
pub fn is_crash_marker(raw: &str) -> bool {
    raw.contains("FATAL EXCEPTION") || raw.contains("ANR in ")
}

/// 目录项：报告 + 起始原始行号。
#[derive(Debug, Clone, PartialEq)]
pub struct CrashEntry {
    pub start_line: u32,
    pub report: CrashReport,
}

/// 按 signature 聚合的崩溃组。
#[derive(Debug, Clone, PartialEq)]
pub struct CrashGroup {
    pub signature: String,
    pub count: usize,
    pub first_line: u32,
    /// 最新的完整报告（最后一个同签名崩溃）。
    pub latest: CrashReport,
}

/// 扫描全部崩溃块并解析。同一签名可能出现多次（复现/重试），用
/// [`group_by_signature`] 聚合后输出目录。
pub fn crash_catalog(lines: &[LogLine], parser: &dyn LogParser) -> Vec<CrashEntry> {
    let mut entries = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if !is_crash_marker(&lines[index].raw) {
            index += 1;
            continue;
        }
        let start = index;
        let mut end = (start + MAX_BLOCK_LINES).min(lines.len());
        for (candidate, line) in lines.iter().enumerate().take(end).skip(start) {
            let raw = line.raw.trim();
            // 块以空行或 buffer 分隔线（`--------- beginning of ...`）结束，
            // 否则紧邻的下一个崩溃会被并进当前块的起始行号。
            if raw.is_empty() || raw.starts_with("---------") {
                end = candidate;
                break;
            }
        }
        let segment: String = lines[start..end]
            .iter()
            .map(|line| line.raw.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for report in parser.parse(&segment) {
            entries.push(CrashEntry {
                start_line: lines[start].line_no.unwrap_or(start as u32 + 1),
                report,
            });
        }
        index = end;
    }
    entries
}

/// 按 signature 聚合，count 降序；组内保留首次行号与最新报告。
pub fn group_by_signature(entries: Vec<CrashEntry>) -> Vec<CrashGroup> {
    let mut groups: Vec<CrashGroup> = Vec::new();
    for entry in entries {
        let signature = entry.report.signature();
        match groups.iter_mut().find(|group| group.signature == signature) {
            Some(group) => {
                group.count += 1;
                group.latest = entry.report.clone();
            }
            None => groups.push(CrashGroup {
                signature,
                count: 1,
                first_line: entry.start_line,
                latest: entry.report,
            }),
        }
    }
    groups.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.first_line.cmp(&b.first_line))
    });
    groups
}
