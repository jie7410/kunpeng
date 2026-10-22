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
#[non_exhaustive]
pub struct ProviderData {
    pub provider: String,
    pub data: serde_json::Value,
}

impl ProviderData {
    pub fn new(provider: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            provider: provider.into(),
            data,
        }
    }
}

/// 扩展思考配置。当前只被 Anthropic 适配器使用；`None` 表示不开启。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ThinkingConfig {
    /// Anthropic 要求的最小值是 1024，且必须小于 max_tokens。
    pub budget_tokens: u32,
}

impl ThinkingConfig {
    pub const fn new(budget_tokens: u32) -> Self {
        Self { budget_tokens }
    }
}

/// 流式回调的控制信号。
///
/// 消费方（UI、取消令牌、预算控制器）返回 `Stop` 后，适配器应停止读取网络流并
/// 返回 `Error::Cancelled`，而不是继续消费和计费。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamControl {
    Continue,
    Stop,
}

/// 流式增量。文本和思考分开，避免 UI 把推理内容当正文显示。
///
/// 借用切片：适配器可以直接把已解析的字符串交给回调，默认回退也不需要为了
/// 发事件 clone 完整回复；需要持久化的消费方自行 `to_owned()`。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamDelta<'a> {
    Text(&'a str),
    Thinking(&'a str),
}

/// 上下文预算。只限制历史消息，不包含工具 schema 和输出预留。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TokenBudget {
    pub max_prompt_tokens: usize,
}

impl TokenBudget {
    pub const fn new(max_prompt_tokens: usize) -> Self {
        Self { max_prompt_tokens }
    }
}

/// 用户消息内容。纯文本保持字符串，多模态用 parts 数组。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl From<String> for Content {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for Content {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<Vec<ContentPart>> for Content {
    fn from(value: Vec<ContentPart>) -> Self {
        Self::Parts(value)
    }
}

/// 多模态内容块。`Image` 是结构化支持；其他类型（PDF、音频、provider 专属块）
/// 用 `Other` 透传，避免每出现一种类型就改一次核心 enum。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ImageDetail>,
    },
    /// 只允许对应 provider 消费；跨 provider 会返回配置错误。
    Other {
        provider: String,
        data: serde_json::Value,
    },
}

/// 图片来源。Base64 适合本地文件，Url 适合公网图片。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

/// OpenAI 图片 detail 参数。Anthropic 不支持，适配器会忽略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    Auto,
    Low,
    High,
}

/// 工具选择策略。适配器负责翻译成 OpenAI/Anthropic 的不同结构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Tool { name: String },
}

/// Provider 专属请求参数透传口。
///
/// 结构化字段覆盖常用参数；其余参数（如 OpenAI 的 `seed`、`response_format`，
/// Anthropic 的 `top_k`、`metadata`）放进对应 map。不要放 `model`、`messages`、
/// `tools` 等核心字段，避免覆盖适配器生成的内容。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic: Option<serde_json::Map<String, serde_json::Value>>,
}

impl ProviderOptions {
    pub fn with_openai(mut self, options: serde_json::Map<String, serde_json::Value>) -> Self {
        self.openai = Some(options);
        self
    }

    pub fn with_anthropic(mut self, options: serde_json::Map<String, serde_json::Value>) -> Self {
        self.anthropic = Some(options);
        self
    }
}

/// 服务端工具调用。只用于展示和审计，Agent 不会执行它。
///
/// Anthropic 的 `server_tool_use` 由服务端运行，客户端只负责在下一轮把原始
/// block 回放；`result` 保存对应的 `*_tool_result` 原始 block。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ServerToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub result: Option<serde_json::Value>,
}

impl ServerToolUse {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
        result: Option<serde_json::Value>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            input,
            result,
        }
    }
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
        content: Content,
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

    pub fn user(c: impl Into<Content>) -> Self {
        Self::User { content: c.into() }
    }

    /// 多模态用户消息。等价于 `Message::user(parts.into())`。
    pub fn user_parts(parts: Vec<ContentPart>) -> Self {
        Self::User {
            content: Content::Parts(parts),
        }
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
#[non_exhaustive]
pub struct ChatRequest<'a> {
    pub messages: &'a [Message],
    pub tools: &'a [ToolDefinition],
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// 仅 Anthropic 使用；`None` 表示不开启 extended thinking。
    pub thinking: Option<ThinkingConfig>,
    /// Anthropic 服务端工具定义，原样传给 API。不要注册到 `ToolRegistry`。
    pub server_tools: &'a [serde_json::Value],
    /// 工具选择策略。None 表示交给 provider 默认值（通常是 auto）。
    pub tool_choice: Option<&'a ToolChoice>,
    /// 停止序列。空切片表示不发送。
    pub stop_sequences: &'a [String],
    /// Provider 专属参数透传。
    pub provider_options: Option<&'a ProviderOptions>,
}

impl<'a> ChatRequest<'a> {
    pub fn new(messages: &'a [Message], tools: &'a [ToolDefinition]) -> Self {
        Self {
            messages,
            tools,
            temperature: None,
            max_tokens: None,
            thinking: None,
            server_tools: &[],
            tool_choice: None,
            stop_sequences: &[],
            provider_options: None,
        }
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn with_thinking(mut self, thinking: ThinkingConfig) -> Self {
        self.thinking = Some(thinking);
        self
    }

    pub fn with_server_tools(mut self, tools: &'a [serde_json::Value]) -> Self {
        self.server_tools = tools;
        self
    }

    pub fn with_tool_choice(mut self, choice: &'a ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    pub fn with_stop_sequences(mut self, sequences: &'a [String]) -> Self {
        self.stop_sequences = sequences;
        self
    }

    pub fn with_provider_options(mut self, options: &'a ProviderOptions) -> Self {
        self.provider_options = Some(options);
        self
    }
}

/// token 统计。字段名沿用 OpenAI 的叫法，Anthropic 的 input/output 在适配器里映射。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl Usage {
    /// 按 prompt + completion 自动计算 total，避免适配器手写时漏加或重复计数。
    pub const fn new(prompt_tokens: u32, completion_tokens: u32) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens.saturating_add(completion_tokens),
        }
    }
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

impl ChatResponse {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            content: None,
            tool_calls: Vec::new(),
            model: model.into(),
            usage: None,
            finish_reason: None,
            thinking: None,
            citations: Vec::new(),
            server_tool_uses: Vec::new(),
            provider_data: None,
        }
    }

    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    pub fn with_tool_calls(mut self, tool_calls: Vec<ToolCall>) -> Self {
        self.tool_calls = tool_calls;
        self
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    pub fn with_finish_reason(mut self, reason: FinishReason) -> Self {
        self.finish_reason = Some(reason);
        self
    }

    pub fn with_thinking(mut self, thinking: impl Into<String>) -> Self {
        self.thinking = Some(thinking.into());
        self
    }

    pub fn with_citations(mut self, citations: Vec<serde_json::Value>) -> Self {
        self.citations = citations;
        self
    }

    pub fn with_server_tool_uses(mut self, server_tool_uses: Vec<ServerToolUse>) -> Self {
        self.server_tool_uses = server_tool_uses;
        self
    }

    pub fn with_provider_data(mut self, provider_data: ProviderData) -> Self {
        self.provider_data = Some(provider_data);
        self
    }
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
}
