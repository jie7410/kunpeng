//! CLI 宿主侧的配置适配层。
//!
//! 核心库不读取环境变量；环境变量、配置文件、命令行参数都应该由宿主入口
//! 转换成 `kunpeng::provider::LlmConfig`。Tauri 入口可以复用同样的构造方式。

pub mod config;
