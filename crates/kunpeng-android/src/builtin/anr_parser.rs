//! ANR 识别解析器（检测级实现）。
//!
//! 当前只识别 logcat 中 ActivityManager 输出的 ANR 摘要行（进程、原因），
//! 不解析 traces.txt 的完整线程堆栈——后者需要结合 `/data/anr/` 拉取，
//! 待在线排查（`DeviceConnector`）落地后作为堆栈来源补齐。

use super::strip_logcat_prefix;
use crate::capability::LogParser;
use crate::types::{CrashKind, CrashReport};

const MAX_EXCERPT_CHARS: usize = 4000;

/// 识别 `ANR in <process>` 摘要块，收集同一进程连续出现的 ANR 相关行。
pub struct AnrParser;

impl LogParser for AnrParser {
    fn name(&self) -> &'static str {
        "anr"
    }

    fn parse(&self, input: &str) -> Vec<CrashReport> {
        let mut reports = Vec::new();
        let mut current: Option<CrashReport> = None;

        for line in input.lines() {
            if let Some((_, rest)) = line.split_once("ANR in ") {
                if let Some(report) = current.take() {
                    reports.push(report);
                }
                let mut report = CrashReport::new(CrashKind::Anr);
                // `ANR in com.example.app (com.example.app/.MainActivity)`
                let process = rest.split_whitespace().next().unwrap_or(rest);
                report.process = Some(process.to_string());
                report.raw_excerpt = line.to_string();
                current = Some(report);
            } else if let Some(report) = current.as_mut() {
                // ANR 块内的 Reason / Load / CPU 行继续归入同一份报告。
                let body = strip_logcat_prefix(line);
                if let Some(reason) = body.strip_prefix("Reason:") {
                    report.message = Some(reason.trim().to_string());
                }
                if let Some(pid) = body.strip_prefix("PID:") {
                    report.pid = pid.split_whitespace().next().and_then(|p| p.parse().ok());
                }
                if !line.trim().is_empty() {
                    report.raw_excerpt.push('\n');
                    report.raw_excerpt.push_str(line);
                } else {
                    reports.push(current.take().expect("checked above"));
                }
            }
        }
        if let Some(report) = current.take() {
            reports.push(report);
        }

        for report in &mut reports {
            if report.raw_excerpt.chars().count() > MAX_EXCERPT_CHARS {
                report.raw_excerpt = report.raw_excerpt.chars().take(MAX_EXCERPT_CHARS).collect();
            }
        }
        reports
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_anr_summary() {
        let input = "\
10-25 15:00:00.100  1000  2000 E ActivityManager: ANR in com.example.app (com.example.app/.MainActivity)
10-25 15:00:00.101  1000  2000 E ActivityManager: PID: 4321
10-25 15:00:00.102  1000  2000 E ActivityManager: Reason: main thread timed out waiting for broadcast
10-25 15:00:00.103  1000  2000 E ActivityManager: CPU usage from 0ms to 100ms
";
        let reports = AnrParser.parse(input);
        assert_eq!(reports.len(), 1);
        let report = &reports[0];
        assert_eq!(report.kind, CrashKind::Anr);
        assert_eq!(report.process.as_deref(), Some("com.example.app"));
        assert_eq!(report.pid, Some(4321));
        assert!(report.message.as_deref().unwrap().contains("main thread"));
    }

    #[test]
    fn ignores_plain_logs() {
        assert!(AnrParser.parse("nothing to see here").is_empty());
    }
}
