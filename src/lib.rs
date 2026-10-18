//! 核心只依赖 tokio/reqwest/serde，不引入 UI 或平台类型。
//!
//! CLI、Tauri、测试都通过 `llm::Agent` + `llm::AgentEvent` 消费同一个内核；
//! UI 层只负责把事件转成展示/传输格式，以及实现工具审批。

pub mod llm;
