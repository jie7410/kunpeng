# Kunpeng

一个与 UI 无关的 Rust LLM Agent 内核，支持 OpenAI 兼容端点与 Anthropic Messages API。

核心能力：

- 有状态 `Agent`：持有会话历史，支持 `send`、`resume`、历史导入/导出
- 工具调用循环：工具注册、schema 声明、执行超时、审批、错误回传
- 事件流：assistant 回复、工具开始/结束、最终停止原因、错误
- 取消令牌：中止进行中的模型请求，并保持历史可回放
- 协议适配：OpenAI Chat Completions / Anthropic Messages
- 重试：408/429/5xx 指数退避，尊重 `Retry-After`
- 配置：环境变量或程序化 builder；API key 在 `Debug` 输出中脱敏
- 无 UI 依赖：CLI、Tauri 或测试共用同一套 API

> 当前版本 `0.1.0`，API 仍可能调整。

文档入口见 [docs/README.md](docs/README.md)。详细分层、调用时序和设计取舍见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

---

## 环境要求

- Rust stable，edition 2021
- 能访问目标 LLM 端点
- 使用 rustls，不依赖系统 OpenSSL，便于跨平台分发

程序不会自动读取 `.env`。请在 shell 中设置环境变量，或使用 `LlmConfig::new(...)` 在代码里构造配置。

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

`src/main.rs` 会注册一个示例天气工具，向模型提问后打印事件和最终回复。

运行独立示例（同样是 echo 工具）：

```bash
cargo run --example basic_agent
```

---

## 配置

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
use kunpeng::llm::LlmConfig;

let config = LlmConfig::new("openai", "sk-...")
    .with_model("gpt-4o-mini")
    .with_timeout_secs(60)
    .with_max_retries(2);

let model = config.build()?;
```

`openai-compatible` 必须同时提供 base URL 和模型名，避免把错误推迟到运行期。

---

## 作为 library 使用

```toml
[dependencies]
kunpeng = { git = "https://github.com/<your-github-user>/kunpeng" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
async-trait = "0.1"
serde_json = "1"
```

完整可运行代码见 [`examples/basic_agent.rs`](examples/basic_agent.rs)：

```rust
use async_trait::async_trait;
use kunpeng::llm::{
    Agent, AgentConfig, AgentEvent, AgentHooks, LlmConfig, Message, Result, Tool, ToolDefinition,
    ToolRegistry,
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
async fn main() -> Result<()> {
    let config = LlmConfig::from_env()?;
    let model = config.build()?;

    let mut tools = ToolRegistry::new();
    tools.register(EchoTool)?;

    let mut agent = Agent::with_history(
        model,
        tools,
        AgentConfig::default(),
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
    let hooks = AgentHooks {
        on_event: Some(&on_event),
        approve: None,
    };

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
- 单个工具默认有 60 秒硬超时，可通过 `AgentConfig::tool_timeout` 调整或关闭

### 工具审批

```rust
use kunpeng::llm::{AgentHooks, ToolCall, ToolDecision};

let approve = |call: &ToolCall| {
    if call.name == "shell" {
        ToolDecision::Deny("需要人工确认".into())
    } else {
        ToolDecision::Allow
    }
};

let hooks = AgentHooks {
    on_event: None,
    approve: Some(&approve),
};
```

被拒绝的调用不会执行，但会写入错误结果，模型能据此换方案。当前审批回调是同步的；Tauri 如果需要真正异步等待用户确认，需要扩展为 async approver。

---

## 事件与停止原因

`AgentEvent` 顺序：

```text
Assistant → (ToolStarted → ToolFinished)* → Finished
```

| 事件 | 时机 |
|---|---|
| `Assistant` | 单次模型回复已写入历史 |
| `ToolStarted` | 审批通过、工具即将执行 |
| `ToolFinished` | 工具成功、失败或被拒绝 |
| `Finished` | 正常结束，带 `StopReason` |
| `Error` | 模型请求失败；历史保留，可 `resume()` |

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

## 项目结构

```text
README.md
docs/
├── README.md              文档索引
└── ARCHITECTURE.md        分层、调用链与设计取舍
src/
├── lib.rs                 library 入口
├── main.rs                CLI 示例
└── llm/
    ├── types.rs           协议无关的语义类型
    ├── error.rs           错误分类
    ├── config.rs          环境变量 / builder
    ├── model.rs           Model：Provider + 模型名 + 默认参数
    ├── tool.rs            Tool trait 与 ToolRegistry
    ├── agent.rs           历史、工具循环、事件、审批、取消、超时
    └── provider/
        ├── protocol.rs    Protocol trait、重试、解析、截断
        ├── openai.rs      OpenAI 兼容适配器
        └── anthropic.rs   Anthropic 适配器
examples/
└── basic_agent.rs         最小可运行示例
```

更完整的模块职责和调用链见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

---

## 开发与测试

```bash
cargo fmt --check
cargo clippy --all-targets -- -W clippy::all
cargo test
cargo doc --no-deps --open
```

当前测试覆盖：

- 两个协议适配器的消息转换与 usage/finish_reason 映射
- Agent 循环、工具拒绝、工具超时、取消、达到轮数
- 配置默认值、API key 脱敏、工具注册校验
- `Message` serde 往返

---

## 已知限制

- 流式输出尚未实现，当前只有非流式请求
- Anthropic 的 `thinking`、服务端工具等未知 block 会被忽略；启用 extended thinking 前需要保存原始 block
- 没有上下文窗口裁剪，长会话可能超过模型上限
- 工具审批是同步回调，暂不支持异步 UI 确认
- 多模态消息和 `tool_choice`、`stop` 等扩展参数尚未暴露

---

## License

尚未添加 `LICENSE` 文件。开源发布前请确定许可证（例如 MIT 或 Apache-2.0），并同步更新 `Cargo.toml` 的 `license` 字段。
