//! 错误类型按“调用方需要怎么处理”来分，而不是统一包成字符串。
//!
//! 几类错误的边界：
//! - 配置错误在启动阶段暴露；
//! - `Api` / `ParseResponse` 保留响应体片段，因为不少网关会在 HTTP 200 里返回
//!   错误 JSON 或 HTML，只给状态码没法排查；
//! - 工具错误单独建模。Agent 会把它们转成 `tool_result.is_error` 回传给模型，
//!   而不是让整轮对话失败。

/// 统一 Result 别名。`E` 保留为默认参数，需要精确错误类型的内部函数可以写
/// `Result<T, MyError>`，常见调用点仍然只写 `Result<T>`。
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// 配置组合不合法，例如 URL 为空、timeout 为 0、兼容端点缺少模型名。
    InvalidConfig(String),
    /// `LLM_PROVIDER` 不在已知别名表里。
    UnsupportedProvider(String),
    /// DNS、TLS、连接、读取超时等传输层失败。
    ///
    /// 存字符串而不是具体的 HTTP 客户端错误，避免 core 依赖 reqwest；
    /// provider 适配器负责把底层错误转换进来。
    Http(String),
    /// 本地文件/进程等 IO 失败。
    Io(std::io::Error),
    /// 服务端返回非 2xx。`body` 已截断，避免日志和 UI 被大响应拖垮。
    Api { status: u16, body: String },
    /// 通用 serde_json 解析错误，主要给 `From<serde_json::Error>` 使用。
    Parse(serde_json::Error),
    /// 响应体解析失败。带上 provider 和 body 片段，便于区分“模型返回错”与
    /// “网关返回了 HTML”。
    ParseResponse {
        provider: &'static str,
        body: String,
        source: serde_json::Error,
    },
    /// 工具注册失败：名称不合规或重名。
    InvalidTool { name: String, reason: String },
    /// 模型请求了未注册的工具。单独建模，UI 可以据此提示“模型产生了无效调用”。
    UnknownTool { name: String },
    /// 工具参数不是合法 JSON。
    ///
    /// 这里不能退化成 `Value::Null`：工具会拿到错误参数；下一轮把 `null` 回放给
    /// OpenAI/Anthropic 时，两家都可能直接返回 400。
    InvalidToolArguments {
        tool: String,
        raw: String,
        source: serde_json::Error,
    },
    /// 工具执行超过配置的工具超时。超时只终止该工具，工具结果仍会
    /// 以错误形式回传给模型，让模型有机会改用别的方案。
    ToolTimeout { name: String, timeout_ms: u64 },
    /// 调用方通过取消令牌中止。模型请求的 future 被 drop，但历史保持可回放。
    Cancelled,
    /// 兜底错误。新增可分类的错误时优先加变体，不要继续往这里塞。
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::InvalidConfig(message) => write!(f, "配置无效: {message}"),
            Error::UnsupportedProvider(provider) => write!(f, "不支持的 provider: {provider}"),
            Error::Http(error) => write!(f, "HTTP 请求失败: {error}"),
            Error::Io(error) => write!(f, "IO 失败: {error}"),
            Error::Api { status, body } => write!(f, "API 返回 {status}: {body}"),
            Error::Parse(error) => write!(f, "响应解析失败: {error}"),
            Error::ParseResponse {
                provider,
                body,
                source,
            } => write!(f, "{provider} 响应解析失败: {source}; 响应体: {body}"),
            Error::InvalidTool { name, reason } => write!(f, "工具 {name} 无效: {reason}"),
            Error::UnknownTool { name } => write!(f, "未注册工具: {name}"),
            Error::InvalidToolArguments { tool, raw, source } => {
                write!(
                    f,
                    "工具 {tool} 参数不是合法 JSON: {source}; 原始参数: {raw}"
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
            Error::Http(_) => None,
            Error::Io(error) => Some(error),
            Error::Parse(error) => Some(error),
            Error::ParseResponse { source, .. } => Some(source),
            Error::InvalidToolArguments { source, .. } => Some(source),
            _ => None,
        }
    }
}

// `?` 能直接作用在 serde_json 的结果上。工具作者写
// `serde_json::from_value(...)?` 时不需要每次 map_err。
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::Parse(error)
    }
}
