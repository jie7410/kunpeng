//! 上下文工程：token 预算、历史裁剪、记忆注入。

mod builder;
mod token;

pub use builder::{ContextBuilder, SlidingWindowContext};
pub use kunpeng_core::TokenBudget;
pub use token::{ApproxTokenCounter, TokenCounter};
