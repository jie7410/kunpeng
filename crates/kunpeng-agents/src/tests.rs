use super::*;
use async_trait::async_trait;
use kunpeng_core::types::{ChatResponse, ToolDefinition};
use kunpeng_core::Error;
use kunpeng_memory::InMemorySessionStore;
use kunpeng_provider::{Adapter, CallContext, Model, Provider};
use kunpeng_runtime::ToolApprover;
use kunpeng_tools::Tool;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};
use tokio::sync::{oneshot, Notify};

struct MockAdapter {
    responses: Mutex<VecDeque<Result<ChatResponse>>>,
}

#[async_trait]
impl Adapter for MockAdapter {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn chat(&self, _ctx: &CallContext<'_>, _req: &ChatRequest<'_>) -> Result<ChatResponse> {
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
            Arc::new(MockAdapter {
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
    let mut response = ChatResponse::new("mock");
    response.content = content.map(str::to_string);
    response.tool_calls = tool_calls;
    response.finish_reason = finish_reason;
    response
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

struct DenyApprover;

#[async_trait]
impl ToolApprover for DenyApprover {
    async fn approve(&self, _call: &ToolCall) -> ToolDecision {
        ToolDecision::Deny("not allowed".into())
    }
}

struct ChannelApprover {
    started: Arc<Notify>,
    receiver: Mutex<Option<oneshot::Receiver<ToolDecision>>>,
}

#[async_trait]
impl ToolApprover for ChannelApprover {
    async fn approve(&self, _call: &ToolCall) -> ToolDecision {
        self.started.notify_one();
        let receiver = self
            .receiver
            .lock()
            .unwrap()
            .take()
            .expect("approval receiver already consumed");
        receiver
            .await
            .unwrap_or_else(|_| ToolDecision::Deny("UI closed".into()))
    }
}

struct NeverApprover;

#[async_trait]
impl ToolApprover for NeverApprover {
    async fn approve(&self, _call: &ToolCall) -> ToolDecision {
        tokio::time::sleep(Duration::from_secs(60)).await;
        ToolDecision::Allow
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
    let mut agent = Agent::new(model, tools, RunConfig::default());

    let events = Arc::new(Mutex::new(Vec::new()));
    let on_event = {
        let events = Arc::clone(&events);
        move |event: AgentEvent| events.lock().unwrap().push(event)
    };
    let approve = DenyApprover;
    let hooks = AgentHooks::default()
        .with_event(&on_event)
        .with_approver(&approve);

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
async fn async_approval_allows_after_ui_decision() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools
        .register(CountingTool {
            calls: Arc::clone(&calls),
        })
        .unwrap();

    let (sender, receiver) = oneshot::channel();
    let approver = ChannelApprover {
        started: Arc::new(Notify::new()),
        receiver: Mutex::new(Some(receiver)),
    };
    let started = Arc::clone(&approver.started);

    let model = mock_model(vec![
        response(
            None,
            vec![tool_call("call-1", "echo")],
            Some(FinishReason::ToolCalls),
        ),
        response(Some("done"), vec![], Some(FinishReason::Stop)),
    ]);
    let mut agent = Agent::new(model, tools, RunConfig::default());

    let handle = tokio::spawn(async move {
        let hooks = AgentHooks::default().with_approver(&approver);
        let result = agent.send(Message::user("hi"), &hooks).await;
        (result, agent.history().to_vec())
    });

    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("approval was not requested");
    sender.send(ToolDecision::Allow).unwrap();

    let (result, history) = tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("agent did not resume after approval")
        .unwrap();

    assert_eq!(result.unwrap(), StopReason::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        &history[2],
        Message::Tool {
            is_error: false,
            ..
        }
    ));
}

#[tokio::test]
async fn approval_timeout_denies_tool() {
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
    let config = RunConfig::default().with_approval_timeout(Some(Duration::from_millis(10)));
    let mut agent = Agent::new(model, tools, config);
    let approver = NeverApprover;
    let hooks = AgentHooks::default().with_approver(&approver);

    let stop = agent.send(Message::user("hi"), &hooks).await.unwrap();

    assert_eq!(stop, StopReason::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        &agent.history()[2],
        Message::Tool { is_error: true, .. }
    ));
}

#[tokio::test]
async fn length_finish_is_not_reported_as_completed() {
    let model = mock_model(vec![response(
        Some("cut"),
        vec![],
        Some(FinishReason::Length),
    )]);
    let mut agent = Agent::new(model, ToolRegistry::new(), RunConfig::default());
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
    let config = RunConfig::default().with_tool_timeout(Some(Duration::from_millis(20)));
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
async fn pause_turn_continues_without_new_user_message() {
    let model = mock_model(vec![
        response(None, vec![], Some(FinishReason::Pause)),
        response(Some("done"), vec![], Some(FinishReason::Stop)),
    ]);
    let mut agent = Agent::new(model, ToolRegistry::new(), RunConfig::default());
    let hooks = AgentHooks::default();

    let stop = agent.send(Message::user("hi"), &hooks).await.unwrap();

    assert_eq!(stop, StopReason::Completed);
    assert_eq!(agent.history().len(), 3);
    assert!(matches!(
        &agent.history()[1],
        Message::Assistant { content: None, .. }
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
    let config = RunConfig::default().with_max_turns(1);
    let mut agent = Agent::new(model, tools, config);
    let hooks = AgentHooks::default();

    let stop = agent.send(Message::user("hi"), &hooks).await.unwrap();

    assert_eq!(stop, StopReason::MaxTurns);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(agent.history().last(), Some(Message::Tool { .. })));
}

struct StreamingAdapter;

#[async_trait]
impl Adapter for StreamingAdapter {
    fn name(&self) -> &'static str {
        "streaming"
    }

    async fn chat(&self, _ctx: &CallContext<'_>, _req: &ChatRequest<'_>) -> Result<ChatResponse> {
        Err(Error::Other(
            "chat() should not be used when stream=true".into(),
        ))
    }

    async fn chat_stream(
        &self,
        _ctx: &CallContext<'_>,
        _req: &ChatRequest<'_>,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<ChatResponse> {
        on_delta(StreamDelta::Text("你".into()));
        on_delta(StreamDelta::Text("好".into()));
        Ok(ChatResponse::new("mock")
            .with_content("你好")
            .with_finish_reason(FinishReason::Stop))
    }
}

#[tokio::test]
async fn streaming_emits_deltas_before_final_assistant() {
    let provider = Arc::new(
        Provider::new(
            "https://example.invalid",
            "key",
            Arc::new(StreamingAdapter),
            5,
        )
        .unwrap(),
    );
    let model = Model::new(provider, "mock");
    let mut agent = Agent::new(model, ToolRegistry::new(), RunConfig::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let on_event = {
        let events = Arc::clone(&events);
        move |event: AgentEvent| events.lock().unwrap().push(event)
    };
    let hooks = AgentHooks::default().with_event(&on_event);

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

struct SlowAdapter {
    started: Arc<Notify>,
}

#[async_trait]
impl Adapter for SlowAdapter {
    fn name(&self) -> &'static str {
        "slow"
    }

    async fn chat(&self, _ctx: &CallContext<'_>, _req: &ChatRequest<'_>) -> Result<ChatResponse> {
        self.started.notify_one();
        tokio::time::sleep(Duration::from_secs(30)).await;
        Err(Error::Other("unreachable".into()))
    }
}

#[tokio::test]
async fn session_store_round_trips_agent_history() {
    let store = Arc::new(InMemorySessionStore::new());
    let mut agent = Agent::new(
        mock_model(vec![]),
        ToolRegistry::new(),
        RunConfig::default(),
    )
    .with_session_store(store.clone(), "session-1");
    agent.push(Message::user("hello"));
    agent.save_session().await.unwrap();

    let mut restored = Agent::new(
        mock_model(vec![]),
        ToolRegistry::new(),
        RunConfig::default(),
    )
    .with_session_store(store, "session-1");
    restored.load_session().await.unwrap();

    assert_eq!(restored.history(), agent.history());
}

#[tokio::test]
async fn cancellation_preserves_valid_history() {
    let started = Arc::new(Notify::new());
    let provider = Arc::new(
        Provider::new(
            "https://example.invalid",
            "key",
            Arc::new(SlowAdapter {
                started: Arc::clone(&started),
            }),
            5,
        )
        .unwrap(),
    );
    let model = Model::new(provider, "mock");
    let mut agent = Agent::new(model, ToolRegistry::new(), RunConfig::default());
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
