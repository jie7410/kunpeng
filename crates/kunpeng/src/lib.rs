//! Kunpeng umbrella crate.
//!
//! 各层可以单独依赖 `kunpeng-core`、`kunpeng-agents` 等；需要统一入口时使用本 crate：
//! `kunpeng::agents::Agent`、`kunpeng::core::Message` 等。

pub use kunpeng_a2a as a2a;
pub use kunpeng_adapter_anthropic as adapter_anthropic;
pub use kunpeng_adapter_openai as adapter_openai;
pub use kunpeng_agents as agents;
pub use kunpeng_anp as anp;
pub use kunpeng_context as context;
pub use kunpeng_core as core;
pub use kunpeng_http as http;
pub use kunpeng_mcp as mcp;
pub use kunpeng_memory as memory;
pub use kunpeng_multi as multi;
pub use kunpeng_provider as provider;
pub use kunpeng_rag as rag;
pub use kunpeng_runtime as runtime;
pub use kunpeng_tools as tools;

/// 注册内置适配器。宿主可以在此基础上追加自定义厂商，或构建只含所需适配器的注册表。
pub fn default_adapter_registry() -> core::Result<provider::AdapterRegistry> {
    let mut registry = provider::AdapterRegistry::new();
    registry.register(adapter_openai::OpenAiAdapter::openai())?;
    registry.register(adapter_openai::OpenAiAdapter::compatible())?;
    registry.register(adapter_openai::OpenAiAdapter::zhipu())?;
    registry.register(adapter_anthropic::AnthropicAdapter::new())?;
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_resolve_through_aliases() {
        let registry = default_adapter_registry().unwrap();
        assert_eq!(registry.resolve("openai").unwrap().name(), "openai");
        assert_eq!(registry.resolve("glm").unwrap().name(), "zhipu");
        assert_eq!(registry.resolve("claude").unwrap().name(), "anthropic");
        assert!(registry.resolve("missing").is_err());
    }
}
