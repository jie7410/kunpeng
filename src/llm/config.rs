//! 配置层：环境变量读取、默认值补齐、构建 `Model`。
//!
//! 这里是唯一读环境变量的地方。UI（Tauri/CLI）可以直接用 `new` + `with_*`
//! 构造配置，不依赖 env；`from_env` 只是给命令行和 CI 用的便捷入口。
//!
//! 配置错误尽量在 `build` 阶段报出来，不要等到第一次请求才失败：URL、timeout、
//! 重试次数、模型名缺失都属于部署配置问题，启动时就该暴露。

use crate::llm::{
    error::{Error, Result},
    provider::protocol_by_name,
    Model, Provider,
};
use std::{env, fmt, sync::Arc};

#[derive(Clone)]
pub struct LlmConfig {
    /// 协议名或别名，例如 `openai`、`anthropic`、`zhipu`。
    pub protocol: String,
    /// 为空时使用该协议的默认地址。`openai-compatible` 没有默认值，必须显式设置。
    pub base_url: Option<String>,
    pub api_key: String,
    /// 为空时使用该协议的默认模型；兼容端点必须显式设置。
    pub model: Option<String>,
    /// 单次 HTTP 请求的总超时（连接 + 读完 body）。
    pub timeout_secs: u64,
    /// 429/408/5xx 的最大重试次数。
    pub max_retries: u32,
}

// 不能 derive Debug：默认实现会把 api_key 原样打印出来。panic、日志或 UI 的
// 调试面板一旦格式化配置就会泄露密钥。
impl fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LlmConfig")
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("api_key", &"***")
            .field("model", &self.model)
            .field("timeout_secs", &self.timeout_secs)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

impl LlmConfig {
    /// 程序化构造。默认值保持保守：120 秒超时、2 次重试。
    pub fn new(protocol: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            protocol: protocol.into().trim().to_string(),
            base_url: None,
            api_key: api_key.into().trim().to_string(),
            model: None,
            timeout_secs: 120,
            max_retries: 2,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// 从环境变量读取配置。
    ///
    /// 变量：
    /// - `LLM_PROVIDER`：协议/别名，缺省 `openai`
    /// - `LLM_API_KEY`：必填
    /// - `LLM_BASE_URL`：可选，覆盖默认地址
    /// - `LLM_MODEL`：可选，覆盖默认模型
    /// - `LLM_TIMEOUT_SECS`：可选，默认 120，必须大于 0
    /// - `LLM_MAX_RETRIES`：可选，默认 2，上限 10
    ///
    /// 所有值读取后都会 trim。`.env`、K8s secret、复制粘贴经常留下换行或空格；
    /// API key 带一个尾随空格就会 401，且在错误信息里几乎看不出来。
    pub fn from_env() -> Result<Self> {
        const ENV_PROTOCOL: &str = "LLM_PROVIDER";
        const ENV_BASE_URL: &str = "LLM_BASE_URL";
        const ENV_API_KEY: &str = "LLM_API_KEY";
        const ENV_MODEL: &str = "LLM_MODEL";
        const ENV_TIMEOUT: &str = "LLM_TIMEOUT_SECS";
        const ENV_MAX_RETRIES: &str = "LLM_MAX_RETRIES";

        let protocol = match env::var(ENV_PROTOCOL) {
            Ok(value) => {
                let value = value.trim();
                if value.is_empty() {
                    "openai".to_string()
                } else {
                    value.to_string()
                }
            }
            Err(env::VarError::NotPresent) => "openai".to_string(),
            Err(env::VarError::NotUnicode(_)) => return Err(Error::NonUnicodeEnv(ENV_PROTOCOL)),
        };

        let api_key = required_env(ENV_API_KEY)?;
        let base_url = option_env(ENV_BASE_URL);
        let model = option_env(ENV_MODEL);

        let timeout_secs = option_env(ENV_TIMEOUT)
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|e| Error::InvalidConfig(format!("{ENV_TIMEOUT} 不是合法整数: {e}")))
            })
            .transpose()?
            .unwrap_or(120);
        if timeout_secs == 0 {
            return Err(Error::InvalidConfig(format!("{ENV_TIMEOUT} 必须大于 0")));
        }

        let max_retries = option_env(ENV_MAX_RETRIES)
            .map(|value| {
                value.parse::<u32>().map_err(|e| {
                    Error::InvalidConfig(format!("{ENV_MAX_RETRIES} 不是合法整数: {e}"))
                })
            })
            .transpose()?
            .unwrap_or(2);
        if max_retries > 10 {
            return Err(Error::InvalidConfig(format!(
                "{ENV_MAX_RETRIES} 不能大于 10"
            )));
        }

        Ok(Self {
            protocol,
            base_url,
            api_key,
            model,
            timeout_secs,
            max_retries,
        })
    }

    /// 解析协议、补齐默认地址/模型，构造共享的 Provider 和 Model。
    ///
    /// `openai-compatible` 故意没有默认地址和模型：兼容端点的 URL、模型命名差异
    /// 太大，猜一个默认值只会把配置错误推迟到运行期。用户必须显式提供这两项。
    pub fn build(&self) -> Result<Model> {
        let configured_protocol = self.protocol.trim();
        let protocol_name = if configured_protocol.is_empty() {
            "openai"
        } else {
            configured_protocol
        };
        let protocol = protocol_by_name(protocol_name)?;

        let base_url = self
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default_base_url(protocol_name).to_string());
        if base_url.is_empty() {
            return Err(Error::InvalidConfig(format!(
                "provider {protocol_name} 没有默认 base_url，请显式设置 LLM_BASE_URL"
            )));
        }

        let model_name = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default_model(protocol_name).to_string());
        if model_name.is_empty() {
            return Err(Error::InvalidConfig(format!(
                "provider {protocol_name} 没有默认模型，请显式设置 LLM_MODEL"
            )));
        }

        // Provider 用 Arc 共享：同一个连接池可以挂多个 Model，后续如果要做
        // “同一网关下切换模型”，不需要重新建 Client。
        let provider = Arc::new(
            Provider::new(base_url, self.api_key.clone(), protocol, self.timeout_secs)?
                .with_retries(self.max_retries),
        );
        Ok(Model::new(provider, model_name))
    }
}

/// 协议默认地址。返回空串表示“没有合理默认值”，由 `build` 转成配置错误。
///
/// `zhipu/glm/bigmodel` 走 OpenAI 兼容协议，只是地址和默认模型不同。
fn default_base_url(protocol: &str) -> &'static str {
    match protocol.trim().to_ascii_lowercase().as_str() {
        "openai" => "https://api.openai.com/v1",
        "openai-compatible" => "",
        "anthropic" | "claude" => "https://api.anthropic.com",
        "zhipu" | "glm" | "bigmodel" => "https://open.bigmodel.cn/api/paas/v4",
        _ => "",
    }
}

fn default_model(protocol: &str) -> &'static str {
    match protocol.trim().to_ascii_lowercase().as_str() {
        "openai" => "gpt-4o-mini",
        "openai-compatible" => "",
        "anthropic" | "claude" => "claude-sonnet-4-5",
        "zhipu" | "glm" | "bigmodel" => "glm-4.6",
        _ => "",
    }
}

/// 必填变量：缺失、空值、非 Unicode 分别报错，UI 可以据此给出不同提示。
fn required_env(key: &'static str) -> Result<String> {
    match env::var(key) {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() {
                Err(Error::EmptyEnv(key))
            } else {
                Ok(value.to_string())
            }
        }
        Err(env::VarError::NotPresent) => Err(Error::MissingEnv(key)),
        Err(env::VarError::NotUnicode(_)) => Err(Error::NonUnicodeEnv(key)),
    }
}

/// 可选变量：空白值视为未设置，避免把空字符串一路传进 URL/模型名。
fn option_env(key: &'static str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_api_key() {
        let config = LlmConfig::new("openai", "super-secret");
        let debug = format!("{config:?}");
        assert!(!debug.contains("super-secret"));
        assert!(debug.contains("***"));
    }

    #[test]
    fn openai_defaults_are_consistent() {
        let model = LlmConfig::new("openai", "key").build().unwrap();
        assert_eq!(model.name(), "gpt-4o-mini");
    }

    #[test]
    fn empty_api_key_is_rejected() {
        let error = LlmConfig::new("openai", "   ").build().unwrap_err();
        assert!(matches!(error, Error::InvalidConfig(_)));
    }

    #[test]
    fn openai_compatible_requires_explicit_base_url_and_model() {
        let error = LlmConfig::new("openai-compatible", "key")
            .build()
            .unwrap_err();
        assert!(matches!(error, Error::InvalidConfig(_)));
    }
}
