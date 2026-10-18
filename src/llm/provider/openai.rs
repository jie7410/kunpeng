//! OpenAI Chat Completions 适配器，也用于智谱等兼容端点。
//!
//! 这里只做 `Message` ↔ OpenAI JSON 的翻译；重试、错误截断、URL 校验在
//! `provider::protocol` 和 `Provider` 里。兼容端点共用这个实现，差异通过
//! base_url/model 配置表达。
//!
//! 两个容易踩的字段：
//! - usage 里是 `completion_tokens`（复数），不是 `completion_token`；
//! - 工具参数在 JSON 里是**字符串**，内部类型是 `Value`，两侧都要显式转换。

use super::protocol::{
    api_error, parse_response, send_with_retry, truncate, CallContext, Protocol,
};
use crate::llm::error::{Error, Result};
use crate::llm::types::{ChatRequest, ChatResponse, FinishReason, Message, ToolCall, Usage};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

pub struct OpenAiProtocol;

#[async_trait]
impl Protocol for OpenAiProtocol {
    async fn chat(&self, ctx: &CallContext<'_>, req: &ChatRequest<'_>) -> Result<ChatResponse> {
        // 请求体只借用历史与工具定义：消息内容和 id 都不复制，只有工具参数
        // 因为协议要求字符串而重新序列化。
        let body = OpenAiRequest {
            model: ctx.model,
            messages: req.messages.iter().map(to_openai_message).collect(),
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
            temperature: req.temperature,
            max_tokens: req.max_tokens,
        };

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
        let raw = response.text().await?;
        let parsed: OpenAiResponse = parse_response("openai", &raw)?;

        // 没有传 n，默认只返回一个 choice。多 choice 属于另一套交互模型，
        // Agent 的循环只消费第一个。
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| Error::Other("OpenAI 响应 choices 为空".into()))?;

        let tool_calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|call| {
                let arguments =
                    parse_tool_arguments(&call.function.name, &call.function.arguments)?;
                Ok(ToolCall {
                    id: call.id,
                    name: call.function.name,
                    arguments,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(ChatResponse {
            // 空字符串统一成 None：部分兼容端点会返回 ""，UI 不应显示空气泡，
            // Anthropic 也不接受空的 assistant text block。
            content: choice.message.content.filter(|content| !content.is_empty()),
            tool_calls,
            model: parsed.model,
            usage: parsed.usage.map(Into::into),
            finish_reason: choice
                .finish_reason
                .as_deref()
                .map(FinishReason::from_openai),
        })
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
/// 用 `Cow` 借用原始字符串，tool 错误时才分配一个带前缀的新字符串。
/// assistant 的 content 不省略：OpenAI 工具调用消息通常是 `content: null`，
/// 部分兼容端点在字段缺失时会报错。
fn to_openai_message(message: &Message) -> OpenAiMessage<'_> {
    match message {
        Message::System { content } => OpenAiMessage {
            role: "system",
            content: Some(Cow::Borrowed(content)),
            tool_calls: None,
            tool_call_id: None,
        },
        Message::User { content } => OpenAiMessage {
            role: "user",
            content: Some(Cow::Borrowed(content)),
            tool_calls: None,
            tool_call_id: None,
        },
        Message::Assistant {
            content,
            tool_calls,
        } => OpenAiMessage {
            role: "assistant",
            content: content.as_deref().map(Cow::Borrowed),
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
            content: Some(if *is_error {
                // OpenAI 没有 is_error 字段，用文本前缀让模型能区分失败结果。
                Cow::Owned(format!("Error: {content}"))
            } else {
                Cow::Borrowed(content)
            }),
            tool_calls: None,
            tool_call_id: Some(tool_call_id),
        },
    }
}

/// 请求体。字段顺序不影响语义，省略 None 避免把空 tool 数组传给兼容端点。
#[derive(Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAiTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    /// 这里用 `max_tokens` 而不是新版的 `max_completion_tokens`：
    /// 目标是 Chat Completions 兼容端点，智谱等实现仍使用旧字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
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

/// 发送给 OpenAI 的消息。`role` 是静态字符串，避免为每个角色分配。
/// `content` 不 skip：assistant 纯工具调用时需要显式 `null`。
#[derive(Serialize)]
struct OpenAiMessage<'a> {
    role: &'static str,
    content: Option<Cow<'a, str>>,
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
#[derive(Deserialize)]
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
        Usage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let value = serde_json::to_value(to_openai_message(&message)).unwrap();
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
        };
        let value = serde_json::to_value(to_openai_message(&message)).unwrap();
        assert!(value["content"].is_null());
        assert_eq!(
            value["tool_calls"][0]["function"]["arguments"],
            r#"{"text":"hi"}"#
        );
    }
}
