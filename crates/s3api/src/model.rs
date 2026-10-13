//! 操作的返回类型。时间统一用 Unix 秒，避免上层再依赖 chrono。

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketInfo {
    pub name: String,
    pub created: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    pub modified: i64,
    pub etag: Option<String>,
    pub storage_class: Option<String>,
    pub content_type: Option<String>,
}

/// ListObjectsV2 的一页。带 delimiter 时 `prefixes` 是子目录。
#[derive(Debug, Clone, Default)]
pub struct ListPage {
    pub objects: Vec<ObjectMeta>,
    pub prefixes: Vec<String>,
    pub next_token: Option<String>,
}

impl ListPage {
    pub fn is_truncated(&self) -> bool {
        self.next_token.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartInfo {
    pub number: u32,
    pub size: u64,
    pub etag: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadInfo {
    pub key: String,
    pub upload_id: String,
    pub initiated: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ListUploadsPage {
    pub uploads: Vec<UploadInfo>,
    pub next_key_marker: Option<String>,
    pub next_upload_id_marker: Option<String>,
}

/// DeleteObjects 里单个 key 的失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteError {
    pub key: String,
    pub code: String,
    pub message: String,
}

/// 上传体的校验方式。PutObject 与 UploadPart 都带 Content-MD5，由服务端校验。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PayloadSigning {
    /// HTTPS 下上传体填 UNSIGNED-PAYLOAD，HTTP 下算 SHA256。
    #[default]
    Auto,
    /// 始终算 SHA256。某些服务端不接受 UNSIGNED-PAYLOAD 时用。
    Sha256,
}

/// GetObject 的字节范围，闭区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn new(start: u64, end: u64) -> Self {
        ByteRange { start, end }
    }

    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start) + 1
    }

    pub fn is_empty(&self) -> bool {
        self.end < self.start
    }

    pub fn header_value(&self) -> String {
        format!("bytes={}-{}", self.start, self.end)
    }
}

/// HeadObject 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadResult {
    pub size: u64,
    pub modified: i64,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub storage_class: Option<String>,
}

/// GetObject 的结果，`body` 未读取。
pub struct GetResult {
    pub size: u64,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub body: futures_util::stream::BoxStream<'static, std::io::Result<bytes::Bytes>>,
}

impl std::fmt::Debug for GetResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GetResult")
            .field("size", &self.size)
            .field("etag", &self.etag)
            .finish_non_exhaustive()
    }
}

/// 对象的访问权限，也就是 canned ACL。
///
/// `Default` 是阿里云 OSS 的说法——对象跟着桶的权限走，AWS 没有对应值。
/// 列在这里是因为 OSS 上绝大多数对象就是这个状态，不认识它就只能显示成
/// 「私有」，而那是错的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acl {
    Default,
    Private,
    PublicRead,
    PublicReadWrite,
}

impl Acl {
    /// 放进 `x-amz-acl` 请求头的值。
    pub fn header_value(self) -> &'static str {
        match self {
            Acl::Default => "default",
            Acl::Private => "private",
            Acl::PublicRead => "public-read",
            Acl::PublicReadWrite => "public-read-write",
        }
    }

    pub fn parse(s: &str) -> Option<Acl> {
        match s.trim() {
            "default" => Some(Acl::Default),
            "private" => Some(Acl::Private),
            "public-read" => Some(Acl::PublicRead),
            "public-read-write" => Some(Acl::PublicReadWrite),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acl_round_trips_through_its_header_value() {
        for acl in [
            Acl::Default,
            Acl::Private,
            Acl::PublicRead,
            Acl::PublicReadWrite,
        ] {
            assert_eq!(Acl::parse(acl.header_value()), Some(acl));
        }
        assert_eq!(Acl::parse("authenticated-read"), None);
    }

    #[test]
    fn byte_range_length_is_inclusive() {
        assert_eq!(ByteRange::new(0, 9).len(), 10);
        assert_eq!(ByteRange::new(0, 0).len(), 1);
        assert_eq!(ByteRange::new(0, 9).header_value(), "bytes=0-9");
    }
}
