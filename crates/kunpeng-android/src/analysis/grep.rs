//! 多维检索与聚合视图。

use crate::types::{LogLevel, LogLine};

/// 检索条件，各维度 AND。
///
/// `time_from`/`time_to` 是时间戳的字典序闭区间：thread-time 与 epoch 两种
/// 格式内部定长时字典序即时间序；精度自由（`"10-25 14:23"` 匹配到分钟）。
#[derive(Debug, Clone, Default)]
pub struct GrepFilter {
    /// 整行大小写不敏感子串。
    pub keyword: Option<String>,
    /// tag 子串，大小写不敏感。
    pub tag: Option<String>,
    pub min_level: Option<LogLevel>,
    pub pid: Option<u32>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
}

pub fn matches(filter: &GrepFilter, line: &LogLine) -> bool {
    if let Some(keyword) = &filter.keyword {
        if !line
            .raw
            .to_ascii_lowercase()
            .contains(&keyword.to_ascii_lowercase())
        {
            return false;
        }
    }
    if let Some(tag) = &filter.tag {
        let Some(actual) = &line.tag else {
            return false;
        };
        if !actual
            .to_ascii_lowercase()
            .contains(&tag.to_ascii_lowercase())
        {
            return false;
        }
    }
    if let Some(min_level) = filter.min_level {
        // 解析不出级别的行保留：过滤的本意是降噪，不是丢弃无格式行。
        if let Some(level) = line.level {
            if level.rank() < min_level.rank() {
                return false;
            }
        }
    }
    if let Some(pid) = filter.pid {
        if line.pid != Some(pid) {
            return false;
        }
    }
    if let (Some(from), Some(timestamp)) = (&filter.time_from, &line.timestamp) {
        if timestamp.as_str() < from.as_str() {
            return false;
        }
    }
    if let (Some(to), Some(timestamp)) = (&filter.time_to, &line.timestamp) {
        if timestamp.as_str() > to.as_str() {
            return false;
        }
    }
    true
}

/// 全部命中行的下标。返回下标而不是克隆，调用方按需分页/开窗。
pub fn grep_indices(lines: &[LogLine], filter: &GrepFilter) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| matches(filter, line))
        .map(|(index, _)| index)
        .collect()
}

/// 聚合维度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountBy {
    Tag,
    Level,
    Pid,
}

/// 对命中行按维度聚合，count 降序、key 升序，取 top N；返回 (key, count)。
pub fn count_by(
    lines: &[LogLine],
    hits: &[usize],
    dimension: CountBy,
    top: usize,
) -> Vec<(String, usize)> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for &index in hits {
        let line = &lines[index];
        let key = match dimension {
            CountBy::Tag => line.tag.clone().unwrap_or_else(|| "<无tag>".into()),
            CountBy::Level => line
                .level
                .map(|level| level.as_char().to_string())
                .unwrap_or_else(|| "<无级别>".into()),
            CountBy::Pid => line
                .pid
                .map(|pid| format!("pid {pid}"))
                .unwrap_or_else(|| "<无pid>".into()),
        };
        if let Some(entry) = counts.iter_mut().find(|(k, _)| *k == key) {
            entry.1 += 1;
        } else {
            counts.push((key, 1));
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts.truncate(top);
    counts
}

/// 命中行 ±before/after 的上下文窗口；相邻窗口重叠时合并。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextWindow {
    /// 窗口首行下标（含）。
    pub start: usize,
    /// 窗口末行下标（含）。
    pub end: usize,
    /// 窗口内命中行的下标。
    pub hits: Vec<usize>,
}

pub fn context_windows(
    lines_len: usize,
    hits: &[usize],
    before: usize,
    after: usize,
    max_windows: usize,
) -> Vec<ContextWindow> {
    let mut windows: Vec<ContextWindow> = Vec::new();
    for &hit in hits {
        let start = hit.saturating_sub(before);
        let end = (hit + after).min(lines_len.saturating_sub(1));
        match windows.last_mut() {
            // 命中下标升序，只会与最后一个窗口重叠。
            Some(last) if start <= last.end + 1 => {
                last.end = last.end.max(end);
                last.hits.push(hit);
            }
            _ => windows.push(ContextWindow {
                start,
                end,
                hits: vec![hit],
            }),
        }
        if windows.len() >= max_windows {
            break;
        }
    }
    windows
}
