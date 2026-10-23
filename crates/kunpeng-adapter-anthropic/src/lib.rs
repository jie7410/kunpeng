//! Anthropic Messages API 适配器。
//!
//! 与 OpenAI 的结构差异都收敛在这里：
//! - system 是顶层字段，不是 messages 里的一条；
//! - user/assistant 的 content 是 block 数组，文本和工具调用可以混排；
//! - 工具结果必须是 user 消息里的 `tool_result` block，并靠 id 与
//!   `tool_use` 配对；同一个 assistant 回复的所有结果要放进同一条 user 消息；
//! - `max_tokens` 必填；开启 extended thinking 时必须大于 `budget_tokens`。
//!
//! Thinking 和服务端工具：
//! - 请求通过 `thinking: {type:"enabled", budget_tokens}` 开启；
//! - 响应的 `thinking` / `redacted_thinking` / 未知 block 会完整存入
//!   `ProviderData`，下一轮按 content 数组原样回放，签名不会丢；
//! - 流式响应额外处理 `thinking_delta` / `signature_delta`；
//! - thinking 文本只用于展示，真正回放依赖原始 block。

use async_trait::async_trait;
use kunpeng_core::error::{Error, InvalidToolArgumentsError, Result};
use kunpeng_core::types::{
    ChatRequest, ChatResponse, Content, ContentPart, FinishReason, ImageSource, Message,
    ProviderData, ServerToolUse, StreamControl, StreamDelta, ToolCall, ToolChoice, Usage,
};
use kunpeng_provider::adapter::{
    api_error, consume_sse, is_event_stream, parse_response, read_body, retry_after,
    send_with_retry, truncate, Adapter, CallContext,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicAdapter;

impl AnthropicAdapter {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Adapter for AnthropicAdapter {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["claude"]
    }

    fn default_base_url(&self) -> &'static str {
        "https://api.anthropic.com"
    }

    fn default_model(&self) -> &'static str {
        "claude-sonnet-4-5"
    }

    async fn chat(&self, ctx: &CallContext<'_>, req: &ChatRequest<'_>) -> Result<ChatResponse> {
        let body = build_body(ctx.model, req, false)?;

        // base_url 可能已经带 /v1（很多代理会这样配），messages_url 负责去重。
        // 重试闭包每次重新序列化 body；timeout 只覆盖“到响应头为止”，
        // body 读取由 read_body 单独限时。
        let response = send_with_retry(ctx.max_retries, ctx.timeout, || {
            ctx.http
                .post(messages_url(ctx.base_url))
                .header("x-api-key", ctx.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
        })
        .await?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after(response.headers());
            let body = read_body(response, ctx.timeout).await.unwrap_or_default();
            return Err(api_error("anthropic", status, body, retry_after));
        }

        let raw = read_body(response, ctx.timeout).await?;
        let parsed: AnthropicResponse = parse_response("anthropic", &raw)?;
        Ok(into_chat_response(parsed, ctx.model))
    }

    /// 流式请求。
    ///
    /// Anthropic 的文本和工具参数是两套 block：
    /// - `text_delta` / `thinking_delta` 直接发 UI；
    /// - `signature_delta` 只进原始 block，用于下一轮回放；
    /// - `input_json_delta` 按 block index 累积，等 `message_stop` 后整体解析。
    async fn chat_stream(
        &self,
        ctx: &CallContext<'_>,
        req: &ChatRequest<'_>,
        on_delta: &mut (dyn for<'a> FnMut(StreamDelta<'a>) -> StreamControl + Send),
    ) -> Result<ChatResponse> {
        let body = build_body(ctx.model, req, true)?;
        let response = send_with_retry(ctx.max_retries, ctx.timeout, || {
            ctx.http
                .post(messages_url(ctx.base_url))
                .header("x-api-key", ctx.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
        })
        .await?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after(response.headers());
            let body = read_body(response, ctx.timeout).await.unwrap_or_default();
            return Err(api_error("anthropic", status, body, retry_after));
        }

        // 代理/兼容端点可能忽略 stream=true，返回普通 JSON。
        if !is_event_stream(&response) {
            let raw = read_body(response, ctx.timeout).await?;
            let parsed: AnthropicResponse = parse_response("anthropic", &raw)?;
            return Ok(into_chat_response(parsed, ctx.model));
        }

        let mut model = None;
        let mut input_tokens = None;
        let mut output_tokens = None;
        let mut stop_reason = None;
        let mut blocks: BTreeMap<usize, StreamBlock> = BTreeMap::new();
        let mut done = false;

        consume_sse(response, ctx.timeout, |event| {
            if done || event.data.is_empty() {
                return Ok(());
            }

            let parsed: AnthropicStreamEvent = parse_response("anthropic", &event.data)?;
            match parsed {
                AnthropicStreamEvent::MessageStart { message } => {
                    if let Some(value) = message.model {
                        if !value.is_empty() {
                            model = Some(value);
                        }
                    }
                    if let Some(usage) = message.usage {
                        input_tokens = usage.input_tokens.or(input_tokens);
                        output_tokens = usage.output_tokens.or(output_tokens);
                    }
                }
                AnthropicStreamEvent::ContentBlockStart {
                    index,
                    content_block,
                } => {
                    let kind = content_block
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    let block = match kind {
                        "text" => {
                            let text = content_block
                                .get("text")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let citations = content_block
                                .get("citations")
                                .and_then(serde_json::Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            if !text.is_empty()
                                && on_delta(StreamDelta::Text(&text)) == StreamControl::Stop
                            {
                                return Err(Error::Cancelled);
                            }
                            StreamBlock::Text { text, citations }
                        }
                        "thinking" => {
                            let thinking = content_block
                                .get("thinking")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let signature = content_block
                                .get("signature")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            if !thinking.is_empty()
                                && on_delta(StreamDelta::Thinking(&thinking)) == StreamControl::Stop
                            {
                                return Err(Error::Cancelled);
                            }
                            StreamBlock::Thinking {
                                thinking,
                                signature,
                            }
                        }
                        "tool_use" => {
                            let id = content_block
                                .get("id")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let name = content_block
                                .get("name")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            if id.is_empty() || name.is_empty() {
                                StreamBlock::Raw {
                                    value: content_block,
                                    partial: String::new(),
                                }
                            } else {
                                StreamBlock::Tool {
                                    id,
                                    name,
                                    initial: content_block
                                        .get("input")
                                        .cloned()
                                        .unwrap_or_else(|| serde_json::json!({})),
                                    partial: String::new(),
                                }
                            }
                        }
                        // redacted_thinking、server_tool_use、web_search_tool_result
                        // 等未知/服务端 block：保留原始 JSON。
                        _ => StreamBlock::Raw {
                            value: content_block,
                            partial: String::new(),
                        },
                    };
                    blocks.insert(index, block);
                }
                AnthropicStreamEvent::ContentBlockDelta { index, delta } => match delta {
                    AnthropicStreamDelta::TextDelta { text } => {
                        if !text.is_empty() {
                            match blocks.entry(index).or_insert_with(|| StreamBlock::Text {
                                text: String::new(),
                                citations: Vec::new(),
                            }) {
                                StreamBlock::Text { text: buffer, .. } => buffer.push_str(&text),
                                // 类型不一致说明服务端违反了事件顺序；保留最终文本
                                // 比 panic 更安全，这里直接忽略这个分片。
                                _ => return Ok(()),
                            }
                            if on_delta(StreamDelta::Text(&text)) == StreamControl::Stop {
                                return Err(Error::Cancelled);
                            }
                        }
                    }
                    AnthropicStreamDelta::CitationsDelta { citation } => {
                        if let Some(StreamBlock::Text { citations, .. }) = blocks.get_mut(&index) {
                            citations.push(citation);
                        }
                    }
                    AnthropicStreamDelta::ThinkingDelta { thinking } => {
                        if !thinking.is_empty() {
                            if let Some(StreamBlock::Thinking {
                                thinking: buffer, ..
                            }) = blocks.get_mut(&index)
                            {
                                buffer.push_str(&thinking);
                                if on_delta(StreamDelta::Thinking(&thinking)) == StreamControl::Stop
                                {
                                    return Err(Error::Cancelled);
                                }
                            }
                        }
                    }
                    AnthropicStreamDelta::SignatureDelta { signature } => {
                        if let Some(StreamBlock::Thinking {
                            signature: buffer, ..
                        }) = blocks.get_mut(&index)
                        {
                            buffer.push_str(&signature);
                        }
                    }
                    AnthropicStreamDelta::InputJsonDelta { partial_json } => {
                        match blocks.get_mut(&index) {
                            Some(StreamBlock::Tool { partial, .. })
                            | Some(StreamBlock::Raw { partial, .. }) => {
                                partial.push_str(&partial_json);
                            }
                            _ => {}
                        }
                    }
                    AnthropicStreamDelta::Unknown => {}
                },
                AnthropicStreamEvent::ContentBlockStop => {}
                AnthropicStreamEvent::MessageDelta { delta, usage } => {
                    if let Some(reason) = delta.stop_reason {
                        stop_reason = Some(reason);
                    }
                    if let Some(usage) = usage {
                        input_tokens = usage.input_tokens.or(input_tokens);
                        output_tokens = usage.output_tokens.or(output_tokens);
                    }
                }
                AnthropicStreamEvent::MessageStop => {
                    done = true;
                }
                AnthropicStreamEvent::Ping | AnthropicStreamEvent::Unknown => {}
                AnthropicStreamEvent::Error { error } => {
                    return Err(Error::Other(format!("Anthropic 流错误: {}", error.message)));
                }
            }
            Ok(())
        })
        .await?;

        let mut text = String::new();
        let mut thinking = String::new();
        let mut citations = Vec::new();
        let mut tool_calls = Vec::new();
        let mut raw_blocks = Vec::new();
        for block in blocks.into_values() {
            match block {
                StreamBlock::Text {
                    text: buffer,
                    citations: block_citations,
                } => {
                    text.push_str(&buffer);
                    citations.extend(block_citations.iter().cloned());
                    let mut raw = serde_json::json!({
                        "type": "text",
                        "text": &buffer,
                    });
                    if !block_citations.is_empty() {
                        raw["citations"] = serde_json::Value::Array(block_citations);
                    }
                    raw_blocks.push(raw);
                }
                StreamBlock::Thinking {
                    thinking: value,
                    signature,
                } => {
                    thinking.push_str(&value);
                    raw_blocks.push(serde_json::json!({
                        "type": "thinking",
                        "thinking": &value,
                        "signature": &signature,
                    }));
                }
                StreamBlock::Tool {
                    id,
                    name,
                    initial,
                    partial,
                } => {
                    let arguments = if partial.trim().is_empty() {
                        if initial.is_null() {
                            serde_json::json!({})
                        } else {
                            initial
                        }
                    } else {
                        parse_stream_arguments(&name, &partial)?
                    };
                    raw_blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": &id,
                        "name": &name,
                        "input": &arguments,
                    }));
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
                StreamBlock::Raw { mut value, partial } => {
                    // server_tool_use 等未知 block 也可能用 input_json_delta 传参数。
                    if !partial.trim().is_empty() {
                        let tool = value
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("server_tool_use");
                        value["input"] = parse_stream_arguments(tool, &partial)?;
                    }
                    raw_blocks.push(value);
                }
            }
        }

        let usage = match (input_tokens, output_tokens) {
            (Some(input), Some(output)) => Some(Usage::new(input, output)),
            (Some(input), None) => Some(Usage::new(input, 0)),
            _ => None,
        };

        let server_tool_uses = project_server_tool_uses(&raw_blocks);
        let provider_data = if raw_blocks.is_empty() {
            None
        } else {
            Some(ProviderData::new(
                "anthropic",
                serde_json::Value::Array(raw_blocks),
            ))
        };

        let mut response = ChatResponse::new(model.unwrap_or_else(|| ctx.model.to_string()));
        response.content = if text.is_empty() { None } else { Some(text) };
        response.tool_calls = tool_calls;
        response.usage = usage;
        response.finish_reason = stop_reason.as_deref().map(finish_reason);
        response.thinking = if thinking.is_empty() {
            None
        } else {
            Some(thinking)
        };
        response.citations = citations;
        response.server_tool_uses = server_tool_uses;
        response.provider_data = provider_data;
        Ok(response)
    }
}

/// 普通响应 JSON → 统一 `ChatResponse`。非流式和“服务端忽略 stream”的回退共用。
fn into_chat_response(parsed: AnthropicResponse, fallback_model: &str) -> ChatResponse {
    let AnthropicResponse {
        model,
        content,
        stop_reason,
        usage,
    } = parsed;

    // content 是 block 数组：text/thinking 可能有多段，tool_use 可以有多个。
    // 文案和思考内容拼接给 UI；原始数组整体存进 provider_data，下一轮请求原样回放。
    let mut text = String::new();
    let mut thinking = String::new();
    let mut citations = Vec::new();
    let mut tool_calls = Vec::new();
    for block in &content {
        match block.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => {
                if let Some(value) = block.get("text").and_then(serde_json::Value::as_str) {
                    text.push_str(value);
                }
                if let Some(items) = block.get("citations").and_then(serde_json::Value::as_array) {
                    citations.extend(items.iter().cloned());
                }
            }
            Some("thinking") => {
                if let Some(value) = block.get("thinking").and_then(serde_json::Value::as_str) {
                    thinking.push_str(value);
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let name = block
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if !id.is_empty() && !name.is_empty() {
                    tool_calls.push(ToolCall {
                        id: id.to_string(),
                        name: name.to_string(),
                        arguments: block
                            .get("input")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({})),
                    });
                }
            }
            // redacted_thinking、server_tool_use、web_search_tool_result 等 block
            // 不进入本地工具循环，但会完整保留在 provider_data 里。
            _ => {}
        }
    }

    // 服务端工具只做只读投影：server_tool_use 按 id 与 *_tool_result 配对。
    let server_tool_uses = project_server_tool_uses(&content);

    let provider_data = if content.is_empty() {
        None
    } else {
        Some(ProviderData::new(
            "anthropic",
            serde_json::Value::Array(content),
        ))
    };

    let mut response = ChatResponse::new(if model.is_empty() {
        fallback_model.to_string()
    } else {
        model
    });
    response.content = if text.is_empty() { None } else { Some(text) };
    response.tool_calls = tool_calls;
    // Anthropic 没有 total_tokens，自己加；缓存读写字段目前未纳入 Usage。
    response.usage = usage.map(|usage| Usage::new(usage.input_tokens, usage.output_tokens));
    response.finish_reason = stop_reason.as_deref().map(finish_reason);
    response.thinking = if thinking.is_empty() {
        None
    } else {
        Some(thinking)
    };
    response.citations = citations;
    response.server_tool_uses = server_tool_uses;
    response.provider_data = provider_data;
    response
}

/// 从原始 content 中提取服务端工具调用，并按 `tool_use_id` 关联 `*_tool_result`。
/// 只用于展示；Agent 不会把返回值放进本地 `tool_calls`。
fn project_server_tool_uses(content: &[serde_json::Value]) -> Vec<ServerToolUse> {
    let mut server_tool_uses: Vec<ServerToolUse> = content
        .iter()
        .filter(|block| {
            block.get("type").and_then(serde_json::Value::as_str) == Some("server_tool_use")
        })
        .filter_map(|block| {
            let id = block.get("id")?.as_str()?;
            let name = block.get("name")?.as_str()?;
            Some(ServerToolUse::new(
                id,
                name,
                block
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
                None,
            ))
        })
        .collect();

    for block in content {
        let Some(kind) = block.get("type").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !kind.ends_with("_tool_result") {
            continue;
        }
        let Some(id) = block.get("tool_use_id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if let Some(server_tool_use) = server_tool_uses
            .iter_mut()
            .find(|server_tool_use| server_tool_use.id == id)
        {
            server_tool_use.result = Some(block.clone());
        }
    }
    server_tool_uses
}

/// 构造请求体。非流式不发送 `stream` 字段，避免兼容代理对多余字段敏感。
fn build_body<'a>(
    model: &'a str,
    req: &'a ChatRequest<'a>,
    stream: bool,
) -> Result<AnthropicRequest<'a>> {
    let max_tokens = req.max_tokens.unwrap_or(4096);
    let thinking = match req.thinking {
        None => None,
        Some(config) => {
            if config.budget_tokens < 1024 {
                return Err(Error::InvalidConfig(
                    "Anthropic thinking.budget_tokens 不能小于 1024".into(),
                ));
            }
            if config.budget_tokens >= max_tokens {
                return Err(Error::InvalidConfig(
                    "Anthropic thinking.budget_tokens 必须小于 max_tokens".into(),
                ));
            }
            if let Some(temperature) = req.temperature {
                if (temperature - 1.0).abs() > f32::EPSILON {
                    return Err(Error::InvalidConfig(
                        "Anthropic 开启 thinking 时 temperature 必须为 1".into(),
                    ));
                }
            }
            Some(AnthropicThinking {
                r#type: "enabled",
                budget_tokens: config.budget_tokens,
            })
        }
    };

    Ok(AnthropicRequest {
        model,
        // 语义层允许多条 system，Anthropic 只接受一个顶层字符串。
        system: system_prompt(req.messages),
        messages: to_anthropic_messages(req.messages)?,
        tools: {
            let mut tools: Vec<AnthropicToolEntry<'_>> = req
                .tools
                .iter()
                .map(|tool| {
                    AnthropicToolEntry::Local(AnthropicTool {
                        name: &tool.name,
                        description: &tool.description,
                        input_schema: &tool.input_schema,
                    })
                })
                .collect();
            // 服务端工具定义原样透传；它们不进入 ToolRegistry，也不会被本地执行。
            tools.extend(req.server_tools.iter().map(AnthropicToolEntry::Server));
            if tools.is_empty() {
                None
            } else {
                Some(tools)
            }
        },
        temperature: req.temperature,
        tool_choice: req.tool_choice.map(to_anthropic_tool_choice),
        stop_sequences: if req.stop_sequences.is_empty() {
            None
        } else {
            Some(req.stop_sequences)
        },
        // 必填。1024 对带工具/长解释的回复太小，容易在不知情的情况下截断；
        // 需要更严格的上限时由调用方显式传 max_tokens。
        max_tokens,
        thinking,
        stream: stream.then_some(true),
        extra: req
            .provider_options
            .and_then(|options| options.anthropic.clone())
            .unwrap_or_default(),
    })
}

/// 语义 ToolChoice → Anthropic 结构。
fn finish_reason(value: &str) -> FinishReason {
    match value {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" => FinishReason::Length,
        "refusal" => FinishReason::ContentFilter,
        "pause_turn" => FinishReason::Pause,
        other => FinishReason::Other(other.to_string()),
    }
}

fn to_anthropic_tool_choice(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Auto => serde_json::json!({"type": "auto"}),
        ToolChoice::None => serde_json::json!({"type": "none"}),
        ToolChoice::Required => serde_json::json!({"type": "any"}),
        ToolChoice::Tool { name } => serde_json::json!({"type": "tool", "name": name}),
        _ => serde_json::json!({"type": "auto"}),
    }
}

/// 解析累积后的工具参数 JSON。错误里保留片段，和 OpenAI 适配器保持一致。
fn parse_stream_arguments(tool: &str, raw: &str) -> Result<serde_json::Value> {
    serde_json::from_str(raw).map_err(|source| {
        Error::InvalidToolArguments(Box::new(InvalidToolArgumentsError::new(
            tool.to_string(),
            truncate(raw, 256),
            source,
        )))
    })
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
fn to_anthropic_messages(messages: &[Message]) -> Result<Vec<AnthropicReqMessage<'_>>> {
    let mut result = Vec::new();
    let mut pending_tool_results: Vec<AnthropicReqBlock<'_>> = Vec::new();

    for message in messages {
        match message {
            // system 已经提到顶层，这里跳过，不能作为 message 再发一次。
            Message::System { .. } => {}
            Message::User { content } => {
                flush_pending(&mut pending_tool_results, &mut result);
                let blocks = content_to_anthropic_blocks(content)?;
                if blocks.is_empty() {
                    return Err(Error::InvalidConfig(
                        "Anthropic user 消息内容不能为空".into(),
                    ));
                }
                result.push(AnthropicReqMessage {
                    role: "user",
                    content: blocks,
                });
            }
            Message::Assistant {
                content,
                tool_calls,
                provider_data,
                ..
            } => {
                flush_pending(&mut pending_tool_results, &mut result);

                // Anthropic 的 thinking/signature、redacted_thinking、服务端工具
                // 必须原样回传。有 provider_data 时直接使用完整 content 数组，
                // 不再根据语义字段重建，避免 block 顺序或签名丢失。
                if let Some(blocks) = provider_data
                    .as_ref()
                    .filter(|data| data.provider == "anthropic")
                    .and_then(|data| data.data.as_array())
                {
                    if !blocks.is_empty() {
                        result.push(AnthropicReqMessage {
                            role: "assistant",
                            content: blocks.iter().map(AnthropicReqBlock::Raw).collect(),
                        });
                        continue;
                    }
                }

                let mut blocks = Vec::new();
                if let Some(content) = content {
                    if !content.is_empty() {
                        blocks.push(AnthropicReqBlock::Structured(
                            AnthropicReqBlockInner::Text { text: content },
                        ));
                    }
                }
                for call in tool_calls {
                    blocks.push(AnthropicReqBlock::Structured(
                        AnthropicReqBlockInner::ToolUse {
                            id: &call.id,
                            name: &call.name,
                            input: &call.arguments,
                        },
                    ));
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
                pending_tool_results.push(AnthropicReqBlock::Structured(
                    AnthropicReqBlockInner::ToolResult {
                        tool_use_id: tool_call_id,
                        content,
                        is_error: *is_error,
                    },
                ));
            }
            _ => return Err(Error::InvalidConfig("Anthropic 不支持该消息类型".into())),
        }
    }
    flush_pending(&mut pending_tool_results, &mut result);
    Ok(result)
}

/// 语义 content → Anthropic content block。
///
/// `Image` 结构化映射；`Other` 只允许 Anthropic 原始块；OpenAI 的 part 不会误发。
fn content_to_anthropic_blocks(content: &Content) -> Result<Vec<AnthropicReqBlock<'_>>> {
    match content {
        Content::Text(text) => {
            if text.is_empty() {
                return Ok(Vec::new());
            }
            Ok(vec![AnthropicReqBlock::Structured(
                AnthropicReqBlockInner::Text { text },
            )])
        }
        Content::Parts(parts) => {
            let mut blocks = Vec::with_capacity(parts.len());
            for part in parts {
                match part {
                    ContentPart::Text { text } => {
                        if !text.is_empty() {
                            blocks.push(AnthropicReqBlock::Structured(
                                AnthropicReqBlockInner::Text { text },
                            ));
                        }
                    }
                    ContentPart::Image { source, .. } => {
                        let source = match source {
                            ImageSource::Base64 { media_type, data } => {
                                AnthropicImageSource::Base64 { media_type, data }
                            }
                            ImageSource::Url { url } => AnthropicImageSource::Url { url },
                            _ => {
                                return Err(Error::InvalidConfig("Anthropic 不支持该图片源".into()))
                            }
                        };
                        blocks.push(AnthropicReqBlock::Structured(
                            AnthropicReqBlockInner::Image { source },
                        ));
                    }
                    ContentPart::Other { provider, data } if provider == "anthropic" => {
                        blocks.push(AnthropicReqBlock::Raw(data));
                    }
                    ContentPart::Other { provider, .. } => {
                        return Err(Error::InvalidConfig(format!(
                            "Anthropic 不支持 {provider} 内容块"
                        )));
                    }
                    _ => return Err(Error::InvalidConfig("Anthropic 不支持未知内容块".into())),
                }
            }
            Ok(blocks)
        }
        _ => Err(Error::InvalidConfig("Anthropic 不支持该内容类型".into())),
    }
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

// ============================================================================
// Adapter Internal: Anthropic wire format
//
// 以下请求/响应/流式类型只用于本适配器的 serde 转换，不从 crate 导出，也不
// 属于稳定 API。Anthropic block 结构变化时可以直接调整；调用方只应依赖
// `Adapter` trait 和 core 的语义类型。
// ============================================================================

/// Anthropic 请求体。`system` 是顶层字符串；`max_tokens` 没有 Option，必填。
#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicReqMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicToolEntry<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<&'a [String]>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    /// 非流式请求省略；流式时显式传 true。
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize)]
struct AnthropicThinking {
    r#type: &'static str,
    budget_tokens: u32,
}

/// 工具声明。Anthropic 直接使用 `{name, description, input_schema}`，
/// 不像 OpenAI 那样再包一层 function。
#[derive(Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
}

/// 本地函数工具和服务端工具共用一个数组。服务端工具带 `type` 字段，直接透传。
#[derive(Serialize)]
#[serde(untagged)]
enum AnthropicToolEntry<'a> {
    Local(AnthropicTool<'a>),
    Server(&'a serde_json::Value),
}

#[derive(Serialize)]
struct AnthropicReqMessage<'a> {
    role: &'static str,
    content: Vec<AnthropicReqBlock<'a>>,
}

/// 请求 content block。语义字段借用消息里的字符串/JSON；`Raw` 直接透传 provider
/// 原始 block，用于 thinking/signature 和未知服务端工具。
#[derive(Serialize)]
#[serde(untagged)]
enum AnthropicReqBlock<'a> {
    Structured(AnthropicReqBlockInner<'a>),
    Raw(&'a serde_json::Value),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicReqBlockInner<'a> {
    Text {
        text: &'a str,
    },
    Image {
        source: AnthropicImageSource<'a>,
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

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicImageSource<'a> {
    Base64 { media_type: &'a str, data: &'a str },
    Url { url: &'a str },
}

fn is_false(value: &bool) -> bool {
    !*value
}

// ---------------- 内部响应体 ----------------

#[derive(Deserialize)]
struct AnthropicResponse {
    model: String,
    /// 保留原始 block 数组，未知类型也能下一轮原样回传。
    content: Vec<serde_json::Value>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

/// 流式 content block 的累积状态。
enum StreamBlock {
    Text {
        text: String,
        citations: Vec<serde_json::Value>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    Tool {
        id: String,
        name: String,
        /// `content_block_start` 里通常是一个空对象，作为没有分片时的兜底。
        initial: serde_json::Value,
        /// `input_json_delta.partial_json` 的拼接结果。
        partial: String,
    },
    /// redacted_thinking、server_tool_use、web_search_tool_result 等未知 block。
    /// 保留 start 原始 JSON；如果后续有 input_json_delta，再合并到 input。
    Raw {
        value: serde_json::Value,
        partial: String,
    },
}

/// Anthropic SSE 事件。`event:` 字段和 JSON 里的 `type` 一致，这里只信 JSON，
/// 避免把心跳/未知事件误判。
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicStreamEvent {
    MessageStart {
        message: AnthropicStreamMessage,
    },
    ContentBlockStart {
        index: usize,
        content_block: serde_json::Value,
    },
    ContentBlockDelta {
        index: usize,
        delta: AnthropicStreamDelta,
    },
    ContentBlockStop,
    MessageDelta {
        #[serde(default)]
        delta: AnthropicStreamMessageDelta,
        #[serde(default)]
        usage: Option<AnthropicStreamUsage>,
    },
    MessageStop,
    Ping,
    Error {
        error: AnthropicStreamError,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct AnthropicStreamMessage {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<AnthropicStreamUsage>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicStreamDelta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    CitationsDelta {
        citation: serde_json::Value,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct AnthropicStreamMessageDelta {
    stop_reason: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct AnthropicStreamUsage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct AnthropicStreamError {
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_defaults_and_aliases() {
        let adapter = AnthropicAdapter::new();
        assert_eq!(adapter.name(), "anthropic");
        assert_eq!(adapter.aliases(), &["claude"]);
        assert_eq!(adapter.default_base_url(), "https://api.anthropic.com");
        assert_eq!(adapter.default_model(), "claude-sonnet-4-5");
    }

    #[test]
    fn normalizes_anthropic_finish_reasons() {
        assert_eq!(finish_reason("tool_use"), FinishReason::ToolCalls);
        assert_eq!(finish_reason("max_tokens"), FinishReason::Length);
        assert_eq!(finish_reason("pause_turn"), FinishReason::Pause);
        assert_eq!(
            finish_reason("something_new"),
            FinishReason::Other("something_new".into())
        );
    }

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
        assert_eq!(to_anthropic_messages(&messages).unwrap().len(), 1);
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
                thinking: None,
                citations: vec![],
                server_tool_uses: vec![],
                provider_data: None,
            },
            Message::tool_result("call-1", "one", false),
            Message::tool_result("call-2", "two", true),
        ];

        let converted = to_anthropic_messages(&messages).unwrap();
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[1].role, "user");
        assert_eq!(converted[1].content.len(), 2);
        assert!(matches!(
            &converted[1].content[1],
            AnthropicReqBlock::Structured(AnthropicReqBlockInner::ToolResult {
                is_error: true,
                ..
            })
        ));
    }

    #[test]
    fn tool_result_serializes_with_expected_shape() {
        let messages = vec![Message::tool_result("call-1", "boom", true)];
        let value = serde_json::to_value(to_anthropic_messages(&messages).unwrap()).unwrap();
        assert_eq!(value[0]["role"], "user");
        assert_eq!(value[0]["content"][0]["type"], "tool_result");
        assert_eq!(value[0]["content"][0]["tool_use_id"], "call-1");
        assert_eq!(value[0]["content"][0]["is_error"], true);
    }

    #[test]
    fn thinking_blocks_are_exposed_and_preserved() {
        let parsed = AnthropicResponse {
            model: "m".into(),
            content: vec![
                serde_json::json!({
                    "type": "thinking",
                    "thinking": "先查天气",
                    "signature": "sig-1",
                }),
                serde_json::json!({
                    "type": "redacted_thinking",
                    "data": "opaque",
                }),
                serde_json::json!({
                    "type": "text",
                    "text": "答案",
                }),
            ],
            stop_reason: Some("end_turn".into()),
            usage: None,
        };

        let response = into_chat_response(parsed, "fallback");
        assert_eq!(response.thinking.as_deref(), Some("先查天气"));
        assert_eq!(response.content.as_deref(), Some("答案"));
        let data = response.provider_data.expect("raw blocks should be kept");
        assert_eq!(data.provider, "anthropic");
        assert_eq!(data.data.as_array().unwrap().len(), 3);
        assert_eq!(data.data[0]["signature"], "sig-1");
        assert_eq!(data.data[1]["type"], "redacted_thinking");
    }

    #[test]
    fn assistant_provider_data_is_replayed_verbatim() {
        let messages = vec![Message::Assistant {
            content: Some("答案".into()),
            tool_calls: vec![],
            thinking: Some("先查天气".into()),
            citations: vec![],
            server_tool_uses: vec![],
            provider_data: Some(ProviderData::new(
                "anthropic",
                serde_json::json!([
                    {"type": "thinking", "thinking": "先查天气", "signature": "sig-1"},
                    {"type": "text", "text": "答案"}
                ]),
            )),
        }];

        let value = serde_json::to_value(to_anthropic_messages(&messages).unwrap()).unwrap();
        assert_eq!(value[0]["content"][0]["type"], "thinking");
        assert_eq!(value[0]["content"][0]["signature"], "sig-1");
        assert_eq!(value[0]["content"][1]["type"], "text");
        assert_eq!(value[0]["content"][1]["text"], "答案");
    }

    #[test]
    fn thinking_request_uses_budget_and_stream() {
        use kunpeng_core::types::ThinkingConfig;

        let messages = vec![Message::user("hi")];
        let request = ChatRequest::new(&messages, &[])
            .with_max_tokens(4096)
            .with_thinking(ThinkingConfig::new(2048));

        let body = build_body("m", &request, true).unwrap();
        let value = serde_json::to_value(body).unwrap();
        assert_eq!(value["thinking"]["type"], "enabled");
        assert_eq!(value["thinking"]["budget_tokens"], 2048);
        assert_eq!(value["stream"], true);
    }

    #[test]
    fn invalid_thinking_budget_is_rejected() {
        use kunpeng_core::types::ThinkingConfig;

        let messages = vec![Message::user("hi")];
        let request = ChatRequest::new(&messages, &[])
            .with_max_tokens(2048)
            .with_thinking(ThinkingConfig::new(2048));
        assert!(build_body("m", &request, false).is_err());
    }

    #[test]
    fn citations_and_server_tools_are_projected() {
        let parsed = AnthropicResponse {
            model: "m".into(),
            content: vec![
                serde_json::json!({
                    "type": "text",
                    "text": "答案",
                    "citations": [{
                        "type": "web_search_result_location",
                        "url": "https://example.com",
                        "title": "Example",
                        "cited_text": "引用"
                    }],
                }),
                serde_json::json!({
                    "type": "server_tool_use",
                    "id": "srv-1",
                    "name": "web_search",
                    "input": {"query": "rust"},
                }),
                serde_json::json!({
                    "type": "web_search_tool_result",
                    "tool_use_id": "srv-1",
                    "content": [{"type": "web_search_result", "url": "https://example.com"}],
                }),
            ],
            stop_reason: Some("end_turn".into()),
            usage: None,
        };

        let response = into_chat_response(parsed, "fallback");
        assert_eq!(response.content.as_deref(), Some("答案"));
        assert_eq!(response.citations.len(), 1);
        assert_eq!(response.citations[0]["url"], "https://example.com");

        assert_eq!(response.server_tool_uses.len(), 1);
        let server_tool_use = &response.server_tool_uses[0];
        assert_eq!(server_tool_use.id, "srv-1");
        assert_eq!(server_tool_use.name, "web_search");
        assert!(server_tool_use.result.is_some());
        // 服务端工具绝不能进入本地 tool_calls。
        assert!(response.tool_calls.is_empty());
    }

    #[test]
    fn server_tools_are_merged_into_request_tools() {
        use kunpeng_core::types::ToolDefinition;

        let messages = vec![Message::user("hi")];
        let local = vec![ToolDefinition {
            name: "echo".into(),
            description: "echo".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let server = vec![serde_json::json!({
            "type": "web_search_20250305",
            "name": "web_search",
            "max_uses": 1,
        })];
        let request = ChatRequest::new(&messages, &local).with_server_tools(&server);

        let value = serde_json::to_value(build_body("m", &request, false).unwrap()).unwrap();
        let tools = value["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "echo");
        assert_eq!(tools[1]["type"], "web_search_20250305");
        assert_eq!(tools[1]["max_uses"], 1);
    }

    #[test]
    fn serializes_multimodal_parts_and_request_extensions() {
        use kunpeng_core::types::{ContentPart, ImageSource, ProviderOptions, ToolChoice};

        let messages = vec![Message::user_parts(vec![
            ContentPart::Text {
                text: "看图".into(),
            },
            ContentPart::Image {
                source: ImageSource::Url {
                    url: "https://example.com/a.png".into(),
                },
                detail: None,
            },
        ])];
        let choice = ToolChoice::Required;
        let stop = vec!["END".to_string()];
        let mut extra = serde_json::Map::new();
        extra.insert("top_k".into(), serde_json::json!(5));
        let options = ProviderOptions::default().with_anthropic(extra);
        let request = ChatRequest::new(&messages, &[])
            .with_tool_choice(&choice)
            .with_stop_sequences(&stop)
            .with_provider_options(&options);

        let value = serde_json::to_value(build_body("m", &request, false).unwrap()).unwrap();
        assert_eq!(value["messages"][0]["content"][0]["type"], "text");
        assert_eq!(value["messages"][0]["content"][1]["type"], "image");
        assert_eq!(value["messages"][0]["content"][1]["source"]["type"], "url");
        assert_eq!(
            value["messages"][0]["content"][1]["source"]["url"],
            "https://example.com/a.png"
        );
        assert_eq!(value["tool_choice"]["type"], "any");
        assert_eq!(value["stop_sequences"][0], "END");
        assert_eq!(value["top_k"], 5);
    }

    // ========================================================================
    // wire 级测试：用本地 TCP 服务器覆盖完整的 HTTP 请求路径（URL、鉴权头、
    // 错误码映射、SSE 全链路）。纯函数测试无法覆盖这些行为。
    // ========================================================================

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    /// 起一个只服务一次请求的 HTTP 服务器，返回 (base_url, 请求头 + 请求体)。
    fn serve_once(
        status: &'static str,
        content_type: &'static str,
        response: String,
    ) -> (String, thread::JoinHandle<(String, serde_json::Value)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                data.extend_from_slice(&buffer[..read]);
                let Some(header_end) = data.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&data[..header_end + 4]).to_string();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                    })
                    .unwrap_or(0);
                if data.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let header_end = data
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            let headers = String::from_utf8_lossy(&data[..header_end]).to_string();
            let body: serde_json::Value = serde_json::from_slice(&data[header_end..]).unwrap();
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
            (headers, body)
        });
        (format!("http://{address}"), handle)
    }

    fn context<'a>(base_url: &'a str, http: &'a reqwest::Client) -> CallContext<'a> {
        CallContext {
            http,
            base_url,
            api_key: "test-key",
            model: "claude-test",
            timeout: Duration::from_secs(5),
            max_retries: 0,
        }
    }

    #[tokio::test]
    async fn chat_sends_headers_and_maps_blocks() {
        let body = r#"{
            "model": "claude-test",
            "content": [
                {"type": "thinking", "thinking": "先想一想", "signature": "sig-1"},
                {"type": "text", "text": "答案"},
                {"type": "tool_use", "id": "t-1", "name": "echo", "input": {"a": 1}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }"#
        .to_string();
        let (base, server) = serve_once("200 OK", "application/json", body);

        let http = reqwest::Client::new();
        let messages = vec![Message::user("hi")];
        let request = ChatRequest::new(&messages, &[]);
        let response = AnthropicAdapter::new()
            .chat(&context(&base, &http), &request)
            .await
            .unwrap();

        assert_eq!(response.thinking.as_deref(), Some("先想一想"));
        assert_eq!(response.content.as_deref(), Some("答案"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "t-1");
        assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
        let usage = response.usage.unwrap();
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (10, 5));
        let data = response.provider_data.expect("原始 block 应被保留");
        assert_eq!(data.data[0]["signature"], "sig-1");

        let (headers, request_body) = server.join().unwrap();
        let headers = headers.to_ascii_lowercase();
        assert!(headers.contains("post /v1/messages"));
        assert!(headers.contains("x-api-key: test-key"));
        assert!(headers.contains("anthropic-version: 2023-06-01"));
        assert_eq!(request_body["model"], "claude-test");
        assert_eq!(request_body["messages"][0]["content"][0]["text"], "hi");
    }

    #[tokio::test]
    async fn chat_maps_5xx_to_api_error() {
        let (base, server) = serve_once(
            "500 Internal Server Error",
            "application/json",
            r#"{"type":"error","error":{"message":"boom"}}"#.to_string(),
        );

        let http = reqwest::Client::new();
        let messages = vec![Message::user("hi")];
        let request = ChatRequest::new(&messages, &[]);
        let error = AnthropicAdapter::new()
            .chat(&context(&base, &http), &request)
            .await
            .unwrap_err();

        match error {
            Error::Api { status, .. } => assert_eq!(status, 500),
            other => panic!("应当映射为 Api: {other:?}"),
        }
        let _ = server.join();
    }

    #[tokio::test]
    async fn stream_replays_thinking_and_accumulates_tool_input() {
        let sse = concat!(
            r#"data: {"type":"message_start","message":{"model":"claude-test","usage":{"input_tokens":10}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"想"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"一想"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-1"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"答案"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t-1","name":"echo","input":{}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"1}"}}"#,
            "\n\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
            "\n\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );
        let (base, server) = serve_once("200 OK", "text/event-stream", sse.to_string());

        let http = reqwest::Client::new();
        let messages = vec![Message::user("hi")];
        let request = ChatRequest::new(&messages, &[]);
        let mut deltas = Vec::new();
        let response = AnthropicAdapter::new()
            .chat_stream(&context(&base, &http), &request, &mut |delta| {
                match delta {
                    StreamDelta::Text(text) => deltas.push(format!("text:{text}")),
                    StreamDelta::Thinking(text) => deltas.push(format!("thinking:{text}")),
                    _ => {}
                }
                StreamControl::Continue
            })
            .await
            .unwrap();

        assert_eq!(deltas, vec!["thinking:想", "thinking:一想", "text:答案"]);
        assert_eq!(response.thinking.as_deref(), Some("想一想"));
        assert_eq!(response.content.as_deref(), Some("答案"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(
            response.tool_calls[0].arguments,
            serde_json::json!({"a": 1})
        );
        assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
        let usage = response.usage.unwrap();
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (10, 5));
        // signature 必须进入原始 block，下一轮请求才能原样回放。
        let data = response.provider_data.expect("原始 block 应被保留");
        assert_eq!(data.data[0]["type"], "thinking");
        assert_eq!(data.data[0]["signature"], "sig-1");
        assert_eq!(data.data[2]["type"], "tool_use");
        assert_eq!(data.data[2]["input"]["a"], 1);

        let (_, request_body) = server.join().unwrap();
        assert_eq!(request_body["stream"], true);
    }
}
