//! 端点模型、凭据与错误类型。

use std::net::IpAddr;

use crate::transport::{CredentialsError, TransportError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }
}

/// 桶名放在主机名里还是路径里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Addressing {
    /// 桶名符合 DNS 规则且 endpoint 主机不是 IP 或 localhost 时用 virtual-host，否则用 path。
    #[default]
    Auto,
    VirtualHost,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub scheme: Scheme,
    pub host: String,
    pub port: Option<u16>,
    pub region: String,
    pub addressing: Addressing,
}

impl Endpoint {
    /// 解析 `https://s3.example.com:9000` 形式的 endpoint。缺 scheme 时按 https 处理。
    pub fn parse(endpoint: &str, region: &str) -> Result<Self, EndpointError> {
        let raw = endpoint.trim();
        if raw.is_empty() {
            return Err(EndpointError::Empty);
        }
        let with_scheme = if raw.contains("://") {
            raw.to_owned()
        } else {
            format!("https://{raw}")
        };
        let url =
            url::Url::parse(&with_scheme).map_err(|e| EndpointError::Invalid(e.to_string()))?;
        let scheme = match url.scheme() {
            "https" => Scheme::Https,
            "http" => Scheme::Http,
            other => return Err(EndpointError::UnsupportedScheme(other.to_owned())),
        };
        let host = url
            .host_str()
            .ok_or_else(|| EndpointError::Invalid("缺少主机名".into()))?
            .to_owned();
        Ok(Endpoint {
            scheme,
            host,
            port: url.port(),
            region: region.trim().to_owned(),
            addressing: Addressing::Auto,
        })
    }

    pub fn with_addressing(mut self, addressing: Addressing) -> Self {
        self.addressing = addressing;
        self
    }

    /// 主机名带端口，用于 Host 头与 URL。默认端口省略。
    fn authority(&self, host: &str) -> String {
        match self.port {
            Some(p) if p != self.scheme.default_port() => format!("{host}:{p}"),
            _ => host.to_owned(),
        }
    }

    pub fn uses_virtual_host(&self, bucket: &str) -> bool {
        match self.addressing {
            Addressing::VirtualHost => true,
            Addressing::Path => false,
            Addressing::Auto => {
                is_dns_compatible_bucket(bucket)
                    && self.host.parse::<IpAddr>().is_err()
                    && self.host != "localhost"
            }
        }
    }

    /// 请求的 Host 头。bucket 为 None 时是服务级操作，例如 ListBuckets。
    pub fn host_header(&self, bucket: Option<&str>) -> String {
        match bucket {
            Some(b) if self.uses_virtual_host(b) => self.authority(&format!("{b}.{}", self.host)),
            _ => self.authority(&self.host),
        }
    }

    /// 请求路径，已按 S3 规则编码，总是以 / 开头。
    pub fn path(&self, bucket: Option<&str>, key: &str) -> String {
        let encoded_key = crate::encode::encode_path(key);
        match bucket {
            None => "/".to_owned(),
            Some(b) if self.uses_virtual_host(b) => format!("/{encoded_key}"),
            Some(b) => {
                if encoded_key.is_empty() {
                    format!("/{b}")
                } else {
                    format!("/{b}/{encoded_key}")
                }
            }
        }
    }

    pub fn url(&self, bucket: Option<&str>, key: &str, canonical_query: &str) -> String {
        let base = format!(
            "{}://{}{}",
            self.scheme.as_str(),
            self.host_header(bucket),
            self.path(bucket, key)
        );
        if canonical_query.is_empty() {
            base
        } else {
            format!("{base}?{canonical_query}")
        }
    }
}

/// DNS 规则：3 到 63 个字符，只含小写字母、数字和连字符，首尾是字母或数字。
/// 含点的桶名在 HTTPS virtual-host 下证书不匹配，按不兼容处理。
fn is_dns_compatible_bucket(bucket: &str) -> bool {
    let len = bucket.len();
    if !(3..=63).contains(&len) {
        return false;
    }
    if bucket.contains('.') {
        return false;
    }
    let bytes = bucket.as_bytes();
    let edge_ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !edge_ok(bytes[0]) || !edge_ok(bytes[len - 1]) {
        return false;
    }
    bytes
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Clone)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

impl Credentials {
    pub fn new(access_key: impl Into<String>, secret_key: impl Into<String>) -> Self {
        Credentials {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            session_token: None,
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.session_token = Some(token.into());
        self
    }
}

/// 不打印密钥。
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("endpoint 为空")]
    Empty,
    #[error("endpoint 无效：{0}")]
    Invalid(String),
    #[error("不支持的协议：{0}")]
    UnsupportedScheme(String),
}

#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("服务端错误 {status} {code}：{message}")]
    Service {
        status: u16,
        code: String,
        message: String,
        request_id: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Credentials(#[from] CredentialsError),
    #[error("解析响应失败：{0}")]
    Decode(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(host: &str) -> Endpoint {
        Endpoint::parse(host, "us-east-1").unwrap()
    }

    #[test]
    fn parses_scheme_host_and_port() {
        let e = ep("https://s3.amazonaws.com");
        assert_eq!(e.scheme, Scheme::Https);
        assert_eq!(e.host, "s3.amazonaws.com");
        assert_eq!(e.port, None);

        let e = ep("http://127.0.0.1:9000");
        assert_eq!(e.scheme, Scheme::Http);
        assert_eq!(e.port, Some(9000));

        // 缺 scheme 时按 https 补全
        assert_eq!(ep("oss-cn-shenzhen.aliyuncs.com").scheme, Scheme::Https);
    }

    #[test]
    fn auto_addressing_picks_virtual_host_only_when_safe() {
        assert!(ep("https://s3.amazonaws.com").uses_virtual_host("examplebucket"));
        // IP 或 localhost 的 endpoint 用 path style
        assert!(!ep("http://127.0.0.1:9000").uses_virtual_host("examplebucket"));
        assert!(!ep("http://localhost:9000").uses_virtual_host("examplebucket"));
        // 含点的桶名在 HTTPS 下证书不匹配
        assert!(!ep("https://s3.amazonaws.com").uses_virtual_host("my.bucket"));
        // 大写和过短的桶名不符合 DNS 规则
        assert!(!ep("https://s3.amazonaws.com").uses_virtual_host("MyBucket"));
        assert!(!ep("https://s3.amazonaws.com").uses_virtual_host("ab"));
    }

    #[test]
    fn explicit_addressing_overrides_auto() {
        let e = ep("https://s3.amazonaws.com").with_addressing(Addressing::Path);
        assert!(!e.uses_virtual_host("examplebucket"));
        assert_eq!(e.host_header(Some("examplebucket")), "s3.amazonaws.com");
        assert_eq!(
            e.path(Some("examplebucket"), "a/b.txt"),
            "/examplebucket/a/b.txt"
        );

        let e = ep("http://127.0.0.1:9000").with_addressing(Addressing::VirtualHost);
        assert_eq!(e.host_header(Some("buck")), "buck.127.0.0.1:9000");
    }

    #[test]
    fn builds_host_and_path() {
        let e = ep("https://s3.amazonaws.com");
        assert_eq!(
            e.host_header(Some("examplebucket")),
            "examplebucket.s3.amazonaws.com"
        );
        assert_eq!(e.path(Some("examplebucket"), "test.txt"), "/test.txt");
        assert_eq!(
            e.path(Some("examplebucket"), "test$file.text"),
            "/test%24file.text"
        );
        assert_eq!(e.path(None, ""), "/");

        let e = ep("http://127.0.0.1:9000");
        assert_eq!(e.host_header(Some("buck")), "127.0.0.1:9000");
        assert_eq!(e.path(Some("buck"), ""), "/buck");
        assert_eq!(
            e.url(Some("buck"), "a b.txt", "x=1"),
            "http://127.0.0.1:9000/buck/a%20b.txt?x=1"
        );
    }

    #[test]
    fn default_port_is_omitted_from_authority() {
        let e = Endpoint::parse("https://s3.amazonaws.com:443", "us-east-1").unwrap();
        assert_eq!(e.host_header(None), "s3.amazonaws.com");
    }
}
