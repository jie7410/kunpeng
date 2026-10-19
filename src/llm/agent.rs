//! 有状态 Agent：持有会话历史，负责一轮用户输入内的 model/tool 循环。
//!
//! 三条约束决定了这里的实现方式：
//!
//! 1. `history` 是唯一事实来源。请求只借用它，模型返回后才把 assistant 消息追加
//!    进去，避免把整段会话复制进 `ChatRequest`。
//! 2. 任何退出路径都要保证历史可回放：assistant 消息里的每个 `tool_call` 必须
//!    有对应的 `tool_result`。取消时也要给未执行/未完成的调用补一条错误结果，
//!    否则下一次 `resume()` 会发出非法历史。
//! 3. 内核不依赖 UI。过程通过 `AgentEvent` 暴露，工具执行前通过 `AgentHooks`
//!    询问是否放行；CLI/Tauri 只负责消费事件和实现审批。

use crate::llm::error::{Error, Result};
use crate::llm::model::Model;
use crate::llm::tool::ToolRegistry;
use crate::llm::types::{ChatRequest, FinishReason, Message, ToolCall};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Notify;

/// 一轮对话的运行参数。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 最多发起多少次模型请求，不是“工具轮数”。
    ///
    /// 每个工具调用后都要再请求一次模型；8 次大约够 3-4 轮工具调用。设太小会在
    /// 工具结果后拿不到最终回复，设太大则可能让模型在错误路径上反复尝试。
    pub max_turns: usize,
    /// 覆盖 `ModelDefault` 的值；None 表示沿用模型默认。
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// 是否使用流式请求。
    ///
    /// 默认开启：文本增量通过 `AgentEvent::AssistantDelta` 发出，UI 可以做打字机
    /// 效果。兼容端点不支持 SSE 时改成 false，Agent 会走普通 `chat`。
    pub stream: bool,
    /// 单个工具的硬超时；None 表示不限制。
    ///
    /// 工具可能跑 shell、读大文件或等外部服务，不能指望它自己守时。超时只终止
    /// 当前工具，结果会作为 `is_error` 回传给模型。
    pub tool_timeout: Option<Duration>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 8,
            temperature: None,
            max_tokens: None,
            stream: true,
            tool_timeout: Some(Duration::from_secs(60)),
        }
    }
}

/// Agent 停止的原因。与 `FinishReason` 分开：后者描述单次模型响应的结束方式，
/// 这里描述整个工具循环为什么退出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StopReason {
    /// 模型返回了不再需要工具的最终回复。
    Completed,
    /// 达到 `max_turns`，历史末尾可能是工具结果。UI 应提示“未完成，可继续”。
    MaxTurns,
    /// 模型因 token 上限停止，正文可能被截断。
    Length,
    /// 内容被安全策略过滤。
    ContentFilter,
    /// 调用方主动取消。历史已补齐取消占位结果，可以再次 `resume()`。
    Cancelled,
}

/// 工具执行前的审批结果。
///
/// Deny 的 reason 会同时写进 tool_result 和 `AgentEvent::ToolFinished`：
/// 模型需要知道被拒绝的原因才能换方案，UI 也需要展示是谁拒绝了什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    Deny(String),
}

/// 单个工具调用的最终结果，事件和 UI 共用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolOutcome {
    Success { content: String },
    Error { message: String },
    Denied { reason: String },
}

/// 工具执行记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolExecution {
    /// 对应 `tool_call.id`。
    pub id: String,
    pub name: String,
    pub outcome: ToolOutcome,
}

/// UI 无关的事件流。
///
/// 事件是拥有所有权的普通数据：CLI 可以直接打印，Tauri 可以序列化后 emit，
/// 测试可以收集到 Vec 里断言。事件顺序固定为：
///
/// `AssistantDelta* → Assistant → (ToolStarted → ToolFinished)* → Finished`
///
/// `AssistantDelta` 是临时内容，只用于展示；最终以 `Assistant` 里的完整
/// `content` 为准。失败时由 `Error` 事件替代 `Finished`；`ToolStarted` 只在
/// 审批通过后发出，被拒绝的调用只有 `ToolFinished(Denied)`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentEvent {
    /// 流式文本增量。可能被取消/失败打断，UI 收到 Error 或 Cancelled 后应丢弃
    /// 当前流式缓冲。
    AssistantDelta {
        text: String,
    },
    /// 单次模型响应。此时 assistant 消息已经写入 history。
    Assistant {
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
        usage: Option<crate::llm::types::Usage>,
        finish_reason: Option<FinishReason>,
    },
    /// 工具即将执行。审批通过后才会发出。
    ToolStarted {
        call: ToolCall,
    },
    ToolFinished {
        execution: ToolExecution,
    },
    /// 正常结束（包括 MaxTurns/Length/ContentFilter/Cancelled）。
    Finished {
        stop_reason: StopReason,
    },
    /// 模型调用失败。history 保留，调用方可以直接 `resume()` 重试。
    Error {
        message: String,
    },
}

/// 调用方注入的观察/审批钩子。
///
/// 用引用而不是 trait object 的所有权，是为了让调用方在 `send().await` 期间
/// 仍能持有自己的捕获状态；回调是同步的，因为它们在 Agent 的 future 内直接调用。
/// Tauri 若需要异步等待用户点“允许”，可以在回调里发事件并立即返回，
/// 真正的异步审批需要把这里扩展成 async approver。
#[derive(Default)]
pub struct AgentHooks<'a> {
    /// 过程事件。回调应尽量轻量，不要在里面做长时间阻塞操作。
    pub on_event: Option<&'a (dyn Fn(AgentEvent) + Send + Sync)>,
    /// 工具审批。None 等价于全部 Allow。
    pub approve: Option<&'a (dyn Fn(&ToolCall) -> ToolDecision + Send + Sync)>,
}

/// 取消令牌。
///
/// 用 `AtomicBool` 保存状态、`Notify` 唤醒等待者：
/// - Agent 在每次模型请求上 `select!` 这个 future，取消可以立即 drop HTTP 请求；
/// - 工具执行期间不 select，只在工具之间检查。这样不会把工具 future 扔在半路，
///   也避免外部命令留下孤儿进程；单个工具的上限由 `tool_timeout` 控制。
#[derive(Clone, Default)]
pub struct CancellationToken {
    inner: Arc<CancelInner>,
}

#[derive(Default)]
struct CancelInner {
    /// 取消标志。用 SeqCst 是最省心的选择：取消频率很低，不值得为省一点内存序
    /// 去推理跨任务的可见性。
    flag: AtomicBool,
    /// 只负责唤醒 `cancelled()` 的等待者，不保存状态；状态在 flag 里。
    notify: Notify,
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// 置位并唤醒所有等待者。`notify_waiters` 不保留许可，
    /// 所以 `cancelled()` 里必须在注册 waiter 前后各检查一次 flag。
    pub fn cancel(&self) {
        self.inner.flag.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.flag.load(Ordering::SeqCst)
    }

    /// 清除取消状态。`resume()` 开始时调用：取消是“中止当前运行”，不是
    /// 永久关闭 Agent；下一次 send/resume 应该可以正常开始。
    pub fn reset(&self) {
        self.inner.flag.store(false, Ordering::SeqCst);
    }

    /// 取消后完成。
    ///
    /// 先检查 flag，再创建并 enable `Notified`，然后再检查一次 flag。这样即使
    /// `cancel()` 正好发生在两步之间，也不会丢掉通知：
    /// - cancel 在 enable 前 → 第二次检查看到 true；
    /// - cancel 在 enable 后 → Notified 已被注册，`notify_waiters` 会唤醒它。
    ///
    /// 外层 loop 用于处理“被唤醒但 flag 又被 reset”的情况。
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// 会话内唯一的可变状态持有者。
///
/// 一次 `send()` 结束后 Agent 仍然存在，UI 可以继续追加消息、保存历史或重试。
/// `Model` 和 `ToolRegistry` 都是可克隆的，内部用 Arc 共享连接和工具实例。
pub struct Agent {
    model: Model,
    tools: ToolRegistry,
    config: AgentConfig,
    /// 会话历史。所有请求都从这里借用，写入只发生在请求返回之后。
    history: Vec<Message>,
    cancel: CancellationToken,
}

impl Agent {
    /// 空历史构造。通常先用 `with_history` 放入 system prompt。
    pub fn new(model: Model, tools: ToolRegistry, config: AgentConfig) -> Self {
        Self::with_history(model, tools, config, Vec::new())
    }

    /// 带初始历史构造。历史可以来自上次会话的持久化结果；调用方负责保证
    /// 它本身是合法的（assistant tool_call 有对应 tool_result）。
    pub fn with_history(
        model: Model,
        tools: ToolRegistry,
        config: AgentConfig,
        history: Vec<Message>,
    ) -> Self {
        Self {
            model,
            tools,
            config,
            history,
            cancel: CancellationToken::new(),
        }
    }

    /// 只读历史。UI 用来渲染消息列表，也可以序列化后落盘。
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// 整体替换历史，用于加载历史会话。不会补 system prompt。
    pub fn set_history(&mut self, history: Vec<Message>) {
        self.history = history;
    }

    /// 追加一条消息（通常是 system prompt 或多模态扩展消息）。
    pub fn push(&mut self, message: Message) {
        self.history.push(message);
    }

    /// 取出取消令牌的克隆。令牌可以跨任务持有；取消会中止当前模型请求，
    /// 但已经写入的历史和工具结果不会回滚。
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// 追加一条用户消息，然后运行到模型给出最终回复或达到停止条件。
    ///
    /// 请求失败时用户消息仍留在历史里。UI 可以展示失败状态，随后调用
    /// `resume()` 重试，不需要让用户重新输入。
    pub async fn send(&mut self, user: Message, hooks: &AgentHooks<'_>) -> Result<StopReason> {
        self.history.push(user);
        self.resume(hooks).await
    }

    /// 不追加消息，继续处理当前历史。
    ///
    /// 适用于三类场景：
    /// - `send()` 因网络/API 错误返回，原样重试；
    /// - `StopReason::MaxTurns` 后继续跑；
    /// - 从持久化历史恢复后接着对话。
    ///
    /// 调用方负责保证历史合法；正常运行产生的中断路径都会补齐 tool_result。
    pub async fn resume(&mut self, hooks: &AgentHooks<'_>) -> Result<StopReason> {
        if self.config.max_turns == 0 {
            return Err(Error::InvalidConfig("max_turns 必须大于 0".into()));
        }

        // 取消是“中止上一次运行”，不是永久状态。新一轮开始前清掉旧标记。
        self.cancel.reset();
        // 工具定义在一轮内不变，只取一次；每轮请求都重新借用它。
        let tool_definitions = self.tools.definitions();

        // 一次循环 = 一次模型请求。工具执行完不会立刻结束，而是进入下一轮让模型
        // 消化工具结果。
        for _ in 0..self.config.max_turns {
            if self.cancel.is_cancelled() {
                emit(
                    hooks,
                    AgentEvent::Finished {
                        stop_reason: StopReason::Cancelled,
                    },
                );
                return Ok(StopReason::Cancelled);
            }

            // 请求块独立成作用域：request 借用了 self.history，块结束后借用释放，
            // 后面才能 push assistant 消息。
            let response = {
                let request = ChatRequest {
                    messages: &self.history,
                    tools: &tool_definitions,
                    temperature: self.config.temperature,
                    max_tokens: self.config.max_tokens,
                };
                let cancel = self.cancel.clone();
                let stream = self.config.stream;
                // biased + 取消分支在前：多个分支同时就绪时优先响应取消，
                // 避免刚好返回的响应让用户觉得“点了取消还在继续”。
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        emit(
                            hooks,
                            AgentEvent::Finished {
                                stop_reason: StopReason::Cancelled,
                            },
                        );
                        return Ok(StopReason::Cancelled);
                    }
                    result = async {
                        if stream {
                            let mut on_delta = |text: String| {
                                emit(hooks, AgentEvent::AssistantDelta { text });
                            };
                            self.model.chat_stream(&request, &mut on_delta).await
                        } else {
                            self.model.chat(&request).await
                        }
                    } => match result {
                        Ok(response) => response,
                        Err(error) => {
                            // 历史保持原样（末尾是 user 或上一轮 tool_result），
                            // 调用方可以直接 resume()，不需要重新输入。
                            emit(
                                hooks,
                                AgentEvent::Error {
                                    message: error.to_string(),
                                },
                            );
                            return Err(error);
                        }
                    },
                }
            };

            let crate::llm::types::ChatResponse {
                content,
                tool_calls,
                usage,
                finish_reason,
                ..
            } = response;

            // 先写历史再发事件：观察者收到 Assistant 时，history/history() 已经
            // 包含这条消息，不会出现事件与状态不一致。
            self.history.push(Message::Assistant {
                content: content.clone(),
                tool_calls: tool_calls.clone(),
            });
            emit(
                hooks,
                AgentEvent::Assistant {
                    content: content.clone(),
                    tool_calls: tool_calls.clone(),
                    usage,
                    finish_reason: finish_reason.clone(),
                },
            );

            if tool_calls.is_empty() {
                // 没有工具调用就结束。这里必须检查 Length/ContentFilter：
                // 否则被截断或过滤的回复会被 UI 当成正常完成。
                let stop_reason = match finish_reason {
                    Some(FinishReason::Length) => StopReason::Length,
                    Some(FinishReason::ContentFilter) => StopReason::ContentFilter,
                    _ => StopReason::Completed,
                };
                emit(
                    hooks,
                    AgentEvent::Finished {
                        stop_reason: stop_reason.clone(),
                    },
                );
                return Ok(stop_reason);
            }

            // 逐个执行。一次响应里的多个工具调用共用同一个 assistant 消息，
            // 每个调用都必须产生一条 tool_result，顺序最好与调用顺序一致。
            let mut calls = tool_calls.into_iter();
            while let Some(call) = calls.next() {
                if self.cancel.is_cancelled() {
                    // 取消发生在工具之间：给当前和剩余调用补错误结果。
                    // 否则 history 会停在 assistant(tool_calls) 而没有 tool_result，
                    // 下一轮请求会被 API 判定为非法。
                    self.record_cancelled(&call);
                    for remaining in calls.by_ref() {
                        self.record_cancelled(&remaining);
                    }
                    emit(
                        hooks,
                        AgentEvent::Finished {
                            stop_reason: StopReason::Cancelled,
                        },
                    );
                    return Ok(StopReason::Cancelled);
                }

                // 审批先于 ToolStarted。被拒绝的调用没有“开始执行”，
                // UI 只应看到 ToolFinished(Denied)。
                let decision = hooks
                    .approve
                    .map(|approve| approve(&call))
                    .unwrap_or(ToolDecision::Allow);

                let ToolCall {
                    id,
                    name,
                    arguments,
                } = call;

                let outcome = match decision {
                    ToolDecision::Deny(reason) => {
                        // 拒绝也写 tool_result。模型需要知道调用失败及原因，
                        // 否则它会以为工具执行成功或继续重复请求。
                        let content = format!("工具调用被拒绝: {reason}");
                        self.history
                            .push(Message::tool_result(id.clone(), content, true));
                        ToolOutcome::Denied { reason }
                    }
                    ToolDecision::Allow => {
                        // 事件需要拥有数据，克隆一份给 UI；真正的执行使用
                        // arguments 的所有权。这里的 clone 只有单次调用大小，
                        // 不是整段历史。
                        emit(
                            hooks,
                            AgentEvent::ToolStarted {
                                call: ToolCall {
                                    id: id.clone(),
                                    name: name.clone(),
                                    arguments: arguments.clone(),
                                },
                            },
                        );
                        // 工具错误不向上传播：回传给模型，让它决定换工具/换参数。
                        // 只有模型调用失败、配置错误和取消会中断整轮。
                        match self.call_tool(&name, arguments).await {
                            Ok(content) => {
                                self.history.push(Message::tool_result(
                                    id.clone(),
                                    content.clone(),
                                    false,
                                ));
                                ToolOutcome::Success { content }
                            }
                            Err(error) => {
                                let message = error.to_string();
                                self.history.push(Message::tool_result(
                                    id.clone(),
                                    message.clone(),
                                    true,
                                ));
                                ToolOutcome::Error { message }
                            }
                        }
                    }
                };

                emit(
                    hooks,
                    AgentEvent::ToolFinished {
                        execution: ToolExecution { id, name, outcome },
                    },
                );
            }
        }

        // 历史末尾是最后一批 tool_result，仍然合法；调用方可以 resume() 继续。
        emit(
            hooks,
            AgentEvent::Finished {
                stop_reason: StopReason::MaxTurns,
            },
        );
        Ok(StopReason::MaxTurns)
    }

    /// 执行单个工具并施加超时。
    ///
    /// `tokio::time::timeout` 超时会 drop 工具 future。工具实现如果启动了子进程
    /// 或写了临时文件，需要自己保证 drop 后不留资源；内核无法替它清理。
    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<String> {
        match self.config.tool_timeout {
            Some(timeout) => {
                match tokio::time::timeout(timeout, self.tools.call(name, arguments)).await {
                    Ok(result) => result,
                    Err(_) => Err(Error::ToolTimeout {
                        name: name.to_string(),
                        timeout_ms: timeout.as_millis() as u64,
                    }),
                }
            }
            None => self.tools.call(name, arguments).await,
        }
    }

    /// 为未执行/未完成的工具调用补一条错误结果。
    /// 内容面向模型，不需要和 UI 文案一致；`is_error=true` 让模型知道调用失败。
    fn record_cancelled(&mut self, call: &ToolCall) {
        self.history.push(Message::tool_result(
            call.id.clone(),
            "操作已取消".to_string(),
            true,
        ));
    }
}

/// 事件回调是调用方提供的，缺失时直接丢弃事件。保持为自由函数，避免在
/// `resume` 的每个返回点重复写 `if let`。
fn emit(hooks: &AgentHooks<'_>, event: AgentEvent) {
    if let Some(on_event) = hooks.on_event {
        on_event(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::provider::protocol::{CallContext, Protocol};
    use crate::llm::provider::Provider;
    use crate::llm::tool::Tool;
    use crate::llm::types::{ChatResponse, ToolDefinition};
    use async_trait::async_trait;
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };

    struct MockProtocol {
        responses: Mutex<VecDeque<Result<ChatResponse>>>,
    }

    #[async_trait]
    impl Protocol for MockProtocol {
        async fn chat(
            &self,
            _ctx: &CallContext<'_>,
            _req: &ChatRequest<'_>,
        ) -> Result<ChatResponse> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(Error::Other("mock 响应已耗尽".into())))
        }
    }

    fn mock_model(responses: Vec<ChatResponse>) -> Model {
        let provider = Arc::new(
            Provider::new(
                "https://example.invalid",
                "key",
                Arc::new(MockProtocol {
                    responses: Mutex::new(responses.into_iter().map(Ok).collect()),
                }),
                5,
            )
            .unwrap(),
        );
        Model::new(provider, "mock")
    }

    fn response(
        content: Option<&str>,
        tool_calls: Vec<ToolCall>,
        finish_reason: Option<FinishReason>,
    ) -> ChatResponse {
        ChatResponse {
            content: content.map(str::to_string),
            tool_calls,
            model: "mock".into(),
            usage: None,
            finish_reason,
        }
    }

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::json!({}),
        }
    }

    struct CountingTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".into(),
                description: "echo".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        async fn call(&self, _arguments: serde_json::Value) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("ok".into())
        }
    }

    struct SlowTool;

    #[async_trait]
    impl Tool for SlowTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "slow".into(),
                description: "slow".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        async fn call(&self, _arguments: serde_json::Value) -> Result<String> {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok("late".into())
        }
    }

    #[tokio::test]
    async fn denied_tool_is_not_executed_and_history_is_valid() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolRegistry::new();
        tools
            .register(CountingTool {
                calls: Arc::clone(&calls),
            })
            .unwrap();

        let model = mock_model(vec![
            response(
                None,
                vec![tool_call("call-1", "echo")],
                Some(FinishReason::ToolCalls),
            ),
            response(Some("done"), vec![], Some(FinishReason::Stop)),
        ]);
        let mut agent = Agent::new(model, tools, AgentConfig::default());

        let events = Arc::new(Mutex::new(Vec::new()));
        let on_event = {
            let events = Arc::clone(&events);
            move |event: AgentEvent| events.lock().unwrap().push(event)
        };
        let approve = |_call: &ToolCall| ToolDecision::Deny("not allowed".into());
        let hooks = AgentHooks {
            on_event: Some(&on_event),
            approve: Some(&approve),
        };

        let stop = agent.send(Message::user("hi"), &hooks).await.unwrap();

        assert_eq!(stop, StopReason::Completed);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(matches!(
            &agent.history()[2],
            Message::Tool { is_error: true, .. }
        ));
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            AgentEvent::ToolFinished {
                execution: ToolExecution {
                    outcome: ToolOutcome::Denied { .. },
                    ..
                }
            }
        )));
    }

    #[tokio::test]
    async fn length_finish_is_not_reported_as_completed() {
        let model = mock_model(vec![response(
            Some("cut"),
            vec![],
            Some(FinishReason::Length),
        )]);
        let mut agent = Agent::new(model, ToolRegistry::new(), AgentConfig::default());
        let hooks = AgentHooks::default();

        let stop = agent.send(Message::user("hi"), &hooks).await.unwrap();
        assert_eq!(stop, StopReason::Length);
    }

    #[tokio::test]
    async fn tool_timeout_becomes_error_result() {
        let mut tools = ToolRegistry::new();
        tools.register(SlowTool).unwrap();
        let model = mock_model(vec![
            response(
                None,
                vec![tool_call("call-1", "slow")],
                Some(FinishReason::ToolCalls),
            ),
            response(Some("done"), vec![], Some(FinishReason::Stop)),
        ]);
        let config = AgentConfig {
            tool_timeout: Some(Duration::from_millis(20)),
            ..AgentConfig::default()
        };
        let mut agent = Agent::new(model, tools, config);
        let hooks = AgentHooks::default();

        let stop = agent.send(Message::user("hi"), &hooks).await.unwrap();

        assert_eq!(stop, StopReason::Completed);
        assert!(matches!(
            &agent.history()[2],
            Message::Tool { is_error: true, .. }
        ));
    }

    #[tokio::test]
    async fn max_turns_returns_with_valid_history() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolRegistry::new();
        tools
            .register(CountingTool {
                calls: Arc::clone(&calls),
            })
            .unwrap();
        let model = mock_model(vec![response(
            None,
            vec![tool_call("call-1", "echo")],
            Some(FinishReason::ToolCalls),
        )]);
        let config = AgentConfig {
            max_turns: 1,
            ..AgentConfig::default()
        };
        let mut agent = Agent::new(model, tools, config);
        let hooks = AgentHooks::default();

        let stop = agent.send(Message::user("hi"), &hooks).await.unwrap();

        assert_eq!(stop, StopReason::MaxTurns);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(agent.history().last(), Some(Message::Tool { .. })));
    }

    struct StreamingProtocol;

    #[async_trait]
    impl Protocol for StreamingProtocol {
        async fn chat(
            &self,
            _ctx: &CallContext<'_>,
            _req: &ChatRequest<'_>,
        ) -> Result<ChatResponse> {
            Err(Error::Other(
                "chat() should not be used when stream=true".into(),
            ))
        }

        async fn chat_stream(
            &self,
            _ctx: &CallContext<'_>,
            _req: &ChatRequest<'_>,
            on_delta: &mut (dyn FnMut(String) + Send),
        ) -> Result<ChatResponse> {
            on_delta("你".into());
            on_delta("好".into());
            Ok(ChatResponse {
                content: Some("你好".into()),
                tool_calls: vec![],
                model: "mock".into(),
                usage: None,
                finish_reason: Some(FinishReason::Stop),
            })
        }
    }

    #[tokio::test]
    async fn streaming_emits_deltas_before_final_assistant() {
        let provider = Arc::new(
            Provider::new(
                "https://example.invalid",
                "key",
                Arc::new(StreamingProtocol),
                5,
            )
            .unwrap(),
        );
        let model = Model::new(provider, "mock");
        let mut agent = Agent::new(model, ToolRegistry::new(), AgentConfig::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let on_event = {
            let events = Arc::clone(&events);
            move |event: AgentEvent| events.lock().unwrap().push(event)
        };
        let hooks = AgentHooks {
            on_event: Some(&on_event),
            approve: None,
        };

        let stop = agent.send(Message::user("hi"), &hooks).await.unwrap();

        assert_eq!(stop, StopReason::Completed);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], AgentEvent::AssistantDelta { text } if text == "你"));
        assert!(matches!(&events[1], AgentEvent::AssistantDelta { text } if text == "好"));
        assert!(matches!(
            &events[2],
            AgentEvent::Assistant { content: Some(text), .. } if text == "你好"
        ));
        assert!(matches!(
            &events[3],
            AgentEvent::Finished {
                stop_reason: StopReason::Completed
            }
        ));
    }

    struct SlowProtocol {
        started: Arc<Notify>,
    }

    #[async_trait]
    impl Protocol for SlowProtocol {
        async fn chat(
            &self,
            _ctx: &CallContext<'_>,
            _req: &ChatRequest<'_>,
        ) -> Result<ChatResponse> {
            self.started.notify_one();
            tokio::time::sleep(Duration::from_secs(30)).await;
            Err(Error::Other("unreachable".into()))
        }
    }

    #[tokio::test]
    async fn cancellation_preserves_valid_history() {
        let started = Arc::new(Notify::new());
        let provider = Arc::new(
            Provider::new(
                "https://example.invalid",
                "key",
                Arc::new(SlowProtocol {
                    started: Arc::clone(&started),
                }),
                5,
            )
            .unwrap(),
        );
        let model = Model::new(provider, "mock");
        let mut agent = Agent::new(model, ToolRegistry::new(), AgentConfig::default());
        let token = agent.cancel_token();

        let handle = tokio::spawn(async move {
            let hooks = AgentHooks::default();
            let result = agent.send(Message::user("hi"), &hooks).await;
            (result, agent.history().to_vec())
        });

        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("mock protocol did not start");
        token.cancel();

        let (result, history) = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("agent did not stop after cancellation")
            .unwrap();

        assert_eq!(result.unwrap(), StopReason::Cancelled);
        assert_eq!(history.len(), 1);
    }
}
