//! 工具抽象与注册表。
//!
//! `Tool` 是内核里唯一允许执行外部副作用的入口。Agent 本身不碰文件、网络或命令；
//! 审批、超时、错误转 tool_result 都在 Agent 层完成，工具实现只关心参数和返回值。

use crate::policy::ToolPolicy;
use async_trait::async_trait;
use kunpeng_core::error::{Error, Result};
use kunpeng_core::types::ToolDefinition;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

/// 工具实现。
///
/// 要求 `Send + Sync`：注册表用 `Arc<dyn Tool>` 共享，Agent 可能被移到
/// `tokio::spawn`（Tauri command 就是这种场景）。`call` 的 future 也必须是 Send，
/// 所以实现里不要跨 await 持有非 Send 的临时值（如 `Rc`、`MutexGuard`）。
#[async_trait]
pub trait Tool: Send + Sync {
    /// 定义会在注册时和每次请求前各调用一次。实现应保持稳定且廉价，
    /// 不要在这里读文件或做网络请求。
    fn definition(&self) -> ToolDefinition;

    /// 参数是模型生成的 JSON。实现需要自行校验字段；内核不会按 schema 做校验。
    async fn call(&self, arguments: serde_json::Value) -> Result<String>;
}

#[derive(Clone)]
struct ToolEntry {
    tool: Arc<dyn Tool>,
    policy: ToolPolicy,
}

/// 工具注册表。
///
/// 用 `BTreeMap` 而不是 `HashMap`：工具定义会进入模型 prompt，顺序不稳定会破坏
/// OpenAI/Anthropic 的前缀缓存命中，也会让快照测试随机失败。
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolEntry>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册工具。名称先校验再插入，避免到请求阶段才发现 API 拒绝。
    ///
    /// 重名直接报错而不是覆盖：覆盖通常意味着两个模块注册了同名工具，静默替换后
    /// 模型看到的描述和实际执行者可能不一致，很难排查。
    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> Result<&mut Self> {
        self.register_arc_with_policy(Arc::new(tool), ToolPolicy::default())
    }

    /// 注册带策略的工具（per-tool 超时、输出截断）。
    pub fn register_with_policy<T: Tool + 'static>(
        &mut self,
        tool: T,
        policy: ToolPolicy,
    ) -> Result<&mut Self> {
        self.register_arc_with_policy(Arc::new(tool), policy)
    }

    /// 注册已经共享的工具对象，供动态 ToolProvider 使用。
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) -> Result<&mut Self> {
        self.register_arc_with_policy(tool, ToolPolicy::default())
    }

    pub fn register_arc_with_policy(
        &mut self,
        tool: Arc<dyn Tool>,
        policy: ToolPolicy,
    ) -> Result<&mut Self> {
        let definition = tool.definition();
        validate_name(&definition.name)?;
        if self.tools.contains_key(&definition.name) {
            return Err(Error::InvalidTool {
                name: definition.name,
                reason: "同名工具已注册".into(),
            });
        }
        self.tools
            .insert(definition.name.clone(), ToolEntry { tool, policy });
        Ok(self)
    }

    /// 从 ToolProvider（例如 MCP）批量注册工具。
    pub async fn register_provider<P: crate::ToolProvider>(
        &mut self,
        provider: &P,
    ) -> Result<&mut Self> {
        for tool in provider.tools().await? {
            self.register_arc(tool)?;
        }
        Ok(self)
    }

    /// 返回按名称排序的工具定义。每次调用都会克隆定义；Agent 每轮只取一次，
    /// 不会在工具循环里重复取。
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|entry| entry.tool.definition())
            .collect()
    }

    /// per-tool 超时；None 表示回落到 `RunConfig::tool_timeout`。
    pub fn timeout_for(&self, name: &str) -> Option<Duration> {
        self.tools.get(name).and_then(|entry| entry.policy.timeout)
    }

    pub fn policy(&self, name: &str) -> Option<&ToolPolicy> {
        self.tools.get(name).map(|entry| &entry.policy)
    }

    /// 未注册工具返回独立错误，由 Agent 转成 `is_error` 的 tool_result。
    ///
    /// 成功输出按工具策略截断；错误不截断，保留完整诊断信息。
    pub async fn call(&self, name: &str, args: serde_json::Value) -> Result<String> {
        let entry = self.tools.get(name).ok_or_else(|| Error::UnknownTool {
            name: name.to_string(),
        })?;
        let output = entry.tool.call(args).await?;
        Ok(match entry.policy.max_output_bytes {
            Some(max) => truncate_output(output, max),
            None => output,
        })
    }
}

/// 按 UTF-8 边界截断工具输出。直接切字节可能截断中文/emoji。
fn truncate_output(mut output: String, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output;
    }
    let mut end = max_bytes;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    output.truncate(end);
    output.push_str("\n...[truncated]");
    output
}

/// OpenAI 的工具名约束：1-64 个 ASCII 字母、数字、下划线或连字符。
/// Anthropic 没有同样严格的限制，但统一按最严的来，避免同一个工具集换 provider
/// 后突然报错。
fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.chars().count() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidTool {
            name: name.to_string(),
            reason: "工具名必须为 1-64 个 ASCII 字母、数字、下划线或连字符".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name.into(),
                description: "echo".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        async fn call(&self, _arguments: serde_json::Value) -> Result<String> {
            Ok("ok".into())
        }
    }

    #[test]
    fn rejects_invalid_and_duplicate_names() {
        let mut registry = ToolRegistry::new();
        assert!(registry.register(EchoTool { name: "bad name" }).is_err());

        registry.register(EchoTool { name: "echo" }).unwrap();
        assert!(registry.register(EchoTool { name: "echo" }).is_err());
    }

    #[test]
    fn definitions_are_sorted_by_name() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool { name: "zeta" }).unwrap();
        registry.register(EchoTool { name: "alpha" }).unwrap();

        let names: Vec<_> = registry
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
    }

    #[tokio::test]
    async fn unknown_tool_is_reported() {
        let registry = ToolRegistry::new();
        let error = registry
            .call("missing", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::UnknownTool { .. }));
    }

    struct FixedTool {
        name: &'static str,
        output: &'static str,
    }

    #[async_trait]
    impl Tool for FixedTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name.into(),
                description: "fixed".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        async fn call(&self, _arguments: serde_json::Value) -> Result<String> {
            Ok(self.output.into())
        }
    }

    #[tokio::test]
    async fn policy_truncates_output_and_overrides_timeout() {
        let mut registry = ToolRegistry::new();
        registry
            .register_with_policy(
                FixedTool {
                    name: "fixed",
                    output: "你好世界",
                },
                ToolPolicy::default()
                    .with_timeout(Duration::from_secs(2))
                    .with_max_output_bytes(4),
            )
            .unwrap();

        assert_eq!(registry.timeout_for("fixed"), Some(Duration::from_secs(2)));
        let output = registry.call("fixed", serde_json::json!({})).await.unwrap();
        assert_eq!(output, "你\n...[truncated]");
    }

    struct StaticProvider {
        tools: Vec<Arc<dyn Tool>>,
    }

    #[async_trait]
    impl crate::ToolProvider for StaticProvider {
        async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
            Ok(self.tools.clone())
        }
    }

    #[tokio::test]
    async fn dynamic_provider_registers_tools() {
        let provider = StaticProvider {
            tools: vec![Arc::new(FixedTool {
                name: "dynamic",
                output: "ok",
            })],
        };
        let mut registry = ToolRegistry::new();
        registry.register_provider(&provider).await.unwrap();
        assert!(registry
            .definitions()
            .iter()
            .any(|tool| tool.name == "dynamic"));
    }
}
