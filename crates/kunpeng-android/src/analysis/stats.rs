//! 全貌统计。

use crate::types::{LogLevel, LogLine};

/// 一份日志的整体画像。聚合条目按 count 降序、key 升序，取 top N。
#[derive(Debug, Clone, PartialEq)]
pub struct LogStats {
    pub total_lines: usize,
    /// 没解析出级别的行（分隔线、嵌入的非 logcat 文本等）。
    pub unparseable_lines: usize,
    pub by_level: Vec<(String, usize)>,
    pub by_tag: Vec<(String, usize)>,
    pub by_pid: Vec<(String, usize)>,
    pub time_range: Option<(String, String)>,
    /// `--------- beginning of X` 出现过的 buffer 名。
    pub buffer_markers: Vec<String>,
    /// 崩溃/ANR 标记行号，供快速跳转。
    pub crash_marker_lines: Vec<u32>,
}

/// 稳定排序：count 降序，同 count 按 key 升序（结果可复现，方便测试与缓存）。
fn top_n(counts: &mut Vec<(String, usize)>, top: usize) {
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts.truncate(top);
}

pub fn stats(lines: &[LogLine], top: usize) -> LogStats {
    let mut level_counts: Vec<(LogLevel, usize)> =
        LogLevel::variants().into_iter().map(|l| (l, 0)).collect();
    let mut tag_counts: Vec<(String, usize)> = Vec::new();
    let mut pid_counts: Vec<(String, usize)> = Vec::new();
    let mut unparseable = 0usize;
    let mut markers = Vec::new();
    let mut crash_markers = Vec::new();
    let mut first_time: Option<&String> = None;
    let mut last_time: Option<&String> = None;

    for line in lines {
        match line.level {
            Some(level) => {
                if let Some(entry) = level_counts.iter_mut().find(|(l, _)| *l == level) {
                    entry.1 += 1;
                }
            }
            None => unparseable += 1,
        }
        if let Some(tag) = &line.tag {
            bump(&mut tag_counts, tag.clone(), 1);
        }
        if let Some(pid) = line.pid {
            bump(&mut pid_counts, format!("pid {pid}"), 1);
        }
        if let Some(timestamp) = &line.timestamp {
            if first_time.is_none() {
                first_time = Some(timestamp);
            }
            last_time = Some(timestamp);
        }
        if line.raw.starts_with("--------- beginning of") {
            if let Some(name) = line.raw.rsplit(' ').next() {
                if !markers.contains(&name.to_string()) {
                    markers.push(name.to_string());
                }
            }
        }
        if crate::analysis::catalog::is_crash_marker(&line.raw) {
            if let Some(line_no) = line.line_no {
                crash_markers.push(line_no);
            }
        }
    }

    // 级别按固定严重度顺序输出，不按 count 排序。
    let by_level = level_counts
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(level, count)| (level.as_char().to_string(), count))
        .collect();
    top_n(&mut tag_counts, top);
    top_n(&mut pid_counts, top);

    LogStats {
        total_lines: lines.len(),
        unparseable_lines: unparseable,
        by_level,
        by_tag: tag_counts,
        by_pid: pid_counts,
        time_range: first_time
            .zip(last_time)
            .map(|(f, l)| (f.clone(), l.clone())),
        buffer_markers: markers,
        crash_marker_lines: crash_markers,
    }
}

fn bump(counts: &mut Vec<(String, usize)>, key: String, delta: usize) {
    if let Some(entry) = counts.iter_mut().find(|(k, _)| *k == key) {
        entry.1 += delta;
    } else {
        counts.push((key, delta));
    }
}
