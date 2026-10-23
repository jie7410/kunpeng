//! 错误类型按“调用方需要怎么处理”来分，而不是统一包成字符串。
//!
//! 几类错误的边界：
//! - 配置错误在启动阶段暴露；
//! - `Api` / `RateLimited` / `ParseResponse` 保留 provider 和响应片段；
//! - `Http` 保留传输错误类别和底层 source，避免退化成无结构字符串；
//! - 工具错误单独建模。Agent 会把它们转成 `tool_result.is_error` 回传给模型，
//!   而不是让整轮对话失败。

use std::fmt;
use std::time::Duration;

/// 统一 Result 别名。`E` 保留为默认参数，需要精确错误类型的内部函数可以写
/// `Result<T, MyError>`，常见调用点仍然只写 `Result<T>`。
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// 传输层错误类别。由 provider/工具层根据具体 HTTP 客户端错误分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HttpErrorKind {
    Timeout,
    Connect,
    Request,
    Body,
    Other,
}

impl fmt::Display for HttpErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("超时"),
            Self::Connect => f.write_str("连接"),
            Self::Request => f.write_str("请求"),
            Self::Body => f.write_str("响应体读取"),
            Self::Other => f.write_str("传输"),
        }
    }
}

/// 带 provider 响应上下文的解析错误。整个结构 Box 化，避免撑大 `Error` enum。
#[derive(Debug)]
pub struct ParseResponseError {
    pub provider: &'static str,
    pub body: String,
    pub source: serde_json::Error,
}

impl ParseResponseError {
    pub fn new(provider: &'static str, body: String, source: serde_json::Error) -> Self {
        Self {
            provider,
            body,
            source,
        }
    }
}

/// 工具参数 JSON 解析错误。与 ParseResponse 一样 Box 化，避免撑大 `Error`。
#[derive(Debug)]
pub struct InvalidToolArgumentsError {
    pub tool: String,
    pub raw: String,
    pub source: serde_json::Error,
}

impl InvalidToolArgumentsError {
    pub fn new(tool: String, raw: String, source: serde_json::Error) -> Self {
        Self { tool, raw, source }
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// 配置组合不合法，例如 URL 为空、timeout 为 0、兼容端点缺少模型名。
    InvalidConfig(String),
    /// `LLM_PROVIDER` 不在已知别名表里。
    UnsupportedProvider(String),
    /// DNS、TLS、连接、读取超时等传输层失败。
    ///
    /// `message` 用于展示，`source` 保留底层错误链；具体 HTTP 客户端类型不会
    /// 进入 core。
    Http {
        kind: HttpErrorKind,
        message: String,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
    /// 服务端返回非 2xx（429 以外的状态）。
    Api {
        provider: &'static str,
        status: u16,
        body: String,
    },
    /// 429 限流。单独建模，调用方/UI 可以读取 `retry_after` 做退避提示。
    RateLimited {
        provider: &'static str,
        retry_after: Option<Duration>,
        body: String,
    },
    /// JSON-RPC 协议错误响应（MCP、A2A）。`code` 保留协议错误码，调用方可以
    /// 编程式区分 method not found、无效参数等，而不必解析错误字符串。
    Rpc {
        protocol: &'static str,
        code: i64,
        message: String,
    },
    /// 本地文件/进程等 IO 失败。
    Io(std::io::Error),
    /// 通用 JSON 序列化/反序列化错误；不假设一定是 provider 响应。
    Json(serde_json::Error),
    /// provider 响应体解析失败。带 provider 和响应片段，便于区分“模型返回错”
    /// 与“网关返回了 HTML”。
    ParseResponse(Box<ParseResponseError>),
    /// 工具注册失败：名称不合规或重名。
    InvalidTool { name: String, reason: String },
    /// 模型请求了未注册的工具。单独建模，UI 可以据此提示“模型产生了无效调用”。
    UnknownTool { name: String },
    /// 工具参数不是合法 JSON。
    ///
    /// 这里不能退化成 `Value::Null`：工具会拿到错误参数；下一轮把 `null` 回放给
    /// OpenAI/Anthropic 时，两家都可能直接返回 400。
    InvalidToolArguments(Box<InvalidToolArgumentsError>),
    /// 工具执行超过配置的工具超时。超时只终止该工具，工具结果仍会
    /// 以错误形式回传给模型，让模型有机会改用别的方案。
    ToolTimeout { name: String, timeout_ms: u64 },
    /// 调用方主动取消（包括流式消费方返回 `StreamControl::Stop`）。
    Cancelled,
    /// 兜底错误。新增可分类的错误时优先加变体，不要继续往这里塞。
    Other(String),
}

impl Error {
    pub fn http(kind: HttpErrorKind, message: impl Into<String>) -> Self {
        Self::Http {
            kind,
            message: message.into(),
            source: None,
        }
    }

    pub fn http_with_source(
        kind: HttpErrorKind,
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Http {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    /// 按状态码分流 429；调用方只需提供已截断的 body。
    pub fn api(
        provider: &'static str,
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    ) -> Self {
        if status == 429 {
            Self::RateLimited {
                provider,
                retry_after,
                body,
            }
        } else {
            Self::Api {
                provider,
                status,
                body,
            }
        }
    }

    /// 构造各等待阶段（响应头、body、流式 chunk、JSON-RPC 响应）的超时错误。
    ///
    /// 超时按“阶段”而不是“整次请求”施加：一次长流式生成可以合法地超过任何
    /// 总上限，只有某一阶段停滞才应该被判定为挂起。阶段名进入错误消息，
    /// 例如 “响应头”、“SSE 流式数据”。
    pub fn timeout(stage: &str, timeout: Duration) -> Self {
        Self::http(
            HttpErrorKind::Timeout,
            format!("{stage}超时（{timeout:?}）"),
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidConfig(message) => write!(f, "配置无效: {message}"),
            Error::UnsupportedProvider(provider) => write!(f, "不支持的 provider: {provider}"),
            Error::Http {
                kind,
                message,
                source,
            } => {
                if let Some(source) = source {
                    write!(f, "HTTP {kind}失败: {message}; source: {source}")
                } else {
                    write!(f, "HTTP {kind}失败: {message}")
                }
            }
            Error::Api {
                provider,
                status,
                body,
            } => write!(f, "{provider} API 返回 {status}: {body}"),
            Error::RateLimited {
                provider,
                retry_after,
                body,
            } => match retry_after {
                Some(delay) => write!(
                    f,
                    "{provider} 触发限流（429），建议等待 {}s: {body}",
                    delay.as_secs()
                ),
                None => write!(f, "{provider} 触发限流（429）: {body}"),
            },
            Error::Rpc {
                protocol,
                code,
                message,
            } => write!(f, "{protocol} JSON-RPC 错误（{code}）: {message}"),
            Error::Io(error) => write!(f, "IO 失败: {error}"),
            Error::Json(error) => write!(f, "JSON 序列化/解析失败: {error}"),
            Error::ParseResponse(error) => write!(
                f,
                "{} 响应解析失败: {}; 响应体: {}",
                error.provider, error.source, error.body
            ),
            Error::InvalidTool { name, reason } => write!(f, "工具 {name} 无效: {reason}"),
            Error::UnknownTool { name } => write!(f, "未注册工具: {name}"),
            Error::InvalidToolArguments(error) => {
                write!(
                    f,
                    "工具 {} 参数不是合法 JSON: {}; 原始参数: {}",
                    error.tool, error.source, error.raw
                )
            }
            Error::ToolTimeout { name, timeout_ms } => {
                write!(f, "工具 {name} 执行超时（{timeout_ms} ms）")
            }
            Error::Cancelled => write!(f, "操作已取消"),
            Error::Other(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Http { source, .. } => source
                .as_ref()
                .map(|source| source.as_ref() as &(dyn std::error::Error + 'static)),
            Error::Io(error) => Some(error),
            Error::Json(error) => Some(error),
            Error::ParseResponse(error) => Some(&error.source),
            Error::InvalidToolArguments(error) => Some(&error.source),
            _ => None,
        }
    }
}

// `?` 能直接作用在 serde_json 的结果上。工具作者写
// `serde_json::from_value(...)?` 时不需要每次 map_err。
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::Json(error)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_is_a_distinct_variant() {
        let error = Error::api(
            "openai",
            429,
            "slow down".into(),
            Some(Duration::from_secs(2)),
        );
        assert!(matches!(
            error,
            Error::RateLimited {
                provider: "openai",
                retry_after: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn api_and_parse_errors_carry_provider() {
        let api = Error::api("anthropic", 500, "boom".into(), None);
        assert!(matches!(
            api,
            Error::Api {
                provider: "anthropic",
                ..
            }
        ));

        let parse = Error::ParseResponse(Box::new(ParseResponseError::new(
            "openai",
            "{}".into(),
            serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
        )));
        assert!(parse.to_string().starts_with("openai 响应解析失败"));
    }

    #[test]
    fn json_error_is_generic_not_response_specific() {
        let error: Error = serde_json::from_str::<serde_json::Value>("{")
            .unwrap_err()
            .into();
        assert!(matches!(error, Error::Json(_)));
        assert!(error.to_string().starts_with("JSON 序列化/解析失败"));
    }

    #[test]
    fn error_enum_stays_small() {
        assert!(std::mem::size_of::<Error>() <= 64);
    }

    #[test]
    fn rpc_errors_carry_protocol_and_code() {
        let error = Error::Rpc {
            protocol: "MCP",
            code: -32601,
            message: "method not found".into(),
        };
        assert!(error.to_string().contains("-32601"));
        assert!(error.to_string().contains("method not found"));
    }

    #[test]
    fn timeout_error_names_the_stage() {
        let error = Error::timeout("响应头", Duration::from_secs(120));
        assert!(matches!(
            error,
            Error::Http {
                kind: HttpErrorKind::Timeout,
                ..
            }
        ));
        assert!(error.to_string().contains("响应头"));
    }
}
