//! Java/Kotlin 崩溃解析器：识别 logcat 中 `FATAL EXCEPTION` 块。
//!
//! 覆盖标准 `E/AndroidRuntime` 输出：进程行、异常行、`at` 堆栈帧、
//! `Caused by` 链。tombstone/watchdog 由后续解析器按同一 trait 追加。

use super::strip_logcat_prefix;
use crate::capability::LogParser;
use crate::types::{CrashKind, CrashReport, StackFrame};

/// 堆栈帧数量上限。崩溃块的帧往往几十帧，超过后保留前 50 帧，
/// 完整内容仍可通过 `raw_excerpt` 与日志工具取到。
const MAX_FRAMES: usize = 50;

/// 原始片段字符上限，防止异常巨块（如超长 message）占满上下文。
const MAX_EXCERPT_CHARS: usize = 4000;

/// 这些前缀的类视为 framework/JDK 帧，不参与“应用帧”判定。
/// 判定影响崩溃指纹与模型定位优先级，宁可保守：未识别前缀按应用帧处理。
const NON_APP_CLASS_PREFIXES: [&str; 10] = [
    "android.",
    "androidx.",
    "com.android.",
    "java.",
    "javax.",
    "kotlin.",
    "kotlinx.",
    "dalvik.",
    "sun.",
    "libcore.",
];

/// 解析 `E/AndroidRuntime( 1234): message` 形式行，剥离日志前缀。
/// 兼容任意 tag 的 thread-time 行；无前缀的裸文本行原样返回。
fn is_app_frame(declaring_class: &str) -> bool {
    !NON_APP_CLASS_PREFIXES
        .iter()
        .any(|prefix| declaring_class.starts_with(prefix))
}

/// 解析单帧 `at com.example.Foo.bar(Foo.kt:42)`。
fn parse_frame(text: &str) -> Option<StackFrame> {
    let rest = text.trim().strip_prefix("at ")?;
    let open = rest.rfind('(')?;
    let close = rest[open..].find(')')? + open;
    let method_part = &rest[..open];
    let source_part = &rest[open + 1..close];

    // method_part 形如 `com.example.Foo.bar` 或 `com.example.Foo$Inner.run`；
    // 类名到最后一个 `.`，内部类 `$` 保留在类名里。
    let dot = method_part.rfind('.')?;
    let declaring_class = &method_part[..dot];
    let method = &method_part[dot + 1..];

    let (file, line) = match source_part.split_once(':') {
        Some((file, line)) => (Some(file.to_string()), line.parse().ok()),
        None => {
            // "Native Method" / "Unknown Source" 没有行号。
            if source_part == "Unknown Source" {
                (None, None)
            } else {
                (Some(source_part.to_string()), None)
            }
        }
    };

    Some(StackFrame::java(
        declaring_class,
        method,
        file,
        line,
        is_app_frame(declaring_class),
    ))
}

/// 收集一个崩溃块：从 `FATAL EXCEPTION` 行起，到空行或下一个分隔线止。
fn parse_block(block: &[String]) -> Option<CrashReport> {
    if block.is_empty() {
        return None;
    }
    let mut report = CrashReport::new(CrashKind::Java);

    // 首行形如 `FATAL EXCEPTION: main`（或带 logcat 前缀）。
    report.thread = block[0]
        .split_once("FATAL EXCEPTION:")?
        .1
        .trim()
        .to_string()
        .into();

    let mut exception_seen = false;
    // 进入 Caused by 段后不再收集主堆栈帧：frames 语义是根异常的调用栈，
    // cause 的栈保留在 causes 文本与 raw_excerpt 中。
    let mut in_cause = false;
    for raw in &block[1..] {
        let body = strip_logcat_prefix(raw);
        if body.starts_with("Process:") {
            let rest = body.strip_prefix("Process:")?.trim();
            // `Process: com.example.app, PID: 1234`
            if let Some((process, pid)) = rest.split_once(", ") {
                report.process = Some(process.trim().to_string());
                if let Some(pid) = pid.strip_prefix("PID: ") {
                    report.pid = pid.trim().parse().ok();
                }
            } else {
                report.process = Some(rest.to_string());
            }
        } else if let Some(cause) = body.strip_prefix("Caused by: ") {
            in_cause = true;
            report.causes.push(cause.to_string());
        } else if !in_cause && body.trim_start().strip_prefix("at ").is_some() {
            if report.frames.len() < MAX_FRAMES {
                if let Some(frame) = parse_frame(body.trim_start()) {
                    report.frames.push(frame);
                }
            }
        } else if !exception_seen {
            // 异常行：`java.lang.NullPointerException: message`；异常也可能没有消息。
            match body.split_once(':') {
                Some((exception, message)) => {
                    report.exception = Some(exception.trim().to_string());
                    report.message = Some(message.trim().to_string());
                }
                None => report.exception = Some(body.trim().to_string()),
            }
            exception_seen = true;
        }
    }

    let excerpt: String = block.join("\n");
    report.raw_excerpt = truncate_chars(&excerpt, MAX_EXCERPT_CHARS);
    Some(report)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push_str("\n...[truncated]");
    truncated
}

/// 识别文本中所有 `FATAL EXCEPTION` 块（混合导出日志可能包含多份崩溃）。
pub struct JavaCrashParser;

impl LogParser for JavaCrashParser {
    fn name(&self) -> &'static str {
        "java-crash"
    }

    fn parse(&self, input: &str) -> Vec<CrashReport> {
        let mut reports = Vec::new();
        let mut block: Vec<String> = Vec::new();

        for line in input.lines() {
            if line.contains("FATAL EXCEPTION") {
                if let Some(report) = parse_block(&block) {
                    reports.push(report);
                }
                block = vec![line.to_string()];
            } else if !block.is_empty() {
                // 块以空行或 `--------- beginning of` 结束。
                if line.trim().is_empty() || line.starts_with("---------") {
                    if let Some(report) = parse_block(&block) {
                        reports.push(report);
                    }
                    block.clear();
                } else {
                    block.push(line.to_string());
                }
            }
        }
        if let Some(report) = parse_block(&block) {
            reports.push(report);
        }
        reports
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
--------- beginning of main
10-25 14:23:01.001  1000  2000 I ActivityManager: Start proc com.example.app
10-25 14:23:01.123  1234  5678 E AndroidRuntime: FATAL EXCEPTION: main
10-25 14:23:01.124  1234  5678 E AndroidRuntime: Process: com.example.app, PID: 1234
10-25 14:23:01.125  1234  5678 E AndroidRuntime: java.lang.NullPointerException: attempt to invoke a virtual method on a null object reference
10-25 14:23:01.126  1234  5678 E AndroidRuntime: \tat com.example.app.ui.MainActivity.onCreate(MainActivity.kt:42)
10-25 14:23:01.127  1234  5678 E AndroidRuntime: \tat android.app.Activity.performCreate(Activity.java:8000)
10-25 14:23:01.128  1234  5678 E AndroidRuntime: \tat com.example.app.core.Foo$Inner.run(Foo.kt:10)
10-25 14:23:01.129  1234  5678 E AndroidRuntime: Caused by: kotlin.UninitializedPropertyAccessException: lateinit property engine has not been initialized
10-25 14:23:01.130  1234  5678 E AndroidRuntime: \tat com.example.app.core.Foo.run(Foo.kt:10)
";

    #[test]
    fn parses_java_crash_block() {
        let reports = JavaCrashParser.parse(SAMPLE);
        assert_eq!(reports.len(), 1);
        let report = &reports[0];

        assert_eq!(report.kind, CrashKind::Java);
        assert_eq!(report.process.as_deref(), Some("com.example.app"));
        assert_eq!(report.pid, Some(1234));
        assert_eq!(report.thread.as_deref(), Some("main"));
        assert_eq!(
            report.exception.as_deref(),
            Some("java.lang.NullPointerException")
        );
        assert_eq!(report.causes.len(), 1);
        assert!(report.causes[0].contains("UninitializedPropertyAccessException"));

        let app: Vec<_> = report.app_frames().iter().map(|f| f.to_string()).collect();
        assert_eq!(
            app,
            vec![
                "com.example.app.ui.MainActivity.onCreate(MainActivity.kt:42)".to_string(),
                "com.example.app.core.Foo$Inner.run(Foo.kt:10)".to_string(),
            ]
        );
        assert_eq!(
            report.signature(),
            "java.lang.NullPointerException at com.example.app.ui.MainActivity.onCreate(MainActivity.kt:42)"
        );
    }

    #[test]
    fn returns_empty_for_plain_logs() {
        assert!(JavaCrashParser
            .parse("10-25 14:23:01.001 I ActivityManager: all good")
            .is_empty());
    }

    #[test]
    fn handles_native_method_frame() {
        let input = "E AndroidRuntime: FATAL EXCEPTION: Thread-2\n\
                     E AndroidRuntime: java.lang.InterruptedException: sleep interrupted\n\
                     E AndroidRuntime: \tat java.lang.Thread.sleep(Native Method)\n\
                     E AndroidRuntime: \tat com.example.Worker.loop(Worker.java:31)\n";
        let reports = JavaCrashParser.parse(input);
        assert_eq!(reports.len(), 1);
        let native_frame = &reports[0].frames[0];
        assert_eq!(native_frame.file.as_deref(), Some("Native Method"));
        assert_eq!(native_frame.line, None);
        assert!(!native_frame.is_app_frame);
    }
}
