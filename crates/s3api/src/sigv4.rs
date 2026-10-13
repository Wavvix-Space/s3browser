//! AWS Signature Version 4：header 签名与查询参数预签名。

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use http::HeaderMap;
use http::header::{HeaderName, HeaderValue};
use sha2::{Digest, Sha256};

use crate::encode::encode_query;
use crate::types::Credentials;

type HmacSha256 = Hmac<Sha256>;

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
/// 空字符串的 SHA256。
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// `X-Amz-Expires` 的上限。
pub const MAX_EXPIRES_SECS: u64 = 604800;

/// 签名时如何处理请求体。
pub enum Payload<'a> {
    Empty,
    Bytes(&'a [u8]),
    /// 已经算好的十六进制 SHA256，流式请求体用这个。
    Precomputed(&'a str),
    /// HTTPS 下跳过负载哈希，配合 Content-MD5 保证完整性。
    Unsigned,
}

impl Payload<'_> {
    pub fn hash(&self) -> String {
        match self {
            Payload::Empty => EMPTY_SHA256.to_owned(),
            Payload::Bytes(b) => hex::encode(Sha256::digest(b)),
            Payload::Precomputed(h) => (*h).to_owned(),
            Payload::Unsigned => UNSIGNED_PAYLOAD.to_owned(),
        }
    }
}

/// 待签名请求的各个部件。`path` 已编码，`query` 是未编码的原始键值对。
pub struct SigningRequest<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a [(String, String)],
}

/// 给 `headers` 补齐签名所需的头并写入 `Authorization`，返回与签名一致的 canonical query string。
/// 调用方必须用这个返回值拼 URL，否则服务端算出的签名会不同。
pub fn sign(
    req: &SigningRequest<'_>,
    headers: &mut HeaderMap,
    payload: &Payload<'_>,
    creds: &Credentials,
    region: &str,
    now: DateTime<Utc>,
) -> String {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let payload_hash = payload.hash();

    set_header(headers, "host", req.host);
    set_header(headers, "x-amz-date", &amz_date);
    set_header(headers, "x-amz-content-sha256", &payload_hash);
    if let Some(token) = &creds.session_token {
        set_header(headers, "x-amz-security-token", token);
    }

    let query = canonical_query(req.query);
    let (canonical_headers, signed_headers) = canonical_headers(headers);
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method, req.path, query, canonical_headers, signed_headers, payload_hash
    );

    let scope = credential_scope(&date, region);
    let string_to_sign = string_to_sign(&amz_date, &scope, &canonical_request);
    let signature = hex::encode(sign_string(&creds.secret_key, &date, region, &string_to_sign));

    let authorization = format!(
        "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key
    );
    set_header(headers, "authorization", &authorization);

    query
}

/// 生成预签名 URL 的查询串。只签 `host` 头，负载固定为 UNSIGNED-PAYLOAD。
pub fn presign(
    req: &SigningRequest<'_>,
    creds: &Credentials,
    region: &str,
    now: DateTime<Utc>,
    expires_secs: u64,
) -> String {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let scope = credential_scope(&date, region);
    let expires = expires_secs.clamp(1, MAX_EXPIRES_SECS);

    let mut headers = HeaderMap::new();
    set_header(&mut headers, "host", req.host);
    let (canonical_headers, signed_headers) = canonical_headers(&headers);

    let mut query: Vec<(String, String)> = req.query.to_vec();
    query.push(("X-Amz-Algorithm".into(), ALGORITHM.into()));
    query.push((
        "X-Amz-Credential".into(),
        format!("{}/{scope}", creds.access_key),
    ));
    query.push(("X-Amz-Date".into(), amz_date.clone()));
    query.push(("X-Amz-Expires".into(), expires.to_string()));
    if let Some(token) = &creds.session_token {
        query.push(("X-Amz-Security-Token".into(), token.clone()));
    }
    query.push(("X-Amz-SignedHeaders".into(), signed_headers.clone()));

    let canonical_query = canonical_query(&query);
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method, req.path, canonical_query, canonical_headers, signed_headers, UNSIGNED_PAYLOAD
    );
    let string_to_sign = string_to_sign(&amz_date, &scope, &canonical_request);
    let signature = hex::encode(sign_string(&creds.secret_key, &date, region, &string_to_sign));

    format!("{canonical_query}&X-Amz-Signature={signature}")
}

fn credential_scope(date: &str, region: &str) -> String {
    format!("{date}/{region}/s3/aws4_request")
}

fn string_to_sign(amz_date: &str, scope: &str, canonical_request: &str) -> String {
    format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    )
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度的密钥");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sign_string(secret_key: &str, date: &str, region: &str, string_to_sign: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    hmac(&k_signing, string_to_sign.as_bytes())
}

/// 按编码后的键排序，键与值都严格编码。无值的参数写成 `key=`。
fn canonical_query(query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (encode_query(k), encode_query(v)))
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// 返回 (canonical headers, signed headers)。
/// `authorization` 不参与签名；`content-length` 与 `user-agent` 由传输层填写，签了会对不上。
fn canonical_headers(headers: &HeaderMap) -> (String, String) {
    let mut entries: Vec<(&str, String)> = Vec::with_capacity(headers.len());
    for name in headers.keys() {
        let n = name.as_str();
        if matches!(n, "authorization" | "content-length" | "user-agent") {
            continue;
        }
        let value = headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(normalize_space)
            .collect::<Vec<_>>()
            .join(",");
        entries.push((n, value));
    }
    entries.sort_by(|a, b| a.0.cmp(b.0));

    let canonical = entries
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();
    let signed = entries
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(";");
    (canonical, signed)
}

/// 去掉首尾空白，内部连续空格压成一个。
fn normalize_space(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut prev_space = false;
    for c in v.trim().chars() {
        if c == ' ' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

fn set_header(headers: &mut HeaderMap, name: &str, value: &str) {
    let name = HeaderName::from_bytes(name.as_bytes()).expect("头名称由内部构造");
    match HeaderValue::from_str(value) {
        Ok(v) => {
            headers.insert(name, v);
        }
        Err(_) => {
            // 非 ASCII 的头值只可能来自用户数据（如 session token），丢弃比构造出坏请求好。
            headers.remove(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // 测试向量来自 AWS《Signature Version 4 测试套件》的 S3 示例。
    const AK: &str = "AKIAIOSFODNN7EXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const HOST: &str = "examplebucket.s3.amazonaws.com";
    const REGION: &str = "us-east-1";

    fn creds() -> Credentials {
        Credentials::new(AK, SK)
    }

    fn when() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2013, 5, 24, 0, 0, 0).unwrap()
    }

    fn signature_of(headers: &HeaderMap) -> String {
        let auth = headers.get("authorization").unwrap().to_str().unwrap();
        auth.rsplit("Signature=").next().unwrap().to_owned()
    }

    #[test]
    fn get_object_with_range() {
        let mut headers = HeaderMap::new();
        set_header(&mut headers, "range", "bytes=0-9");
        let req = SigningRequest {
            method: "GET",
            host: HOST,
            path: "/test.txt",
            query: &[],
        };
        sign(&req, &mut headers, &Payload::Empty, &creds(), REGION, when());

        assert_eq!(
            signature_of(&headers),
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert_eq!(
            headers.get("x-amz-content-sha256").unwrap(),
            EMPTY_SHA256
        );
    }

    #[test]
    fn put_object_with_extra_signed_headers() {
        let body = b"Welcome to Amazon S3.";
        let mut headers = HeaderMap::new();
        set_header(&mut headers, "date", "Fri, 24 May 2013 00:00:00 GMT");
        set_header(&mut headers, "x-amz-storage-class", "REDUCED_REDUNDANCY");
        let req = SigningRequest {
            method: "PUT",
            host: HOST,
            path: "/test%24file.text",
            query: &[],
        };
        sign(
            &req,
            &mut headers,
            &Payload::Bytes(body),
            &creds(),
            REGION,
            when(),
        );

        assert_eq!(
            signature_of(&headers),
            "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
        );
    }

    #[test]
    fn list_objects_with_query() {
        let mut headers = HeaderMap::new();
        // 故意乱序传入，canonical query 必须按键排序
        let query = vec![
            ("prefix".to_owned(), "J".to_owned()),
            ("max-keys".to_owned(), "2".to_owned()),
        ];
        let req = SigningRequest {
            method: "GET",
            host: HOST,
            path: "/",
            query: &query,
        };
        let canonical = sign(&req, &mut headers, &Payload::Empty, &creds(), REGION, when());

        assert_eq!(canonical, "max-keys=2&prefix=J");
        assert_eq!(
            signature_of(&headers),
            "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        );
    }

    #[test]
    fn presigned_get() {
        let req = SigningRequest {
            method: "GET",
            host: HOST,
            path: "/test.txt",
            query: &[],
        };
        let query = presign(&req, &creds(), REGION, when(), 86400);

        assert!(query.ends_with(
            "&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        ));
        assert!(query.contains("X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request"));
        assert!(query.contains("X-Amz-SignedHeaders=host"));
    }

    #[test]
    fn session_token_is_signed() {
        let creds = creds().with_token("token123");
        let mut headers = HeaderMap::new();
        let req = SigningRequest {
            method: "GET",
            host: HOST,
            path: "/test.txt",
            query: &[],
        };
        sign(&req, &mut headers, &Payload::Empty, &creds, REGION, when());

        assert_eq!(headers.get("x-amz-security-token").unwrap(), "token123");
        let auth = headers.get("authorization").unwrap().to_str().unwrap();
        assert!(auth.contains("x-amz-security-token"), "{auth}");
    }

    #[test]
    fn presign_carries_session_token_and_clamps_expiry() {
        let creds = creds().with_token("token123");
        let req = SigningRequest {
            method: "GET",
            host: HOST,
            path: "/test.txt",
            query: &[],
        };
        let query = presign(&req, &creds, REGION, when(), MAX_EXPIRES_SECS * 2);

        assert!(query.contains("X-Amz-Security-Token=token123"));
        assert!(query.contains(&format!("X-Amz-Expires={MAX_EXPIRES_SECS}")));
    }

    #[test]
    fn transport_headers_are_not_signed() {
        let mut headers = HeaderMap::new();
        set_header(&mut headers, "content-length", "21");
        set_header(&mut headers, "user-agent", "S3Browser/0.1");
        let req = SigningRequest {
            method: "GET",
            host: HOST,
            path: "/test.txt",
            query: &[],
        };
        sign(&req, &mut headers, &Payload::Empty, &creds(), REGION, when());

        // 与不带这两个头时的签名一致
        let mut bare = HeaderMap::new();
        sign(&req, &mut bare, &Payload::Empty, &creds(), REGION, when());
        assert_eq!(signature_of(&headers), signature_of(&bare));
    }

    #[test]
    fn header_values_are_space_normalized() {
        assert_eq!(normalize_space("  a   b  "), "a b");
        assert_eq!(normalize_space("Fri, 24 May 2013 00:00:00 GMT"), "Fri, 24 May 2013 00:00:00 GMT");
    }

    #[test]
    fn valueless_query_params_keep_equals_sign() {
        assert_eq!(
            canonical_query(&[("uploads".to_owned(), String::new())]),
            "uploads="
        );
    }
}
