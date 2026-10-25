//! 启动时间线：boot 里程碑、相邻间隔与卡点。
//!
//! 里程碑以 events buffer 的 `boot_progress_*` / `sf_stop_bootanim` /
//! `wm_boot_animation_done` 为准——它们携带自内核启动起的 elapsedMs，
//! 可以直接做差。main buffer 的关键词标记（SystemServer 等）没有统一的
//! elapsed 基准，单独列出 wall_markers，不做跨时钟对齐。

use crate::analysis::events::{decode_event, DecodedEvent};
use crate::types::LogLine;

/// 一个启动里程碑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    pub name: String,
    /// 自内核启动起的毫秒数（事件 payload）。
    pub elapsed_ms: i64,
    pub line_no: u32,
    /// 与上一里程碑的间隔；首个为 `None`。
    pub delta_from_previous_ms: Option<i64>,
}

/// main buffer 中的启动相关关键词标记（无 elapsed 基准，仅按行序展示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WallMarker {
    pub line_no: u32,
    pub timestamp: Option<String>,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootTimeline {
    /// 按 elapsedMs 升序。
    pub milestones: Vec<Milestone>,
    /// 间隔最大的前 3 段，格式 `a -> b`。
    pub slowest_gaps: Vec<(String, i64)>,
    pub wall_markers: Vec<WallMarker>,
}

const WALL_MARKER_KEYWORDS: [&str; 5] = [
    "Linux version",
    "Entered the Android system server",
    "System now ready",
    "System is ready",
    "Boot completed",
];

fn as_milestone(event: &DecodedEvent) -> Option<Milestone> {
    let is_boot = event.name.starts_with("boot_progress_")
        || event.name == "sf_stop_bootanim"
        || event.name == "wm_boot_animation_done";
    if !is_boot {
        return None;
    }
    let elapsed_ms = event.values.first()?.parse().ok()?;
    Some(Milestone {
        name: event.name.clone(),
        elapsed_ms,
        line_no: event.line_no,
        delta_from_previous_ms: None,
    })
}

pub fn boot_timeline(lines: &[LogLine]) -> BootTimeline {
    // 同一里程碑可能出现多次（重启/多 buffer 重复），保留首次出现的值。
    let mut milestones: Vec<Milestone> = Vec::new();
    for line in lines {
        if let Some(event) = decode_event(line) {
            if let Some(milestone) = as_milestone(&event) {
                if !milestones
                    .iter()
                    .any(|existing| existing.name == milestone.name)
                {
                    milestones.push(milestone);
                }
            }
        }
    }
    milestones.sort_by_key(|milestone| milestone.elapsed_ms);
    for index in 1..milestones.len() {
        let previous = milestones[index - 1].elapsed_ms;
        milestones[index].delta_from_previous_ms = Some(milestones[index].elapsed_ms - previous);
    }

    let mut slowest_gaps: Vec<(String, i64)> = milestones
        .windows(2)
        .filter_map(|pair| {
            pair[1]
                .delta_from_previous_ms
                .map(|delta| (format!("{} -> {}", pair[0].name, pair[1].name), delta))
        })
        .collect();
    slowest_gaps.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    slowest_gaps.truncate(3);

    let wall_markers = lines
        .iter()
        .filter(|line| {
            WALL_MARKER_KEYWORDS
                .iter()
                .any(|keyword| line.raw.contains(keyword))
        })
        .map(|line| WallMarker {
            line_no: line.line_no.unwrap_or(0),
            timestamp: line.timestamp.clone(),
            raw: line.raw.clone(),
        })
        .collect();

    BootTimeline {
        milestones,
        slowest_gaps,
        wall_markers,
    }
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

    const BOOT_LOG: &str = "\
--------- beginning of events
10-25 08:00:05.000  0  0 I boot_progress_start: [5000]
10-25 08:00:08.000  0  0 I boot_progress_preload_start: [8000]
10-25 08:00:12.000  0  0 I boot_progress_preload_end: [12000]
10-25 08:00:13.000  0  0 I boot_progress_system_run: [13000]
10-25 08:00:41.000  0  0 I boot_progress_pms_ready: [41000]
10-25 08:01:15.000  0  0 I boot_progress_enable_screen: [75000]
--------- beginning of main
10-25 08:00:13.500  0  500 I SystemServer: Entered the Android system server
10-25 08:01:16.000  1000  2000 I ActivityManager: System now ready
";

    #[test]
    fn builds_milestones_with_deltas() {
        let lines = parse_all(BOOT_LOG);
        let timeline = boot_timeline(&lines);
        assert_eq!(timeline.milestones.len(), 6);
        assert_eq!(timeline.milestones[0].name, "boot_progress_start");
        assert_eq!(timeline.milestones[0].delta_from_previous_ms, None);
        assert_eq!(
            timeline.milestones[1].delta_from_previous_ms,
            Some(3000),
            "8s - 5s"
        );
        assert_eq!(
            timeline.milestones[4].name, "boot_progress_pms_ready",
            "按 elapsed 排序而非行序"
        );
    }

    #[test]
    fn flags_slowest_gaps() {
        let timeline = boot_timeline(&parse_all(BOOT_LOG));
        assert_eq!(timeline.slowest_gaps.len(), 3);
        // 最大间隔：pms_ready(41s) -> enable_screen(75s) = 34s
        assert_eq!(
            timeline.slowest_gaps[0].0,
            "boot_progress_pms_ready -> boot_progress_enable_screen"
        );
        assert_eq!(timeline.slowest_gaps[0].1, 34000);
    }

    #[test]
    fn collects_wall_markers() {
        let timeline = boot_timeline(&parse_all(BOOT_LOG));
        assert_eq!(timeline.wall_markers.len(), 2);
        assert!(timeline.wall_markers[0]
            .raw
            .contains("Entered the Android system server"));
    }
}
