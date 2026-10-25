//! Android 诊断领域层：把“日志分析、代码定位、日志+代码联合分析”做成可组装的能力。
//!
//! 本 crate 不引入新的 Agent 机制，只做三件事：
//!
//! 1. 定义领域模型（[`types`]）：崩溃报告、堆栈帧、日志切片、代码命中位置。
//! 2. 定义能力接缝（[`capability`]）：日志来源、日志解析、代码检索、设备连接。
//!    工具只依赖这些 trait，不依赖具体实现——后期把 `LogProvider` 换成
//!    adb logcat 实现、`CodeIndex` 换成符号索引，工具层和 Agent 无需改动。
//! 3. 提供默认离线实现（[`builtin`]）和组装入口（[`env`]、[`agent`]）。
//!
//! 分层与升级路径：
//!
//! ```text
//! agent.rs    预设 profile（system prompt + 工具集选择）
//!   └─ env.rs      AndroidEnv：能力装配 + 生成 ToolRegistry
//!        └─ tools/ Tool 桥接：参数 JSON → capability 调用 → 文本/JSON 结果
//!             └─ capability/  接缝 trait（LogProvider / LogParser / CodeIndex / DeviceConnector）
//!                  └─ builtin/  离线默认实现（可整体替换）
//! ```
//!
//! 在线排查（adb）的接入点已经预留：实现 [`capability::DeviceConnector`]（计划中的
//! `AdbConnector`：`adb devices -l`、`adb -s <serial> shell`、`adb -s <serial> logcat -d`），
//! 再用 [`capability::DeviceLogProvider`] 把它适配成 `LogProvider`。这样
//! `android_log_fetch` 从离线日志切到设备实时日志时，模型看到的工具签名不变。
//!
//! 最小可用示例（离线崩溃分诊）：
//!
//! ```no_run
//! use kunpeng_android::{agent::{build_agent, AndroidProfile}, env::AndroidEnvBuilder};
//! use kunpeng_core::{Message, Result};
//! use kunpeng_runtime::{AgentHooks, RunConfig};
//!
//! async fn run(model: impl kunpeng_core::ChatModel + 'static, logs: &str) -> Result<()> {
//!     let env = AndroidEnvBuilder::offline("./repo-root", logs)?.build()?;
//!     let mut agent = build_agent(model, &env, AndroidProfile::CrashTriage, RunConfig::default())?;
//!     let stop = agent
//!         .send(Message::user("分析日志中的崩溃并定位到源码"), &AgentHooks::default())
//!         .await?;
//!     println!("{stop:?}");
//!     Ok(())
//! }
//! ```
//!
//! 设计取舍：解析器是确定性的纯函数（不把原始 logcat 直接丢给模型），一是省 token，
//! 二是堆栈帧、行号这类信息机器解析比 LLM 可靠；工具输出保持结构化 JSON，方便
//! 后期把中间结果落盘做崩溃去重与知识库匹配。

pub mod agent;
pub mod analysis;
pub mod builtin;
pub mod capability;
pub mod env;
pub mod tools;
pub mod types;

#[cfg(test)]
mod tests;
