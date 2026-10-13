//! S3 客户端：请求构造、签名、重试与错误解析。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, Method};

use crate::model::PayloadSigning;
use crate::sigv4::{self, Payload, SigningRequest};
use crate::transport::{
    Body, CredentialsProvider, HttpRequest, HttpResponse, HttpTransport, TransportError,
};
use crate::types::{Endpoint, S3Error, Scheme};
use crate::xml;

/// 小于这个尺寸的请求体一律算 SHA256：代价可以忽略，兼容性最好。
const ALWAYS_SIGN_BELOW: usize = 64 * 1024;

/// 错误响应体最多读这么多，避免服务端返回一大坨 HTML。
const MAX_ERROR_BODY: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        RetryConfig {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(10),
        }
    }
}

pub struct S3Client {
    transport: Arc<dyn HttpTransport>,
    credentials: Arc<dyn CredentialsProvider>,
    endpoint: Endpoint,
    payload_signing: PayloadSigning,
    retry: RetryConfig,
    /// 服务端用 301 告知真实 region 时缓存在这里，后续请求直接用。
    region_override: Mutex<Option<String>>,
}

impl S3Client {
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        credentials: Arc<dyn CredentialsProvider>,
        endpoint: Endpoint,
    ) -> Self {
        S3Client {
            transport,
            credentials,
            endpoint,
            payload_signing: PayloadSigning::Auto,
            retry: RetryConfig::default(),
            region_override: Mutex::new(None),
        }
    }

    pub fn with_payload_signing(mut self, signing: PayloadSigning) -> Self {
        self.payload_signing = signing;
        self
    }

    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// 当前生效的 region，可能已被 301 响应修正过。
    pub fn region(&self) -> String {
        self.region_override
            .lock()
            .expect("region 锁")
            .clone()
            .unwrap_or_else(|| self.endpoint.region.clone())
    }

    /// 给 transfer 用：sha256 模式下流式体必须由上层预先算好哈希。
    #[allow(dead_code)]
    pub(crate) fn payload_signing(&self) -> PayloadSigning {
        self.payload_signing
    }

    /// 发送请求并在失败时重试。返回的响应体尚未读取。
    pub(crate) async fn execute(&self, req: Request) -> Result<HttpResponse, S3Error> {
        let creds = self.credentials.credentials().await?;
        let replayable = req.body.is_replayable();
        let mut body = req.body;
        let mut attempt = 0u32;
        let mut redirected = false;

        loop {
            attempt += 1;
            let attempt_body = match &body {
                Body::Empty => Body::Empty,
                Body::Bytes(b) => Body::Bytes(b.clone()),
                Body::Stream { .. } => std::mem::replace(&mut body, Body::Empty),
            };

            let mut headers = req.headers.clone();
            let payload = payload_for(
                &attempt_body,
                req.payload_hash.as_deref(),
                self.payload_signing,
                self.endpoint.scheme,
            );
            let bucket = req.bucket.as_deref();
            let host = self.endpoint.host_header(bucket);
            let path = self.endpoint.path(bucket, &req.key);
            let region = self.region();

            let signing = SigningRequest {
                method: req.method.as_str(),
                host: &host,
                path: &path,
                query: &req.query,
            };
            let canonical_query =
                sigv4::sign(&signing, &mut headers, &payload, &creds, &region, now());

            let http_req = HttpRequest {
                method: req.method.clone(),
                url: self.endpoint.url(bucket, &req.key, &canonical_query),
                headers,
                body: attempt_body,
            };

            // URL 里只有操作参数，签名在 Authorization 头上，凭据不会进日志
            let url = http_req.url.clone();
            let started = std::time::Instant::now();
            let result = self.transport.send(http_req).await;
            let elapsed_ms = started.elapsed().as_millis();

            let response = match result {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(
                        method = %req.method,
                        %url,
                        attempt,
                        elapsed_ms,
                        "S3 请求失败：{e}"
                    );
                    if e.is_retryable() && replayable && attempt < self.retry.max_attempts {
                        self.backoff(attempt).await;
                        continue;
                    }
                    return Err(S3Error::Transport(e));
                }
            };

            tracing::debug!(
                method = %req.method,
                %url,
                status = response.status,
                request_id = response
                    .header("x-amz-request-id")
                    .or_else(|| response.header("x-oss-request-id"))
                    .unwrap_or("-"),
                attempt,
                elapsed_ms,
                "S3 请求"
            );

            if (200..300).contains(&response.status) {
                return Ok(response);
            }

            // 桶在别的 region，换 region 重签一次。
            if response.status == 301
                && !redirected
                && let Some(region) = response.header("x-amz-bucket-region")
            {
                let region = region.to_owned();
                *self.region_override.lock().expect("region 锁") = Some(region);
                redirected = true;
                if replayable {
                    continue;
                }
            }

            let retryable = is_retryable_status(response.status);
            if retryable && replayable && attempt < self.retry.max_attempts {
                self.backoff(attempt).await;
                continue;
            }
            return Err(self.error_from(response).await);
        }
    }

    /// 指数退避加随机抖动，避免多个任务同时重试。
    async fn backoff(&self, attempt: u32) {
        let base = self
            .retry
            .initial_backoff
            .saturating_mul(1u32 << (attempt - 1).min(16))
            .min(self.retry.max_backoff);
        let jitter = fastrand::u64(0..=(base.as_millis() as u64 / 2).max(1));
        self.transport
            .sleep(base + Duration::from_millis(jitter))
            .await;
    }

    async fn error_from(&self, response: HttpResponse) -> S3Error {
        let status = response.status;
        let request_id = response
            .header("x-amz-request-id")
            .map(str::to_owned)
            .or_else(|| response.header("x-oss-request-id").map(str::to_owned));
        let body = collect_limited(response.body, MAX_ERROR_BODY)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
        if text.trim().is_empty() {
            // HEAD 响应没有体，只能按状态码给个说法。
            return S3Error::Service {
                status,
                code: code_for_status(status).to_owned(),
                message: String::new(),
                request_id,
            };
        }
        xml::parse_error(status, &text, request_id.as_deref())
    }

    /// 生成预签名 URL。签名与凭据都在这里，所以不放在 ops 里。
    pub(crate) async fn presign(
        &self,
        bucket: &str,
        key: &str,
        expires_secs: u64,
    ) -> Result<String, S3Error> {
        let creds = self.credentials.credentials().await?;
        let host = self.endpoint.host_header(Some(bucket));
        let path = self.endpoint.path(Some(bucket), key);
        let signing = SigningRequest {
            method: "GET",
            host: &host,
            path: &path,
            query: &[],
        };
        let query = sigv4::presign(&signing, &creds, &self.region(), now(), expires_secs);
        Ok(self.endpoint.url(Some(bucket), key, &query))
    }

    /// 读完响应体并转成字符串。
    pub(crate) async fn read_text(&self, response: HttpResponse) -> Result<String, S3Error> {
        let body = collect_limited(response.body, usize::MAX)
            .await
            .map_err(|e| S3Error::Transport(e))?;
        String::from_utf8(body.to_vec()).map_err(|e| S3Error::Decode(e.to_string()))
    }
}

/// 一次请求的描述。`key` 未编码，`query` 的键值都未编码。
pub(crate) struct Request {
    pub method: Method,
    pub bucket: Option<String>,
    pub key: String,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub body: Body,
    /// 流式请求体在 `sha256` 模式下必须由调用方预先算好哈希。
    pub payload_hash: Option<String>,
}

impl Request {
    pub fn new(method: Method) -> Self {
        Request {
            method,
            bucket: None,
            key: String::new(),
            query: Vec::new(),
            headers: HeaderMap::new(),
            body: Body::Empty,
            payload_hash: None,
        }
    }

    pub fn get() -> Self {
        Request::new(Method::GET)
    }

    pub fn put() -> Self {
        Request::new(Method::PUT)
    }

    pub fn post() -> Self {
        Request::new(Method::POST)
    }

    pub fn delete() -> Self {
        Request::new(Method::DELETE)
    }

    pub fn head() -> Self {
        Request::new(Method::HEAD)
    }

    pub fn bucket(mut self, bucket: &str) -> Self {
        self.bucket = Some(bucket.to_owned());
        self
    }

    pub fn key(mut self, key: &str) -> Self {
        self.key = key.to_owned();
        self
    }

    pub fn query(mut self, name: &str, value: impl ToString) -> Self {
        self.query.push((name.to_owned(), value.to_string()));
        self
    }

    /// 无值的查询参数，例如 `?uploads`。
    pub fn flag(mut self, name: &str) -> Self {
        self.query.push((name.to_owned(), String::new()));
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            self.headers.insert(n, v);
        }
        self
    }

    pub fn maybe_header(self, name: &str, value: Option<&str>) -> Self {
        match value {
            Some(v) => self.header(name, v),
            None => self,
        }
    }

    pub fn body(mut self, body: Body) -> Self {
        self.body = body;
        self
    }

    /// 带 Content-MD5 的请求体，服务端会校验并以 ETag 回传。
    pub fn body_with_md5(self, body: Body, md5: Option<[u8; 16]>) -> Self {
        match md5 {
            Some(digest) => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(digest);
                self.header("content-md5", &encoded).body(body)
            }
            None => self.body(body),
        }
    }

    /// 流式体在 sha256 模式下的预算哈希。
    #[allow(dead_code)]
    pub fn payload_hash(mut self, hash: Option<String>) -> Self {
        self.payload_hash = hash;
        self
    }
}

pub fn md5_of(data: &[u8]) -> [u8; 16] {
    use md5::Digest as _;
    md5::Md5::digest(data).into()
}

fn payload_for<'a>(
    body: &'a Body,
    precomputed: Option<&'a str>,
    signing: PayloadSigning,
    scheme: Scheme,
) -> Payload<'a> {
    if let Some(hash) = precomputed {
        return Payload::Precomputed(hash);
    }
    match body {
        Body::Empty => Payload::Empty,
        Body::Bytes(b) => {
            let must_sign = signing == PayloadSigning::Sha256
                || scheme == Scheme::Http
                || b.len() < ALWAYS_SIGN_BELOW;
            if must_sign {
                Payload::Bytes(b)
            } else {
                Payload::Unsigned
            }
        }
        // 流式体没法先读一遍再发，只能不签。sha256 模式下调用方必须给 payload_hash。
        Body::Stream { .. } => Payload::Unsigned,
    }
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

fn code_for_status(status: u16) -> &'static str {
    match status {
        301 => "PermanentRedirect",
        304 => "NotModified",
        403 => "AccessDenied",
        404 => "NotFound",
        409 => "Conflict",
        412 => "PreconditionFailed",
        416 => "InvalidRange",
        _ => "",
    }
}

async fn collect_limited(
    mut body: futures_util::stream::BoxStream<'static, std::io::Result<Bytes>>,
    limit: usize,
) -> Result<Bytes, TransportError> {
    let mut buf = BytesMut::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| TransportError::Io(e.to_string()))?;
        let room = limit.saturating_sub(buf.len());
        if room == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    Ok(buf.freeze())
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_bodies_are_always_signed() {
        let body = Body::Bytes(Bytes::from_static(b"<Delete/>"));
        assert!(matches!(
            payload_for(&body, None, PayloadSigning::Auto, Scheme::Https),
            Payload::Bytes(_)
        ));
    }

    #[test]
    fn large_bodies_are_unsigned_over_https() {
        let body = Body::Bytes(Bytes::from(vec![0u8; ALWAYS_SIGN_BELOW + 1]));
        assert!(matches!(
            payload_for(&body, None, PayloadSigning::Auto, Scheme::Https),
            Payload::Unsigned
        ));
        // 明文 HTTP 或显式要求时仍然算哈希
        assert!(matches!(
            payload_for(&body, None, PayloadSigning::Auto, Scheme::Http),
            Payload::Bytes(_)
        ));
        assert!(matches!(
            payload_for(&body, None, PayloadSigning::Sha256, Scheme::Https),
            Payload::Bytes(_)
        ));
    }

    #[test]
    fn precomputed_hash_wins() {
        let body = Body::Bytes(Bytes::from_static(b"x"));
        assert!(matches!(
            payload_for(&body, Some("abc"), PayloadSigning::Auto, Scheme::Https),
            Payload::Precomputed("abc")
        ));
    }

    #[test]
    fn md5_matches_known_value() {
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(md5_of(b"")),
            "1B2M2Y8AsgTpgAmY7PhCfg=="
        );
    }

    #[test]
    fn retryable_statuses() {
        for s in [408, 429, 500, 502, 503, 504] {
            assert!(is_retryable_status(s), "{s} 应该重试");
        }
        for s in [400, 403, 404, 409, 412] {
            assert!(!is_retryable_status(s), "{s} 不该重试");
        }
    }
}
