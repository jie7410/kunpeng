//! 多源时间线：把多个会话按时间戳归并成一条统一时间线。
//!
//! 时间键分两种格式：thread-time（`MM-DD HH:MM:SS.mmm`，按“年内毫秒”
//! 折算成可比较的 u64）与 epoch（秒.毫秒）。同一时间线内格式必须一致，
//! 混用直接报错——错误的时间顺序比没有时间线更误导。
//! thread-time 没有年份，跨年日志会回绕；这是 logcat 格式的固有限制。

use kunpeng_core::{Error, Result};

use crate::types::LogLine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeFormat {
    Wall,
    Epoch,
}

/// 解析 thread-time 时间戳为可比较的“年内毫秒”。失败返回 `None`。
pub fn wall_key(timestamp: &str) -> Option<u64> {
    let mut parts = timestamp.split_whitespace();
    let date = parts.next()?;
    let time = parts.next()?;
    let (month, day) = date.split_once('-')?;
    let mut time_parts = time.split(':');
    let hour: u64 = time_parts.next()?.parse().ok()?;
    let minute: u64 = time_parts.next()?.parse().ok()?;
    let last = time_parts.next()?;
    let (second, millis): (u64, u64) = match last.split_once('.') {
        Some((second, millis)) => (second.parse().ok()?, millis.parse().ok()?),
        None => (last.parse().ok()?, 0),
    };
    let month: u64 = month.parse().ok()?;
    let day: u64 = day.parse().ok()?;
    Some(
        (month * 64 + day) * 86_400_000
            + hour * 3_600_000
            + minute * 60_000
            + second * 1_000
            + millis,
    )
}

/// 解析 epoch 时间戳（秒.毫秒）为毫秒。
pub fn epoch_key(timestamp: &str) -> Option<u64> {
    let seconds: f64 = timestamp.parse().ok()?;
    Some((seconds * 1000.0).round() as u64)
}

/// 判定一段行的时间戳格式：采样前 50 条带时间戳的行，占多数者胜出。
pub fn detect_format(lines: &[LogLine]) -> Option<TimeFormat> {
    let sample = lines
        .iter()
        .filter_map(|line| line.timestamp.as_deref())
        .take(50);
    let (mut wall, mut epoch) = (0, 0);
    for timestamp in sample {
        if timestamp.contains(':') {
            wall += 1;
        } else {
            epoch += 1;
        }
    }
    match wall.cmp(&epoch) {
        std::cmp::Ordering::Greater => Some(TimeFormat::Wall),
        std::cmp::Ordering::Less => Some(TimeFormat::Epoch),
        std::cmp::Ordering::Equal => None,
    }
}

fn key_of(format: TimeFormat, timestamp: &str) -> Option<u64> {
    match format {
        TimeFormat::Wall => wall_key(timestamp),
        TimeFormat::Epoch => epoch_key(timestamp),
    }
}

/// 归并后的一条时间线条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedEntry {
    pub session: String,
    pub line_no: u32,
    pub timestamp: Option<String>,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedTimeline {
    pub format: TimeFormat,
    pub entries: Vec<MergedEntry>,
    /// 无时间戳被跳过 + 超出 limit 被截断的行数。
    pub omitted: usize,
}

/// 把多个会话归并为按时间排序的统一时间线。
///
/// `sessions` 是 (标签, 行) 序列；`keyword` 可选，对整行原文做子串过滤。
pub fn cross_timeline(
    sessions: &[(String, &[LogLine])],
    keyword: Option<&str>,
    limit: usize,
) -> Result<MergedTimeline> {
    // 先统一格式：任一会话检测不出格式、或格式不一致都报错。
    let mut format: Option<TimeFormat> = None;
    for (label, lines) in sessions {
        let detected = detect_format(lines).ok_or_else(|| Error::InvalidTool {
            name: "log_cross_timeline".into(),
            reason: format!("会话 {label} 无法判定时间戳格式（缺少或格式杂乱）"),
        })?;
        match format {
            None => format = Some(detected),
            Some(existing) if existing == detected => {}
            Some(existing) => {
                return Err(Error::InvalidTool {
                    name: "log_cross_timeline".into(),
                    reason: format!(
                        "时间戳格式不一致：会话间混用了 {:?} 与 {detected:?}，无法对齐",
                        existing
                    ),
                })
            }
        }
    }
    let format = format.expect("sessions 非空时 format 必有值");

    let mut keyed: Vec<(u64, MergedEntry)> = Vec::new();
    let mut omitted = 0usize;
    for (label, lines) in sessions {
        // `lines` 经引用模式绑定为 `&&[LogLine]`，显式 iter 走切片迭代器。
        for line in lines.iter() {
            if let Some(keyword) = keyword {
                if !line.raw.contains(keyword) {
                    continue;
                }
            }
            match line.timestamp.as_deref().and_then(|ts| key_of(format, ts)) {
                Some(key) => keyed.push((
                    key,
                    MergedEntry {
                        session: label.clone(),
                        line_no: line.line_no.unwrap_or(0),
                        timestamp: line.timestamp.clone(),
                        raw: line.raw.clone(),
                    },
                )),
                None => omitted += 1,
            }
        }
    }
    keyed.sort_by_key(|(key, _)| *key);
    if keyed.len() > limit {
        omitted += keyed.len() - limit;
        keyed.truncate(limit);
    }
    Ok(MergedTimeline {
        format,
        entries: keyed.into_iter().map(|(_, entry)| entry).collect(),
        omitted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LogLine;

    #[test]
    fn wall_key_parses_thread_time() {
        let early = wall_key("10-25 08:00:00.000").unwrap();
        let later = wall_key("10-25 08:00:01.500").unwrap();
        assert_eq!(later - early, 1500);
        assert!(wall_key("10-26 08:00:00.000").unwrap() > later);
    }

    #[test]
    fn epoch_key_parses_seconds() {
        assert_eq!(epoch_key("1729849381.123"), Some(1_729_849_381_123));
    }

    fn parse_all(text: &str) -> Vec<LogLine> {
        text.lines()
            .enumerate()
            .map(|(i, raw)| LogLine::parse_numbered(raw, i as u32 + 1))
            .collect()
    }

    #[test]
    fn merges_sessions_in_time_order() {
        let main_log = parse_all(
            "10-25 08:00:02.000  1  1 I Main: second event\n\
             10-25 08:00:00.000  1  1 I Main: first event",
        );
        let events_log = parse_all("10-25 08:00:01.000  0  0 I boot_progress_x: [1000]");
        let timeline = cross_timeline(
            &[("main".into(), &main_log), ("events".into(), &events_log)],
            None,
            100,
        )
        .unwrap();
        assert_eq!(timeline.entries.len(), 3);
        let order: Vec<&str> = timeline
            .entries
            .iter()
            .map(|entry| entry.raw.split_whitespace().last().unwrap())
            .collect();
        assert_eq!(order, ["event", "[1000]", "event"]);
        assert_eq!(timeline.entries[0].session, "main");
    }

    #[test]
    fn rejects_mixed_time_formats() {
        let wall = parse_all("10-25 08:00:00.000  1  1 I A: x");
        let epoch = parse_all("1729849381.123  1  1 I B: y");
        let error = cross_timeline(
            &[("wall".into(), &wall), ("epoch".into(), &epoch)],
            None,
            100,
        )
        .unwrap_err();
        assert!(matches!(error, Error::InvalidTool { .. }));
    }
}
