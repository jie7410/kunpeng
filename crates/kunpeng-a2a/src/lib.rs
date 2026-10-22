//! A2A（Agent2Agent）HTTP JSON-RPC 适配器。
//!
//! 覆盖 A2A 的核心路径：Agent Card 发现、同步 `message/send`、流式
//! `message/stream`、`tasks/get`/`tasks/resubscribe`，以及 push notification
//! 配置管理。SSE 事件以原始 JSON 交给回调，调用方可以按 Task/status/artifact
//! 结构自行投影。
//!
//! 适配器只依赖 `kunpeng-multi` 的信封和 `kunpeng-core` 的错误类型，不依赖
//! Agent 实现或 runtime。

use async_trait::async_trait;
use kunpeng_core::{Error, HttpErrorKind, Result, StreamControl};

fn http_error(error: reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        HttpErrorKind::Timeout
    } else if error.is_connect() {
        HttpErrorKind::Connect
    } else if error.is_body() {
        HttpErrorKind::Body
    } else if error.is_request() {
        HttpErrorKind::Request
    } else {
        HttpErrorKind::Other
    };
    Error::http_with_source(kind, error.to_string(), error)
}
use kunpeng_multi::{AgentEnvelope, AgentId, AgentPayload, AgentProtocol};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// A2A Agent Card 的最小结构化视图；未知字段保留在 `extra`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCard {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub capabilities: Value,
    #[serde(default)]
    pub skills: Vec<Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// 一次 A2A 调用的归一化结果。
///
/// A2A 的 `message/send` 可能直接返回 Message，也可能返回 Task。这里保留原始
/// JSON，同时提取常见字段，调用方不需要立刻匹配两套结构。
#[derive(Debug, Clone)]
pub struct A2aResponse {
    pub raw: Value,
    pub task_id: Option<String>,
    pub state: Option<String>,
    pub text: Option<String>,
}

impl A2aResponse {
    fn from_value(raw: Value) -> Self {
        let task_id = raw.get("id").and_then(Value::as_str).map(str::to_string);
        let state = raw
            .get("status")
            .and_then(|status| status.get("state"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let text = extract_text(&raw);
        Self {
            raw,
            task_id,
            state,
            text,
        }
    }
}

/// A2A push notification 配置。`id` 由服务端在 set/get 时返回。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PushNotificationConfig {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl PushNotificationConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: None,
            authentication: None,
            id: None,
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn with_authentication(mut self, authentication: Value) -> Self {
        self.authentication = Some(authentication);
        self
    }
}

/// A2A 同步/流式客户端。一个实例对应一个远端 endpoint。
#[derive(Clone)]
pub struct A2aClient {
    http: reqwest::Client,
    endpoint: String,
    agent_id: AgentId,
}

impl A2aClient {
    pub fn new(endpoint: impl Into<String>, agent_id: impl Into<AgentId>) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            agent_id: agent_id.into(),
        }
    }

    pub fn with_http(
        http: reqwest::Client,
        endpoint: impl Into<String>,
        agent_id: impl Into<AgentId>,
    ) -> Self {
        Self {
            http,
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            agent_id: agent_id.into(),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub async fn agent_card(&self) -> Result<AgentCard> {
        let url = format!("{}/.well-known/agent.json", self.endpoint);
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?;
        response.json::<AgentCard>().await.map_err(http_error)
    }

    /// 发送一条文本消息。`context_id` 用于把多轮消息归到同一个 A2A context。
    pub async fn send_message(
        &self,
        text: impl Into<String>,
        context_id: Option<&str>,
    ) -> Result<A2aResponse> {
        let request_id = self.next_request_id();
        let message_id = format!("{}-{request_id}", self.agent_id);
        let text = text.into();
        let request =
            build_send_message(&self.agent_id, &text, context_id, &message_id, request_id);
        let result = self.post_rpc(request).await?;
        Ok(A2aResponse::from_value(result))
    }

    pub async fn get_task(&self, task_id: &str) -> Result<A2aResponse> {
        let request_id = self.next_request_id();
        let result = self
            .post_rpc(json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tasks/get",
                "params": { "id": task_id }
            }))
            .await?;
        Ok(A2aResponse::from_value(result))
    }

    /// 发送消息并消费 `message/stream` 的 SSE 事件。
    ///
    /// 每个 `result` 原样交给 `on_event`；最终返回最后一个 result 的归一化视图。
    pub async fn send_message_stream<F>(
        &self,
        text: impl Into<String>,
        context_id: Option<&str>,
        on_event: &mut F,
    ) -> Result<A2aResponse>
    where
        F: FnMut(&Value) -> StreamControl + Send,
    {
        let request_id = self.next_request_id();
        let message_id = format!("{}-{request_id}", self.agent_id);
        let text = text.into();
        let request =
            build_stream_message(&self.agent_id, &text, context_id, &message_id, request_id);
        let result = self.post_streaming_rpc(request, on_event).await?;
        Ok(A2aResponse::from_value(result))
    }

    /// 重新订阅一个已有 task 的 SSE 事件。
    pub async fn resubscribe<F>(&self, task_id: &str, on_event: &mut F) -> Result<Value>
    where
        F: FnMut(&Value) -> StreamControl + Send,
    {
        let request_id = self.next_request_id();
        self.post_streaming_rpc(
            json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tasks/resubscribe",
                "params": { "id": task_id }
            }),
            on_event,
        )
        .await
    }

    /// 设置 task 的 push notification webhook。
    pub async fn set_push_notification_config(
        &self,
        task_id: &str,
        config: &PushNotificationConfig,
    ) -> Result<Value> {
        self.post_rpc(json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "tasks/pushNotificationConfig/set",
            "params": {
                "id": task_id,
                "pushNotificationConfig": config
            }
        }))
        .await
    }

    pub async fn get_push_notification_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<Value> {
        self.post_rpc(json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "tasks/pushNotificationConfig/get",
            "params": {
                "id": task_id,
                "pushNotificationConfigId": config_id
            }
        }))
        .await
    }

    pub async fn list_push_notification_configs(&self, task_id: &str) -> Result<Value> {
        self.post_rpc(json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "tasks/pushNotificationConfig/list",
            "params": { "id": task_id }
        }))
        .await
    }

    pub async fn delete_push_notification_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<Value> {
        self.post_rpc(json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "tasks/pushNotificationConfig/delete",
            "params": {
                "id": task_id,
                "pushNotificationConfigId": config_id
            }
        }))
        .await
    }

    async fn post_streaming_rpc<F>(&self, request: Value, on_event: &mut F) -> Result<Value>
    where
        F: FnMut(&Value) -> StreamControl + Send,
    {
        let response = self
            .http
            .post(&self.endpoint)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&request)
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?;

        if !is_event_stream(&response) {
            let body: Value = response.json().await.map_err(http_error)?;
            return rpc_result(body);
        }

        let mut decoder = SseDecoder::default();
        let mut last = None;
        let mut response = response;
        while let Some(chunk) = response.chunk().await.map_err(http_error)? {
            for data in decoder.push(&chunk) {
                if data == "[DONE]" {
                    continue;
                }
                let event: Value = serde_json::from_str(&data)
                    .map_err(|error| Error::Other(format!("A2A SSE 事件不是 JSON: {error}")))?;
                if let Some(error) = event.get("error") {
                    let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("未知错误");
                    return Err(Error::Other(format!(
                        "A2A 流式请求失败（{code}）: {message}"
                    )));
                }
                if let Some(result) = event.get("result").cloned() {
                    if on_event(&result) == StreamControl::Stop {
                        return Err(Error::Cancelled);
                    }
                    last = Some(result);
                }
            }
        }
        for data in decoder.finish() {
            if data == "[DONE]" {
                continue;
            }
            let event: Value = serde_json::from_str(&data)
                .map_err(|error| Error::Other(format!("A2A SSE 事件不是 JSON: {error}")))?;
            if let Some(error) = event.get("error") {
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误");
                return Err(Error::Other(format!(
                    "A2A 流式请求失败（{code}）: {message}"
                )));
            }
            if let Some(result) = event.get("result").cloned() {
                if on_event(&result) == StreamControl::Stop {
                    return Err(Error::Cancelled);
                }
                last = Some(result);
            }
        }
        last.ok_or_else(|| Error::Other("A2A 流式响应没有 result 事件".into()))
    }

    async fn post_rpc(&self, request: Value) -> Result<Value> {
        let response = self
            .http
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?;
        let body: Value = response.json().await.map_err(http_error)?;
        rpc_result(body)
    }

    fn next_request_id(&self) -> u64 {
        // 单客户端串行请求通常足够；并发场景在调用方复制 client 后各自维护序号。
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[async_trait]
impl AgentProtocol for A2aClient {
    async fn send_envelope(&self, envelope: &AgentEnvelope) -> Result<Value> {
        let text = payload_text(&envelope.payload);
        let request_id = self.next_request_id();
        let request = build_send_message(
            &envelope.from,
            &text,
            Some(&envelope.correlation_id),
            &envelope.id,
            request_id,
        );
        self.post_rpc(request).await
    }
}

/// 构造 A2A `message/send` 请求。抽成纯函数便于测试协议字段。
pub fn build_send_message(
    from: &AgentId,
    text: &str,
    context_id: Option<&str>,
    message_id: &str,
    request_id: u64,
) -> Value {
    build_message_request(
        "message/send",
        from,
        text,
        context_id,
        message_id,
        request_id,
    )
}

/// 构造 A2A `message/stream` 请求。
pub fn build_stream_message(
    from: &AgentId,
    text: &str,
    context_id: Option<&str>,
    message_id: &str,
    request_id: u64,
) -> Value {
    build_message_request(
        "message/stream",
        from,
        text,
        context_id,
        message_id,
        request_id,
    )
}

fn build_message_request(
    method: &str,
    from: &AgentId,
    text: &str,
    context_id: Option<&str>,
    message_id: &str,
    request_id: u64,
) -> Value {
    let mut message = json!({
        "role": "user",
        "parts": [{ "kind": "text", "text": text }],
        "messageId": message_id,
        "metadata": { "from": from.as_str() }
    });
    if let Some(context_id) = context_id {
        message["contextId"] = Value::String(context_id.to_string());
    }
    json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": method,
        "params": { "message": message }
    })
}

fn payload_text(payload: &AgentPayload) -> String {
    match payload {
        AgentPayload::Text { content } | AgentPayload::Result { content } => content.clone(),
        AgentPayload::Task { task } => task.clone(),
        AgentPayload::Handoff { reason, task } => format!("{reason}\n{task}"),
        _ => serde_json::to_string(payload).unwrap_or_default(),
    }
}

fn extract_text(value: &Value) -> Option<String> {
    let mut texts = Vec::new();
    collect_parts(value, &mut texts);
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

fn collect_parts(value: &Value, texts: &mut Vec<String>) {
    if let Some(parts) = value.get("parts").and_then(Value::as_array) {
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                texts.push(text.to_string());
            } else {
                texts.push(part.to_string());
            }
        }
    }
    if let Some(artifacts) = value.get("artifacts").and_then(Value::as_array) {
        for artifact in artifacts {
            collect_parts(artifact, texts);
        }
    }
}

fn rpc_result(body: Value) -> Result<Value> {
    if let Some(error) = body.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Err(Error::Other(format!("A2A 请求失败（{code}）: {message}")));
    }
    body.get("result")
        .cloned()
        .ok_or_else(|| Error::Other("A2A 响应缺少 result".into()))
}

fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase().contains("text/event-stream"))
        .unwrap_or(true)
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
    data: String,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=index).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&String::from_utf8_lossy(&line), &mut events);
        }
        events
    }

    fn finish(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let mut line = std::mem::take(&mut self.buffer);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&String::from_utf8_lossy(&line), &mut events);
        }
        if let Some(event) = self.dispatch() {
            events.push(event);
        }
        events
    }

    fn line(&mut self, line: &str, events: &mut Vec<String>) {
        if line.is_empty() {
            if let Some(event) = self.dispatch() {
                events.push(event);
            }
            return;
        }
        if line.starts_with(':') {
            return;
        }
        if let Some(value) = line.strip_prefix("data:") {
            let value = value.strip_prefix(' ').unwrap_or(value);
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(value);
        }
    }

    fn dispatch(&mut self) -> Option<String> {
        if self.data.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.data))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut data = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            data.extend_from_slice(&buffer[..read]);
            let Some(header_end) = data.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let header_end = header_end + 4;
            let headers = String::from_utf8_lossy(&data[..header_end]);
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
            if data.len() >= header_end + content_length {
                break;
            }
        }
        data
    }

    fn serve_once(response: String) -> (String, thread::JoinHandle<Value>) {
        serve_once_with(response, "application/json")
    }

    fn serve_once_with(
        response: String,
        content_type: &'static str,
    ) -> (String, thread::JoinHandle<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let body_start = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            let body: Value = serde_json::from_slice(&request[body_start..]).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .unwrap();
            body
        });
        (format!("http://{address}/rpc"), handle)
    }

    #[tokio::test]
    async fn sends_message_over_http() {
        let (endpoint, server) = serve_once(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "id": "task-1",
                    "status": { "state": "completed" },
                    "artifacts": [{
                        "parts": [{ "kind": "text", "text": "done" }]
                    }]
                }
            })
            .to_string(),
        );
        let client = A2aClient::new(endpoint, "planner");
        let response = client.send_message("hello", Some("ctx-1")).await.unwrap();
        assert_eq!(response.task_id.as_deref(), Some("task-1"));
        assert_eq!(response.text.as_deref(), Some("done"));

        let request = server.join().unwrap();
        assert_eq!(request["method"], "message/send");
        assert_eq!(request["params"]["message"]["parts"][0]["text"], "hello");
        assert_eq!(request["params"]["message"]["metadata"]["from"], "planner");
    }

    #[tokio::test]
    async fn stops_stream_when_consumer_requests_it() {
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"id\":\"task-1\",\"status\":{\"state\":\"working\"}}}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"id\":\"task-1\",\"status\":{\"state\":\"completed\"}}}\n\n"
        );
        let (endpoint, _server) = serve_once_with(body.to_string(), "text/event-stream");
        let client = A2aClient::new(endpoint, "planner");
        let result = client
            .send_message_stream("hello", None, &mut |_| StreamControl::Stop)
            .await;
        assert!(matches!(result, Err(Error::Cancelled)));
    }

    #[tokio::test]
    async fn streams_message_events() {
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"id\":\"task-1\",\"status\":{\"state\":\"working\"}}}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"id\":\"task-1\",\"status\":{\"state\":\"completed\"},\"artifacts\":[{\"parts\":[{\"kind\":\"text\",\"text\":\"done\"}]}]}}\n\n"
        );
        let (endpoint, server) = serve_once_with(body.to_string(), "text/event-stream");
        let client = A2aClient::new(endpoint, "planner");
        let mut seen = Vec::new();
        let response = client
            .send_message_stream("hello", Some("ctx-1"), &mut |event| {
                seen.push(event.clone());
                StreamControl::Continue
            })
            .await
            .unwrap();

        assert_eq!(response.state.as_deref(), Some("completed"));
        assert_eq!(response.text.as_deref(), Some("done"));
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["status"]["state"], "working");

        let request = server.join().unwrap();
        assert_eq!(request["method"], "message/stream");
    }

    #[tokio::test]
    async fn sets_push_notification_config() {
        let (endpoint, server) = serve_once(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "id": "cfg-1", "url": "https://example.com/hook" }
            })
            .to_string(),
        );
        let client = A2aClient::new(endpoint, "planner");
        let config = PushNotificationConfig::new("https://example.com/hook").with_token("secret");
        let result = client
            .set_push_notification_config("task-1", &config)
            .await
            .unwrap();
        assert_eq!(result["id"], "cfg-1");

        let request = server.join().unwrap();
        assert_eq!(request["method"], "tasks/pushNotificationConfig/set");
        assert_eq!(request["params"]["id"], "task-1");
        assert_eq!(
            request["params"]["pushNotificationConfig"]["url"],
            "https://example.com/hook"
        );
        assert_eq!(
            request["params"]["pushNotificationConfig"]["token"],
            "secret"
        );
    }

    #[test]
    fn builds_a2a_message_send_request() {
        let request =
            build_send_message(&AgentId::new("planner"), "hello", Some("ctx-1"), "msg-1", 7);
        assert_eq!(request["method"], "message/send");
        assert_eq!(request["params"]["message"]["role"], "user");
        assert_eq!(request["params"]["message"]["parts"][0]["kind"], "text");
        assert_eq!(request["params"]["message"]["parts"][0]["text"], "hello");
        assert_eq!(request["params"]["message"]["contextId"], "ctx-1");
        assert_eq!(request["params"]["message"]["messageId"], "msg-1");
        assert_eq!(request["params"]["message"]["metadata"]["from"], "planner");
    }

    #[test]
    fn extracts_task_state_and_text() {
        let response = A2aResponse::from_value(json!({
            "id": "task-1",
            "status": { "state": "completed" },
            "artifacts": [{
                "parts": [{ "kind": "text", "text": "done" }]
            }]
        }));
        assert_eq!(response.task_id.as_deref(), Some("task-1"));
        assert_eq!(response.state.as_deref(), Some("completed"));
        assert_eq!(response.text.as_deref(), Some("done"));
    }

    #[test]
    fn maps_handoff_payload_to_text() {
        let payload = AgentPayload::Handoff {
            reason: "需要专家".into(),
            task: "排查".into(),
        };
        assert_eq!(payload_text(&payload), "需要专家\n排查");
    }
}
