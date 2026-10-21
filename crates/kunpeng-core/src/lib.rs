//! 核心框架层：协议无关的语义类型、模型抽象和错误。
//!
//! 这一层不依赖 HTTP、具体 provider、tools、memory、context、agents 或 UI。

pub mod chat_model;
pub mod error;
pub mod types;

pub use chat_model::ChatModel;
pub use error::{Error, Result};
pub use types::{
    ChatRequest, ChatResponse, Content, ContentPart, FinishReason, ImageDetail, ImageSource,
    Message, ProviderData, ProviderOptions, ServerToolUse, StreamDelta, ThinkingConfig,
    TokenBudget, ToolCall, ToolChoice, ToolDefinition, Usage,
};
