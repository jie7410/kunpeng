//! Provider 传输层与适配器 API。
//!
//! 这一层负责：
//! - `Provider`：HTTP Client、base URL、API key、超时和重试；
//! - `Adapter`：厂商协议适配接口与 SSE/重试/响应解析工具；
//! - `AdapterRegistry`：显式注册/解析协议名称和别名；
//! - `LlmConfig` + `Model`：把配置解析结果绑定到具体模型。
//!
//! 具体厂商在独立 crate 中实现 `Adapter`：
//! `kunpeng-adapter-openai`、`kunpeng-adapter-anthropic`。`kunpeng-provider`
//! 本身不认识任何厂商名称。

pub mod adapter;
mod config;
mod model;
mod provider;
mod registry;

pub use adapter::{
    api_error, consume_sse, http_error, is_event_stream, parse_response, send_with_retry, truncate,
    Adapter, CallContext, SseEvent,
};
pub use config::LlmConfig;
pub use model::{Model, ModelDefault};
pub use provider::Provider;
pub use registry::AdapterRegistry;
