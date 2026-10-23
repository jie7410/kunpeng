use crate::Tool;
use async_trait::async_trait;
use kunpeng_core::error::{Error, Result};
use kunpeng_core::types::ToolDefinition;
use kunpeng_http::http_error;
use std::time::Duration;

/// HTTP GET 工具。默认要求显式 host allowlist，避免模型任意访问内网。
///
/// 重定向在每一跳都重新校验 host：只在首个 URL 上检查 allowlist 的话，
/// 允许列表内主机上的一个 302 就能跳到内网元数据地址（SSRF）。
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
        let allowed_hosts: Vec<String> = allowed_hosts
            .into_iter()
            .map(|host| host.to_ascii_lowercase())
            .collect();
        let redirect_allowed = allowed_hosts.clone();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                let host = attempt.url().host_str().unwrap_or_default().to_string();
                if host_allowed(&redirect_allowed, &host) {
                    attempt.follow()
                } else {
                    attempt.error(Error::Other(format!(
                        "http_fetch 重定向到不允许的 host: {host}"
                    )))
                }
            }))
            .build()
            .map_err(http_error)?;
        Ok(Self {
            client,
            allowed_hosts,
            max_bytes,
        })
    }

    fn host_allowed(&self, host: &str) -> bool {
        host_allowed(&self.allowed_hosts, host)
    }
}

fn host_allowed(allowed_hosts: &[String], host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    allowed_hosts
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
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
    use std::io::{Read, Write};
    use std::net::TcpListener;

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

    /// 允许的主机 302 跳到未允许的 host 时，重定向策略必须拒绝。
    #[tokio::test]
    async fn redirect_to_unallowed_host_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            let head = "HTTP/1.1 302 Found\r\nLocation: http://192.0.2.1/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(head.as_bytes());
        });

        // 首跳 127.0.0.1 在允许列表内；重定向目标是未允许的网段地址。
        let tool = HttpFetchTool::new(vec!["127.0.0.1".into()], 1024).unwrap();
        let error = tool
            .call(serde_json::json!({ "url": format!("http://{address}/hop") }))
            .await
            .unwrap_err();
        // 自定义原因在 reqwest 重定向错误的 source 链里，逐层收集再断言。
        let mut chain = String::new();
        let mut current: Option<&dyn std::error::Error> = Some(&error);
        while let Some(err) = current {
            chain.push_str(&err.to_string());
            chain.push_str(" | ");
            current = err.source();
        }
        assert!(chain.contains("重定向"), "got: {chain}");
        let _ = server.join();
    }
}
