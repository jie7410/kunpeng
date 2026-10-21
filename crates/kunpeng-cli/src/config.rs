//! 环境变量到 `LlmConfig` 的适配。
//!
//! 这里只做宿主层的工作：读取、trim、类型转换和默认值。协议别名、URL 和模型
//! 默认值仍由 `kunpeng-provider` 的 `LlmConfig::build` 和适配器负责。

use anyhow::{bail, Context, Result};
use kunpeng::provider::LlmConfig;
use std::env;

const ENV_PROTOCOL: &str = "LLM_PROVIDER";
const ENV_BASE_URL: &str = "LLM_BASE_URL";
const ENV_API_KEY: &str = "LLM_API_KEY";
const ENV_MODEL: &str = "LLM_MODEL";
const ENV_TIMEOUT: &str = "LLM_TIMEOUT_SECS";
const ENV_MAX_RETRIES: &str = "LLM_MAX_RETRIES";

/// 从环境变量构造配置。
///
/// `.env`、K8s secret 和复制粘贴经常留下换行或空格，所有值都会 trim。
pub fn from_env() -> Result<LlmConfig> {
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
        Err(env::VarError::NotUnicode(_)) => {
            bail!("环境变量 {ENV_PROTOCOL} 包含非 Unicode 字符")
        }
    };

    let api_key = required(ENV_API_KEY)?;
    let mut config = LlmConfig::new(protocol, api_key);

    if let Some(base_url) = optional(ENV_BASE_URL) {
        config = config.with_base_url(base_url);
    }
    if let Some(model) = optional(ENV_MODEL) {
        config = config.with_model(model);
    }

    if let Some(timeout) = optional(ENV_TIMEOUT) {
        let timeout = timeout
            .parse::<u64>()
            .with_context(|| format!("环境变量 {ENV_TIMEOUT} 不是合法整数"))?;
        if timeout == 0 {
            bail!("环境变量 {ENV_TIMEOUT} 必须大于 0");
        }
        config = config.with_timeout_secs(timeout);
    }

    if let Some(retries) = optional(ENV_MAX_RETRIES) {
        let retries = retries
            .parse::<u32>()
            .with_context(|| format!("环境变量 {ENV_MAX_RETRIES} 不是合法整数"))?;
        if retries > 10 {
            bail!("环境变量 {ENV_MAX_RETRIES} 不能大于 10");
        }
        config = config.with_max_retries(retries);
    }

    Ok(config)
}

fn required(key: &'static str) -> Result<String> {
    match env::var(key) {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() {
                bail!("环境变量 {key} 不能为空")
            } else {
                Ok(value.to_string())
            }
        }
        Err(env::VarError::NotPresent) => bail!("请先设置环境变量 {key}"),
        Err(env::VarError::NotUnicode(_)) => bail!("环境变量 {key} 包含非 Unicode 字符"),
    }
}

fn optional(key: &'static str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
