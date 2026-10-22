//! ReAct / 工具循环策略和 Agent 运行时门面。

use crate::{AgentStrategy, RunContext};
use async_trait::async_trait;
use kunpeng_context::ContextBuilder;
use kunpeng_core::types::{
    ChatRequest, FinishReason, Message, StreamControl, StreamDelta, ToolCall,
};
use kunpeng_core::{ChatModel, Result};
use kunpeng_memory::SessionStore;
use kunpeng_runtime::{
    emit, AgentEvent, AgentHooks, CancellationToken, RunConfig, StopReason, ToolDecision,
    ToolExecution, ToolOutcome,
};
use kunpeng_tools::ToolRegistry;
use std::{sync::Arc, time::Duration};

/// Agent 门面：持有模型、工具、上下文、策略和取消令牌。
///
/// 具体行为由 `AgentStrategy` 决定；默认使用 `ReActAgent`。
pub struct Agent {
    model: Arc<dyn ChatModel>,
    tools: ToolRegistry,
    config: RunConfig,
    context: Arc<dyn ContextBuilder>,
    strategy: Box<dyn AgentStrategy>,
    cancel: CancellationToken,
    session_store: Option<Arc<dyn SessionStore>>,
    session_id: Option<String>,
}

impl Agent {
    /// 使用默认 ReAct 策略。`Model` 实现了 `ChatModel`，可以直接传入。
    pub fn new(model: impl ChatModel + 'static, tools: ToolRegistry, config: RunConfig) -> Self {
        Self::with_strategy(Arc::new(model), tools, config, Box::new(ReActAgent::new()))
    }

    /// 共享模型并注入自定义策略，供 Plan-Execute/Router 等使用。
    pub fn with_strategy(
        model: Arc<dyn ChatModel>,
        tools: ToolRegistry,
        config: RunConfig,
        strategy: Box<dyn AgentStrategy>,
    ) -> Self {
        Self {
            model,
            tools,
            config,
            context: Arc::new(kunpeng_context::SlidingWindowContext::default()),
            strategy,
            cancel: CancellationToken::new(),
            session_store: None,
            session_id: None,
        }
    }

    /// 替换上下文构建器（摘要、RAG 注入等后续实现）。
    pub fn with_context(mut self, context: Arc<dyn ContextBuilder>) -> Self {
        self.context = context;
        self
    }

    pub fn with_history(
        model: impl ChatModel + 'static,
        tools: ToolRegistry,
        config: RunConfig,
        history: Vec<Message>,
    ) -> Self {
        let mut agent = Self::new(model, tools, config);
        agent.set_history(history);
        agent
    }

    pub fn history(&self) -> &[Message] {
        self.strategy.history()
    }

    pub fn set_history(&mut self, history: Vec<Message>) {
        let target = self.strategy.history_mut();
        target.clear();
        target.extend(history);
    }

    pub fn push(&mut self, message: Message) {
        self.strategy.history_mut().push(message);
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// 绑定会话存储与 session id。构造后调用 `load_session()` 恢复历史。
    pub fn with_session_store(
        mut self,
        store: Arc<dyn SessionStore>,
        session_id: impl Into<String>,
    ) -> Self {
        self.session_store = Some(store);
        self.session_id = Some(session_id.into());
        self
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// 从绑定的 SessionStore 加载历史。
    pub async fn load_session(&mut self) -> Result<()> {
        let history = {
            let (store, session_id) = self.session_binding()?;
            store.load(session_id).await?
        };
        self.set_history(history);
        Ok(())
    }

    /// 把当前历史写入绑定的 SessionStore。
    pub async fn save_session(&self) -> Result<()> {
        let (store, session_id) = self.session_binding()?;
        store.save(session_id, self.history()).await
    }

    fn session_binding(&self) -> Result<(&Arc<dyn SessionStore>, &str)> {
        match (&self.session_store, &self.session_id) {
            (Some(store), Some(session_id)) => Ok((store, session_id.as_str())),
            _ => Err(kunpeng_core::Error::InvalidConfig(
                "未配置 session_store / session_id".into(),
            )),
        }
    }

    /// 追加用户消息并运行策略。
    pub async fn send(&mut self, user: Message, hooks: &AgentHooks<'_>) -> Result<StopReason> {
        self.strategy.history_mut().push(user);
        self.resume(hooks).await
    }

    /// 不追加消息，继续执行策略；网络错误后可以直接重试。
    pub async fn resume(&mut self, hooks: &AgentHooks<'_>) -> Result<StopReason> {
        let ctx = RunContext {
            model: self.model.as_ref(),
            tools: &self.tools,
            context: self.context.as_ref(),
            config: &self.config,
            hooks,
            cancel: self.cancel.clone(),
        };
        self.strategy.run(&ctx).await
    }
}

/// 默认的 ReAct/工具循环策略。它可以独立测试，也可以由 `Agent` 门面持有。
pub struct ReActAgent {
    history: Vec<Message>,
}

impl Default for ReActAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl ReActAgent {
    pub fn new() -> Self {
        Self {
            history: Vec::new(),
        }
    }

    pub fn with_history(history: Vec<Message>) -> Self {
        Self { history }
    }
}

#[async_trait]
impl AgentStrategy for ReActAgent {
    fn history(&self) -> &[Message] {
        &self.history
    }

    fn history_mut(&mut self) -> &mut Vec<Message> {
        &mut self.history
    }

    async fn run(&mut self, ctx: &RunContext<'_>) -> Result<StopReason> {
        run_react(&mut self.history, ctx).await
    }
}

async fn run_react(history: &mut Vec<Message>, ctx: &RunContext<'_>) -> Result<StopReason> {
    let config = ctx.config;
    if config.max_turns == 0 {
        return Err(kunpeng_core::Error::InvalidConfig(
            "max_turns 必须大于 0".into(),
        ));
    }

    ctx.cancel.reset();
    let tool_definitions = ctx.tools.definitions();

    for _ in 0..config.max_turns {
        if ctx.cancel.is_cancelled() {
            emit(
                ctx.hooks,
                AgentEvent::Finished {
                    stop_reason: StopReason::Cancelled,
                },
            );
            return Ok(StopReason::Cancelled);
        }

        // 每次请求前先构建上下文；预算为 None 时返回完整历史。
        let messages = ctx.context.build(history, config.context_budget).await?;

        let response = {
            let mut request = ChatRequest::new(messages.as_ref(), &tool_definitions);
            request.temperature = config.temperature;
            request.max_tokens = config.max_tokens;
            request.thinking = config.thinking;
            request.server_tools = &config.server_tools;
            request.tool_choice = config.tool_choice.as_ref();
            request.stop_sequences = &config.stop_sequences;
            request.provider_options = Some(&config.provider_options);
            let cancel = ctx.cancel.clone();
            let stream = config.stream;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    emit(
                        ctx.hooks,
                        AgentEvent::Finished {
                            stop_reason: StopReason::Cancelled,
                        },
                    );
                    return Ok(StopReason::Cancelled);
                }
                result = async {
                    if stream {
                        let mut on_delta = |delta: StreamDelta<'_>| {
                            if ctx.cancel.is_cancelled() {
                                return StreamControl::Stop;
                            }
                            match delta {
                                StreamDelta::Text(text) => {
                                    emit(ctx.hooks, AgentEvent::AssistantDelta { text: text.to_string() });
                                }
                                StreamDelta::Thinking(text) => {
                                    emit(ctx.hooks, AgentEvent::ThinkingDelta { text: text.to_string() });
                                }
                                _ => {}
                            }
                            StreamControl::Continue
                        };
                        ctx.model.chat_stream(&request, &mut on_delta).await
                    } else {
                        ctx.model.chat(&request).await
                    }
                } => match result {
                    Ok(response) => response,
                    Err(kunpeng_core::Error::Cancelled) => {
                        emit(
                            ctx.hooks,
                            AgentEvent::Finished {
                                stop_reason: StopReason::Cancelled,
                            },
                        );
                        return Ok(StopReason::Cancelled);
                    }
                    Err(error) => {
                        emit(
                            ctx.hooks,
                            AgentEvent::Error {
                                message: error.to_string(),
                            },
                        );
                        return Err(error);
                    }
                },
            }
        };

        let kunpeng_core::types::ChatResponse {
            content,
            tool_calls,
            usage,
            finish_reason,
            thinking,
            citations,
            server_tool_uses,
            provider_data,
            ..
        } = response;

        history.push(Message::Assistant {
            content: content.clone(),
            tool_calls: tool_calls.clone(),
            thinking: thinking.clone(),
            citations: citations.clone(),
            server_tool_uses: server_tool_uses.clone(),
            provider_data,
        });
        emit(
            ctx.hooks,
            AgentEvent::Assistant {
                content: content.clone(),
                tool_calls: tool_calls.clone(),
                usage,
                finish_reason: finish_reason.clone(),
                thinking,
                citations,
                server_tool_uses,
            },
        );

        if tool_calls.is_empty() {
            if matches!(finish_reason.as_ref(), Some(FinishReason::Pause)) {
                continue;
            }
            let stop_reason = match finish_reason {
                Some(FinishReason::Length) => StopReason::Length,
                Some(FinishReason::ContentFilter) => StopReason::ContentFilter,
                _ => StopReason::Completed,
            };
            emit(
                ctx.hooks,
                AgentEvent::Finished {
                    stop_reason: stop_reason.clone(),
                },
            );
            return Ok(stop_reason);
        }

        let mut calls = tool_calls.into_iter();
        while let Some(call) = calls.next() {
            if ctx.cancel.is_cancelled() {
                record_cancelled(history, &call);
                for remaining in calls.by_ref() {
                    record_cancelled(history, &remaining);
                }
                emit(
                    ctx.hooks,
                    AgentEvent::Finished {
                        stop_reason: StopReason::Cancelled,
                    },
                );
                return Ok(StopReason::Cancelled);
            }

            // 审批先于 ToolStarted；审批期间同时监听取消和超时。
            let decision = match ctx.hooks.approve {
                None => ToolDecision::Allow,
                Some(approver) => {
                    let timeout = config.approval_timeout;
                    let cancel = ctx.cancel.clone();
                    let approval = async {
                        match timeout {
                            Some(timeout) => {
                                match tokio::time::timeout(timeout, approver.approve(&call)).await {
                                    Ok(decision) => decision,
                                    Err(_) => ToolDecision::Deny("审批超时".into()),
                                }
                            }
                            None => approver.approve(&call).await,
                        }
                    };
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            record_cancelled(history, &call);
                            for remaining in calls.by_ref() {
                                record_cancelled(history, &remaining);
                            }
                            emit(
                                ctx.hooks,
                                AgentEvent::Finished {
                                    stop_reason: StopReason::Cancelled,
                                },
                            );
                            return Ok(StopReason::Cancelled);
                        }
                        decision = approval => decision,
                    }
                }
            };

            let ToolCall {
                id,
                name,
                arguments,
            } = call;

            let outcome = match decision {
                ToolDecision::Deny(reason) => {
                    let content = format!("工具调用被拒绝: {reason}");
                    history.push(Message::tool_result(id.clone(), content, true));
                    ToolOutcome::Denied { reason }
                }
                ToolDecision::Allow => {
                    emit(
                        ctx.hooks,
                        AgentEvent::ToolStarted {
                            call: ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: arguments.clone(),
                            },
                        },
                    );
                    let timeout = ctx.tools.timeout_for(&name).or(config.tool_timeout);
                    match call_tool(ctx.tools, timeout, &name, arguments).await {
                        Ok(content) => {
                            history.push(Message::tool_result(id.clone(), content.clone(), false));
                            ToolOutcome::Success { content }
                        }
                        Err(error) => {
                            let message = error.to_string();
                            history.push(Message::tool_result(id.clone(), message.clone(), true));
                            ToolOutcome::Error { message }
                        }
                    }
                }
            };

            emit(
                ctx.hooks,
                AgentEvent::ToolFinished {
                    execution: ToolExecution { id, name, outcome },
                },
            );
        }
    }

    emit(
        ctx.hooks,
        AgentEvent::Finished {
            stop_reason: StopReason::MaxTurns,
        },
    );
    Ok(StopReason::MaxTurns)
}

fn record_cancelled(history: &mut Vec<Message>, call: &ToolCall) {
    history.push(Message::tool_result(
        call.id.clone(),
        "操作已取消".to_string(),
        true,
    ));
}

async fn call_tool(
    tools: &ToolRegistry,
    timeout: Option<Duration>,
    name: &str,
    arguments: serde_json::Value,
) -> Result<String> {
    match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, tools.call(name, arguments)).await {
            Ok(result) => result,
            Err(_) => Err(kunpeng_core::Error::ToolTimeout {
                name: name.to_string(),
                timeout_ms: timeout.as_millis() as u64,
            }),
        },
        None => tools.call(name, arguments).await,
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
