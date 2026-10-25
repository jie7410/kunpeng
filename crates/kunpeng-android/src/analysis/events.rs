//! event log 解码：`events` buffer 的 `EVENT_NAME: [payload]` 行 → 结构化事件。
//!
//! logcat 已经把二进制 event 解成文本（payload 为逗号分隔值；字符串可含
//! 逗号/嵌套列表）。本模块按已知事件表给 payload 字段命名；未知事件保留
//! 原始值。字段名以 AOSP `EventLogTags.logtags` 为准，随版本演进可能有出入，
//! 表只做“最有用的那部分”，解码失败不报错、按未知事件透传。

use crate::types::LogLine;

struct EventSpec {
    name: &'static str,
    fields: &'static [&'static str],
    description: &'static str,
}

const EVENT_SPECS: &[EventSpec] = &[
    EventSpec {
        name: "boot_progress_start",
        fields: &["elapsedMs"],
        description: "zygote 启动",
    },
    EventSpec {
        name: "boot_progress_preload_start",
        fields: &["elapsedMs"],
        description: "zygote 资源预加载开始",
    },
    EventSpec {
        name: "boot_progress_preload_end",
        fields: &["elapsedMs"],
        description: "zygote 资源预加载结束",
    },
    EventSpec {
        name: "boot_progress_system_run",
        fields: &["elapsedMs"],
        description: "SystemServer 开始运行",
    },
    EventSpec {
        name: "boot_progress_pms_ready",
        fields: &["elapsedMs"],
        description: "PackageManager 就绪",
    },
    EventSpec {
        name: "boot_progress_ams_ready",
        fields: &["elapsedMs"],
        description: "ActivityManager 就绪",
    },
    EventSpec {
        name: "boot_progress_enable_screen",
        fields: &["elapsedMs"],
        description: "系统允许亮屏",
    },
    EventSpec {
        name: "sf_stop_bootanim",
        fields: &["elapsedMs"],
        description: "SurfaceFlinger 结束开机动画",
    },
    EventSpec {
        name: "wm_boot_animation_done",
        fields: &["elapsedMs"],
        description: "开机动画完成",
    },
    EventSpec {
        name: "am_proc_start",
        fields: &["uid", "pid", "processName", "processType"],
        description: "进程启动",
    },
    EventSpec {
        name: "am_proc_died",
        fields: &["uid", "pid", "processName", "status"],
        description: "进程退出",
    },
    EventSpec {
        name: "am_kill",
        fields: &["user", "pid", "processName", "oomAdj", "procState"],
        description: "进程被杀（含 LMK）",
    },
    EventSpec {
        name: "am_anr",
        fields: &["user", "pid", "processName", "component", "subject"],
        description: "发生 ANR",
    },
    EventSpec {
        name: "am_crash",
        fields: &["user", "pid", "processName", "flags"],
        description: "应用崩溃",
    },
    EventSpec {
        name: "am_wtf",
        fields: &["user", "pid", "processName", "flags"],
        description: "What a Terrible Failure",
    },
];

fn lookup(name: &str) -> Option<(&'static [&'static str], &'static str)> {
    if let Some(spec) = EVENT_SPECS.iter().find(|spec| spec.name == name) {
        return Some((spec.fields, spec.description));
    }
    // 新版本会追加 boot_progress_* 里程碑；统一按 elapsedMs 解读。
    if name.starts_with("boot_progress_") {
        return Some((&["elapsedMs"], "启动里程碑"));
    }
    None
}

/// 顶层逗号分隔，跳过嵌套列表与引号字符串内的逗号；去掉字符串值的引号。
fn split_payload(inner: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut in_quote = false;
    for character in inner.chars() {
        match character {
            '"' => {
                in_quote = !in_quote;
                current.push(character);
            }
            '[' if !in_quote => {
                depth += 1;
                current.push(character);
            }
            ']' if !in_quote => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ',' if depth == 0 && !in_quote => {
                values.push(current.trim().trim_matches('"').to_string());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    let last = current.trim();
    if !last.is_empty() {
        values.push(last.trim_matches('"').to_string());
    }
    values
}

/// 解码后的 event log 事件。
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedEvent {
    pub line_no: u32,
    pub timestamp: Option<String>,
    /// event 名，即 logcat 的 tag。
    pub name: String,
    /// 命中已知事件表时的字段名（与 values 位置对齐）；未知事件为 None。
    pub fields: Option<&'static [&'static str]>,
    /// payload 顶层值。
    pub values: Vec<String>,
    pub description: Option<&'static str>,
}

/// 尝试把一行解码成 event：要求 tag 存在且 message 是 `[...]` 形式。
/// 不是 event 行返回 `None`（main/system buffer 的普通行大多如此）。
pub fn decode_event(line: &LogLine) -> Option<DecodedEvent> {
    let name = line.tag.as_deref()?;
    let message = line.message.as_deref()?;
    let inner = message.strip_prefix('[')?.strip_suffix(']')?;
    let values = split_payload(inner);
    let (fields, description) = match lookup(name) {
        Some((fields, description)) => (Some(fields), Some(description)),
        None => (None, None),
    };
    Some(DecodedEvent {
        line_no: line.line_no.unwrap_or(0),
        timestamp: line.timestamp.clone(),
        name: name.to_string(),
        fields,
        values,
        description,
    })
}

/// 扫描会话中全部 event 行；`name_filter` 为事件名子串过滤。
pub fn decode_events(lines: &[LogLine], name_filter: Option<&str>) -> Vec<DecodedEvent> {
    lines
        .iter()
        .filter_map(decode_event)
        .filter(|event| match name_filter {
            Some(filter) => event.name.contains(filter),
            None => true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LogLine;

    #[test]
    fn decodes_known_event_with_fields() {
        let line = LogLine::parse(
            "10-25 08:00:20.000  1000  2000 I am_proc_start: [1000,12345,com.example.app,activity]",
        );
        let event = decode_event(&line).unwrap();
        assert_eq!(event.name, "am_proc_start");
        assert_eq!(
            event.values,
            ["1000", "12345", "com.example.app", "activity"]
        );
        let fields = event.fields.unwrap();
        assert_eq!(fields[2], "processName");
        // 字段名与值对齐
        assert_eq!(fields.len(), event.values.len());
    }

    #[test]
    fn decodes_boot_progress_by_prefix() {
        let line = LogLine::parse("10-25 08:00:05.000  0  0 I boot_progress_new_thing: [5000]");
        let event = decode_event(&line).unwrap();
        assert_eq!(event.fields.unwrap(), &["elapsedMs"]);
        assert_eq!(event.values, ["5000"]);
    }

    #[test]
    fn unknown_event_passes_through() {
        let line = LogLine::parse("10-25 08:00:01.000  0  0 I some_future_event: [1,a]");
        let event = decode_event(&line).unwrap();
        assert_eq!(event.fields, None);
        assert_eq!(event.description, None);
        assert_eq!(event.values.len(), 2);
    }

    #[test]
    fn handles_nested_payload_and_quoted_comma() {
        let line = LogLine::parse("10-25 08:00:01.000  0  0 I complex_event: [[1,2],\"a,b\",3]");
        let event = decode_event(&line).unwrap();
        assert_eq!(event.values, ["[1,2]", "a,b", "3"]);
    }

    #[test]
    fn plain_logcat_lines_are_not_events() {
        let line = LogLine::parse("10-25 08:00:01.000  0  0 I ActivityManager: normal message");
        assert!(decode_event(&line).is_none());
    }
}
