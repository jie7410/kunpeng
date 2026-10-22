# API 稳定性与扩展规则

当前版本是 `0.1.x`，核心能力仍在演进。这里定义“哪些 API 可以依赖、哪些可能调整、新增能力应该放在哪里”，避免把所有 `pub` 都误认为稳定契约。

## 稳定性等级

### Stable

承诺在 1.0 前尽量保持向后兼容；破坏性变更需要 minor 版本并写迁移说明。

- `agents::Agent`、`agents::RunContext` 的构造/访问
- `agents::ReActAgent`
- `core::ChatModel`（`chat` 必需，`chat_stream` 有默认实现）
- `core::Error` 的分类语义（HTTP kind/source、Api/RateLimited、Json/ParseResponse）
- `core::StreamDelta<'a>`
- `core::Message`、`ChatRequest`、`ChatResponse` 的语义字段
- `runtime::AgentEvent`、`StopReason`、`ToolDecision`
- `tools::Tool`、`ToolRegistry`、`ToolPolicy`
- `memory::Memory`、`SessionStore`
- `context::ContextBuilder`
- `provider::LlmConfig` 的 builder 与 `build`
- `core::TokenBudget`

### Experimental

可以使用，但签名可能随功能完善调整；升级时优先提供默认实现或迁移函数。

- `agents::AgentStrategy`
- `core::StreamControl`
- `provider::Provider`
- `provider::Adapter`、`provider::AdapterRegistry`、`provider::CallContext`
- `adapter_openai::OpenAiAdapter`
- `adapter_anthropic::AnthropicAdapter`
- `context::TokenCounter`
- `rag::RagContextBuilder`
- `multi::AgentBus`、`LocalBus`、`AgentProtocol`、`Worker`、`Supervisor`
- `tools::ToolProvider`
- `mcp::McpToolset`、`mcp::McpClient`、`mcp::McpTransport`
- `mcp::InboundHandler`、`mcp::SamplingHandler`
- `mcp::McpResource`、`mcp::McpResourceTemplate`、`mcp::McpPrompt`、`mcp::McpPage`
- `a2a::A2aClient`、`a2a::PushNotificationConfig`
- `anp::AnpClient`、`anp::AnpDiscovery`、`anp::HttpDidResolver`
- `anp::DidResolver`、`anp::DidDocument`、`anp::DidService`

### Internal / Adapter

不作为普通业务 API，也不从 crate 导出。它们只描述厂商 wire format 或内部实现，
可以随厂商 API/内部重构调整；调用方只应依赖 `Adapter` trait 和 core 语义类型。

- `adapter_openai` 的 wire types：`OpenAiRequest`、`OpenAiMessage`、`OpenAiContent`、`OpenAiTool`、`OpenAiResponse`、`OpenAiStreamResponse`、`OpenAiUsage` 等
- `adapter_anthropic` 的 wire types：`AnthropicRequest`、`AnthropicReqMessage`、`AnthropicReqBlock`、`AnthropicResponse`、`AnthropicStreamEvent`、`AnthropicUsage` 等
- `provider::adapter` 的 SSE 解码器与内部 HTTP 工具
- `ProviderData` 中的 provider 原始 JSON

这些 wire types 是私有实现，新增/修改字段不遵循 core 的 `#[non_exhaustive]`
兼容规则；需要稳定访问厂商原始数据时，使用 `ProviderData`。

## 扩展规则

### 1. 公共 enum 默认 `#[non_exhaustive]`

```rust
#[non_exhaustive]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}
```

外部匹配必须保留 `_` 分支；新增变体不会破坏下游。

### 2. 公共数据 struct 用 builder + `#[non_exhaustive]`

```rust
#[non_exhaustive]
pub struct RunConfig { /* fields */ }

impl RunConfig {
    pub fn new() -> Self;
    pub fn with_stream(self, stream: bool) -> Self;
}
```

这样新增字段不需要下游改字面量构造。已有字段仍可读；写入优先通过 builder，避免依赖字段顺序。

当前已采用该模式的类型：

- `RunConfig`
- `AgentHooks`
- `ChatRequest`
- `TokenBudget`
- `ToolPolicy`
- `ProviderOptions`
- `ThinkingConfig`
- `MemoryItem`
- `AgentEnvelope`
- `Usage`、`ProviderData`、`ServerToolUse`

### 3. Trait 新增能力优先给默认实现

```rust
pub trait ChatModel {
    async fn chat(&self, req: &ChatRequest<'_>) -> Result<ChatResponse>;

    async fn chat_stream(...) -> Result<ChatResponse> {
        // 默认回退到 chat
    }
}
```

如果新能力语义不同（Embedding、Rerank、A2A 流式），新增独立 trait，不要往核心 trait 里堆方法。

### 4. 协议适配不进入 core

| 协议 | 放置 |
|---|---|
| MCP | `kunpeng-mcp`，实现 `ToolProvider`，resources/prompts/sampling 保持独立 |
| A2A | `kunpeng-a2a`，映射 `AgentEnvelope` / Task，支持同步、流式与 push 配置 |
| ANP | `kunpeng-anp`，JSON-LD 信封与 DID 发现 |

`LocalBus` 只服务进程内编排；A2A/ANP 需要更丰富的任务/流式/身份模型时，使用独立协议 trait 或 crate。

### 5. serde 是持久化契约

`Message`、`AgentEvent`、`AgentEnvelope` 的 serde 结构会进入会话文件/数据库/IPC：

- 不随意改 `type`/`role` tag；
- 新字段使用 `#[serde(default)]`；
- 删除字段前先保留读取兼容；
- 跨版本会话需要 schema version 或迁移函数。

### 6. 低层 provider API 明确标注 Adapter

普通业务使用 `core::ChatModel`，通过 `provider::LlmConfig` + `default_adapter_registry` 构建 `provider::Model`。自定义协议实现 `provider::Adapter`。

## 协议能力状态

- MCP tools/resources/prompts/sampling、A2A 同步/流式/push 配置、ANP `did:web` 发现均已实现。
- 新增协议能力仍应放在对应 crate 内；不要把这些 wire/profile 细节放进 core 或 agents。

## 测试命令

workspace 已拆分，可按 crate 独立测试：

```bash
cargo test -p kunpeng-provider
cargo test -p kunpeng-adapter-openai
cargo test -p kunpeng-adapter-anthropic
cargo test -p kunpeng-multi
cargo test -p kunpeng-context
cargo test -p kunpeng-rag
cargo test -p kunpeng-tools
cargo test -p kunpeng-mcp
cargo test -p kunpeng-a2a
cargo test -p kunpeng-anp
```

## PR 检查清单

- 新增公共字段是否考虑 builder / `#[non_exhaustive]`？
- 新增 trait 方法是否有默认实现？
- 是否把协议专属类型放进了 core？
- 是否改变 serde 字段/tag？
- 是否新增了真实测试，而不是只验证“能编译”？
- 模块测试是否可以在不依赖网络的情况下运行？
