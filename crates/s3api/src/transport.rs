//! HTTP 与凭据的边界接口。`s3api` 只依赖这两个 trait，不绑定具体的 HTTP 库或运行时。

use std::io;
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use http::{HeaderMap, Method};

/// 请求体。流式变体自带长度，因为 S3 不接受分块传输编码。
pub enum Body {
    Empty,
    Bytes(Bytes),
    Stream {
        stream: BoxStream<'static, io::Result<Bytes>>,
        len: u64,
    },
}

impl Body {
    pub fn len(&self) -> u64 {
        match self {
            Body::Empty => 0,
            Body::Bytes(b) => b.len() as u64,
            Body::Stream { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 流式请求体失败后无法重放，客户端内部不对其重试。
    pub fn is_replayable(&self) -> bool {
        !matches!(self, Body::Stream { .. })
    }
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Body::Empty => write!(f, "Body::Empty"),
            Body::Bytes(b) => write!(f, "Body::Bytes({} 字节)", b.len()),
            Body::Stream { len, .. } => write!(f, "Body::Stream({len} 字节)"),
        }
    }
}

impl From<Bytes> for Body {
    fn from(b: Bytes) -> Self {
        Body::Bytes(b)
    }
}

impl From<Vec<u8>> for Body {
    fn from(v: Vec<u8>) -> Self {
        Body::Bytes(Bytes::from(v))
    }
}

impl From<String> for Body {
    fn from(s: String) -> Self {
        Body::Bytes(Bytes::from(s.into_bytes()))
    }
}

pub struct HttpRequest {
    pub method: Method,
    /// 完整 URL，路径与查询串已按 S3 规则编码。
    pub url: String,
    pub headers: HeaderMap,
    pub body: Body,
}

impl HttpRequest {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        HttpRequest {
            method,
            url: url.into(),
            headers: HeaderMap::new(),
            body: Body::Empty,
        }
    }
}

impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("body", &self.body)
            .finish_non_exhaustive()
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: BoxStream<'static, io::Result<Bytes>>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

pub trait HttpTransport: Send + Sync {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>>;

    /// 重试退避用。由传输层实现，因为只有它知道自己跑在哪个运行时上。
    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()>;
}

pub trait CredentialsProvider: Send + Sync {
    fn credentials(&self) -> BoxFuture<'_, Result<crate::types::Credentials, CredentialsError>>;
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("连接失败：{0}")]
    Connect(String),
    #[error("请求超时")]
    Timeout,
    #[error("读写失败：{0}")]
    Io(String),
    #[error("请求无效：{0}")]
    InvalidRequest(String),
}

impl TransportError {
    /// 这些错误重发一次可能就好了。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            TransportError::Connect(_) | TransportError::Timeout | TransportError::Io(_)
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    #[error("获取凭据失败：{0}")]
    Fetch(String),
    #[error("凭据被拒绝：{0}")]
    Rejected(String),
}

/// 固定凭据，给填写 access key 的账户用。
pub struct StaticCredentials(pub crate::types::Credentials);

impl CredentialsProvider for StaticCredentials {
    fn credentials(&self) -> BoxFuture<'_, Result<crate::types::Credentials, CredentialsError>> {
        let creds = self.0.clone();
        Box::pin(async move { Ok(creds) })
    }
}
