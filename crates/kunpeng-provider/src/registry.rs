//! 适配器注册表：把协议名称/别名解析成 `Adapter`。
//!
//! 注册表是显式的，不依赖全局状态；宿主可以只注册自己需要的厂商，也可以在
//! 测试里注册 mock adapter。Provider 本身不认识 `openai` 或 `anthropic`。

use crate::adapter::Adapter;
use kunpeng_core::error::{Error, Result};
use std::{collections::BTreeMap, sync::Arc};

/// 适配器名称到实例的映射。别名可以指向同一个实例。
#[derive(Clone, Default)]
pub struct AdapterRegistry {
    adapters: BTreeMap<String, Arc<dyn Adapter>>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册适配器及其别名。重名直接报错，避免静默覆盖导致请求发到错误厂商。
    pub fn register<A: Adapter + 'static>(&mut self, adapter: A) -> Result<&mut Self> {
        self.register_arc(Arc::new(adapter))
    }

    pub fn register_arc(&mut self, adapter: Arc<dyn Adapter>) -> Result<&mut Self> {
        let names: Vec<String> = std::iter::once(adapter.name())
            .chain(adapter.aliases().iter().copied())
            .map(normalize)
            .collect();
        if names.iter().any(|name| name.is_empty()) {
            return Err(Error::InvalidConfig("adapter 名称不能为空".into()));
        }
        // 先完整校验再插入，避免注册失败后留下“部分别名已生效”的状态。
        let mut seen = std::collections::BTreeSet::new();
        for name in &names {
            if !seen.insert(name) || self.adapters.contains_key(name) {
                return Err(Error::InvalidConfig(format!(
                    "adapter 名称或别名重复: {name}"
                )));
            }
        }
        for name in names {
            self.adapters.insert(name, Arc::clone(&adapter));
        }
        Ok(self)
    }

    pub fn resolve(&self, name: &str) -> Result<Arc<dyn Adapter>> {
        self.adapters
            .get(&normalize(name))
            .cloned()
            .ok_or_else(|| Error::UnsupportedProvider(name.trim().to_string()))
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.adapters.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }
}

fn normalize(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use kunpeng_core::types::{ChatRequest, ChatResponse};

    struct Stub {
        name: &'static str,
        aliases: &'static [&'static str],
    }

    #[async_trait]
    impl Adapter for Stub {
        fn name(&self) -> &'static str {
            self.name
        }

        fn aliases(&self) -> &'static [&'static str] {
            self.aliases
        }

        async fn chat(
            &self,
            _ctx: &crate::adapter::CallContext<'_>,
            _req: &ChatRequest<'_>,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse::new("stub"))
        }
    }

    #[test]
    fn resolves_aliases_case_insensitively() {
        let mut registry = AdapterRegistry::new();
        registry
            .register(Stub {
                name: "vendor",
                aliases: &["Vendor-Alias"],
            })
            .unwrap();
        assert_eq!(registry.resolve(" VENDOR ").unwrap().name(), "vendor");
        assert_eq!(registry.resolve("vendor-alias").unwrap().name(), "vendor");
        assert!(registry.resolve("missing").is_err());
    }

    #[test]
    fn rejects_duplicate_names() {
        let mut registry = AdapterRegistry::new();
        registry
            .register(Stub {
                name: "vendor",
                aliases: &["alias"],
            })
            .unwrap();
        let error = match registry.register_arc(Arc::new(Stub {
            name: "alias",
            aliases: &[],
        })) {
            Ok(_) => panic!("重复名称应当报错"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("重复"));
    }

    #[test]
    fn failed_registration_does_not_leave_partial_aliases() {
        let mut registry = AdapterRegistry::new();
        registry
            .register(Stub {
                name: "vendor",
                aliases: &[],
            })
            .unwrap();
        let error = match registry.register(Stub {
            name: "fresh",
            aliases: &["vendor"],
        }) {
            Ok(_) => panic!("别名冲突应当报错"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("重复"));
        assert!(registry.resolve("fresh").is_err());
    }
}
