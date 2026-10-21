//! OpenAI Chat Completions 适配器，也用于智谱等兼容端点。
//!
//! 这里只做 `Message` ↔ OpenAI JSON 的翻译；重试、错误截断、URL 校验在
//! `kunpeng_provider::adapter` 和 `Provider` 里。兼容端点共用这个实现，差异通过
//! base_url/model 配置表达。
//!
//! 两个容易踩的字段：
//! - usage 里是 `completion_tokens`（复数），不是 `completion_token`；
//! - 工具参数在 JSON 里是**字符串**，内部类型是 `Value`，两侧都要显式转换。

use async_trait::async_trait;
use kunpeng_core::error::{Error, Result};
use kunpeng_core::types::{
    ChatRequest, ChatResponse, Content, ContentPart, FinishReason, ImageDetail, ImageSource,
    Message, StreamDelta, ToolCall, ToolChoice, Usage,
};
use kunpeng_provider::adapter::{
    api_error, consume_sse, http_error, is_event_stream, parse_response, send_with_retry, truncate,
    Adapter, CallContext,
};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::BTreeMap};

#[derive(Debug, Clone, Copy)]
pub struct OpenAiAdapter {
    name: &'static str,
    aliases: &'static [&'static str],
    base_url: &'static str,
    model: &'static str,
}

impl OpenAiAdapter {
    pub const fn new(
        name: &'static str,
        aliases: &'static [&'static str],
        base_url: &'static str,
        model: &'static str,
    ) -> Self {
        Self {
            name,
            aliases,
            base_url,
            model,
        }
    }

    pub const fn openai() -> Self {
        Self::new("openai", &[], "https://api.openai.com/v1", "gpt-4o-mini")
    }

    /// 兼容端点必须显式提供 base URL 和模型名。
    pub const fn compatible() -> Self {
        Self::new("openai-compatible", &[], "", "")
    }

    /// 智谱 Chat Completions 与 OpenAI 兼容，只是地址和模型不同。
    pub const fn zhipu() -> Self {
        Self::new(
            "zhipu",
            &["glm", "bigmodel"],
            "https://open.bigmodel.cn/api/paas/v4",
            "glm-4.6",
        )
    }
}

#[async_trait]
impl Adapter for OpenAiAdapter {
    fn name(&self) -> &'static str {
        self.name
    }

    fn aliases(&self) -> &'static [&'static str] {
        self.aliases
    }

    fn default_base_url(&self) -> &'static str {
        self.base_url
    }

    fn default_model(&self) -> &'static str {
        self.model
    }

    async fn chat(&self, ctx: &CallContext<'_>, req: &ChatRequest<'_>) -> Result<ChatResponse> {
        if !req.server_tools.is_empty() {
            return Err(Error::InvalidConfig(
                "server_tools 只支持 Anthropic 服务端工具".into(),
            ));
        }
        // 请求体只借用历史与工具定义：消息内容和 id 都不复制，只有工具参数
        // 因为协议要求字符串而重新序列化。
        let body = build_body(ctx.model, req, false)?;

        // base_url 约定包含版本前缀（如 /v1），这里只拼资源路径。
        // 闭包会在每次重试时重新构造并序列化请求体。
        let response = send_with_retry(ctx.max_retries, || {
            ctx.http
                .post(format!("{}/chat/completions", ctx.base_url))
                .bearer_auth(ctx.api_key)
                .json(&body)
        })
        .await?;

        let status = response.status();
        if !status.is_success() {
            // 非 2xx 的 body 可能是 JSON 错误，也可能是网关 HTML。先读文本，
            // 交给 api_error 截断，保证错误里能看到服务端原话。
            let body = response.text().await.unwrap_or_default();
            return Err(api_error(status, body));
        }

        // 不直接 response.json()：解析失败时要把 body 片段带进错误。
        let raw = response.text().await.map_err(http_error)?;
        let parsed: OpenAiResponse = parse_response("openai", &raw)?;
        into_chat_response(parsed, ctx.model)
    }

    /// 流式请求。OpenAI 的 tool_calls 参数会分片到达，按 `index` 累积后
    /// 再一次性解析；文本分片通过 `on_delta` 立即交给 UI。
    async fn chat_stream(
        &self,
        ctx: &CallContext<'_>,
        req: &ChatRequest<'_>,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<ChatResponse> {
        if !req.server_tools.is_empty() {
            return Err(Error::InvalidConfig(
                "server_tools 只支持 Anthropic 服务端工具".into(),
            ));
        }
        let body = build_body(ctx.model, req, true)?;
        let response = send_with_retry(ctx.max_retries, || {
            ctx.http
                .post(format!("{}/chat/completions", ctx.base_url))
                .bearer_auth(ctx.api_key)
                .json(&body)
        })
        .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(api_error(status, body));
        }

        // 兼容端点可能忽略 stream=true，直接返回 JSON。按 content-type 回退，
        // 避免把一份普通响应当成空的 SSE 流。
        if !is_event_stream(&response) {
            let raw = response.text().await.map_err(http_error)?;
            let parsed: OpenAiResponse = parse_response("openai", &raw)?;
            return into_chat_response(parsed, ctx.model);
        }

        let mut content = String::new();
        let mut model = None;
        let mut usage = None;
        let mut finish_reason = None;
        let mut tool_calls: BTreeMap<usize, ToolCallAccum> = BTreeMap::new();
        let mut done = false;

        consume_sse(response, |event| {
            if done || event.data.is_empty() {
                return Ok(());
            }
            if event.data.trim() == "[DONE]" {
                done = true;
                return Ok(());
            }

            let parsed: OpenAiStreamResponse = parse_response("openai", &event.data)?;
            if let Some(error) = parsed.error {
                return Err(Error::Other(format!("OpenAI 流错误: {}", error.message)));
            }
            if let Some(value) = parsed.model {
                if !value.is_empty() {
                    model = Some(value);
                }
            }
            if let Some(value) = parsed.usage {
                usage = Some(value.into());
            }

            for choice in parsed.choices {
                if let Some(text) = choice.delta.content {
                    if !text.is_empty() {
                        content.push_str(&text);
                        on_delta(StreamDelta::Text(text));
                    }
                }

                if let Some(calls) = choice.delta.tool_calls {
                    for call in calls {
                        let accumulator = tool_calls.entry(call.index).or_default();
                        if let Some(id) = call.id {
                            accumulator.id = Some(id);
                        }
                        if let Some(function) = call.function {
                            if let Some(name) = function.name {
                                accumulator.name = Some(name);
                            }
                            if let Some(arguments) = function.arguments {
                                accumulator.arguments.push_str(&arguments);
                            }
                        }
                    }
                }

                if let Some(reason) = choice.finish_reason {
                    finish_reason = Some(normalize_finish_reason(&reason));
                }
            }
            Ok(())
        })
        .await?;

        let tool_calls = tool_calls
            .into_values()
            .map(ToolCallAccum::into_tool_call)
            .collect::<Result<Vec<_>>>()?;

        let mut response = ChatResponse::new(model.unwrap_or_else(|| ctx.model.to_string()));
        response.content = if content.is_empty() {
            None
        } else {
            Some(content)
        };
        response.tool_calls = tool_calls;
        response.usage = usage;
        response.finish_reason = finish_reason;
        Ok(response)
    }
}

/// 普通响应 JSON → 统一 `ChatResponse`。非流式和“服务端忽略 stream”的回退共用。
fn into_chat_response(parsed: OpenAiResponse, fallback_model: &str) -> Result<ChatResponse> {
    let OpenAiResponse {
        model,
        choices,
        usage,
    } = parsed;

    // 没有传 n，默认只返回一个 choice。多 choice 属于另一套交互模型，
    // Agent 的循环只消费第一个。
    let choice = choices
        .into_iter()
        .next()
        .ok_or_else(|| Error::Other("OpenAI 响应 choices 为空".into()))?;

    let tool_calls = choice
        .message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|call| {
            let arguments = parse_tool_arguments(&call.function.name, &call.function.arguments)?;
            Ok(ToolCall {
                id: call.id,
                name: call.function.name,
                arguments,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut response = ChatResponse::new(if model.is_empty() {
        fallback_model.to_string()
    } else {
        model
    });
    // 空字符串统一成 None：部分兼容端点会返回 ""，UI 不应显示空气泡，
    // Anthropic 也不接受空的 assistant text block。
    response.content = choice.message.content.filter(|content| !content.is_empty());
    response.tool_calls = tool_calls;
    response.usage = usage.map(Into::into);
    response.finish_reason = choice.finish_reason.as_deref().map(normalize_finish_reason);
    Ok(response)
}

/// 构造请求体。非流式不发送 `stream` 字段，避免兼容端点对未知/多余字段敏感。
fn build_body<'a>(
    model: &'a str,
    req: &'a ChatRequest<'a>,
    stream: bool,
) -> Result<OpenAiRequest<'a>> {
    Ok(OpenAiRequest {
        model,
        messages: req
            .messages
            .iter()
            .map(to_openai_message)
            .collect::<Result<Vec<_>>>()?,
        // 空数组和“不传”在部分兼容端点上行为不同，显式用 None 省略字段。
        tools: if req.tools.is_empty() {
            None
        } else {
            Some(
                req.tools
                    .iter()
                    .map(|tool| OpenAiTool {
                        r#type: "function",
                        function: OpenAiFunction {
                            name: &tool.name,
                            description: &tool.description,
                            parameters: &tool.input_schema,
                        },
                    })
                    .collect(),
            )
        },
        tool_choice: req.tool_choice.map(to_openai_tool_choice),
        stop: if req.stop_sequences.is_empty() {
            None
        } else {
            Some(req.stop_sequences)
        },
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        stream: stream.then_some(true),
        extra: req
            .provider_options
            .and_then(|options| options.openai.clone())
            .unwrap_or_default(),
    })
}

/// 语义 ToolChoice → OpenAI 结构。
fn normalize_finish_reason(value: &str) -> FinishReason {
    match value {
        "stop" => FinishReason::Stop,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_string()),
    }
}

fn to_openai_tool_choice(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Auto => serde_json::json!("auto"),
        ToolChoice::None => serde_json::json!("none"),
        ToolChoice::Required => serde_json::json!("required"),
        ToolChoice::Tool { name } => serde_json::json!({
            "type": "function",
            "function": { "name": name },
        }),
        // core 新增工具选择策略时，保持向后兼容的 auto 行为。
        _ => serde_json::json!("auto"),
    }
}

/// 把协议里的参数 JSON 字符串解析成 `Value`。
///
/// 空字符串按 `{}` 处理：不带参数的工具在部分实现里会返回空串。非空但非法时
/// 必须报错——退回 `Null` 会让工具拿到错误参数，而且下一轮回放时 `null` 也不是
/// 合法的 JSON 参数对象。
fn parse_tool_arguments(tool: &str, raw: &str) -> Result<serde_json::Value> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(raw).map_err(|source| Error::InvalidToolArguments {
        tool: tool.to_string(),
        raw: truncate(raw, 256),
        source,
    })
}

/// 语义消息 → OpenAI 消息。
///
/// 纯文本仍序列化为字符串；多模态转换为 content parts 数组。`Other` 内容块
/// 只允许 provider 为 openai，避免把 Anthropic 的 block 误发给 OpenAI。
fn to_openai_message(message: &Message) -> Result<OpenAiMessage<'_>> {
    let message = match message {
        Message::System { content } => OpenAiMessage {
            role: "system",
            content: Some(OpenAiContent::Text(Cow::Borrowed(content))),
            tool_calls: None,
            tool_call_id: None,
        },
        Message::User { content } => OpenAiMessage {
            role: "user",
            content: Some(to_openai_content(content)?),
            tool_calls: None,
            tool_call_id: None,
        },
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => OpenAiMessage {
            role: "assistant",
            content: content
                .as_deref()
                .map(|text| OpenAiContent::Text(Cow::Borrowed(text))),
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(
                    tool_calls
                        .iter()
                        .map(|call| OpenAiToolCall {
                            id: &call.id,
                            r#type: "function",
                            function: OpenAiCallFunction {
                                name: &call.name,
                                // OpenAI 要求 arguments 是 JSON 字符串，
                                // 不是对象。这里只在序列化边界转一次。
                                arguments: call.arguments.to_string(),
                            },
                        })
                        .collect(),
                )
            },
            tool_call_id: None,
        },
        Message::Tool {
            tool_call_id,
            content,
            is_error,
        } => OpenAiMessage {
            role: "tool",
            content: Some(OpenAiContent::Text(if *is_error {
                // OpenAI 没有 is_error 字段，用文本前缀让模型能区分失败结果。
                Cow::Owned(format!("Error: {content}"))
            } else {
                Cow::Borrowed(content)
            })),
            tool_calls: None,
            tool_call_id: Some(tool_call_id),
        },
        _ => return Err(Error::InvalidConfig("OpenAI 适配器不支持该消息类型".into())),
    };
    Ok(message)
}

fn to_openai_content(content: &Content) -> Result<OpenAiContent<'_>> {
    match content {
        Content::Text(text) => Ok(OpenAiContent::Text(Cow::Borrowed(text))),
        Content::Parts(parts) => {
            let mut converted = Vec::with_capacity(parts.len());
            for part in parts {
                match part {
                    ContentPart::Text { text } => {
                        converted.push(OpenAiContentPart::Structured(
                            OpenAiContentPartInner::Text { text },
                        ));
                    }
                    ContentPart::Image { source, detail } => {
                        let url = match source {
                            ImageSource::Base64 { media_type, data } => {
                                Cow::Owned(format!("data:{media_type};base64,{data}"))
                            }
                            ImageSource::Url { url } => Cow::Borrowed(url.as_str()),
                            _ => return Err(Error::InvalidConfig("OpenAI 不支持该图片源".into())),
                        };
                        converted.push(OpenAiContentPart::Structured(
                            OpenAiContentPartInner::ImageUrl {
                                image_url: OpenAiImageUrl {
                                    url,
                                    detail: *detail,
                                },
                            },
                        ));
                    }
                    ContentPart::Other { provider, data } if provider == "openai" => {
                        converted.push(OpenAiContentPart::Raw(data));
                    }
                    ContentPart::Other { provider, .. } => {
                        return Err(Error::InvalidConfig(format!(
                            "OpenAI 不支持 {provider} 内容块"
                        )));
                    }
                    _ => return Err(Error::InvalidConfig("OpenAI 不支持未知内容块".into())),
                }
            }
            Ok(OpenAiContent::Parts(converted))
        }
        _ => Err(Error::InvalidConfig("OpenAI 不支持该内容类型".into())),
    }
}

// ============================================================================
// Adapter Internal: OpenAI wire format
//
// 以下类型只用于本适配器的 serde 序列化/反序列化，不从 crate 导出，也不属于
// 稳定 API。厂商字段变化时可以直接调整；调用方只应依赖 `Adapter` trait 和
// core 的语义类型。
// ============================================================================

/// 请求体。字段顺序不影响语义，省略 None 避免把空 tool 数组传给兼容端点。
#[derive(Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAiTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<&'a [String]>,
    /// 这里用 `max_tokens` 而不是新版的 `max_completion_tokens`：
    /// 目标是 Chat Completions 兼容端点，智谱等实现仍使用旧字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    /// 非流式请求省略该字段；流式时显式传 true。
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

/// 工具声明。OpenAI 用 `{type:"function", function:{...}}` 包一层，
/// 与内部扁平的 `ToolDefinition` 不同。
#[derive(Serialize)]
struct OpenAiTool<'a> {
    r#type: &'static str,
    function: OpenAiFunction<'a>,
}

#[derive(Serialize)]
struct OpenAiFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

/// 纯文本仍用字符串，多模态用 parts 数组；`untagged` 让 serde 自动选择。
#[derive(Serialize)]
#[serde(untagged)]
enum OpenAiContent<'a> {
    Text(Cow<'a, str>),
    Parts(Vec<OpenAiContentPart<'a>>),
}

/// 结构化 part 与 provider 原始 part 的包装，Raw 直接序列化为原始 JSON 对象。
#[derive(Serialize)]
#[serde(untagged)]
enum OpenAiContentPart<'a> {
    Structured(OpenAiContentPartInner<'a>),
    Raw(&'a serde_json::Value),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiContentPartInner<'a> {
    Text { text: &'a str },
    ImageUrl { image_url: OpenAiImageUrl<'a> },
}

#[derive(Serialize)]
struct OpenAiImageUrl<'a> {
    url: Cow<'a, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<ImageDetail>,
}

/// 发送给 OpenAI 的消息。`role` 是静态字符串，避免为每个角色分配。
/// `content` 不 skip：assistant 纯工具调用时需要显式 `null`。
#[derive(Serialize)]
struct OpenAiMessage<'a> {
    role: &'static str,
    content: Option<OpenAiContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiToolCall<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Serialize)]
struct OpenAiToolCall<'a> {
    id: &'a str,
    r#type: &'static str,
    function: OpenAiCallFunction<'a>,
}

#[derive(Serialize)]
struct OpenAiCallFunction<'a> {
    name: &'a str,
    /// 协议要求 JSON 字符串，不能直接序列化成对象。
    arguments: String,
}

/// 响应只用到现在需要的字段，其余字段 serde 会忽略。
/// 不引入 `deny_unknown_fields`：兼容端点经常多加字段，严格解析反而更脆。
#[derive(Deserialize)]
struct OpenAiResponse {
    model: String,
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiRespMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiRespMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiRespToolCall>>,
}

#[derive(Deserialize)]
struct OpenAiRespToolCall {
    id: String,
    function: OpenAiRespCallFunction,
}

#[derive(Deserialize)]
struct OpenAiRespCallFunction {
    name: String,
    arguments: String,
}

/// usage 字段名以 OpenAI 为准。`completion_tokens` 是复数；写错时 serde 的
/// `default` 会把它静默变成 0，所以有单独的测试固定这个字段。
#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

impl From<OpenAiUsage> for Usage {
    fn from(usage: OpenAiUsage) -> Self {
        let mut normalized = Usage::new(usage.prompt_tokens, usage.completion_tokens);
        normalized.total_tokens = usage.total_tokens;
        normalized
    }
}

/// 流式 chunk。`choices` 可能为空（例如只带 usage 的最后一帧）。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OpenAiStreamResponse {
    model: Option<String>,
    choices: Vec<OpenAiStreamChoice>,
    usage: Option<OpenAiUsage>,
    error: Option<OpenAiStreamError>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OpenAiStreamChoice {
    delta: OpenAiStreamDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OpenAiStreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<OpenAiStreamToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamToolCall {
    /// 同一条回复里多个工具调用的分片共用一个 index。
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OpenAiStreamFunction>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OpenAiStreamFunction {
    name: Option<String>,
    /// JSON 字符串的分片，需要按顺序拼接。
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamError {
    #[serde(default)]
    message: String,
}

/// 累积一个工具调用的分片。id/name 通常只在第一帧出现，arguments 可能拆成很多帧。
#[derive(Debug, Default)]
struct ToolCallAccum {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolCallAccum {
    fn into_tool_call(self) -> Result<ToolCall> {
        let id = self
            .id
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Other("OpenAI 流式工具调用缺少 id".into()))?;
        let name = self
            .name
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Other("OpenAI 流式工具调用缺少 name".into()))?;
        let arguments = parse_tool_arguments(&name, &self.arguments)?;
        Ok(ToolCall {
            id,
            name,
            arguments,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_defaults_and_aliases() {
        assert_eq!(OpenAiAdapter::openai().name(), "openai");
        assert_eq!(
            OpenAiAdapter::openai().default_base_url(),
            "https://api.openai.com/v1"
        );
        assert_eq!(OpenAiAdapter::openai().default_model(), "gpt-4o-mini");
        assert_eq!(OpenAiAdapter::zhipu().aliases(), &["glm", "bigmodel"]);
        assert_eq!(OpenAiAdapter::compatible().default_base_url(), "");
    }

    #[test]
    fn normalizes_openai_finish_reasons() {
        assert_eq!(
            normalize_finish_reason("tool_calls"),
            FinishReason::ToolCalls
        );
        assert_eq!(normalize_finish_reason("length"), FinishReason::Length);
        assert_eq!(
            normalize_finish_reason("something_new"),
            FinishReason::Other("something_new".into())
        );
    }

    #[test]
    fn parses_completion_tokens_from_protocol_field() {
        let raw = r#"{
            "model": "m",
            "choices": [],
            "usage": {"prompt_tokens": 3, "completion_tokens": 7, "total_tokens": 10}
        }"#;
        let response: OpenAiResponse = serde_json::from_str(raw).unwrap();
        let usage: Usage = response.usage.unwrap().into();
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.total_tokens, 10);
    }

    #[test]
    fn invalid_tool_arguments_are_not_silently_null() {
        let error = parse_tool_arguments("echo", "{not json").unwrap_err();
        assert!(matches!(error, Error::InvalidToolArguments { .. }));
    }

    #[test]
    fn empty_tool_arguments_mean_empty_object() {
        assert_eq!(
            parse_tool_arguments("echo", "  ").unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn tool_errors_are_visible_to_the_model() {
        let message = Message::tool_result("call-1", "boom", true);
        let value = serde_json::to_value(to_openai_message(&message).unwrap()).unwrap();
        assert_eq!(value["role"], "tool");
        assert_eq!(value["content"], "Error: boom");
    }

    #[test]
    fn assistant_tool_arguments_are_json_strings() {
        let message = Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"text": "hi"}),
            }],
            thinking: None,
            citations: vec![],
            server_tool_uses: vec![],
            provider_data: None,
        };
        let value = serde_json::to_value(to_openai_message(&message).unwrap()).unwrap();
        assert!(value["content"].is_null());
        assert_eq!(
            value["tool_calls"][0]["function"]["arguments"],
            r#"{"text":"hi"}"#
        );
    }

    #[test]
    fn serializes_multimodal_parts_and_request_extensions() {
        use kunpeng_core::types::{
            ContentPart, ImageDetail, ImageSource, ProviderOptions, ToolChoice, ToolDefinition,
        };

        let messages = vec![Message::user_parts(vec![
            ContentPart::Text {
                text: "看图".into(),
            },
            ContentPart::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "abc".into(),
                },
                detail: Some(ImageDetail::High),
            },
        ])];
        let tools = vec![ToolDefinition {
            name: "echo".into(),
            description: "echo".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let choice = ToolChoice::Tool {
            name: "echo".into(),
        };
        let stop = vec!["END".to_string()];
        let mut extra = serde_json::Map::new();
        extra.insert("seed".into(), serde_json::json!(7));
        let options = ProviderOptions::default().with_openai(extra);
        let request = ChatRequest::new(&messages, &tools)
            .with_tool_choice(&choice)
            .with_stop_sequences(&stop)
            .with_provider_options(&options);

        let value = serde_json::to_value(build_body("m", &request, false).unwrap()).unwrap();
        assert_eq!(value["messages"][0]["content"][0]["type"], "text");
        assert_eq!(value["messages"][0]["content"][1]["type"], "image_url");
        assert_eq!(
            value["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,abc"
        );
        assert_eq!(
            value["messages"][0]["content"][1]["image_url"]["detail"],
            "high"
        );
        assert_eq!(value["tool_choice"]["function"]["name"], "echo");
        assert_eq!(value["stop"][0], "END");
        assert_eq!(value["seed"], 7);
    }
}
