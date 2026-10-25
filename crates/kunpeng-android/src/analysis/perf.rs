//! 性能信号扫描：启动耗时、掉帧、StrictMode、GC、LMK。
//!
//! 这些信号散落在 main buffer 的普通日志里，靠确定性模式匹配收集，
//! 给“代码优化”场景提供运行期证据。匹配关键词刻意保守（多重条件），
//! 宁可漏报也不要把普通行误报成性能问题。

use crate::types::LogLine;

/// 一条性能信号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfEntry {
    pub line_no: u32,
    pub raw: String,
    pub process: Option<String>,
    /// 可量化指标的毫秒值（Displayed 耗时、掉帧时长等）。
    pub value_ms: Option<i64>,
}

/// 同类信号聚合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfGroup {
    pub kind: &'static str,
    pub description: &'static str,
    pub count: usize,
    /// 样例上限，避免巨量重复行占满输出。
    pub entries: Vec<PerfEntry>,
}

/// 样例上限。
const MAX_ENTRIES: usize = 10;

/// 解析 `+3s456ms` / `+2041ms` 形式的时长；纯数字不是时长。
fn parse_duration(text: &str) -> Option<i64> {
    let text = text.trim().trim_start_matches('+');
    if let Some(millis) = text.strip_suffix("ms") {
        if let Ok(value) = millis.parse() {
            return Some(value);
        }
    }
    let (seconds, rest) = text.split_once('s')?;
    let seconds: i64 = seconds.parse().ok()?;
    let millis: i64 = rest.trim_end_matches("ms").parse().ok()?;
    Some(seconds * 1000 + millis)
}

fn entry_of(line: &LogLine, process: Option<String>, value_ms: Option<i64>) -> PerfEntry {
    PerfEntry {
        line_no: line.line_no.unwrap_or(0),
        raw: line.raw.clone(),
        process,
        value_ms,
    }
}

/// 从 Displayed 行提取进程与耗时：
/// `Displayed com.example.app/.MainActivity: +3s456ms`。
fn scan_displayed(line: &LogLine) -> Option<PerfEntry> {
    let message = line.message.as_deref()?;
    let rest = message.trim().strip_prefix("Displayed ")?;
    let (process, tail) = rest.split_once(':')?;
    // tail 形如 " +3s456ms (…)"; 取最后一个 +Nms 片段（GPU 完成时间在后）。
    let duration = tail.split_whitespace().filter_map(parse_duration).max();
    Some(entry_of(line, Some(process.trim().to_string()), duration))
}

/// `Choreographer: Skipped 120 frames!  (taking 2041ms)`。
fn scan_skipped_frames(line: &LogLine) -> Option<PerfEntry> {
    let message = line.message.as_deref()?;
    if !(message.contains("Skipped ") && message.contains("frames")) {
        return None;
    }
    let taking = message
        .split(['(', ')'])
        .find_map(|part| part.split_whitespace().find_map(parse_duration));
    let frames = message
        .strip_prefix("Skipped ")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|count| count.parse::<i64>().ok());
    // value 用掉帧市场；frames 数写进 process 槽位太混乱，留在原文里。
    let process = line.pid.map(|pid| format!("pid {pid}"));
    let _ = frames;
    Some(entry_of(line, process, taking))
}

fn scan_keyword(line: &LogLine, needles: &[&str]) -> Option<PerfEntry> {
    let hit = needles.iter().any(|needle| line.raw.contains(needle));
    if !hit {
        return None;
    }
    let process = line.pid.map(|pid| format!("pid {pid}"));
    Some(entry_of(line, process, None))
}

/// LMK kill 行：`lowmemorykiller: Kill 'com.example.app'`。
fn scan_lmk(line: &LogLine) -> Option<PerfEntry> {
    if !(line.raw.contains("lowmemorykiller") && line.raw.contains("Kill")) {
        return None;
    }
    let process = line.raw.split('\'').nth(1).map(str::to_string);
    Some(entry_of(line, process, None))
}

/// 扫描会话中的全部性能信号，按类别分组返回（固定类别顺序）。
pub fn perf_signals(lines: &[LogLine]) -> Vec<PerfGroup> {
    let mut displayed = Vec::new();
    let mut skipped = Vec::new();
    let mut strict = Vec::new();
    let mut gc = Vec::new();
    let mut lmk = Vec::new();

    for line in lines {
        if let Some(entry) = scan_displayed(line) {
            displayed.push(entry);
        }
        if let Some(entry) = scan_skipped_frames(line) {
            skipped.push(entry);
        }
        if let Some(entry) = scan_keyword(line, &["StrictMode policy violation", "StrictMode:"]) {
            strict.push(entry);
        }
        if let Some(entry) = scan_keyword(
            line,
            &[
                "concurrent copying GC freed",
                "Blocking GC",
                "WaitForGcToComplete",
            ],
        ) {
            gc.push(entry);
        }
        if let Some(entry) = scan_lmk(line) {
            lmk.push(entry);
        }
    }

    let groups = [
        ("app_displayed", "应用启动到首帧（Displayed）", displayed),
        ("skipped_frames", "主线程掉帧（Choreographer）", skipped),
        ("strict_mode", "StrictMode 违规", strict),
        ("gc_pressure", "GC 压力", gc),
        ("lmk_kill", "低内存查杀（LMK）", lmk),
    ];
    groups
        .into_iter()
        .filter(|(_, _, entries)| !entries.is_empty())
        .map(|(kind, description, entries)| {
            let count = entries.len();
            let sample = entries.into_iter().take(MAX_ENTRIES).collect();
            PerfGroup {
                kind,
                description,
                count,
                entries: sample,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LogLine;

    fn parse_all(text: &str) -> Vec<LogLine> {
        text.lines()
            .enumerate()
            .map(|(i, raw)| LogLine::parse_numbered(raw, i as u32 + 1))
            .collect()
    }

    const PERF_LOG: &str = "\
10-25 08:01:30.100  1000  2050 I ActivityTaskManager: Displayed com.example.app/.MainActivity: +3s456ms
10-25 08:01:35.100  1234  5678 I Choreographer: Skipped 120 frames!  (taking 2041ms)
10-25 08:01:36.100  1234  5678 D StrictMode policy violation: ~NetworkOnMainThreadOperation
10-25 08:01:37.100  1234  5678 I art: Background concurrent copying GC freed 10MB
10-25 08:01:38.100  1000  1500 I lowmemorykiller: Kill 'com.example.app' (12345)
";

    #[test]
    fn parses_duration_tokens() {
        assert_eq!(parse_duration("+3s456ms"), Some(3456));
        assert_eq!(parse_duration("+2041ms"), Some(2041));
        assert_eq!(parse_duration("plain"), None);
    }

    #[test]
    fn collects_all_signal_groups() {
        let groups = perf_signals(&parse_all(PERF_LOG));
        let kinds: Vec<_> = groups.iter().map(|group| group.kind).collect();
        assert_eq!(
            kinds,
            [
                "app_displayed",
                "skipped_frames",
                "strict_mode",
                "gc_pressure",
                "lmk_kill"
            ]
        );

        let displayed = &groups[0].entries[0];
        assert_eq!(
            displayed.process.as_deref(),
            Some("com.example.app/.MainActivity")
        );
        assert_eq!(displayed.value_ms, Some(3456));

        let skipped = &groups[1].entries[0];
        assert_eq!(skipped.value_ms, Some(2041));

        let lmk = &groups[4].entries[0];
        assert_eq!(lmk.process.as_deref(), Some("com.example.app"));
    }

    #[test]
    fn ignores_normal_lines() {
        assert!(perf_signals(&parse_all("10-25 08:00:00.000  1  1 I App: all good")).is_empty());
    }
}
