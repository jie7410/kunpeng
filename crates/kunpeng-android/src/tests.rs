//! 集成测试：能力装配、沙箱、会话工具与 mock 模型端到端循环。
//!
//! 全部离线运行，不依赖网络与真实设备。

use async_trait::async_trait;
use kunpeng_core::types::{ChatRequest, ChatResponse, FinishReason, ToolCall};
use kunpeng_core::{Error, Message, Result};
use kunpeng_provider::{Adapter, CallContext, Model, Provider};
use kunpeng_runtime::{AgentHooks, RunConfig, StopReason};
use kunpeng_tools::Tool;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::{build_agent, AndroidProfile};
use crate::builtin::{FsCodeIndex, InMemorySessions, StaticLogProvider};
use crate::capability::{
    CodeIndex, DeviceConnector, DeviceLogProvider, LogProvider, LogSessionStore,
};
use crate::env::AndroidEnvBuilder;
use crate::tools::cluster::TemplateClusterTool;
use crate::tools::crash::CrashCatalogTool;
use crate::tools::cross::{CrossTimelineTool, LogDiffTool};
use crate::tools::events::{BootTimelineTool, EventDecodeTool};
use crate::tools::log::{LogGrepTool, LogOpenTool, LogReadTool, LogSessionsTool, LogStatsTool};
use crate::tools::perf::PerfSignalsTool;
use crate::types::{CodeQuery, DeviceInfo, DeviceState, LogLine, LogQuery, LogSlice};

const SAMPLE_CRASH: &str = "\
10-25 14:23:01.001  1000  2000 I ActivityManager: Start proc com.example.app
10-25 14:23:01.123  1234  5678 E AndroidRuntime: FATAL EXCEPTION: main
10-25 14:23:01.124  1234  5678 E AndroidRuntime: Process: com.example.app, PID: 1234
10-25 14:23:01.125  1234  5678 E AndroidRuntime: java.lang.NullPointerException: engine is null
10-25 14:23:01.126  1234  5678 E AndroidRuntime: \tat com.example.app.ui.MainActivity.onCreate(MainActivity.kt:6)
10-25 14:23:01.127  1234  5678 E AndroidRuntime: \tat android.app.Activity.performCreate(Activity.java:8000)
";

/// 覆盖统计/检索/模板/目录的多用途样本：
/// 18 行；行 7 与 14 是两次同签名崩溃；行 1/13 是 buffer 分隔线。
const MULTI_LOG: &str = "\
--------- beginning of main
10-25 14:00:00.001  1000  2000 I ActivityManager: Start proc com.example.app
10-25 14:00:00.101  1234  1234 D ExampleApp: retry 1 failed
10-25 14:00:00.201  1234  1234 D ExampleApp: retry 2 failed
10-25 14:00:00.301  1234  1234 D ExampleApp: retry 3 failed
10-25 14:00:00.401  1234  1234 W ExampleApp: buffer 0x7f2a1b3c4d overflow
10-25 14:00:01.123  1234  5678 E AndroidRuntime: FATAL EXCEPTION: main
10-25 14:00:01.124  1234  5678 E AndroidRuntime: Process: com.example.app, PID: 1234
10-25 14:00:01.125  1234  5678 E AndroidRuntime: java.lang.NullPointerException: engine is null
10-25 14:00:01.126  1234  5678 E AndroidRuntime: \tat com.example.app.ui.MainActivity.onCreate(MainActivity.kt:6)
10-25 14:00:01.127  1234  5678 E AndroidRuntime: \tat android.app.Activity.performCreate(Activity.java:8000)
10-25 14:00:02.000  1000  2000 I ActivityManager: Process com.example.app exited
--------- beginning of crash
10-25 14:05:01.123  1234  5678 E AndroidRuntime: FATAL EXCEPTION: main
10-25 14:05:01.124  1234  5678 E AndroidRuntime: Process: com.example.app, PID: 1234
10-25 14:05:01.125  1234  5678 E AndroidRuntime: java.lang.NullPointerException: engine is null
10-25 14:05:01.126  1234  5678 E AndroidRuntime: \tat com.example.app.ui.MainActivity.onCreate(MainActivity.kt:6)
10-25 14:05:01.127  1234  5678 E AndroidRuntime: \tat android.app.Activity.performCreate(Activity.java:8000)
";

// ---------- mock 模型（与 kunpeng-agents 测试同构） ----------

struct MockAdapter {
    responses: Mutex<VecDeque<Result<ChatResponse>>>,
}

#[async_trait]
impl Adapter for MockAdapter {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn chat(&self, _ctx: &CallContext<'_>, _req: &ChatRequest<'_>) -> Result<ChatResponse> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(Error::Other("mock 响应已耗尽".into())))
    }
}

fn mock_model(responses: Vec<ChatResponse>) -> Model {
    let provider = Arc::new(
        Provider::new(
            "https://example.invalid",
            "key",
            Arc::new(MockAdapter {
                responses: Mutex::new(responses.into_iter().map(Ok).collect()),
            }),
            5,
        )
        .unwrap(),
    );
    Model::new(provider, "mock")
}

// ---------- mock 设备连接器 ----------

struct MockConnector {
    logcat_calls: Mutex<Vec<String>>,
}

impl MockConnector {
    fn new() -> Self {
        Self {
            logcat_calls: Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl DeviceConnector for MockConnector {
    async fn devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![DeviceInfo::new("emulator-5554", DeviceState::Online)])
    }

    async fn shell(&self, _serial: &str, command: &str) -> Result<String> {
        Ok(format!("ran: {command}"))
    }

    async fn logcat(&self, serial: &str, _query: &LogQuery) -> Result<LogSlice> {
        self.logcat_calls.lock().unwrap().push(serial.to_string());
        Ok(LogSlice::new(
            vec![LogLine::from_raw("E AndroidRuntime: FATAL EXCEPTION: main")],
            false,
        ))
    }
}

// ---------- 测试辅助 ----------

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn temp_repo() -> PathBuf {
    let root = std::env::temp_dir().join(format!("kunpeng-android-test-{}", unique_suffix()));
    let src = root.join("app/src/main/java/com/example/app/ui");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("MainActivity.kt"),
        "package com.example.app.ui\n\nclass MainActivity {\n    fun onCreate() {\n        val engine = createEngine()\n        engine.start()\n    }\n}\n",
    )
    .unwrap();
    root
}

fn parse_output(output: &str) -> serde_json::Value {
    serde_json::from_str(output).expect("工具输出应为合法 JSON")
}

async fn multi_session() -> Arc<dyn LogSessionStore> {
    let root = temp_repo();
    let store = InMemorySessions::new(Some(&root)).unwrap();
    store.preload("多用途样本", MULTI_LOG).unwrap();
    Arc::new(store)
}

// ---------- 会话存储 ----------

#[tokio::test]
async fn sessions_store_lifecycle() {
    let root = temp_repo();
    std::fs::write(
        root.join("extra.log"),
        "10-25 14:00:00.001 1 1 I T: hello\n",
    )
    .unwrap();
    let store = InMemorySessions::new(Some(&root)).unwrap();

    let first = store.open_text("一份", "line1\nline2").await.unwrap();
    let second = store.open_file("", "extra.log").await.unwrap();
    assert_eq!(first.id, "s1");
    assert_eq!(second.id, "s2");
    assert_eq!(second.label, "extra.log");
    assert!(second.source.contains("extra.log"));

    store.set_active(&second.id).await.unwrap();
    assert_eq!(store.active().await.unwrap().id, "s2");
    store.rename(&second.id, "问题机").await.unwrap();

    // 关闭活动会话后回退到剩余会话。
    store.close(&second.id).await.unwrap();
    assert_eq!(store.active().await.unwrap().id, "s1");
    assert_eq!(store.list().await.len(), 1);

    // 行号标注。
    let lines = store.snapshot(&first.id).await.unwrap();
    assert_eq!(lines[1].line_no, Some(2));
}

#[tokio::test]
async fn sessions_store_rejects_escape() {
    let root = temp_repo();
    let outside = std::env::temp_dir().join(format!("kunpeng-android-out-{}", unique_suffix()));
    std::fs::write(&outside, "secret").unwrap();
    let store = InMemorySessions::new(Some(&root)).unwrap();
    let relative = format!("../{}", outside.file_name().unwrap().to_string_lossy());
    let error = store.open_file("", &relative).await.unwrap_err();
    assert!(matches!(error, Error::InvalidConfig(_)));
}

// ---------- 会话工具 ----------

#[tokio::test]
async fn log_open_tool_returns_overview() {
    let root = temp_repo();
    let sessions: Arc<dyn LogSessionStore> = Arc::new(InMemorySessions::new(Some(&root)).unwrap());
    let tool = LogOpenTool::new(sessions.clone());
    let output = parse_output(
        &tool
            .call(serde_json::json!({ "text": MULTI_LOG, "label": "样本" }))
            .await
            .unwrap(),
    );
    assert_eq!(output["session"]["label"], "样本");
    assert_eq!(output["session"]["line_count"], 18);
    assert_eq!(
        output["overview"]["buffers"],
        serde_json::json!(["main", "crash"])
    );
    assert_eq!(
        output["overview"]["crash_marker_lines"],
        serde_json::json!([7, 14])
    );
    assert_eq!(sessions.active().await.unwrap().label, "样本");
}

#[tokio::test]
async fn log_sessions_tool_actions() {
    let sessions = multi_session().await;
    let tool = LogSessionsTool::new(sessions.clone());
    let output = parse_output(
        &tool
            .call(serde_json::json!({ "action": "list" }))
            .await
            .unwrap(),
    );
    assert_eq!(output["sessions"][0]["label"], "多用途样本");
    assert_eq!(output["sessions"][0]["active"], true);

    tool.call(serde_json::json!({ "action": "rename", "session": "s1", "label": "问题机" }))
        .await
        .unwrap();
    let output = parse_output(
        &tool
            .call(serde_json::json!({ "action": "list" }))
            .await
            .unwrap(),
    );
    assert_eq!(output["sessions"][0]["label"], "问题机");
    assert!(tool
        .call(serde_json::json!({ "action": "use", "session": "s404" }))
        .await
        .is_err());
}

#[tokio::test]
async fn log_stats_tool_reports_overview() {
    let sessions = multi_session().await;
    let tool = LogStatsTool::new(sessions);
    let output = parse_output(&tool.call(serde_json::json!({})).await.unwrap());
    assert_eq!(output["total_lines"], 18);
    assert_eq!(output["unparseable_lines"], 2);
    let levels: Vec<_> = output["by_level"].as_array().unwrap().clone();
    assert!(levels.contains(&serde_json::json!(["D", 3])));
    assert!(levels.contains(&serde_json::json!(["E", 10])));
    assert_eq!(output["crash_marker_lines"], serde_json::json!([7, 14]));
}

#[tokio::test]
async fn log_grep_tool_views() {
    let sessions = multi_session().await;
    let tool = LogGrepTool::new(sessions);

    // lines 视图分页。
    let output = parse_output(
        &tool
            .call(serde_json::json!({ "keyword": "retry", "limit": 2 }))
            .await
            .unwrap(),
    );
    assert_eq!(output["total_matches"], 3);
    assert_eq!(output["shown"], 2);
    assert_eq!(output["has_more"], true);

    // count_by_tag 聚合。
    let output = parse_output(
        &tool
            .call(serde_json::json!({ "keyword": "retry", "view": "count_by_tag" }))
            .await
            .unwrap(),
    );
    assert_eq!(output["counts"][0], serde_json::json!(["ExampleApp", 3]));

    // 时间窗 + tag 组合：第二次崩溃的 5 行。
    let output = parse_output(
        &tool
            .call(serde_json::json!({ "tag": "AndroidRuntime", "time_from": "10-25 14:05" }))
            .await
            .unwrap(),
    );
    assert_eq!(output["total_matches"], 5);

    // context 视图：命中行标记。
    let output = parse_output(
        &tool
            .call(serde_json::json!({
                "keyword": "NullPointerException",
                "view": "context",
                "before": 2,
                "after": 2,
            }))
            .await
            .unwrap(),
    );
    let windows = output["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0]["start_line"], 7);
    assert_eq!(windows[0]["lines"][2]["hit"], true);
    assert_eq!(windows[0]["lines"][0]["hit"], false);
}

#[tokio::test]
async fn log_read_tool_pages() {
    let sessions = multi_session().await;
    let tool = LogReadTool::new(sessions);
    let output = parse_output(
        &tool
            .call(serde_json::json!({ "start_line": 3, "count": 2 }))
            .await
            .unwrap(),
    );
    assert_eq!(output["lines"][0]["line_no"], 3);
    assert_eq!(output["next_line"], 5);

    assert!(tool
        .call(serde_json::json!({ "start_line": 99 }))
        .await
        .is_err());
}

#[tokio::test]
async fn template_cluster_tool_masks_numbers() {
    let sessions = multi_session().await;
    let tool = TemplateClusterTool::new(sessions);
    let output = parse_output(
        &tool
            .call(serde_json::json!({ "min_count": 1, "top": 20 }))
            .await
            .unwrap(),
    );
    let templates = output["templates"].as_array().unwrap();
    let retry = templates
        .iter()
        .find(|stat| stat["template"].as_str().unwrap().contains("retry"))
        .expect("应聚类出 retry 模板");
    assert_eq!(retry["count"], 3);
    assert!(retry["template"]
        .as_str()
        .unwrap()
        .contains("retry <N> failed"));
    let hex = templates
        .iter()
        .find(|stat| stat["template"].as_str().unwrap().contains("buffer"))
        .expect("应聚类出 buffer 模板");
    assert!(hex["template"].as_str().unwrap().contains("<H>"));
}

#[tokio::test]
async fn crash_catalog_tool_groups_same_signature() {
    let sessions = multi_session().await;
    let parser = Arc::new(
        crate::capability::CompositeLogParser::new()
            .with_parser(Box::new(crate::builtin::JavaCrashParser))
            .with_parser(Box::new(crate::builtin::AnrParser)),
    );
    let tool = CrashCatalogTool::new(sessions, parser);
    let output = parse_output(&tool.call(serde_json::json!({})).await.unwrap());
    assert_eq!(output["total"], 2);
    let groups = output["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["count"], 2);
    assert_eq!(groups[0]["first_line"], 7);
    assert!(groups[0]["signature"]
        .as_str()
        .unwrap()
        .contains("MainActivity.kt:6"));
}

// ---------- 装配与注册表 ----------

#[test]
fn offline_env_registers_log_family_tools() {
    let root = temp_repo();
    let env = AndroidEnvBuilder::offline(&root, SAMPLE_CRASH)
        .unwrap()
        .build()
        .unwrap();
    let registry = env.tool_registry().unwrap();

    let mut names: Vec<_> = registry
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "android_boot_timeline",
            "android_code_read",
            "android_code_search",
            "android_crashes",
            "android_event_log_decode",
            "android_log_parse",
            "android_perf_signals",
            "log_cross_timeline",
            "log_diff",
            "log_grep",
            "log_open",
            "log_read",
            "log_sessions",
            "log_stats",
            "log_template_cluster",
        ]
    );
    assert!(!env.device_tools_enabled());
}

#[test]
fn device_tools_registered_only_with_connector() {
    let root = temp_repo();
    let env = AndroidEnvBuilder::offline(&root, "log")
        .unwrap()
        .with_device_connector(Arc::new(MockConnector::new()))
        .build()
        .unwrap();
    let registry = env.tool_registry().unwrap();
    assert_eq!(registry.definitions().len(), 17);
    assert!(registry
        .definitions()
        .iter()
        .any(|definition| definition.name == "android_device_shell"));
}

#[test]
fn online_profile_requires_connector() {
    let root = temp_repo();
    let env = AndroidEnvBuilder::offline(&root, "log")
        .unwrap()
        .build()
        .unwrap();
    let error = build_agent(
        mock_model(vec![]),
        &env,
        AndroidProfile::OnlineDiagnosis,
        RunConfig::default(),
    )
    .err()
    .expect("OnlineDiagnosis 未配置设备连接器应当报错");
    assert!(matches!(error, Error::InvalidConfig(_)));
}

#[test]
fn system_prompt_mentions_log_tools() {
    let root = temp_repo();
    let env = AndroidEnvBuilder::offline(&root, SAMPLE_CRASH)
        .unwrap()
        .build()
        .unwrap();
    let prompt = AndroidProfile::CrashTriage.system_prompt(&env);
    assert!(prompt.contains("android_crashes"));
    assert!(prompt.contains("log_grep"));
    assert!(prompt.contains("未配置"));
}

// ---------- 代码索引沙箱与检索 ----------

#[tokio::test]
async fn fs_code_index_search_and_read() {
    let root = temp_repo();
    let index = FsCodeIndex::new(&root).unwrap();

    let hits = index.search(&CodeQuery::new("MainActivity")).await.unwrap();
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|hit| hit
        .path
        .ends_with("app/src/main/java/com/example/app/ui/MainActivity.kt")));

    // 扩展名过滤
    let hits = index
        .search(&CodeQuery::new("MainActivity").with_file_glob("*.java"))
        .await
        .unwrap();
    assert!(hits.is_empty());

    let snippet = index
        .read_range("app/src/main/java/com/example/app/ui/MainActivity.kt", 3, 4)
        .await
        .unwrap();
    assert_eq!(snippet.start_line, 3);
    assert_eq!(snippet.end_line, 4);
    assert!(snippet.content.contains("class MainActivity"));

    // 结束行超出文件末尾时截到实际末行。
    let snippet = index
        .read_range(
            "app/src/main/java/com/example/app/ui/MainActivity.kt",
            7,
            99,
        )
        .await
        .unwrap();
    assert_eq!(snippet.end_line, 8);
}

#[tokio::test]
async fn fs_code_index_rejects_escape() {
    let root = temp_repo();
    let outside = std::env::temp_dir().join(format!("kunpeng-android-out-{}", unique_suffix()));
    std::fs::write(&outside, "secret").unwrap();

    let index = FsCodeIndex::new(&root).unwrap();
    let relative = format!("../{}", outside.file_name().unwrap().to_string_lossy());
    let error = index.read_range(&relative, 1, 1).await.unwrap_err();
    assert!(matches!(error, Error::InvalidConfig(_)));
}

// ---------- 日志源（预留的实时通道） ----------

#[tokio::test]
async fn static_log_provider_filters_and_truncates() {
    let provider = StaticLogProvider::from_text("t", SAMPLE_CRASH);
    let slice = provider
        .fetch(&LogQuery::new().with_min_level(crate::types::LogLevel::Error))
        .await
        .unwrap();
    assert_eq!(slice.lines.len(), 5);
    assert!(!slice.truncated);

    let slice = provider
        .fetch(&LogQuery::new().with_keyword("fatal exception"))
        .await
        .unwrap();
    assert_eq!(slice.lines.len(), 1);

    let slice = provider
        .fetch(&LogQuery::new().with_max_lines(2))
        .await
        .unwrap();
    assert_eq!(slice.lines.len(), 2);
    assert!(slice.truncated);
}

#[tokio::test]
async fn device_log_provider_delegates_to_connector() {
    let connector = Arc::new(MockConnector::new());
    let provider = DeviceLogProvider::new(connector.clone(), "emulator-5554");
    let slice = provider.fetch(&LogQuery::new()).await.unwrap();
    assert!(!slice.is_empty());
    assert_eq!(
        connector.logcat_calls.lock().unwrap().as_slice(),
        ["emulator-5554"]
    );
    assert!(provider.describe().contains("emulator-5554"));
}

// ---------- 阶段二：语义分析工具 ----------

/// events buffer 样本：4 个启动里程碑 + am_proc_start/am_kill。
const BOOT_EVENTS_LOG: &str = "\
--------- beginning of events
10-25 08:00:05.000  0  0 I boot_progress_start: [5000]
10-25 08:00:13.000  0  0 I boot_progress_system_run: [13000]
10-25 08:00:41.000  0  0 I boot_progress_pms_ready: [41000]
10-25 08:01:15.000  0  0 I boot_progress_enable_screen: [75000]
10-25 08:00:20.000  1000  2000 I am_proc_start: [1000,12345,com.example.app,activity]
10-25 08:00:22.000  1000  2000 I am_kill: [0,12345,com.example.app,900,10]
";

const PERF_SAMPLE_LOG: &str = "\
10-25 08:01:30.100  1000  2050 I ActivityTaskManager: Displayed com.example.app/.MainActivity: +3s456ms
10-25 08:01:35.100  1234  5678 I Choreographer: Skipped 120 frames!  (taking 2041ms)
10-25 08:01:36.100  1234  5678 D StrictMode policy violation: ~NetworkOnMainThreadOperation
10-25 08:01:37.100  1234  5678 I art: Background concurrent copying GC freed 10MB
10-25 08:01:38.100  1000  1500 I lowmemorykiller: Kill 'com.example.app' (12345)
";

async fn open_session(label: &str, log: &str) -> (Arc<dyn LogSessionStore>, String) {
    let root = temp_repo();
    let store = InMemorySessions::new(Some(&root)).unwrap();
    let id = store.preload(label, log).unwrap();
    (Arc::new(store), id)
}

#[tokio::test]
async fn event_decode_tool_decodes_and_filters() {
    let (sessions, _) = open_session("boot", BOOT_EVENTS_LOG).await;
    let tool = EventDecodeTool::new(sessions);

    let output = parse_output(&tool.call(serde_json::json!({})).await.unwrap());
    assert_eq!(output["total"], 6);

    // name 过滤 + 字段命名。
    let output = parse_output(
        &tool
            .call(serde_json::json!({ "name": "am_proc_start" }))
            .await
            .unwrap(),
    );
    assert_eq!(output["total"], 1);
    let fields = &output["events"][0]["fields"];
    assert_eq!(fields[0]["name"], "uid");
    assert_eq!(fields[2]["name"], "processName");
    assert_eq!(fields[2]["value"], "com.example.app");
}

#[tokio::test]
async fn boot_timeline_tool_reports_milestones_and_gaps() {
    let (sessions, _) = open_session("boot", BOOT_EVENTS_LOG).await;
    let tool = BootTimelineTool::new(sessions);
    let output = parse_output(&tool.call(serde_json::json!({})).await.unwrap());

    let milestones = output["milestones"].as_array().unwrap();
    assert_eq!(milestones.len(), 4, "am_* 事件不是里程碑");
    assert_eq!(milestones[0]["name"], "boot_progress_start");

    let gaps = output["slowest_gaps"].as_array().unwrap();
    assert_eq!(
        gaps[0]["gap"],
        "boot_progress_pms_ready -> boot_progress_enable_screen"
    );
    assert_eq!(gaps[0]["ms"], 34000);
}

#[tokio::test]
async fn perf_signals_tool_groups_by_kind() {
    let (sessions, _) = open_session("perf", PERF_SAMPLE_LOG).await;
    let tool = PerfSignalsTool::new(sessions);
    let output = parse_output(&tool.call(serde_json::json!({})).await.unwrap());

    let signals = output["signals"].as_array().unwrap();
    let kinds: Vec<&str> = signals
        .iter()
        .map(|group| group["kind"].as_str().unwrap())
        .collect();
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
    assert_eq!(signals[0]["entries"][0]["value_ms"], 3456);
    assert_eq!(signals[4]["entries"][0]["process"], "com.example.app");
}

#[tokio::test]
async fn cross_timeline_tool_merges_sessions() {
    let root = temp_repo();
    let store = InMemorySessions::new(Some(&root)).unwrap();
    store
        .preload(
            "main",
            "10-25 08:00:02.000  1  1 I Main: second\n10-25 08:00:00.000  1  1 I Main: first",
        )
        .unwrap();
    store
        .open_text(
            "events",
            "10-25 08:00:01.000  0  0 I boot_progress_x: [1000]",
        )
        .await
        .unwrap();
    let sessions: Arc<dyn LogSessionStore> = Arc::new(store);
    let tool = CrossTimelineTool::new(sessions);

    // 缺省归并全部会话，按时间排序。
    let output = parse_output(&tool.call(serde_json::json!({})).await.unwrap());
    let entries = output["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert!(entries[0]["raw"].as_str().unwrap().contains("first"));
    assert_eq!(entries[1]["session"], "events");
    assert!(entries[2]["raw"].as_str().unwrap().contains("second"));

    // keyword 过滤。
    let output = parse_output(
        &tool
            .call(serde_json::json!({ "keyword": "boot_progress" }))
            .await
            .unwrap(),
    );
    assert_eq!(output["entries"].as_array().unwrap().len(), 1);

    // 单会话不满足对齐条件。
    let error = tool
        .call(serde_json::json!({ "sessions": ["s1"] }))
        .await
        .unwrap_err();
    assert!(matches!(error, kunpeng_core::Error::InvalidTool { .. }));
}

#[tokio::test]
async fn log_diff_tool_reports_sections() {
    let root = temp_repo();
    let store = InMemorySessions::new(Some(&root)).unwrap();
    let baseline = store
        .preload(
            "baseline",
            "10-25 08:00:00.001  1  1 D App: retry 1 failed\n\
             10-25 08:00:00.002  1  1 D App: retry 2 failed\n\
             10-25 08:00:00.003  1  1 D App: cache hit rate 0.9",
        )
        .unwrap();
    let target = store
        .open_text(
            "target",
            "10-25 08:10:00.001  1  1 D App: retry 1 failed\n\
             10-25 08:10:00.002  1  1 D App: retry 2 failed\n\
             10-25 08:10:00.003  1  1 D App: retry 3 failed\n\
             10-25 08:10:00.004  1  1 D App: new error 42",
        )
        .await
        .unwrap()
        .id;
    let sessions: Arc<dyn LogSessionStore> = Arc::new(store);
    let tool = LogDiffTool::new(sessions);

    let output = parse_output(
        &tool
            .call(serde_json::json!({ "baseline": baseline, "target": target }))
            .await
            .unwrap(),
    );
    let missing = output["missing_in_target"].as_array().unwrap();
    assert!(missing.len() == 1 && missing[0][0].as_str().unwrap().contains("cache"));
    let new_templates = output["new_in_target"].as_array().unwrap();
    assert!(new_templates
        .iter()
        .any(|item| item[0].as_str().unwrap().contains("new error")));
    let changed = output["changed"].as_array().unwrap();
    assert_eq!(changed[0]["baseline"], 2);
    assert_eq!(changed[0]["target"], 3);

    // 同一会话不可自比。
    assert!(tool
        .call(serde_json::json!({ "baseline": baseline, "target": baseline }))
        .await
        .is_err());
}

// ---------- 端到端 ----------

#[tokio::test]
async fn crash_triage_end_to_end() {
    let root = temp_repo();
    let env = AndroidEnvBuilder::offline(&root, SAMPLE_CRASH)
        .unwrap()
        .build()
        .unwrap();

    let parse_call = ChatResponse::new("mock")
        .with_tool_calls(vec![ToolCall {
            id: "call-1".into(),
            name: "android_log_parse".into(),
            arguments: serde_json::json!({ "log_text": SAMPLE_CRASH }),
        }])
        .with_finish_reason(FinishReason::ToolCalls);
    let final_answer = ChatResponse::new("mock")
        .with_content("根因：engine 未初始化")
        .with_finish_reason(FinishReason::Stop);

    let mut agent = build_agent(
        mock_model(vec![parse_call, final_answer]),
        &env,
        AndroidProfile::CrashTriage,
        RunConfig::default(),
    )
    .unwrap();

    let stop = agent
        .send(Message::user("分析日志中的崩溃"), &AgentHooks::default())
        .await
        .unwrap();
    assert!(matches!(stop, StopReason::Completed));

    let tool_results: Vec<&Message> = agent
        .history()
        .iter()
        .filter(|message| matches!(message, Message::Tool { .. }))
        .collect();
    assert_eq!(tool_results.len(), 1);
    match tool_results[0] {
        Message::Tool {
            content, is_error, ..
        } => {
            assert!(!is_error);
            assert!(content.contains("java.lang.NullPointerException"));
            assert!(content.contains("MainActivity.kt:6"));
        }
        other => panic!("期望 Tool 消息，实际 {other:?}"),
    }
}
