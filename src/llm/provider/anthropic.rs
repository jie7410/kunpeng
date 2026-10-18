//! Anthropic Messages API 适配器。
//!
//! 与 OpenAI 的结构差异都收敛在这里：
//! - system 是顶层字段，不是 messages 里的一条；
//! - user/assistant 的 content 是 block 数组，文本和工具调用可以混排；
//! - 工具结果必须是 user 消息里的 `tool_result` block，并靠 id 与
//!   `tool_use` 配对；同一个 assistant 回复的所有结果要放进同一条 user 消息；
//! - `max_tokens` 必填。
//!
//! 注意：响应里的未知 block（extended thinking、服务端工具等）目前会被忽略。
//! 启用这些能力前需要把原始 block 存进历史并原样回放，否则多轮请求会被拒绝。

use super::protocol::{api_error, parse_response, send_with_retry, CallContext, Protocol};
use crate::llm::error::Result;
use crate::llm::types::{ChatRequest, ChatResponse, FinishReason, Message, ToolCall, Usage};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub struct AnthropicProtocol;

#[async_trait]
impl Protocol for AnthropicProtocol {
    async fn chat(&self, ctx: &CallContext<'_>, req: &ChatRequest<'_>) -> Result<ChatResponse> {
        let body = AnthropicRequest {
            model: ctx.model,
            // 语义层允许多条 system，Anthropic 只接受一个顶层字符串。
            system: system_prompt(req.messages),
            messages: to_anthropic_messages(req.messages),
            tools: if req.tools.is_empty() {
                None
            } else {
                Some(
                    req.tools
                        .iter()
                        .map(|tool| AnthropicTool {
                            name: &tool.name,
                            description: &tool.description,
                            input_schema: &tool.input_schema,
                        })
                        .collect(),
                )
            },
            temperature: req.temperature,
            // 必填。1024 对带工具/长解释的回复太小，容易在不知情的情况下截断；
            // 需要更严格的上限时由调用方显式传 max_tokens。
            max_tokens: req.max_tokens.unwrap_or(4096),
        };

        // base_url 可能已经带 /v1（很多代理会这样配），messages_url 负责去重。
        // 重试闭包每次重新序列化 body。
        let response = send_with_retry(ctx.max_retries, || {
            ctx.http
                .post(messages_url(ctx.base_url))
                .header("x-api-key", ctx.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
        })
        .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(api_error(status, body));
        }

        let raw = response.text().await?;
        let parsed: AnthropicResponse = parse_response("anthropic", &raw)?;

        // content 是 block 数组：text 可能有多段，tool_use 可以有多个。
        // 纯文本拼接成一条 assistant content；工具调用单独收集。
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in parsed.content {
            match block {
                AnthropicRespBlock::Text { text: value } => text.push_str(&value),
                AnthropicRespBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: input,
                    });
                }
                // 未知 block 丢弃。见文件头说明：extended thinking 需要原样回放。
                AnthropicRespBlock::Unknown => {}
            }
        }

        Ok(ChatResponse {
            content: if text.is_empty() { None } else { Some(text) },
            tool_calls,
            model: parsed.model,
            // Anthropic 没有 total_tokens，自己加；缓存读写字段目前未纳入 Usage。
            usage: parsed.usage.map(|usage| Usage {
                prompt_tokens: usage.input_tokens,
                completion_tokens: usage.output_tokens,
                total_tokens: usage.input_tokens + usage.output_tokens,
            }),
            finish_reason: parsed
                .stop_reason
                .as_deref()
                .map(FinishReason::from_anthropic),
        })
    }
}

/// 接受带或不带 `/v1` 的 base_url。
///
/// 官方地址是 `https://api.anthropic.com`，但代理/网关常把 base_url 配成
/// `.../v1`。直接拼 `/v1/messages` 会得到 `/v1/v1/messages`。
fn messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// 把所有非空 system 消息拼成一个顶层 system。
///
/// 空 content 直接过滤：Anthropic 不接受空的 text block；调用方偶尔会用空字符串
/// 占位。多条 system 用换行连接，保留段落关系。
fn system_prompt(messages: &[Message]) -> Option<String> {
    let mut systems = messages.iter().filter_map(|message| match message {
        Message::System { content } if !content.is_empty() => Some(content.as_str()),
        _ => None,
    });
    let first = systems.next()?;
    let mut system = first.to_string();
    for content in systems {
        system.push('\n');
        system.push_str(content);
    }
    Some(system)
}

/// 语义消息 → Anthropic messages。
///
/// 工具结果不能像 OpenAI 那样每条独立成消息。Anthropic 要求同一个 assistant
/// 回复产生的多个 `tool_result` 放在同一条 user 消息的 content 数组里，所以这里
/// 先缓存连续的 Tool 消息，遇到 User/Assistant 或结尾时再 flush。
fn to_anthropic_messages(messages: &[Message]) -> Vec<AnthropicReqMessage<'_>> {
    let mut result = Vec::new();
    let mut pending_tool_results: Vec<AnthropicReqBlock<'_>> = Vec::new();

    for message in messages {
        match message {
            // system 已经提到顶层，这里跳过，不能作为 message 再发一次。
            Message::System { .. } => {}
            Message::User { content } => {
                flush_pending(&mut pending_tool_results, &mut result);
                result.push(AnthropicReqMessage {
                    role: "user",
                    content: vec![AnthropicReqBlock::Text { text: content }],
                });
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                flush_pending(&mut pending_tool_results, &mut result);
                let mut blocks = Vec::new();
                if let Some(content) = content {
                    if !content.is_empty() {
                        blocks.push(AnthropicReqBlock::Text { text: content });
                    }
                }
                for call in tool_calls {
                    blocks.push(AnthropicReqBlock::ToolUse {
                        id: &call.id,
                        name: &call.name,
                        input: &call.arguments,
                    });
                }
                // 空 assistant 消息没有意义，而且会被 API 拒绝。
                if !blocks.is_empty() {
                    result.push(AnthropicReqMessage {
                        role: "assistant",
                        content: blocks,
                    });
                }
            }
            Message::Tool {
                tool_call_id,
                content,
                is_error,
            } => {
                pending_tool_results.push(AnthropicReqBlock::ToolResult {
                    tool_use_id: tool_call_id,
                    content,
                    is_error: *is_error,
                });
            }
        }
    }
    flush_pending(&mut pending_tool_results, &mut result);
    result
}

/// 把缓存的结果合并成一条 user 消息。`mem::take` 直接拿走 Vec，避免再复制 block。
fn flush_pending<'a>(
    pending: &mut Vec<AnthropicReqBlock<'a>>,
    result: &mut Vec<AnthropicReqMessage<'a>>,
) {
    if !pending.is_empty() {
        result.push(AnthropicReqMessage {
            role: "user",
            content: std::mem::take(pending),
        });
    }
}

// ---------------- 内部请求体 ----------------

/// Anthropic 请求体。`system` 是顶层字符串；`max_tokens` 没有 Option，必填。
#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicReqMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    max_tokens: u32,
}

/// 工具声明。Anthropic 直接使用 `{name, description, input_schema}`，
/// 不像 OpenAI 那样再包一层 function。
#[derive(Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
}

#[derive(Serialize)]
struct AnthropicReqMessage<'a> {
    role: &'static str,
    content: Vec<AnthropicReqBlock<'a>>,
}

/// 请求 content block。借用语义消息里的字符串/JSON，不在转换时复制。
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicReqBlock<'a> {
    Text {
        text: &'a str,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a serde_json::Value,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        /// false 时省略字段；Anthropic 默认就是 false。
        #[serde(skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

// ---------------- 内部响应体 ----------------

#[derive(Deserialize)]
struct AnthropicResponse {
    model: String,
    content: Vec<AnthropicRespBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

/// 响应 content block。
///
/// `#[serde(other)]` 让新增 block 类型不会导致整个响应解析失败；代价是这些块
/// 不会进入历史。启用 extended thinking/server tools 时必须改成保存原始 JSON。
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicRespBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_url_handles_v1_suffix() {
        assert_eq!(
            messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://api.anthropic.com/v1/"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn system_messages_are_collected_and_removed() {
        let messages = vec![
            Message::system("first"),
            Message::user("hello"),
            Message::system("second"),
        ];
        assert_eq!(system_prompt(&messages).as_deref(), Some("first\nsecond"));
        assert_eq!(to_anthropic_messages(&messages).len(), 1);
    }

    #[test]
    fn consecutive_tool_results_are_grouped_into_one_user_message() {
        let messages = vec![
            Message::Assistant {
                content: None,
                tool_calls: vec![
                    ToolCall {
                        id: "call-1".into(),
                        name: "echo".into(),
                        arguments: serde_json::json!({"a": 1}),
                    },
                    ToolCall {
                        id: "call-2".into(),
                        name: "echo".into(),
                        arguments: serde_json::json!({}),
                    },
                ],
            },
            Message::tool_result("call-1", "one", false),
            Message::tool_result("call-2", "two", true),
        ];

        let converted = to_anthropic_messages(&messages);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[1].role, "user");
        assert_eq!(converted[1].content.len(), 2);
        assert!(matches!(
            converted[1].content[1],
            AnthropicReqBlock::ToolResult { is_error: true, .. }
        ));
    }

    #[test]
    fn tool_result_serializes_with_expected_shape() {
        let messages = vec![Message::tool_result("call-1", "boom", true)];
        let value = serde_json::to_value(to_anthropic_messages(&messages)).unwrap();
        assert_eq!(value[0]["role"], "user");
        assert_eq!(value[0]["content"][0]["type"], "tool_result");
        assert_eq!(value[0]["content"][0]["tool_use_id"], "call-1");
        assert_eq!(value[0]["content"][0]["is_error"], true);
    }
}
