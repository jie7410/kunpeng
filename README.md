# Kunpeng

一个与 UI 无关的 Rust LLM Agent 内核，支持 OpenAI 兼容端点与 Anthropic Messages API。

核心能力：

- 有状态 `Agent`：持有会话历史，支持 `send`、`resume`、历史导入/导出
- 工具调用循环：工具注册、schema 声明、执行超时、异步审批、错误回传
- 事件流：assistant 增量/完整回复、工具开始/结束、最终停止原因、错误
- 流式输出：OpenAI / Anthropic SSE 文本增量，工具参数分片自动累积
- 扩展思考：Anthropic thinking，流式 `thinking_delta`，原始 block 与签名原样回放
- 服务端工具：Anthropic `server_tool_use` 原样回放，`pause_turn` 自动续跑
- Citations：原始 citation 对象投影到 `Assistant.citations`，流式 `citations_delta` 累积
- 多模态：文本/图片 content parts，其他类型可用 provider 原始块透传
- 请求扩展：`tool_choice`、`stop_sequences`、`ProviderOptions` 透传
- 上下文预算：按轮次滑动窗口裁剪，保留 tool_call/tool_result 分组
- 可插拔策略：`AgentStrategy` + 默认 `ReActAgent`
- 记忆与会话：`Memory`、`SessionStore`、内存实现、RAG 上下文注入
- 多智能体：AgentBus、LocalBus、Handoff、Worker / Supervisor
- 取消令牌：中止进行中的模型请求，并保持历史可回放
- 协议适配：OpenAI Chat Completions / Anthropic Messages
- 外部 Agent 协议：MCP tools/resources/prompts/sampling、A2A 同步/流式/push 配置、ANP JSON-LD + DID 发现，均为独立 crate
- 重试：408/429/5xx 指数退避，尊重 `Retry-After`
- 配置：宿主层环境变量适配 + 核心程序化 builder；API key 在 `Debug` 输出中脱敏
- 无 UI 依赖：CLI、Tauri 或测试共用同一套 API

> 当前版本 `0.1.0`，API 仍可能调整。

文档入口见 [docs/README.md](docs/README.md)。详细分层、调用时序和设计取舍见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

---

## 环境要求

- Rust stable，edition 2021
- 能访问目标 LLM 端点
- 使用 rustls，不依赖系统 OpenSSL，便于跨平台分发

程序不会自动读取 `.env`。请在 shell 中设置环境变量，或使用 `provider::LlmConfig::new(...)` 在代码里构造配置。

---

## 快速开始

```bash
git clone https://github.com/<your-github-user>/kunpeng.git
cd kunpeng
```

`<your-github-user>` 是占位符，关联远端仓库后替换。

### Windows PowerShell

```powershell
$env:LLM_PROVIDER = "openai"
$env:LLM_API_KEY  = "sk-..."
# $env:LLM_MODEL  = "gpt-4o-mini"
cargo run
```

### Linux / macOS

```bash
export LLM_PROVIDER=openai
export LLM_API_KEY=sk-...
# export LLM_MODEL=gpt-4o-mini
cargo run
```

`cargo run` 会进入交互式 CLI。直接输入问题，流式输出会边生成边打印，结束后显示停止原因。

| 命令 | 作用 |
|---|---|
| `/help` | 显示帮助 |
| `/clear` | 清空历史，保留 system prompt |
| `/retry` | 重试当前历史（用于上一次请求失败） |
| `/exit` / `/quit` | 退出 |

例如：

```text
you> 北京今天天气如何？
tool started: get_weather {"city":"北京"}
tool finished: get_weather Success { content: "北京 今天晴..." }
北京晴
[stop: Completed]
you> /exit
```

运行独立示例（同样是 echo 工具，非交互）：

```bash
cargo run --example basic_agent
```

---

## 配置

环境变量由 CLI 宿主读取（`kunpeng_cli::config`）；`kunpeng-provider` 的 `LlmConfig` 只接受已解析的值。Tauri 或其他 UI 可以改用配置文件、命令行参数或自己的设置面板，然后走同一套 builder。

| 环境变量 | 必填 | 默认值 | 说明 |
|---|---|---|---|
| `LLM_PROVIDER` | 否 | `openai` | 协议或别名 |
| `LLM_API_KEY` | 是 | - | API key |
| `LLM_BASE_URL` | 否 | 按 provider 决定 | 覆盖默认地址 |
| `LLM_MODEL` | 否 | 按 provider 决定 | 覆盖默认模型 |
| `LLM_TIMEOUT_SECS` | 否 | `120` | 单次 HTTP 请求总超时，必须大于 0 |
| `LLM_MAX_RETRIES` | 否 | `2` | 重试次数，上限 10 |

### Provider 别名与默认值

| `LLM_PROVIDER` | 默认 base URL | 默认模型 | 协议实现 |
|---|---|---|---|
| `openai` | `https://api.openai.com/v1` | `gpt-4o-mini` | OpenAI |
| `openai-compatible` | 无，必须显式设置 | 无，必须显式设置 | OpenAI |
| `anthropic` / `claude` | `https://api.anthropic.com` | `claude-sonnet-4-5` | Anthropic |
| `zhipu` / `glm` / `bigmodel` | `https://open.bigmodel.cn/api/paas/v4` | `glm-4.6` | OpenAI 兼容 |

`zhipu` 等别名复用 OpenAI 适配器，只是默认地址和模型不同。

### 程序化配置

```rust
use kunpeng::provider::LlmConfig;

let config = LlmConfig::new("openai", "sk-...")
    .with_model("gpt-4o-mini")
    .with_timeout_secs(60)
    .with_max_retries(2);

let registry = kunpeng::default_adapter_registry()?;
let model = config.build(&registry)?;
```

`openai-compatible` 必须同时提供 base URL 和模型名，避免把错误推迟到运行期。

### 自定义 Adapter

`kunpeng-provider` 不内置厂商名；OpenAI/Anthropic 分别在独立 crate 中实现 `Adapter`。自定义协议实现 `Adapter` 的元数据（`name`、`aliases`、默认 base URL/模型）和 `chat`/`chat_stream` 翻译，然后注册到 `AdapterRegistry`：

```rust
use kunpeng::{
    adapter_openai::OpenAiAdapter,
    provider::{AdapterRegistry, LlmConfig},
};

let mut registry = AdapterRegistry::new();
registry.register(OpenAiAdapter::openai())?;
registry.register(OpenAiAdapter::compatible())?;

let model = LlmConfig::new("openai", "sk-...").build(&registry)?;
```

完整参考实现见 [`crates/kunpeng-adapter-openai/src/lib.rs`](crates/kunpeng-adapter-openai/src/lib.rs) 和 [`crates/kunpeng-adapter-anthropic/src/lib.rs`](crates/kunpeng-adapter-anthropic/src/lib.rs)。

内置注册表：

```rust
let registry = kunpeng::default_adapter_registry()?;
let model = LlmConfig::new("openai", "sk-...").build(&registry)?;
```

### 错误分类

`kunpeng_core::Error` 不把所有失败压成字符串：

| 变体 | 用途 |
|---|---|
| `Http { kind, message, source }` | 传输失败；`kind` 区分超时/连接/请求/响应体 |
| `Api { provider, status, body }` | 非 429 的服务端错误 |
| `RateLimited { provider, retry_after, body }` | 429 限流，可读取建议等待时间 |
| `Json` | 通用 serde JSON 错误 |
| `ParseResponse(Box<_>)` | 带 provider/body 上下文的响应解析失败 |
| `InvalidToolArguments(Box<_>)` | 工具参数 JSON 错误 |
| `Cancelled` | 调用方取消，包括流式回调返回 `StreamControl::Stop` |

### 流式控制

`chat_stream` 回调接收借用的 `StreamDelta<'_>`，避免默认回退或适配器层重复 clone；返回 `StreamControl::Stop` 可让消费方中途停止读取并得到 `Error::Cancelled`。

---

## 作为 library 使用

```toml
[dependencies]
kunpeng = { git = "https://github.com/<your-github-user>/kunpeng" }
anyhow = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
async-trait = "0.1"
serde_json = "1"
```

完整可运行代码见 [`crates/kunpeng-cli/examples/basic_agent.rs`](crates/kunpeng-cli/examples/basic_agent.rs)：

```rust
use async_trait::async_trait;
use kunpeng::{
    agents::Agent,
    core::{Message, Result, ToolDefinition},
    provider::LlmConfig,
    runtime::{AgentEvent, AgentHooks, RunConfig},
    tools::{Tool, ToolRegistry},
};

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "回显输入文本".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "要回显的文本" }
                },
                "required": ["text"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        Ok(arguments
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 宿主层负责读取环境变量；核心库不读 env。
    let config = LlmConfig::new(
        std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "openai".into()),
        std::env::var("LLM_API_KEY")?,
    );
    let registry = kunpeng::default_adapter_registry()?;
    let model = config.build(&registry)?;

    let mut tools = ToolRegistry::new();
    tools.register(EchoTool)?;

    let mut agent = Agent::with_history(
        model,
        tools,
        RunConfig::default(),
        vec![Message::system("你是一个可以调用工具的助手。")],
    );

    let on_event = |event: AgentEvent| match event {
        AgentEvent::Assistant {
            content: Some(text),
            ..
        } => println!("assistant: {text}"),
        AgentEvent::ToolFinished { execution } => {
            println!("tool {}: {:?}", execution.name, execution.outcome);
        }
        _ => {}
    };
    let hooks = AgentHooks::default().with_event(&on_event);

    let stop = agent.send(Message::user("请回显 hello"), &hooks).await?;
    println!("stop: {stop:?}");
    Ok(())
}
```

---

## 工具

实现 `Tool` 后注册到 `ToolRegistry`：

- 工具名必须为 1-64 个 ASCII 字母、数字、下划线或连字符
- 同名工具会被拒绝，不会静默覆盖
- `input_schema` 原样传给 provider，内核不按 schema 做参数校验
- `call` 返回 `Result<String>`；错误会被包装成 `is_error=true` 的 `tool_result` 回传给模型
- 单个工具默认有 60 秒硬超时，可通过 `RunConfig::tool_timeout` 调整或关闭

### 工具策略

`ToolPolicy` 支持 per-tool 超时和输出截断：

```rust
use std::time::Duration;
use kunpeng::tools::{ToolPolicy, ToolRegistry};

let mut tools = ToolRegistry::new();
tools.register_with_policy(
    my_tool,
    ToolPolicy::default()
        .with_timeout(Duration::from_secs(5))
        .with_max_output_bytes(32 * 1024),
)?;
```

- 单个工具超时优先于 `RunConfig::tool_timeout`；
- 输出超过上限时按 UTF-8 边界截断，并追加 `...[truncated]`；
- 错误信息不截断，保留完整诊断。

### 内置工具

`tools-builtin` 默认开启，提供：

| 工具 | 说明 |
|---|---|
| `current_time` | 当前 Unix 秒 |
| `fs_read` | 读取沙箱目录内文件，拒绝越界路径 |
| `http_fetch` | GET 文本，要求显式 host allowlist |

```rust
use std::time::Duration;
use kunpeng::tools::{
    ToolPolicy,
    builtin::{FsReadTool, HttpFetchTool, TimeTool},
};

let mut tools = ToolRegistry::new();
tools.register(TimeTool)?;
tools.register_with_policy(
    FsReadTool::new("./sandbox", 64 * 1024)?,
    ToolPolicy::default().with_timeout(Duration::from_secs(5)),
)?;
tools.register_with_policy(
    HttpFetchTool::new(vec!["example.com".into()], 64 * 1024)?,
    ToolPolicy::default().with_timeout(Duration::from_secs(10)),
)?;
```

关闭内置工具：

```bash
cargo build --no-default-features
```

高风险工具（shell 等）没有默认内置，需要显式实现、审批和 feature 隔离。

### 工具审批

审批是异步 trait，适合 Tauri 先发事件、再等待前端通过 IPC 返回结果：

```rust
use async_trait::async_trait;
use kunpeng::{
    core::ToolCall,
    runtime::{AgentHooks, ToolApprover, ToolDecision},
};
use std::{
    collections::HashMap,
    sync::Mutex,
};
use tokio::sync::oneshot;

struct UiApprover {
    pending: Mutex<HashMap<String, oneshot::Sender<ToolDecision>>>,
}

#[async_trait]
impl ToolApprover for UiApprover {
    async fn approve(&self, call: &ToolCall) -> ToolDecision {
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(call.id.clone(), sender);

        // Tauri 示例：
        // app.emit("tool-approval-request", call).unwrap();
        // 前端确认后由 command 取出 sender 并 send(Allow/Deny)

        receiver
            .await
            .unwrap_or_else(|_| ToolDecision::Deny("UI 已关闭".into()))
    }
}

let approver = UiApprover {
    pending: Mutex::new(HashMap::new()),
};
let hooks = AgentHooks::default().with_approver(&approver);
```

行为：

- `approve: None` 表示全部放行；
- 审批等待期间可以取消，取消后历史会补齐取消结果；
- `RunConfig::approval_timeout` 默认 300 秒，超时按 `Deny("审批超时")` 处理；设为 `None` 表示一直等；
- 被拒绝的调用不会执行，但会写入错误结果，模型能据此换方案；
- `ToolStarted` 只在审批通过后发出。

---

## 事件与停止原因

`AgentEvent` 顺序：

```text
AssistantDelta* / ThinkingDelta* → Assistant → (ToolStarted → ToolFinished)* → Finished
```

| 事件 | 时机 |
|---|---|
| `AssistantDelta` | 流式正文分片；临时内容，不能当最终消息 |
| `ThinkingDelta` | 流式思考分片；只在 Anthropic 开启 thinking 时出现 |
| `Assistant` | 单次模型回复已写入历史，包含 `content` / `thinking` / `citations` / `server_tool_uses` |
| `ToolStarted` | 审批通过、工具即将执行 |
| `ToolFinished` | 工具成功、失败或被拒绝 |
| `Finished` | 正常结束，带 `StopReason` |
| `Error` | 模型请求失败；历史保留，可 `resume()` |

`RunConfig::stream` 默认 `true`。取消或流式解析失败时，UI 应丢弃当前增量缓冲；以最终的 `Assistant` 或 `Error` 为准。兼容端点不支持 SSE 时：

```rust
let config = RunConfig::default().with_stream(false);
```

### Extended thinking

当前仅 Anthropic 支持。CLI 可以直接用环境变量开启：

```powershell
$env:LLM_THINKING_BUDGET = "2048"
cargo run
```

library 用法：

```rust
use kunpeng::{core::ThinkingConfig, runtime::RunConfig};

let config = RunConfig::default().with_thinking(ThinkingConfig::new(2048));
```

约束：

- `budget_tokens` 最小 1024，且必须小于 `max_tokens`
- 开启 thinking 时 `temperature` 必须为 1 或不设置
- thinking 文本通过 `ThinkingDelta` / `Assistant.thinking` 展示
- `Message::Assistant.provider_data` 保存原始 thinking/signature 和未知 block，下一轮自动原样回放；不要手动修改它

### Server tools 与 citations

服务端工具由 Anthropic 执行，不会进入本地 `ToolCall` 或 `ToolRegistry`。CLI 可以通过 JSON 数组声明：

```powershell
$env:LLM_SERVER_TOOLS = '[{"type":"web_search_20250305","name":"web_search","max_uses":3}]'
```

library 用法：

```rust
use kunpeng::runtime::RunConfig;

let config = RunConfig::default().with_server_tools(vec![serde_json::json!({
    "type": "web_search_20250305",
    "name": "web_search",
    "max_uses": 3
})]);
```

行为：

- 服务端工具调用通过 `Assistant.server_tool_uses` 只读展示；
- `server_tool_use` / `*_tool_result` 保留在 `provider_data`，下一轮原样回放；
- Anthropic 返回 `pause_turn` 时，Agent 自动继续请求，不要求用户再发消息；
- 文本 block 的 citations 进入 `Assistant.citations`；流式 `citations_delta` 会同时补进原始 text block；
- 服务端工具不会触发本地审批、超时或 `Tool::call`。

注意：`server_tools` 只支持 Anthropic；OpenAI 适配器会返回配置错误。

### 多模态与请求扩展

用户消息可以保持纯文本，也可以使用 content parts：

```rust
use kunpeng::{
    core::{ContentPart, ImageDetail, ImageSource, Message},
    runtime::RunConfig,
};

let message = Message::user_parts(vec![
    ContentPart::Text {
        text: "这张图里有什么？".into(),
    },
    ContentPart::Image {
        source: ImageSource::Url {
            url: "https://example.com/a.png".into(),
        },
        detail: Some(ImageDetail::High),
    },
]);
```

本地图片：

```rust
let image = ContentPart::Image {
    source: ImageSource::Base64 {
        media_type: "image/png".into(),
        data: "<base64>".into(),
    },
    detail: None,
};
```

映射规则：

| 语义类型 | OpenAI | Anthropic |
|---|---|---|
| `Content::Text` | content 字符串 | text block |
| `ContentPart::Image` + Url | `image_url` | image `source.url` |
| `ContentPart::Image` + Base64 | data URL | image `source.base64` |
| `ContentPart::Other { provider }` | 仅 provider=`openai` 透传 | 仅 provider=`anthropic` 透传 |
| `ImageDetail` | `detail` 字段 | 忽略 |

其他多模态（PDF、音频等）先用 `ContentPart::Other` 透传 provider 原始 block：

```rust
let pdf = ContentPart::Other {
    provider: "anthropic".into(),
    data: serde_json::json!({
        "type": "document",
        "source": {
            "type": "base64",
            "media_type": "application/pdf",
            "data": "<base64>"
        }
    }),
};
```

请求扩展：

```rust
use kunpeng::{
    core::{ProviderOptions, ToolChoice},
    runtime::RunConfig,
};

let mut extra = serde_json::Map::new();
extra.insert("seed".into(), serde_json::json!(42));

let config = RunConfig::default()
    .with_tool_choice(ToolChoice::Tool {
        name: "get_weather".into(),
    })
    .with_stop_sequences(vec!["END".into()])
    .with_provider_options(ProviderOptions::default().with_openai(extra));
```

映射：

- `ToolChoice::Auto / None / Required / Tool` 对应 OpenAI 的 `auto/none/required/function`；
- Anthropic 的 `Required` 映射为 `any`；
- `stop_sequences` 对应 OpenAI `stop`、Anthropic `stop_sequences`；
- `ProviderOptions` 只合并对应 provider 的字段，适合 `seed`、`response_format`、`top_k`、`metadata` 等；
- 不要用 `ProviderOptions` 覆盖 `model`、`messages`、`tools`、`stream` 等核心字段。

`StopReason`：

- `Completed`：模型给出最终回复
- `MaxTurns`：达到 `max_turns`
- `Length`：模型因 token 上限停止
- `ContentFilter`：内容被安全策略过滤
- `Cancelled`：调用方取消

事件是可序列化的普通数据，CLI 可以直接打印，Tauri 可以序列化后 `emit`。

---

## 取消、重试与恢复

### 取消

```rust
let token = agent.cancel_token();

// 另一个任务中
token.cancel();
```

取消会立即中止进行中的模型请求。Agent 会给未完成的工具调用补错误结果，保证历史仍可回放；之后可以再次 `resume()`。

### 重试

- 只重试 408、429、500、502、503、504
- 优先使用服务端 `Retry-After`，否则从 500ms 开始指数退避
- timeout 按单次尝试计算，总耗时可能约为 `timeout × (重试次数 + 1)`
- `LLM_MAX_RETRIES` 上限 10

### 历史恢复

`agent.history()` 返回只读历史；`Agent::with_history` / `set_history` 可以恢复会话。`Message` 支持 serde，可直接落盘。

模型请求失败时历史不会追加错误消息，调用方可以直接重试，不需要让用户重新输入。

---

## 上下文预算

`RunConfig::context_budget` 控制每次请求发送多少历史。默认 `None`，不裁剪：

```rust
use kunpeng::{context::TokenBudget, runtime::RunConfig};

let config = RunConfig::default()
    .with_context_budget(Some(TokenBudget::new(60_000)));
```

默认 `SlidingWindowContext` 的规则：

- 按 User 消息切分轮次，旧轮次整轮丢弃；
- system 消息始终保留；
- 最新一轮始终保留，即使单轮超过预算；
- assistant 的 `tool_calls` 和对应 `tool_result` 不会被拆开；
- token 使用 `ApproxTokenCounter` 估算：约 4 字符/token，图片按 1000 token。

自定义上下文策略实现 `ContextBuilder`，再注入：

```rust
use std::sync::Arc;
use kunpeng::agents::Agent;

let agent = Agent::new(model, tools, config)
    .with_context(Arc::new(my_context_builder));
```

后续的摘要压缩、RAG 检索注入都可以实现同一个 trait，不需要修改 ReAct 循环。

---

## Agent 策略

`Agent` 是运行时门面，持有模型、工具、上下文、配置和取消令牌；默认策略是 `ReActAgent`。

自定义策略实现 `AgentStrategy`：

```rust
use async_trait::async_trait;
use kunpeng::{
    agents::{AgentStrategy, RunContext},
    core::{Message, Result},
    runtime::StopReason,
};

struct MyAgent;

#[async_trait]
impl AgentStrategy for MyAgent {
    fn history(&self) -> &[Message] {
        todo!()
    }

    fn history_mut(&mut self) -> &mut Vec<Message> {
        todo!()
    }

    async fn run(&mut self, ctx: &RunContext<'_>) -> Result<StopReason> {
        // 使用 ctx.model / ctx.tools / ctx.context / ctx.config / ctx.hooks
        todo!()
    }
}
```

注入方式：

```rust
use std::sync::Arc;
use kunpeng::agents::Agent;

let agent = Agent::with_strategy(
    Arc::new(model),
    tools,
    config,
    Box::new(MyAgent),
);
```

后续的 Plan-Execute、Router、Reflection、Supervisor 都可以作为独立策略接入。

---

## 记忆、RAG 与会话

### Memory

`Memory` 负责长期记忆的写入与检索；`RagContextBuilder` 把命中的记忆注入 system 消息。

```rust
use std::sync::Arc;
use kunpeng::{
    agents::Agent,
    context::SlidingWindowContext,
    memory::{InMemoryMemory, MemoryItem},
    rag::RagContextBuilder,
};

let memory = Arc::new(InMemoryMemory::new());
memory.remember(MemoryItem::new("用户偏好 Rust")).await?;

let context = Arc::new(RagContextBuilder::new(
    Arc::new(SlidingWindowContext::default()),
    memory.clone(),
).with_limit(4));

let mut agent = Agent::new(model, tools, config).with_context(context);
```

`InMemoryMemory` 使用关键词/子串评分，不是语义检索；生产环境可以实现 `Memory` 接入向量库或外部记忆服务。

### SessionStore

```rust
use std::sync::Arc;
use kunpeng::{
    agents::Agent,
    memory::InMemorySessionStore,
};

let store = Arc::new(InMemorySessionStore::new());
let mut agent = Agent::new(model, tools, config)
    .with_session_store(store, "session-1");

agent.load_session().await?;   // 恢复历史
agent.send(Message::user("继续上次的话题"), &hooks).await?;
agent.save_session().await?;   // 显式保存
```

当前是显式保存，不会在每次 `send` 后自动落盘；便于 UI 决定何时持久化。

### 自定义记忆/检索

- 实现 `Memory`：接入向量数据库、Embedding 或外部服务；
- 实现 `SessionStore`：SQLite、Redis、文件或云端；
- 实现 `ContextBuilder`：摘要压缩、RAG 注入、租户隔离策略。

这些实现都通过 trait 注入，不需要修改 `ReActAgent`。

---

## 多智能体

多智能体通过消息通信，不共享 `Arc<Mutex<Agent>>`。每个 Worker 独立持有 Agent 和 Session。

```rust
use std::sync::Arc;
use kunpeng::{
    multi::{AgentId, AgentWorker, LocalBus, Supervisor},
    runtime::AgentHooks,
};

let bus = Arc::new(LocalBus::new());
let mut supervisor = Supervisor::new(bus.clone());

supervisor
    .register(AgentWorker::new("researcher", researcher_agent))
    .await?;
supervisor
    .bus()
    .register(AgentId::new("client"))
    .await?;

supervisor
    .send_task("client", "researcher", "总结 Rust 所有权")
    .await?;

let hooks = AgentHooks::default();
supervisor
    .step(&"researcher".into(), &hooks)
    .await?;

let reply = supervisor
    .bus()
    .recv(&"client".into())
    .await?
    .expect("result");
```

消息类型：

| Payload | 用途 |
|---|---|
| `Task` | 派发任务 |
| `Result` | 返回结果，带 `correlation_id` |
| `Handoff` | 带原因转交给另一个 Agent |
| `Text` | 普通消息 |

- `AgentEnvelope` 包含 `id`、`from`、`to`、`correlation_id`、`payload`；
- `LocalBus` 使用 tokio mpsc mailbox；
- `AgentWorker` 把任务作为 user 消息发送给 Agent，返回最后一条 assistant 文本；
- `Supervisor::step` 消费一条消息，Task/Handoff 会执行 Worker 并自动回传 Result；
- 跨进程通信实现 `AgentBus` 即可，不需要改 Supervisor 或 Worker。

---

## 协议适配

三个协议各自独立 crate，不进入 core，也不要求 runtime/agents 认识它们。

| crate | 传输 | 已实现 | 边界 |
|---|---|---|---|
| `kunpeng-mcp` | stdio JSON-RPC | `initialize`、tools、resources、resource templates、prompts、sampling、`ToolProvider` | HTTP/WebSocket 传输通过 `McpTransport` 扩展 |
| `kunpeng-a2a` | HTTP JSON-RPC/SSE | Agent Card、`message/send`、`message/stream`、`tasks/get`、`tasks/resubscribe`、push notification 配置、`AgentProtocol` | webhook 接收端由宿主提供 |
| `kunpeng-anp` | HTTP JSON-LD | 可配置 `@context`、信封映射、`did:web` 解析、ANP service 发现、`AgentProtocol` | 其他 DID method 实现 `DidResolver` |

MCP 工具直接注册进现有工具系统：

```rust
use std::sync::Arc;
use kunpeng::mcp::{McpToolset, StdioTransport};

let transport = Arc::new(StdioTransport::connect(
    "npx",
    &["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
)?);
let mcp = McpToolset::connect(transport).await?;
tools.register_provider(&mcp).await?;

// resources / prompts
let resources = mcp.client().list_resources().await?;
let mut arguments = std::collections::BTreeMap::new();
arguments.insert("text".into(), "hello".into());
let prompt = mcp.client().get_prompt("summarize", &arguments).await?;
```

需要处理 server 发起的 `sampling/createMessage` 时，在初始化前注册 handler：

```rust
use kunpeng::mcp::McpClient;

let client = McpClient::new(transport).with_sampling_handler(my_sampling_handler);
let mcp = McpToolset::from_client(std::sync::Arc::new(client)).await?;
```

A2A：

```rust
use kunpeng::{
    a2a::{A2aClient, PushNotificationConfig},
    core::StreamControl,
};

let a2a = A2aClient::new("https://agent.example/a2a", "planner");
let reply = a2a.send_message("分析这份数据", Some("ctx-1")).await?;

let mut events = Vec::new();
let streamed = a2a
    .send_message_stream("继续分析", Some("ctx-1"), &mut |event| {
        events.push(event.clone());
        StreamControl::Continue
    })
    .await?;

let config = PushNotificationConfig::new("https://host.example/a2a/push");
a2a.set_push_notification_config("task-1", &config).await?;
```

ANP 的 `@context` 由部署方提供，避免把快速演进的规范版本写死：

```rust
use kunpeng::{anp::AnpClient, multi::AgentProtocol};

let anp = AnpClient::new("https://agent.example/anp", "https://w3id.org/anp/v1");
let response = anp.send_envelope(&envelope).await?;
```

从 `did:web` 发现 ANP service：

```rust
use std::sync::Arc;
use kunpeng::anp::{AnpClient, AnpDiscovery, HttpDidResolver};

let discovered = AnpDiscovery::new(Arc::new(HttpDidResolver::new()))
    .discover("did:web:agent.example")
    .await?;
let anp = AnpClient::new(discovered.endpoint, "https://w3id.org/anp/v1");
```

---

## 项目结构

```text
Cargo.toml                  workspace 根
crates/
├── kunpeng/                umbrella：kunpeng::core / agents / ...
├── kunpeng-core/           types / error / ChatModel
├── kunpeng-provider/       Provider / Adapter API / AdapterRegistry / LlmConfig / Model
├── kunpeng-adapter-openai/ OpenAI Chat Completions 适配器
├── kunpeng-adapter-anthropic/ Anthropic Messages 适配器
├── kunpeng-runtime/        RunConfig / 事件 / 审批 / 取消
├── kunpeng-agents/         Agent 门面 / AgentStrategy / RunContext / ReActAgent
├── kunpeng-tools/          Tool / ToolRegistry / ToolPolicy / builtin
├── kunpeng-context/        TokenCounter / SlidingWindow
├── kunpeng-memory/         Memory / SessionStore / InMemory
├── kunpeng-rag/            RagContextBuilder
├── kunpeng-multi/          AgentBus / Envelope / AgentProtocol / Supervisor
├── kunpeng-mcp/            MCP stdio 客户端 / ToolProvider
├── kunpeng-a2a/            A2A HTTP JSON-RPC 客户端
├── kunpeng-anp/            ANP JSON-LD HTTP 传输
└── kunpeng-cli/            env 配置适配 + interactive CLI + examples
docs/
├── README.md               文档索引
├── ARCHITECTURE.md         分层、调用链与设计取舍
└── API_STABILITY.md        API 稳定性等级与扩展规则
```

`kunpeng` umbrella 保留统一导入路径：

```rust
use kunpeng::{agents::Agent, core::Message, runtime::RunConfig};
```

也可以按需只依赖单层 crate。

更完整的模块职责和调用链见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

---

## 开发与测试

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -W clippy::all
cargo test --workspace
cargo doc --workspace --no-deps --open
```

按 crate 测试：

```bash
cargo test -p kunpeng-agents
cargo test -p kunpeng-context
cargo test -p kunpeng-rag
cargo test -p kunpeng-tools
cargo test -p kunpeng-multi
cargo test -p kunpeng-mcp
cargo test -p kunpeng-a2a
cargo test -p kunpeng-anp
```

当前测试覆盖：

- 两个协议适配器的消息转换与 usage/finish_reason 映射
- SSE 解码：跨 chunk、CRLF、多行 data、UTF-8 字符拆分
- Agent 流式增量事件、工具拒绝、异步审批、审批超时、工具超时、取消、达到轮数
- Anthropic thinking：请求参数、thinking 文本提取、原始 block/签名回放
- 服务端工具：request 合并、只读投影、`pause_turn` 续跑
- Citations：非流式提取、流式 `citations_delta` 累积
- 多模态：OpenAI image_url、Anthropic image block、请求扩展参数序列化
- 上下文：滑动窗口裁剪、system 保留、tool 分组、最新轮保留
- 记忆：内存检索、RAG 注入、SessionStore 往返
- 工具：策略超时/截断、fs 沙箱、http allowlist、time
- 多智能体：LocalBus 路由、Supervisor Task/Result、correlation id
- 配置默认值、API key 脱敏、工具注册校验
- `Message` serde 往返

---

## 已知限制

- Anthropic 服务端工具由 Anthropic 执行，不会作为本地 `ToolCall` 执行；通过 `server_tool_uses` 只读展示
- Anthropic 的未知 delta（除 text/thinking/signature/input_json/citations 外）不进入语义字段，原始 block 仍会保留
- 当前只有 InMemory Memory/SessionStore；向量检索和生产持久化需要实现对应 trait
- 没有内置 shell 工具；高风险命令执行需要自行实现并接入审批

---

## License

尚未添加 `LICENSE` 文件。开源发布前请确定许可证（例如 MIT 或 Apache-2.0），并同步更新 `Cargo.toml` 的 `license` 字段。
