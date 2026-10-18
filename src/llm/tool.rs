//! 工具抽象与注册表。
//!
//! `Tool` 是内核里唯一允许执行外部副作用的入口。Agent 本身不碰文件、网络或命令；
//! 审批、超时、错误转 tool_result 都在 Agent 层完成，工具实现只关心参数和返回值。

use crate::llm::error::{Error, Result};
use crate::llm::types::ToolDefinition;
use async_trait::async_trait;
use std::{collections::BTreeMap, sync::Arc};

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

/// 工具注册表。
///
/// 用 `BTreeMap` 而不是 `HashMap`：工具定义会进入模型 prompt，顺序不稳定会破坏
/// OpenAI/Anthropic 的前缀缓存命中，也会让快照测试随机失败。
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
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
        let definition = tool.definition();
        validate_name(&definition.name)?;
        if self.tools.contains_key(&definition.name) {
            return Err(Error::InvalidTool {
                name: definition.name,
                reason: "同名工具已注册".into(),
            });
        }
        self.tools.insert(definition.name.clone(), Arc::new(tool));
        Ok(self)
    }

    /// 返回按名称排序的工具定义。每次调用都会克隆定义；Agent 每轮只取一次，
    /// 不会在工具循环里重复取。
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    /// 未注册工具返回独立错误，由 Agent 转成 `is_error` 的 tool_result。
    pub async fn call(&self, name: &str, args: serde_json::Value) -> Result<String> {
        let tool = self.tools.get(name).ok_or_else(|| Error::UnknownTool {
            name: name.to_string(),
        })?;
        tool.call(args).await
    }
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
}
