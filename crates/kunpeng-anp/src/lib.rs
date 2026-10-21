//! ANP（Agent Network Protocol）适配器的传输层。
//!
//! ANP 仍处在快速演进期，不同实现的 JSON-LD `@context`、发现机制和 DID 认证
//! 细节并不完全一致。这里不硬编码某一版规范，而是提供：
//!
//! - 可配置的 JSON-LD context；
//! - `AgentEnvelope` 到 JSON-LD 文档的稳定映射；
//! - HTTP POST 发送和原始响应返回；
//! - `AgentProtocol` 实现，供多智能体层调用；
//! - `did:web` DID 文档解析与 service endpoint 发现。
//!
//! `@context`、DID service type 和发现策略都可以配置；不同 ANP profile 不需要
//! 修改 `kunpeng-multi`。

use async_trait::async_trait;
use kunpeng_core::{Error, Result};

fn http_error(error: reqwest::Error) -> Error {
    Error::Http(error.to_string())
}
use kunpeng_multi::{AgentEnvelope, AgentProtocol};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{sync::Arc, time::Duration};

/// ANP JSON-LD 文档的最小映射。
///
/// `payload` 保留 `AgentPayload` 的 serde 结构；如果对端要求 ANP 原生 payload，
/// 可以先用 `send_document` 发送自定义文档。
#[derive(Clone)]
pub struct AnpClient {
    http: reqwest::Client,
    endpoint: String,
    context: Value,
}

/// DID 文档的通用结构。未知字段保留在 `extra`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DidDocument {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub service: Vec<DidService>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// DID document service entry。`type` 可能是字符串或字符串数组，因此保留为 Value。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DidService {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type")]
    pub service_type: Value,
    #[serde(rename = "serviceEndpoint", default)]
    pub service_endpoint: Value,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl DidService {
    /// 兼容 DID Core 的字符串、对象（uri/url/href）和数组形式。
    pub fn endpoint(&self) -> Option<String> {
        endpoint_value(&self.service_endpoint)
    }

    fn matches_type(&self, service_type: &str) -> bool {
        match &self.service_type {
            Value::String(value) => value == service_type,
            Value::Array(values) => values
                .iter()
                .any(|value| value.as_str() == Some(service_type)),
            _ => false,
        }
    }
}

impl DidDocument {
    pub fn service_of_type(&self, service_type: &str) -> Option<&DidService> {
        self.service
            .iter()
            .find(|service| service.matches_type(service_type))
    }

    pub fn service_endpoint(&self, service_type: &str) -> Option<String> {
        self.service_of_type(service_type)?.endpoint()
    }
}

/// DID resolver 抽象。测试可以注入静态文档，生产可以接 did:web 或其他 method。
#[async_trait]
pub trait DidResolver: Send + Sync {
    async fn resolve(&self, did: &str) -> Result<DidDocument>;
}

/// `did:web` resolver。
///
/// `did:web:example.com` -> `https://example.com/.well-known/did.json`；
/// `did:web:example.com:user:alice` -> `https://example.com/user/alice/did.json`。
/// 本地测试地址（`localhost` / `127.0.0.1` / `::1`）使用 http。
#[derive(Clone)]
pub struct HttpDidResolver {
    http: reqwest::Client,
}

impl Default for HttpDidResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpDidResolver {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    pub fn with_http(http: reqwest::Client) -> Self {
        Self { http }
    }
}

#[async_trait]
impl DidResolver for HttpDidResolver {
    async fn resolve(&self, did: &str) -> Result<DidDocument> {
        let url = did_web_url(did)?;
        let response = self
            .http
            .get(url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?;
        response.json::<DidDocument>().await.map_err(http_error)
    }
}

/// 发现结果：DID、解析出的 endpoint 和原始 service entry。
#[derive(Debug, Clone)]
pub struct AnpEndpoint {
    pub did: String,
    pub endpoint: String,
    pub service: DidService,
}

/// 从 DID 文档中选择 ANP service。`service_type` 可配置，默认 `ANP`。
pub struct AnpDiscovery {
    resolver: Arc<dyn DidResolver>,
    service_type: String,
}

impl AnpDiscovery {
    pub fn new(resolver: Arc<dyn DidResolver>) -> Self {
        Self {
            resolver,
            service_type: "ANP".into(),
        }
    }

    pub fn with_service_type(mut self, service_type: impl Into<String>) -> Self {
        self.service_type = service_type.into();
        self
    }

    pub async fn discover(&self, did: &str) -> Result<AnpEndpoint> {
        let document = self.resolver.resolve(did).await?;
        let service = document
            .service_of_type(&self.service_type)
            .cloned()
            .ok_or_else(|| {
                Error::Other(format!(
                    "DID 文档 {did} 没有 service type {}",
                    self.service_type
                ))
            })?;
        let endpoint = service.endpoint().ok_or_else(|| {
            Error::Other(format!(
                "DID 文档 {did} 的 service {} 缺少 endpoint",
                service.id
            ))
        })?;
        Ok(AnpEndpoint {
            did: did.to_string(),
            endpoint,
            service,
        })
    }
}

impl AnpClient {
    /// `context` 可以是字符串或完整 JSON-LD context 对象，由部署方决定。
    pub fn new(endpoint: impl Into<String>, context: impl Into<Value>) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: endpoint.into(),
            context: context.into(),
        }
    }

    pub fn with_http(
        http: reqwest::Client,
        endpoint: impl Into<String>,
        context: impl Into<Value>,
    ) -> Self {
        Self {
            http,
            endpoint: endpoint.into(),
            context: context.into(),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// 从 DID 解析 ANP endpoint，再构造 client。
    pub async fn from_did(
        did: &str,
        context: impl Into<Value>,
        resolver: Arc<dyn DidResolver>,
        service_type: &str,
    ) -> Result<Self> {
        let discovered = AnpDiscovery::new(resolver)
            .with_service_type(service_type)
            .discover(did)
            .await?;
        Ok(Self::new(discovered.endpoint, context))
    }

    /// 发送任意 JSON-LD 文档；返回对端 JSON 响应。
    pub async fn send_document(&self, document: Value) -> Result<Value> {
        let response = self
            .http
            .post(&self.endpoint)
            .json(&document)
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?;
        let text = response.text().await.map_err(http_error)?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(Error::Parse)
    }

    /// 把内核信封转换成当前 client 的 JSON-LD profile。
    pub fn envelope_document(&self, envelope: &AgentEnvelope) -> Value {
        build_document(&self.context, envelope)
    }
}

#[async_trait]
impl AgentProtocol for AnpClient {
    async fn send_envelope(&self, envelope: &AgentEnvelope) -> Result<Value> {
        self.send_document(self.envelope_document(envelope)).await
    }
}

/// 纯函数映射，便于测试和自定义 transport 复用。
pub fn build_document(context: &Value, envelope: &AgentEnvelope) -> Value {
    json!({
        "@context": context,
        "@type": "AgentEnvelope",
        "id": envelope.id,
        "from": envelope.from,
        "to": envelope.to,
        "correlationId": envelope.correlation_id,
        "payload": envelope.payload
    })
}

fn endpoint_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Array(values) => values.iter().find_map(endpoint_value),
        Value::Object(map) => ["uri", "url", "href"]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str).map(str::to_string)),
        _ => None,
    }
}

/// `did:web` -> HTTP(S) DID document URL。
pub fn did_web_url(did: &str) -> Result<String> {
    let rest = did
        .strip_prefix("did:web:")
        .ok_or_else(|| Error::Other(format!("DID resolver 只支持 did:web: {did}")))?;
    let mut parts = rest.split(':');
    let host = percent_decode(parts.next().unwrap_or_default())?;
    if host.is_empty()
        || host.contains('/')
        || host.contains('\\')
        || host.chars().any(char::is_whitespace)
    {
        return Err(Error::Other(format!("did:web host 无效: {host}")));
    }
    let local = host == "localhost"
        || host.starts_with("localhost:")
        || host == "127.0.0.1"
        || host.starts_with("127.0.0.1:")
        || host == "[::1]"
        || host.starts_with("[::1]:");
    let scheme = if local { "http" } else { "https" };

    let mut path = Vec::new();
    for part in parts {
        let segment = percent_decode(part)?;
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains('/') {
            return Err(Error::Other(format!(
                "did:web path segment 无效: {segment}"
            )));
        }
        path.push(segment);
    }
    if path.is_empty() {
        Ok(format!("{scheme}://{host}/.well-known/did.json"))
    } else {
        Ok(format!("{scheme}://{host}/{}/did.json", path.join("/")))
    }
}

fn percent_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(Error::Other(format!("DID 百分号编码无效: {value}")));
            }
            let high = hex(bytes[index + 1])?;
            let low = hex(bytes[index + 2])?;
            output.push(high * 16 + low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).map_err(|error| Error::Other(format!("DID 不是 UTF-8: {error}")))
}

fn hex(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::Other(format!(
            "DID 百分号编码无效: {}",
            byte as char
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kunpeng_multi::{AgentId, AgentPayload};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut data = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            data.extend_from_slice(&buffer[..read]);
            let Some(header_end) = data.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let header_end = header_end + 4;
            let headers = String::from_utf8_lossy(&data[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                })
                .unwrap_or(0);
            if data.len() >= header_end + content_length {
                break;
            }
        }
        data
    }

    fn serve_once(response: String) -> (String, thread::JoinHandle<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let body_start = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            let body: Value = serde_json::from_slice(&request[body_start..]).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .unwrap();
            body
        });
        (format!("http://{address}/anp"), handle)
    }

    fn serve_get_once<F>(make_response: F) -> (String, thread::JoinHandle<String>)
    where
        F: FnOnce(&str) -> String + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let response = make_response(&base);
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (base, handle)
    }

    #[test]
    fn builds_did_web_urls() {
        assert_eq!(
            did_web_url("did:web:example.com").unwrap(),
            "https://example.com/.well-known/did.json"
        );
        assert_eq!(
            did_web_url("did:web:example.com:user:alice").unwrap(),
            "https://example.com/user/alice/did.json"
        );
        assert_eq!(
            did_web_url("did:web:localhost%3A8080").unwrap(),
            "http://localhost:8080/.well-known/did.json"
        );
        assert!(did_web_url("did:key:z6Mk").is_err());
    }

    #[test]
    fn reads_service_endpoint_shapes() {
        let document: DidDocument = serde_json::from_value(json!({
            "id": "did:web:example.com",
            "service": [
                { "id": "#anp", "type": ["ANP", "Agent"], "serviceEndpoint": ["https://agent.example/anp"] },
                { "id": "#other", "type": "Other", "serviceEndpoint": { "uri": "https://other.example" } }
            ]
        }))
        .unwrap();
        assert_eq!(
            document.service_endpoint("ANP").as_deref(),
            Some("https://agent.example/anp")
        );
        assert_eq!(
            document.service_endpoint("Other").as_deref(),
            Some("https://other.example")
        );
        assert!(document.service_endpoint("missing").is_none());
    }

    #[tokio::test]
    async fn discovers_anp_endpoint_from_did_web() {
        let (base, server) = serve_get_once(|base| {
            let port = base.rsplit(':').next().unwrap();
            json!({
                "id": format!("did:web:127.0.0.1%3A{port}"),
                "service": [{
                    "id": "#anp",
                    "type": "ANP",
                    "serviceEndpoint": "http://127.0.0.1:9/anp"
                }]
            })
            .to_string()
        });
        let port = base.rsplit(':').next().unwrap();
        let resolver = Arc::new(HttpDidResolver::new());
        let discovered = AnpDiscovery::new(resolver)
            .discover(&format!("did:web:127.0.0.1%3A{port}"))
            .await
            .unwrap();
        assert_eq!(discovered.endpoint, "http://127.0.0.1:9/anp");
        assert_eq!(discovered.service.id, "#anp");
        let request = server.join().unwrap();
        assert!(request.starts_with("GET /.well-known/did.json"));
    }

    #[tokio::test]
    async fn posts_jsonld_envelope() {
        let (endpoint, server) = serve_once(r#"{"ok":true}"#.to_string());
        let client = AnpClient::new(endpoint, "https://example.test/anp");
        let envelope = AgentEnvelope::new(
            AgentId::new("planner"),
            AgentId::new("worker"),
            AgentPayload::Task {
                task: "整理数据".into(),
            },
        );

        let response = client.send_envelope(&envelope).await.unwrap();
        assert_eq!(response["ok"], true);

        let request = server.join().unwrap();
        assert_eq!(request["@context"], "https://example.test/anp");
        assert_eq!(request["@type"], "AgentEnvelope");
        assert_eq!(request["from"], "planner");
        assert_eq!(request["to"], "worker");
        assert_eq!(request["payload"]["kind"], "task");
    }

    #[test]
    fn maps_envelope_to_jsonld() {
        let envelope = AgentEnvelope::new(
            AgentId::new("planner"),
            AgentId::new("worker"),
            AgentPayload::Task {
                task: "整理数据".into(),
            },
        );
        let document = build_document(&json!("https://example.test/anp"), &envelope);
        assert_eq!(document["@context"], "https://example.test/anp");
        assert_eq!(document["@type"], "AgentEnvelope");
        assert_eq!(document["from"], "planner");
        assert_eq!(document["to"], "worker");
        assert_eq!(document["payload"]["kind"], "task");
        assert_eq!(document["payload"]["task"], "整理数据");
        assert_eq!(document["id"], envelope.id);
        assert_eq!(document["correlationId"], envelope.correlation_id);
    }
}
