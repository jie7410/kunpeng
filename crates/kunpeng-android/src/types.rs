//! Android 诊断领域模型。
//!
//! 这些类型会进入工具输出、会话历史和后期的崩溃知识库，serde 结构是持久化契约：
//! 新字段必须 `#[serde(default)]`，不要改动已有 tag。

use serde::{Deserialize, Serialize};

/// logcat 日志级别。排序即严重程度，`rank` 供 `min_level` 过滤使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LogLevel {
    Verbose,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl LogLevel {
    /// 解析 logcat 单字符缩写或全称。不认识的输入返回 `None`，由调用方决定报错或忽略。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "V" | "VERBOSE" => Some(Self::Verbose),
            "D" | "DEBUG" => Some(Self::Debug),
            "I" | "INFO" => Some(Self::Info),
            "W" | "WARN" => Some(Self::Warn),
            "E" | "ERROR" => Some(Self::Error),
            "F" | "FATAL" | "ASSERT" => Some(Self::Fatal),
            _ => None,
        }
    }

    /// 全部级别，按严重度升序。统计输出用固定顺序，不依赖 count 排序。
    pub const fn variants() -> [Self; 6] {
        [
            Self::Verbose,
            Self::Debug,
            Self::Info,
            Self::Warn,
            Self::Error,
            Self::Fatal,
        ]
    }

    pub const fn as_char(self) -> char {
        match self {
            Self::Verbose => 'V',
            Self::Debug => 'D',
            Self::Info => 'I',
            Self::Warn => 'W',
            Self::Error => 'E',
            Self::Fatal => 'F',
        }
    }

    pub const fn rank(self) -> u8 {
        match self {
            Self::Verbose => 0,
            Self::Debug => 1,
            Self::Info => 2,
            Self::Warn => 3,
            Self::Error => 4,
            Self::Fatal => 5,
        }
    }
}

/// 日志拉取条件。
///
/// 条件之间是 AND 关系；`max_lines` 限制返回规模，超出时 `LogSlice::truncated`
/// 置位，提示模型缩小过滤范围而不是继续拉全量。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LogQuery {
    /// 大小写不敏感的子串匹配，作用于整行原文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyword: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// 只保留不低于该级别的行；无法解析级别的行按保留处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_level: Option<LogLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub max_lines: usize,
}

impl LogQuery {
    pub const DEFAULT_MAX_LINES: usize = 400;

    pub fn new() -> Self {
        Self {
            keyword: None,
            tag: None,
            min_level: None,
            pid: None,
            max_lines: Self::DEFAULT_MAX_LINES,
        }
    }

    pub fn with_keyword(mut self, keyword: impl Into<String>) -> Self {
        self.keyword = Some(keyword.into());
        self
    }

    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    pub fn with_min_level(mut self, level: LogLevel) -> Self {
        self.min_level = Some(level);
        self
    }

    pub fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }

    pub fn with_max_lines(mut self, max_lines: usize) -> Self {
        self.max_lines = max_lines;
        self
    }
}

impl Default for LogQuery {
    fn default() -> Self {
        Self::new()
    }
}

/// 单行日志。尽力解析 logcat 的 thread-time 格式，解析失败时字段留空、
/// `raw` 始终保留原文——过滤和展示都不应该依赖解析成功。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LogLine {
    pub raw: String,
    /// 原始文件中的行号（1-based），由日志加载器填写；直接 `parse` 的行没有。
    /// grep/read/crashes 的结果都用它引用原始位置，模型据此引用证据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_no: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<LogLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl LogLine {
    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self::parse(&raw.into())
    }

    /// 带行号解析：加载日志文件/文本时逐行调用。
    pub fn parse_numbered(raw: &str, line_no: u32) -> Self {
        let mut line = Self::parse(raw);
        line.line_no = Some(line_no);
        line
    }

    /// 尽力解析 `MM-DD HH:MM:SS.mmm PID TID LEVEL TAG: message`（以及 epoch
    /// 时间戳变体）。定位策略：找到独立的级别字符 token，其前两个数字 token
    /// 视为 PID/TID，其后带冒号的 token 视为 tag。任何一步不满足即整体回退。
    pub fn parse(raw: &str) -> Self {
        let mut line = Self {
            raw: raw.to_string(),
            line_no: None,
            timestamp: None,
            pid: None,
            tid: None,
            level: None,
            tag: None,
            message: None,
        };

        let tokens: Vec<&str> = raw.split_whitespace().collect();
        let Some(index) = tokens.iter().position(|t| {
            t.len() == 1
                && LogLevel::parse(t).is_some()
                && t.chars().all(|c| c.is_ascii_uppercase())
        }) else {
            return line;
        };

        // tag 必须紧跟级别且自带冒号（logcat 标准 thread-time 格式如此）。
        let Some(tag_token) = tokens.get(index + 1) else {
            return line;
        };
        let Some(tag) = tag_token.strip_suffix(':') else {
            return line;
        };

        let (Some(pid), Some(tid)) = (
            index.checked_sub(2).and_then(|i| tokens[i].parse().ok()),
            index.checked_sub(1).and_then(|i| tokens[i].parse().ok()),
        ) else {
            return line;
        };

        line.timestamp = Some(tokens[..index - 2].join(" "));
        line.pid = Some(pid);
        line.tid = Some(tid);
        line.level = LogLevel::parse(tokens[index]);
        line.tag = Some(tag.to_string());
        line.message = Some(tokens[index + 2..].join(" "));
        line
    }
}

/// 日志片段。`truncated` 表示命中行数超过 `max_lines` 被截断。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LogSlice {
    pub lines: Vec<LogLine>,
    pub truncated: bool,
}

impl LogSlice {
    pub fn new(lines: Vec<LogLine>, truncated: bool) -> Self {
        Self { lines, truncated }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// 一个已加载日志会话的元数据。
///
/// `time_range` 取首/尾两条带时间戳的行（原文），跨月/跨年的导出日志可能
/// 不反映真实顺序；精确排序交给检索/时间线工具。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LogSessionMeta {
    pub id: String,
    pub label: String,
    /// 来源描述：`"粘贴文本"`、`"file: xxx.txt"`、未来的 `"device: <serial>"`。
    pub source: String,
    pub line_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_range: Option<(String, String)>,
}

impl LogSessionMeta {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        source: impl Into<String>,
        line_count: usize,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            source: source.into(),
            line_count,
            time_range: None,
        }
    }
}

/// 崩溃/异常类别。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CrashKind {
    Java,
    /// native tombstone / signal 崩溃。
    Native,
    /// "Application Not Responding"，主线程阻塞超时。
    Anr,
    /// 系统 watchdog 触发的重启/杀进程。
    Watchdog,
    Other(String),
}

impl CrashKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Java => "java",
            Self::Native => "native",
            Self::Anr => "anr",
            Self::Watchdog => "watchdog",
            Self::Other(_) => "other",
        }
    }
}

/// 一帧调用栈。Java/Kotlin 帧填 `declaring_class`/`method`/`file`/`line`；
/// native 帧（tombstone）填 `native_module`/`native_offset`，两类字段互斥。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StackFrame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declaring_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_module: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_offset: Option<String>,
    /// 是否应用自身的帧（非 framework/JDK 前缀）。定位源码时优先看应用帧。
    pub is_app_frame: bool,
}

impl StackFrame {
    /// Java 帧：`com.example.Foo.bar(Foo.kt:42)`。
    pub fn java(
        declaring_class: impl Into<String>,
        method: impl Into<String>,
        file: Option<String>,
        line: Option<u32>,
        is_app_frame: bool,
    ) -> Self {
        Self {
            declaring_class: Some(declaring_class.into()),
            method: Some(method.into()),
            file,
            line,
            native_module: None,
            native_offset: None,
            is_app_frame,
        }
    }

    /// `Foo.kt:42` 形式的文件位置；无文件信息时返回 `None`。
    pub fn location(&self) -> Option<String> {
        self.file.as_ref().map(|file| match self.line {
            Some(line) => format!("{file}:{line}"),
            None => file.clone(),
        })
    }
}

impl std::fmt::Display for StackFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.declaring_class, &self.method) {
            (Some(class), Some(method)) => {
                write!(f, "{class}.{method}")?;
                if let Some(location) = self.location() {
                    write!(f, "({location})")?;
                }
                Ok(())
            }
            _ => {
                let module = self.native_module.as_deref().unwrap_or("<native>");
                let offset = self.native_offset.as_deref().unwrap_or("?");
                write!(f, "{module}+{offset}")
            }
        }
    }
}

/// 一份结构化崩溃报告，由确定性解析器产出。
///
/// `raw_excerpt` 保留原始片段（有上限），供模型需要上下文时引用；
/// `signature` 是崩溃指纹（异常类型 + 顶层应用帧），用于去重和知识库匹配。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CrashReport {
    pub kind: CrashKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// 异常类型，如 `java.lang.NullPointerException`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exception: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// `Caused by:` 链，按出现顺序保留原文。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frames: Vec<StackFrame>,
    #[serde(default)]
    pub raw_excerpt: String,
}

impl CrashReport {
    pub fn new(kind: CrashKind) -> Self {
        Self {
            kind,
            process: None,
            pid: None,
            thread: None,
            exception: None,
            message: None,
            causes: vec![],
            frames: vec![],
            raw_excerpt: String::new(),
        }
    }

    /// 崩溃指纹：异常类型 + 顶层应用帧（无应用帧时退回顶层帧）。
    ///
    /// 刻意不包含 PID/时间戳/消息文本：指纹用于跨次崩溃聚合，这些字段每次都变。
    pub fn signature(&self) -> String {
        let frame = self
            .frames
            .iter()
            .find(|frame| frame.is_app_frame)
            .or_else(|| self.frames.first());
        match (self.exception.as_deref(), frame) {
            (Some(exception), Some(frame)) => format!("{exception} at {frame}"),
            (Some(exception), None) => exception.to_string(),
            (None, Some(frame)) => format!("{} at {frame}", self.kind.as_str()),
            (None, None) => self.kind.as_str().to_string(),
        }
    }

    pub fn app_frames(&self) -> Vec<&StackFrame> {
        self.frames
            .iter()
            .filter(|frame| frame.is_app_frame)
            .collect()
    }
}

/// 代码检索条件。`pattern` 当前是大小写敏感子串；正则/符号级检索由
/// `CodeIndex` 实现决定，查询语义随实现升级，工具签名不变。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CodeQuery {
    pub pattern: String,
    /// 形如 `*.kt`（扩展名过滤）或 `src/main`（路径子串）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_glob: Option<String>,
    pub max_results: usize,
}

impl CodeQuery {
    pub const DEFAULT_MAX_RESULTS: usize = 50;

    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            file_glob: None,
            max_results: Self::DEFAULT_MAX_RESULTS,
        }
    }

    pub fn with_file_glob(mut self, file_glob: impl Into<String>) -> Self {
        self.file_glob = Some(file_glob.into());
        self
    }

    pub fn with_max_results(mut self, max_results: usize) -> Self {
        self.max_results = max_results;
        self
    }
}

/// 一次检索命中。`path` 相对于代码根目录，行号 1-based。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CodeHit {
    pub path: String,
    pub line_number: u32,
    pub line_text: String,
}

/// 代码片段，行号为 1-based 闭区间。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CodeSnippet {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub content: String,
}

/// 分析结论中引用的代码位置，行号为 1-based 闭区间。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CodeLocation {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
}

impl CodeLocation {
    pub fn new(path: impl Into<String>, start_line: u32, end_line: u32) -> Self {
        Self {
            path: path.into(),
            start_line,
            end_line,
        }
    }
}

/// 设备连接状态，对应 `adb devices` 的状态列。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DeviceState {
    Online,
    Offline,
    Unauthorized,
    Other(String),
}

/// 一台已连接设备的描述信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeviceInfo {
    pub serial: String,
    pub state: DeviceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_level: Option<u32>,
}

impl DeviceInfo {
    pub fn new(serial: impl Into<String>, state: DeviceState) -> Self {
        Self {
            serial: serial.into(),
            state,
            model: None,
            api_level: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_thread_time_line() {
        let line = LogLine::parse(
            "10-25 14:23:01.123  1234  5678 E AndroidRuntime: FATAL EXCEPTION: main",
        );
        assert_eq!(line.timestamp.as_deref(), Some("10-25 14:23:01.123"));
        assert_eq!(line.pid, Some(1234));
        assert_eq!(line.tid, Some(5678));
        assert_eq!(line.level, Some(LogLevel::Error));
        assert_eq!(line.tag.as_deref(), Some("AndroidRuntime"));
        assert_eq!(line.message.as_deref(), Some("FATAL EXCEPTION: main"));
    }

    #[test]
    fn parses_epoch_line() {
        let line =
            LogLine::parse("1729849381.123 1234 567 W ActivityManager: Displayed com.example");
        assert_eq!(line.timestamp.as_deref(), Some("1729849381.123"));
        assert_eq!(line.level, Some(LogLevel::Warn));
        assert_eq!(line.tag.as_deref(), Some("ActivityManager"));
    }

    #[test]
    fn falls_back_to_raw_on_unrecognized_format() {
        let line = LogLine::parse("just some plain text");
        assert_eq!(line.raw, "just some plain text");
        assert_eq!(line.timestamp, None);
        assert_eq!(line.level, None);
    }

    #[test]
    fn signature_prefers_app_frame() {
        // 通过反序列化构造，避免依赖解析器实现。
        let report: CrashReport = serde_json::from_value(serde_json::json!({
            "kind": "java",
            "exception": "java.lang.NullPointerException",
            "frames": [
                {"declaring_class": "android.app.Activity", "method": "onCreate", "file": "Activity.java", "line": 1, "is_app_frame": false},
                {"declaring_class": "com.example.app.Foo", "method": "bar", "file": "Foo.kt", "line": 42, "is_app_frame": true}
            ]
        }))
        .unwrap();
        assert_eq!(
            report.signature(),
            "java.lang.NullPointerException at com.example.app.Foo.bar(Foo.kt:42)"
        );
        assert_eq!(report.app_frames().len(), 1);
    }
}
