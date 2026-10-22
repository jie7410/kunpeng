use crate::Tool;
use async_trait::async_trait;
use kunpeng_core::error::{Error, Result};
use kunpeng_core::HttpErrorKind;

fn http_error(error: reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        HttpErrorKind::Timeout
    } else if error.is_connect() {
        HttpErrorKind::Connect
    } else if error.is_body() {
        HttpErrorKind::Body
    } else if error.is_request() {
        HttpErrorKind::Request
    } else {
        HttpErrorKind::Other
    };
    Error::http_with_source(kind, error.to_string(), error)
}
use kunpeng_core::types::ToolDefinition;
use std::time::Duration;

/// HTTP GET 工具。默认要求显式 host allowlist，避免模型任意访问内网。
pub struct HttpFetchTool {
    client: reqwest::Client,
    allowed_hosts: Vec<String>,
    max_bytes: usize,
}

impl HttpFetchTool {
    pub fn new(allowed_hosts: Vec<String>, max_bytes: usize) -> Result<Self> {
        if allowed_hosts.is_empty() {
            return Err(Error::InvalidConfig(
                "http_fetch 至少需要一个 allowed_hosts 条目".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(http_error)?;
        Ok(Self {
            client,
            allowed_hosts: allowed_hosts
                .into_iter()
                .map(|host| host.to_ascii_lowercase())
                .collect(),
            max_bytes,
        })
    }

    fn host_allowed(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.allowed_hosts
            .iter()
            .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
    }
}

#[async_trait]
impl Tool for HttpFetchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "http_fetch".into(),
            description: "抓取 allowlist 内 URL 的文本内容".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "http/https URL"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let raw_url = arguments
            .get("url")
            .and_then(|value| value.as_str())
            .ok_or_else(|| Error::Other("http_fetch 缺少 url".into()))?;
        let url = reqwest::Url::parse(raw_url)
            .map_err(|error| Error::Other(format!("http_fetch URL 无效: {error}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::Other("http_fetch 只支持 http/https".into()));
        }
        let host = url
            .host_str()
            .ok_or_else(|| Error::Other("http_fetch URL 缺少 host".into()))?;
        if !self.host_allowed(host) {
            return Err(Error::Other(format!("http_fetch 不允许访问 host: {host}")));
        }

        let response = self.client.get(url).send().await.map_err(http_error)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(http_error)?;
        let bytes = &bytes[..bytes.len().min(self.max_bytes)];
        let body = String::from_utf8_lossy(bytes).into_owned();
        if !status.is_success() {
            return Err(Error::api("http_fetch", status.as_u16(), body, None));
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_rejects_unknown_hosts() {
        let tool = HttpFetchTool::new(vec!["example.com".into()], 1024).unwrap();
        assert!(tool.host_allowed("example.com"));
        assert!(tool.host_allowed("docs.example.com"));
        assert!(!tool.host_allowed("evil.com"));
        assert!(!tool.host_allowed("127.0.0.1"));
    }

    #[test]
    fn empty_allowlist_is_rejected() {
        assert!(HttpFetchTool::new(Vec::new(), 1024).is_err());
    }
}
