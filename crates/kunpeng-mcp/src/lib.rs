//! MCP（Model Context Protocol）适配器。
//!
//! 这一层只做 MCP 客户端：传输、JSON-RPC、tools/resources/prompts、sampling
//! 请求处理，并把远端工具包装成 `kunpeng_tools::Tool`。它不依赖 Agent/runtime，
//! 也不读全局配置。
//!
//! 当前实现覆盖 stdio 传输；HTTP/WebSocket transport 可以通过 `McpTransport`
//! 接入，不需要修改 core。

use async_trait::async_trait;
use kunpeng_core::types::ToolDefinition;
use kunpeng_core::{Error, Result};
use kunpeng_tools::{Tool, ToolProvider};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

const PROTOCOL_VERSION: &str = "2024-11-05";
const CLIENT_NAME: &str = "kunpeng";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// server 发起的 JSON-RPC 请求（例如 sampling/createMessage）。
///
/// 传输层收到带 `id` 和 `method` 的消息时调用该 handler，并把返回值写回同一个
/// `id`。普通客户端请求不会经过这里。
#[async_trait]
pub trait InboundHandler: Send + Sync {
    async fn handle(&self, method: &str, params: Value) -> Result<Value>;

    /// 是否在 initialize 时声明 sampling 能力。
    fn supports_sampling(&self) -> bool {
        false
    }
}

/// sampling/createMessage 的宿主实现。
///
/// `params` 保持 MCP 原始结构（messages/modelPreferences/systemPrompt/maxTokens），
/// 返回值应包含 `role`、`content`，可选 `model`、`stopReason`。内核不把 sampling
/// 绑定到具体模型，避免 MCP 适配器反向依赖 Agent。
#[async_trait]
pub trait SamplingHandler: Send + Sync {
    async fn create_message(&self, params: Value) -> Result<Value>;
}

struct NoopInbound;

#[async_trait]
impl InboundHandler for NoopInbound {
    async fn handle(&self, method: &str, _params: Value) -> Result<Value> {
        Err(Error::Other(format!(
            "MCP server 请求 {method} 没有注册 handler"
        )))
    }
}

struct SamplingInbound {
    handler: Arc<dyn SamplingHandler>,
}

#[async_trait]
impl InboundHandler for SamplingInbound {
    async fn handle(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "sampling/createMessage" => self.handler.create_message(params).await,
            other => Err(Error::Other(format!(
                "MCP sampling handler 不支持方法 {other}"
            ))),
        }
    }

    fn supports_sampling(&self) -> bool {
        true
    }
}

/// MCP JSON-RPC 传输。
///
/// `request` 必须返回 id 匹配的响应；中间通知/日志行由传输实现跳过。若 server
/// 发起请求，传输层应调用 `inbound` 处理。单独抽成 trait 是为了让测试不启动
/// 进程，也方便以后接 HTTP/WebSocket 传输。
#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn request(&self, request: Value, inbound: &dyn InboundHandler) -> Result<Value>;

    /// MCP 的 `notifications/initialized` 等无 id 消息。
    async fn notify(&self, _notification: Value) -> Result<()> {
        Ok(())
    }
}

/// MCP 客户端。一个实例对应一个 server 连接。
pub struct McpClient {
    transport: Arc<dyn McpTransport>,
    inbound: Arc<dyn InboundHandler>,
    next_id: AtomicU64,
}

impl McpClient {
    pub fn new(transport: Arc<dyn McpTransport>) -> Self {
        Self {
            transport,
            inbound: Arc::new(NoopInbound),
            next_id: AtomicU64::new(1),
        }
    }

    /// 注册 server 发起的请求处理器。
    pub fn with_inbound_handler(mut self, handler: Arc<dyn InboundHandler>) -> Self {
        self.inbound = handler;
        self
    }

    /// 注册 sampling/createMessage 处理器，并让 initialize 声明 sampling 能力。
    pub fn with_sampling_handler(mut self, handler: Arc<dyn SamplingHandler>) -> Self {
        self.inbound = Arc::new(SamplingInbound { handler });
        self
    }

    /// 发送 initialize，并通知 server 客户端已就绪。
    pub async fn initialize(&self) -> Result<Value> {
        let capabilities = if self.inbound.supports_sampling() {
            json!({ "sampling": {} })
        } else {
            json!({})
        };
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": capabilities,
                    "clientInfo": {
                        "name": CLIENT_NAME,
                        "version": CLIENT_VERSION
                    }
                }),
            )
            .await?;
        self.transport
            .notify(json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {}
            }))
            .await?;
        Ok(result)
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolDefinition>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Other("MCP tools/list 响应缺少 tools 数组".into()))?;
        tools.iter().map(McpToolDefinition::from_value).collect()
    }

    /// 调用远端工具，把 MCP content 数组压平成文本。
    ///
    /// MCP 允许一个调用返回多个 content part；模型协议只接受字符串，所以这里
    /// 按顺序拼接 text part，其余类型保留 JSON 表示。
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<String> {
        let result = self
            .request(
                "tools/call",
                json!({
                    "name": name,
                    "arguments": arguments
                }),
            )
            .await?;

        let text = content_text(&result);
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(Error::Other(format!("MCP 工具 {name} 返回错误: {text}")));
        }
        Ok(text)
    }

    /// 列出资源首页。需要分页时使用 `list_resources_page`。
    pub async fn list_resources(&self) -> Result<Vec<McpResource>> {
        Ok(self.list_resources_page(None).await?.items)
    }

    pub async fn list_resources_page(&self, cursor: Option<&str>) -> Result<McpPage<McpResource>> {
        self.list_page(
            "resources/list",
            "resources",
            cursor,
            McpResource::from_value,
        )
        .await
    }

    /// 读取一个资源。MCP 允许一次返回多个 content（文本或 base64 blob）。
    pub async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>> {
        let result = self
            .request("resources/read", json!({ "uri": uri }))
            .await?;
        let contents = result
            .get("contents")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Other("MCP resources/read 响应缺少 contents 数组".into()))?;
        contents
            .iter()
            .map(McpResourceContent::from_value)
            .collect()
    }

    pub async fn list_resource_templates(&self) -> Result<Vec<McpResourceTemplate>> {
        Ok(self.list_resource_templates_page(None).await?.items)
    }

    pub async fn list_resource_templates_page(
        &self,
        cursor: Option<&str>,
    ) -> Result<McpPage<McpResourceTemplate>> {
        self.list_page(
            "resources/templates/list",
            "resourceTemplates",
            cursor,
            McpResourceTemplate::from_value,
        )
        .await
    }

    pub async fn list_prompts(&self) -> Result<Vec<McpPrompt>> {
        Ok(self.list_prompts_page(None).await?.items)
    }

    pub async fn list_prompts_page(&self, cursor: Option<&str>) -> Result<McpPage<McpPrompt>> {
        self.list_page("prompts/list", "prompts", cursor, McpPrompt::from_value)
            .await
    }

    /// 获取 prompt 模板并填充参数。返回的 messages 保留 MCP 原始结构。
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: &BTreeMap<String, String>,
    ) -> Result<McpPromptResult> {
        let result = self
            .request(
                "prompts/get",
                json!({
                    "name": name,
                    "arguments": arguments
                }),
            )
            .await?;
        McpPromptResult::from_value(result)
    }

    async fn list_page<T>(
        &self,
        method: &str,
        key: &str,
        cursor: Option<&str>,
        parse: fn(&Value) -> Result<T>,
    ) -> Result<McpPage<T>> {
        let params = match cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        let result = self.request(method, params).await?;
        let values = result
            .get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Other(format!("MCP {method} 响应缺少 {key} 数组")))?;
        let items = values.iter().map(parse).collect::<Result<Vec<_>>>()?;
        Ok(McpPage {
            items,
            next_cursor: result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        let response = self
            .transport
            .request(request, self.inbound.as_ref())
            .await?;

        if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            return Err(Error::Other(format!(
                "MCP {method} 失败（{code}）: {message}"
            )));
        }

        response
            .get("result")
            .cloned()
            .ok_or_else(|| Error::Other(format!("MCP {method} 响应缺少 result")))
    }
}

/// MCP `tools/list` 的单个工具描述。
#[derive(Debug, Clone)]
pub struct McpToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl McpToolDefinition {
    fn from_value(value: &Value) -> Result<Self> {
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| Error::Other("MCP 工具缺少 name".into()))?
            .to_string();
        Ok(Self {
            name,
            description: value
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input_schema: value
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" })),
        })
    }
}

/// 分页结果。`next_cursor` 为 None 表示没有下一页。
#[derive(Debug, Clone)]
pub struct McpPage<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

/// MCP resource 描述。
#[derive(Debug, Clone)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: Option<String>,
    pub size: Option<u64>,
    pub raw: Value,
}

impl McpResource {
    fn from_value(value: &Value) -> Result<Self> {
        Ok(Self {
            uri: required_str(value, "uri", "resource")?,
            name: optional_str(value, "name").unwrap_or_default(),
            description: optional_str(value, "description").unwrap_or_default(),
            mime_type: optional_str(value, "mimeType"),
            size: value.get("size").and_then(Value::as_u64),
            raw: value.clone(),
        })
    }
}

/// 资源读取结果。文本资源用 `text`，二进制资源用 base64 的 `blob`。
#[derive(Debug, Clone)]
pub struct McpResourceContent {
    pub uri: String,
    pub mime_type: Option<String>,
    pub text: Option<String>,
    pub blob: Option<String>,
    pub raw: Value,
}

impl McpResourceContent {
    fn from_value(value: &Value) -> Result<Self> {
        Ok(Self {
            uri: required_str(value, "uri", "resource content")?,
            mime_type: optional_str(value, "mimeType"),
            text: optional_str(value, "text"),
            blob: optional_str(value, "blob"),
            raw: value.clone(),
        })
    }
}

/// resource template，支持 RFC 6570 风格 URI 模板。
#[derive(Debug, Clone)]
pub struct McpResourceTemplate {
    pub uri_template: String,
    pub name: String,
    pub description: String,
    pub mime_type: Option<String>,
    pub raw: Value,
}

impl McpResourceTemplate {
    fn from_value(value: &Value) -> Result<Self> {
        Ok(Self {
            uri_template: required_str(value, "uriTemplate", "resource template")?,
            name: optional_str(value, "name").unwrap_or_default(),
            description: optional_str(value, "description").unwrap_or_default(),
            mime_type: optional_str(value, "mimeType"),
            raw: value.clone(),
        })
    }
}

/// MCP prompt 描述。
#[derive(Debug, Clone)]
pub struct McpPrompt {
    pub name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArgument>,
    pub raw: Value,
}

impl McpPrompt {
    fn from_value(value: &Value) -> Result<Self> {
        let arguments = value
            .get("arguments")
            .and_then(Value::as_array)
            .map(|arguments| {
                arguments
                    .iter()
                    .map(McpPromptArgument::from_value)
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            name: required_str(value, "name", "prompt")?,
            description: optional_str(value, "description").unwrap_or_default(),
            arguments,
            raw: value.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct McpPromptArgument {
    pub name: String,
    pub description: String,
    pub required: bool,
    pub raw: Value,
}

impl McpPromptArgument {
    fn from_value(value: &Value) -> Result<Self> {
        Ok(Self {
            name: required_str(value, "name", "prompt argument")?,
            description: optional_str(value, "description").unwrap_or_default(),
            required: value
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            raw: value.clone(),
        })
    }
}

/// prompt 渲染结果。messages 保留 MCP 原始 message/content 结构。
#[derive(Debug, Clone)]
pub struct McpPromptResult {
    pub description: Option<String>,
    pub messages: Vec<Value>,
    pub raw: Value,
}

impl McpPromptResult {
    fn from_value(value: Value) -> Result<Self> {
        let messages = value
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(Self {
            description: value
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            messages,
            raw: value,
        })
    }
}

fn required_str(value: &Value, key: &str, kind: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Other(format!("MCP {kind} 缺少 {key}")))
}

fn optional_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// 构造常见的文本 sampling 响应，方便宿主实现 `SamplingHandler`。
pub fn sampling_text_result(model: impl Into<String>, text: impl Into<String>) -> Value {
    json!({
        "role": "assistant",
        "content": { "type": "text", "text": text.into() },
        "model": model.into(),
        "stopReason": "endTurn"
    })
}

/// 把 MCP 工具暴露成内核 `Tool`。
pub struct McpTool {
    client: Arc<McpClient>,
    definition: ToolDefinition,
}

impl McpTool {
    fn new(client: Arc<McpClient>, definition: McpToolDefinition) -> Self {
        Self {
            client,
            definition: ToolDefinition {
                name: definition.name,
                description: definition.description,
                input_schema: definition.input_schema,
            },
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn call(&self, arguments: Value) -> Result<String> {
        self.client
            .call_tool(&self.definition.name, arguments)
            .await
    }
}

/// 一个 MCP server 的工具集合，可作为 `ToolProvider` 批量注册。
pub struct McpToolset {
    client: Arc<McpClient>,
    tools: Vec<Arc<dyn Tool>>,
}

impl McpToolset {
    /// 连接 transport，执行 initialize 并拉取工具列表。
    pub async fn connect(transport: Arc<dyn McpTransport>) -> Result<Self> {
        Self::from_client(Arc::new(McpClient::new(transport))).await
    }

    pub async fn from_client(client: Arc<McpClient>) -> Result<Self> {
        client.initialize().await?;
        let definitions = client.list_tools().await?;
        let tools: Vec<Arc<dyn Tool>> = definitions
            .into_iter()
            .map(|definition| {
                Arc::new(McpTool::new(Arc::clone(&client), definition)) as Arc<dyn Tool>
            })
            .collect();
        Ok(Self { client, tools })
    }

    /// 重新拉取工具列表。适用于 server 动态增删工具的场景。
    pub async fn refresh(&mut self) -> Result<()> {
        let definitions = self.client.list_tools().await?;
        self.tools = definitions
            .into_iter()
            .map(|definition| {
                Arc::new(McpTool::new(Arc::clone(&self.client), definition)) as Arc<dyn Tool>
            })
            .collect();
        Ok(())
    }

    pub fn client(&self) -> Arc<McpClient> {
        Arc::clone(&self.client)
    }
}

#[async_trait]
impl ToolProvider for McpToolset {
    async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(self.tools.clone())
    }
}

/// stdio 传输。stdout 只读 JSON-RPC，一行一条消息。
pub struct StdioTransport {
    inner: Mutex<StdioInner>,
}

struct StdioInner {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for StdioInner {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl StdioTransport {
    /// 启动子进程并连接 stdin/stdout。
    ///
    /// stderr 继承父进程，server 日志不会被当成 JSON 解析。
    pub fn connect<C: AsRef<OsStr>, A: AsRef<OsStr>>(command: C, args: &[A]) -> Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Other("MCP server stdin 不可用".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Other("MCP server stdout 不可用".into()))?;
        Ok(Self {
            inner: Mutex::new(StdioInner {
                child,
                stdin,
                stdout: BufReader::new(stdout),
            }),
        })
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn request(&self, request: Value, inbound: &dyn InboundHandler) -> Result<Value> {
        let id = request.get("id").cloned();
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');

        let mut inner = self.inner.lock().await;
        inner.stdin.write_all(line.as_bytes()).await?;
        inner.stdin.flush().await?;

        loop {
            let mut buffer = String::new();
            let read = inner.stdout.read_line(&mut buffer).await?;
            if read == 0 {
                return Err(Error::Other("MCP server 关闭了 stdout".into()));
            }
            let trimmed = buffer.trim();
            if trimmed.is_empty() {
                continue;
            }
            let response: Value = serde_json::from_str(trimmed)?;

            // server 发起的请求：调用宿主 handler，并把结果写回同一个 id。
            if let (Some(method), Some(request_id)) = (
                response.get("method").and_then(Value::as_str),
                response.get("id").cloned(),
            ) {
                let params = response.get("params").cloned().unwrap_or_else(|| json!({}));
                let reply = match inbound.handle(method, params).await {
                    Ok(result) => json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "result": result
                    }),
                    Err(error) => json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": { "code": -32603, "message": error.to_string() }
                    }),
                };
                let mut reply_line = serde_json::to_string(&reply)?;
                reply_line.push('\n');
                inner.stdin.write_all(reply_line.as_bytes()).await?;
                inner.stdin.flush().await?;
                continue;
            }

            // 通知（没有 id）和日志直接跳过；只接受 id 匹配的响应。
            if response.get("id") == id.as_ref() {
                return Ok(response);
            }
        }
    }

    async fn notify(&self, notification: Value) -> Result<()> {
        let mut line = serde_json::to_string(&notification)?;
        line.push('\n');
        let mut inner = self.inner.lock().await;
        inner.stdin.write_all(line.as_bytes()).await?;
        inner.stdin.flush().await?;
        Ok(())
    }
}

fn content_text(result: &Value) -> String {
    let Some(parts) = result.get("content").and_then(Value::as_array) else {
        return result.to_string();
    };
    let mut texts = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    texts.push(text.to_string());
                }
            }
            _ => texts.push(part.to_string()),
        }
    }
    if texts.is_empty() {
        result.to_string()
    } else {
        texts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::sync::Mutex;

    struct MockTransport {
        responses: Mutex<VecDeque<Value>>,
        requests: Mutex<Vec<Value>>,
        notifications: Mutex<Vec<Value>>,
    }

    impl MockTransport {
        fn with_responses(responses: impl IntoIterator<Item = Value>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
                notifications: Mutex::new(Vec::new()),
            }
        }

        async fn requests(&self) -> Vec<Value> {
            self.requests.lock().await.clone()
        }

        async fn notifications(&self) -> Vec<Value> {
            self.notifications.lock().await.clone()
        }
    }

    #[async_trait]
    impl McpTransport for MockTransport {
        async fn request(&self, request: Value, _inbound: &dyn InboundHandler) -> Result<Value> {
            self.requests.lock().await.push(request);
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| Error::Other("MockTransport 没有剩余响应".into()))
        }

        async fn notify(&self, notification: Value) -> Result<()> {
            self.notifications.lock().await.push(notification);
            Ok(())
        }
    }

    fn initialize_result() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "mock", "version": "1" }
            }
        })
    }

    fn tools_result() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "回显",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } }
                    }
                }]
            }
        })
    }

    #[tokio::test]
    async fn lists_and_calls_tools() {
        let transport = Arc::new(MockTransport::with_responses([
            initialize_result(),
            tools_result(),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "result": {
                    "content": [{ "type": "text", "text": "hello" }]
                }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "result": { "tools": [] }
            }),
        ]));
        let mock = Arc::clone(&transport);
        let mut toolset = McpToolset::connect(transport).await.unwrap();
        let tools = toolset.tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].definition().name, "echo");

        let mut registry = kunpeng_tools::ToolRegistry::new();
        registry.register_provider(&toolset).await.unwrap();
        assert_eq!(registry.definitions()[0].name, "echo");

        assert_eq!(
            tools[0].call(json!({ "text": "hello" })).await.unwrap(),
            "hello"
        );

        let requests = mock.requests().await;
        assert_eq!(requests[0]["method"], "initialize");
        assert_eq!(requests[1]["method"], "tools/list");
        assert_eq!(requests[2]["method"], "tools/call");
        assert_eq!(requests[2]["params"]["name"], "echo");
        assert_eq!(
            mock.notifications().await[0]["method"],
            "notifications/initialized"
        );

        toolset.refresh().await.unwrap();
        assert!(toolset.tools().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reports_json_rpc_errors() {
        let transport = Arc::new(MockTransport::with_responses([
            initialize_result(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "error": { "code": -32601, "message": "method not found" }
            }),
        ]));
        let error = match McpToolset::connect(transport).await {
            Ok(_) => panic!("应当返回 JSON-RPC 错误"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("method not found"));
    }

    #[tokio::test]
    async fn flags_tool_error_content() {
        let transport = Arc::new(MockTransport::with_responses([
            initialize_result(),
            tools_result(),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "result": {
                    "isError": true,
                    "content": [{ "type": "text", "text": "boom" }]
                }
            }),
        ]));
        let toolset = McpToolset::connect(transport).await.unwrap();
        let tool = &toolset.tools().await.unwrap()[0];
        let error = tool.call(json!({})).await.unwrap_err();
        assert!(error.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn reads_resources_and_prompts() {
        let transport = Arc::new(MockTransport::with_responses([
            initialize_result(),
            json!({
                "id": 2,
                "result": {
                    "resources": [{
                        "uri": "file:///a.txt",
                        "name": "a",
                        "mimeType": "text/plain",
                        "size": 3
                    }],
                    "nextCursor": "next"
                }
            }),
            json!({
                "id": 3,
                "result": {
                    "contents": [{
                        "uri": "file:///a.txt",
                        "mimeType": "text/plain",
                        "text": "abc"
                    }]
                }
            }),
            json!({
                "id": 4,
                "result": {
                    "resourceTemplates": [{
                        "uriTemplate": "file:///{path}",
                        "name": "files"
                    }]
                }
            }),
            json!({
                "id": 5,
                "result": {
                    "prompts": [{
                        "name": "summarize",
                        "description": "总结",
                        "arguments": [{ "name": "text", "required": true }]
                    }]
                }
            }),
            json!({
                "id": 6,
                "result": {
                    "description": "summary",
                    "messages": [{
                        "role": "user",
                        "content": { "type": "text", "text": "总结 abc" }
                    }]
                }
            }),
        ]));
        let client = McpClient::new(transport);
        client.initialize().await.unwrap();

        let page = client.list_resources_page(None).await.unwrap();
        assert_eq!(page.items[0].uri, "file:///a.txt");
        assert_eq!(page.items[0].size, Some(3));
        assert_eq!(page.next_cursor.as_deref(), Some("next"));

        let contents = client.read_resource("file:///a.txt").await.unwrap();
        assert_eq!(contents[0].text.as_deref(), Some("abc"));

        let templates = client.list_resource_templates().await.unwrap();
        assert_eq!(templates[0].uri_template, "file:///{path}");

        let prompts = client.list_prompts().await.unwrap();
        assert_eq!(prompts[0].name, "summarize");
        assert!(prompts[0].arguments[0].required);

        let mut arguments = BTreeMap::new();
        arguments.insert("text".into(), "abc".into());
        let prompt = client.get_prompt("summarize", &arguments).await.unwrap();
        assert_eq!(prompt.description.as_deref(), Some("summary"));
        assert_eq!(prompt.messages.len(), 1);
    }

    struct EchoSampling;

    #[async_trait]
    impl SamplingHandler for EchoSampling {
        async fn create_message(&self, params: Value) -> Result<Value> {
            assert_eq!(params["maxTokens"], 32);
            Ok(sampling_text_result("sampler", "sampled"))
        }
    }

    struct SamplingTransport {
        requests: Mutex<Vec<Value>>,
    }

    #[async_trait]
    impl McpTransport for SamplingTransport {
        async fn request(&self, request: Value, inbound: &dyn InboundHandler) -> Result<Value> {
            self.requests.lock().await.push(request.clone());
            match request["method"].as_str().unwrap_or_default() {
                "initialize" => Ok(initialize_result()),
                "tools/list" => Ok(tools_result()),
                "tools/call" => {
                    let sampled = inbound
                        .handle(
                            "sampling/createMessage",
                            json!({
                                "messages": [{ "role": "user", "content": { "type": "text", "text": "hi" } }],
                                "maxTokens": 32
                            }),
                        )
                        .await?;
                    Ok(json!({
                        "id": request["id"],
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": sampled["content"]["text"]
                            }]
                        }
                    }))
                }
                other => Err(Error::Other(format!("unexpected method {other}"))),
            }
        }
    }

    #[tokio::test]
    async fn handles_server_sampling_requests() {
        let transport = Arc::new(SamplingTransport {
            requests: Mutex::new(Vec::new()),
        });
        let mock = Arc::clone(&transport);
        let client =
            Arc::new(McpClient::new(transport).with_sampling_handler(Arc::new(EchoSampling)));
        let toolset = McpToolset::from_client(Arc::clone(&client)).await.unwrap();
        let tools = toolset.tools().await.unwrap();
        assert_eq!(tools[0].call(json!({})).await.unwrap(), "sampled");

        let requests = mock.requests.lock().await;
        assert_eq!(requests[0]["method"], "initialize");
        assert_eq!(requests[0]["params"]["capabilities"]["sampling"], json!({}));
    }
}
