//! 协议无关的语义层。
//!
//! Agent 只认识这里的 `Message` / `ChatRequest` / `ChatResponse`；OpenAI 的
//! `role/content/tool_calls` 和 Anthropic 的 block 数组在 provider 层转换。
//!
//! 消息建模成 enum 而不是 `role: String + content: String`，是因为 assistant
//! 至少有三种状态：纯文本、纯工具调用、文本+工具调用；tool 结果又必须绑定
//! `tool_call_id`。拆成 enum 后，非法组合在编译期就不存在。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider 生成的调用 id。回传 `tool_result` 时必须原样带回；
    /// Anthropic 靠它把 tool_use 和 tool_result 配对，丢了就会被 API 拒绝。
    pub id: String,
    pub name: String,
    /// 参数保持为 `Value` 而不是字符串：
    /// - 工具实现可以直接按字段取值；
    /// - 回放给 OpenAI 时再序列化成字符串，避免双重转义。
    pub arguments: serde_json::Value,
}

/// Provider 私有的不透明数据。
///
/// Anthropic extended thinking 的 `thinking` block 带有 `signature`，下一轮请求
/// 必须原样回传；`redacted_thinking`、服务端工具等 block 也有同样要求。语义层
/// 无法为每种 block 建类型，所以把完整 content 数组存这里。
///
/// 只有同名 provider 会消费它；切换 provider 时该字段会被忽略。调用方不应修改
/// `data`，否则签名和原始 block 可能不匹配。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderData {
    pub provider: String,
    pub data: serde_json::Value,
}

/// 扩展思考配置。当前只被 Anthropic 适配器使用；`None` 表示不开启。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingConfig {
    /// Anthropic 要求的最小值是 1024，且必须小于 max_tokens。
    pub budget_tokens: u32,
}

/// 流式增量。文本和思考分开，避免 UI 把推理内容当正文显示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamDelta {
    Text(String),
    Thinking(String),
}

/// 服务端工具调用。只用于展示和审计，Agent 不会执行它。
///
/// Anthropic 的 `server_tool_use` 由服务端运行，客户端只负责在下一轮把原始
/// block 回放；`result` 保存对应的 `*_tool_result` 原始 block。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub result: Option<serde_json::Value>,
}

/// 协议无关的会话消息。
///
/// serde 使用 `role` 作为内部 tag，直接落盘即可恢复会话；这也是为什么
/// `content` 为 None 时跳过序列化，读回时用 `default` 补成 None。
///
/// `#[non_exhaustive]` 让后续增加消息类型（例如图片、thinking block）不会成为
/// 破坏性变更；外部匹配需要保留 `_` 分支。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Message {
    /// 系统提示。Anthropic 只接受一个顶层 system 字段，provider 会把多条拼接；
    /// OpenAI 则按原位置保留。语义层不提前合并，避免丢掉位置信息。
    System {
        content: String,
    },

    User {
        content: String,
    },

    /// 模型回复。`tool_calls` 非空时，Agent 必须在下一轮为每个调用补上
    /// `Message::Tool`，否则历史无法回放。
    Assistant {
        /// 纯工具调用时为 None；OpenAI 接受 content=null。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default)]
        tool_calls: Vec<ToolCall>,
        /// 合并后的 thinking 文本，仅供展示；真正回放用 `provider_data`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        /// 原始 citation 对象，结构随 Anthropic 文档类型变化，先保持 JSON。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        citations: Vec<serde_json::Value>,
        /// 服务端工具调用，只读展示；不会进入本地工具循环。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        server_tool_uses: Vec<ServerToolUse>,
        /// 原始 provider block。Anthropic 的 thinking/signature 必须靠它原样回传。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_data: Option<ProviderData>,
    },

    /// 工具执行结果。`is_error` 只有 Anthropic 协议有原生字段；OpenAI 适配器会
    /// 把错误内容加上 `Error: ` 前缀，至少让模型能看出来。
    Tool {
        tool_call_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

impl Message {
    pub fn system(c: impl Into<String>) -> Self {
        Self::System { content: c.into() }
    }

    pub fn user(c: impl Into<String>) -> Self {
        Self::User { content: c.into() }
    }

    pub fn assistant(c: impl Into<String>) -> Self {
        Self::Assistant {
            content: Some(c.into()),
            tool_calls: vec![],
            thinking: None,
            citations: vec![],
            server_tool_uses: vec![],
            provider_data: None,
        }
    }

    pub fn tool_result(id: impl Into<String>, content: impl Into<String>, is_error: bool) -> Self {
        Self::Tool {
            tool_call_id: id.into(),
            content: content.into(),
            is_error,
        }
    }
}

/// 工具 schema。`input_schema` 直接透传给 provider，不做本地校验：
/// JSON Schema 子集各家支持程度不同，内核做校验容易和实际 API 行为不一致。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// 单次模型调用参数。
///
/// 消息和工具定义都是借用切片。Agent 的 history 可能很长，如果每轮都按值传入，
/// 一轮对话会退化成 O(n²) 次消息复制；借用后请求期间 history 只读，push 发生在
/// 请求返回之后。
///
/// 没有 `stream` 字段：当前协议层只实现非流式。流式需要单独的事件和增量解析，
/// 留一个永远被忽略的 bool 只会让调用方误以为生效。
#[derive(Debug, Clone, Copy)]
pub struct ChatRequest<'a> {
    pub messages: &'a [Message],
    pub tools: &'a [ToolDefinition],
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// 仅 Anthropic 使用；`None` 表示不开启 extended thinking。
    pub thinking: Option<ThinkingConfig>,
    /// Anthropic 服务端工具定义，原样传给 API。不要注册到 `ToolRegistry`。
    pub server_tools: &'a [serde_json::Value],
}

/// token 统计。字段名沿用 OpenAI 的叫法，Anthropic 的 input/output 在适配器里映射。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// 统一后的结束原因。
///
/// OpenAI 用 `stop/tool_calls/length/content_filter`，Anthropic 用
/// `end_turn/stop_sequence/tool_use/max_tokens/refusal`。UI 不应该记住两套字符串，
/// 所以在这里归一化；未知值放 `Other` 保留原文，方便新协议出现时不丢信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    /// Anthropic 服务端工具长任务暂停，需要原样回放后继续请求。
    Pause,
    Other(String),
}

impl FinishReason {
    pub(crate) fn from_openai(value: &str) -> Self {
        match value {
            "stop" => Self::Stop,
            "tool_calls" | "function_call" => Self::ToolCalls,
            "length" => Self::Length,
            "content_filter" => Self::ContentFilter,
            other => Self::Other(other.to_string()),
        }
    }

    pub(crate) fn from_anthropic(value: &str) -> Self {
        match value {
            "end_turn" | "stop_sequence" => Self::Stop,
            "tool_use" => Self::ToolCalls,
            "max_tokens" => Self::Length,
            "refusal" => Self::ContentFilter,
            "pause_turn" => Self::Pause,
            other => Self::Other(other.to_string()),
        }
    }
}

/// 一次模型调用的结果。
///
/// `content` 和 `tool_calls` 可以同时存在：Anthropic 一次回复可以同时有 text 和
/// tool_use block；OpenAI 也允许文本加工具调用。Agent 会先展示文本，再执行工具。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub model: String,
    pub usage: Option<Usage>,
    pub finish_reason: Option<FinishReason>,
    /// 合并后的 thinking 文本，仅供展示。
    pub thinking: Option<String>,
    /// 原始 citation 对象，供 UI 渲染脚注/引用。
    pub citations: Vec<serde_json::Value>,
    /// 服务端工具调用，只读展示。
    pub server_tool_uses: Vec<ServerToolUse>,
    /// 原始 provider block，供下一轮原样回放。
    pub provider_data: Option<ProviderData>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_serde_round_trip() {
        let messages = vec![
            Message::system("system"),
            Message::user("user"),
            Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "hi"}),
                }],
                thinking: Some("thinking".into()),
                citations: vec![serde_json::json!({"type": "char_location"})],
                server_tool_uses: vec![ServerToolUse {
                    id: "srv-1".into(),
                    name: "web_search".into(),
                    input: serde_json::json!({"query": "rust"}),
                    result: Some(serde_json::json!({"type": "web_search_tool_result"})),
                }],
                provider_data: Some(ProviderData {
                    provider: "anthropic".into(),
                    data: serde_json::json!([{"type": "thinking", "thinking": "thinking"}]),
                }),
            },
            Message::tool_result("call-1", "failed", true),
        ];

        for message in messages {
            let json = serde_json::to_string(&message).unwrap();
            let parsed: Message = serde_json::from_str(&json).unwrap();
            assert_eq!(message, parsed);
        }
    }

    #[test]
    fn finish_reasons_are_normalized() {
        assert_eq!(
            FinishReason::from_openai("tool_calls"),
            FinishReason::ToolCalls
        );
        assert_eq!(
            FinishReason::from_anthropic("tool_use"),
            FinishReason::ToolCalls
        );
        assert_eq!(
            FinishReason::from_anthropic("max_tokens"),
            FinishReason::Length
        );
        assert_eq!(
            FinishReason::from_anthropic("pause_turn"),
            FinishReason::Pause
        );
        assert_eq!(
            FinishReason::from_openai("something_new"),
            FinishReason::Other("something_new".into())
        );
    }
}
