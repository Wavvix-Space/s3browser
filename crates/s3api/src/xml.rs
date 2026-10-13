//! S3 响应的 XML 模型，以及两种请求体的生成。

use serde::Deserialize;

use crate::model::{
    Acl, BucketInfo, DeleteError, ListPage, ListUploadsPage, ObjectMeta, PartInfo, UploadInfo,
};
use crate::types::S3Error;

fn decode<T: for<'de> Deserialize<'de>>(xml: &str) -> Result<T, S3Error> {
    quick_xml::de::from_str(xml).map_err(|e| S3Error::Decode(e.to_string()))
}

/// ETag 在 XML 里带双引号，存的时候去掉。
fn strip_quotes(s: &str) -> String {
    s.trim_matches('"').to_owned()
}

/// 解析 `2013-05-24T00:00:00.000Z`，失败按 0 处理而不是让整个列表失败。
fn parse_time(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.timestamp())
        .unwrap_or(0)
}

// ---------- ListBuckets ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListAllMyBucketsResult {
    #[serde(default)]
    buckets: Buckets,
}

#[derive(Debug, Default, Deserialize)]
struct Buckets {
    #[serde(default, rename = "Bucket")]
    bucket: Vec<XmlBucket>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct XmlBucket {
    name: String,
    #[serde(default)]
    creation_date: String,
}

pub fn parse_list_buckets(xml: &str) -> Result<Vec<BucketInfo>, S3Error> {
    let parsed: ListAllMyBucketsResult = decode(xml)?;
    Ok(parsed
        .buckets
        .bucket
        .into_iter()
        .map(|b| BucketInfo {
            created: parse_time(&b.creation_date),
            name: b.name,
        })
        .collect())
}

// ---------- ListObjectsV2 ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListBucketResult {
    #[serde(default)]
    is_truncated: bool,
    #[serde(default)]
    next_continuation_token: Option<String>,
    #[serde(default)]
    contents: Vec<XmlObject>,
    #[serde(default)]
    common_prefixes: Vec<XmlPrefix>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct XmlObject {
    key: String,
    #[serde(default)]
    last_modified: String,
    #[serde(default)]
    e_tag: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    storage_class: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct XmlPrefix {
    prefix: String,
}

pub fn parse_list_objects(xml: &str) -> Result<ListPage, S3Error> {
    let parsed: ListBucketResult = decode(xml)?;
    // 有的服务端 IsTruncated 为 true 却不给 token，此时按结束处理，避免死循环。
    let next_token = if parsed.is_truncated {
        parsed.next_continuation_token.filter(|t| !t.is_empty())
    } else {
        None
    };
    Ok(ListPage {
        objects: parsed
            .contents
            .into_iter()
            .map(|o| ObjectMeta {
                key: o.key,
                size: o.size,
                modified: parse_time(&o.last_modified),
                etag: (!o.e_tag.is_empty()).then(|| strip_quotes(&o.e_tag)),
                storage_class: o.storage_class,
                content_type: None,
            })
            .collect(),
        prefixes: parsed.common_prefixes.into_iter().map(|p| p.prefix).collect(),
        next_token,
    })
}

// ---------- Multipart ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InitiateMultipartUploadResult {
    upload_id: String,
}

pub fn parse_initiate_multipart(xml: &str) -> Result<String, S3Error> {
    let parsed: InitiateMultipartUploadResult = decode(xml)?;
    Ok(parsed.upload_id)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CompleteMultipartUploadResult {
    #[serde(default)]
    e_tag: String,
}

pub fn parse_complete_multipart(xml: &str) -> Result<String, S3Error> {
    let parsed: CompleteMultipartUploadResult = decode(xml)?;
    Ok(strip_quotes(&parsed.e_tag))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListPartsResult {
    #[serde(default)]
    is_truncated: bool,
    #[serde(default)]
    next_part_number_marker: Option<String>,
    #[serde(default, rename = "Part")]
    parts: Vec<XmlPart>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct XmlPart {
    part_number: u32,
    #[serde(default)]
    e_tag: String,
    #[serde(default)]
    size: u64,
}

/// 返回这一页的分片和下一页的 marker。
pub fn parse_list_parts(xml: &str) -> Result<(Vec<PartInfo>, Option<String>), S3Error> {
    let parsed: ListPartsResult = decode(xml)?;
    let next = if parsed.is_truncated {
        parsed.next_part_number_marker.filter(|m| !m.is_empty())
    } else {
        None
    };
    let parts = parsed
        .parts
        .into_iter()
        .map(|p| PartInfo {
            number: p.part_number,
            size: p.size,
            etag: strip_quotes(&p.e_tag),
        })
        .collect();
    Ok((parts, next))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListMultipartUploadsResult {
    #[serde(default)]
    is_truncated: bool,
    #[serde(default)]
    next_key_marker: Option<String>,
    #[serde(default)]
    next_upload_id_marker: Option<String>,
    #[serde(default, rename = "Upload")]
    uploads: Vec<XmlUpload>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct XmlUpload {
    key: String,
    upload_id: String,
    #[serde(default)]
    initiated: String,
}

pub fn parse_list_uploads(xml: &str) -> Result<ListUploadsPage, S3Error> {
    let parsed: ListMultipartUploadsResult = decode(xml)?;
    let (next_key, next_id) = if parsed.is_truncated {
        (
            parsed.next_key_marker.filter(|m| !m.is_empty()),
            parsed.next_upload_id_marker.filter(|m| !m.is_empty()),
        )
    } else {
        (None, None)
    };
    Ok(ListUploadsPage {
        uploads: parsed
            .uploads
            .into_iter()
            .map(|u| UploadInfo {
                key: u.key,
                upload_id: u.upload_id,
                initiated: parse_time(&u.initiated),
            })
            .collect(),
        next_key_marker: next_key,
        next_upload_id_marker: next_id,
    })
}

// ---------- Copy ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CopyResult {
    #[serde(default)]
    e_tag: String,
}

/// CopyObjectResult 与 CopyPartResult 结构相同。
pub fn parse_copy_result(xml: &str) -> Result<String, S3Error> {
    let parsed: CopyResult = decode(xml)?;
    Ok(strip_quotes(&parsed.e_tag))
}

// ---------- DeleteObjects ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DeleteResult {
    #[serde(default, rename = "Error")]
    errors: Vec<XmlDeleteError>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct XmlDeleteError {
    #[serde(default)]
    key: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

pub fn parse_delete_result(xml: &str) -> Result<Vec<DeleteError>, S3Error> {
    let parsed: DeleteResult = decode(xml)?;
    Ok(parsed
        .errors
        .into_iter()
        .map(|e| DeleteError {
            key: e.key,
            code: e.code,
            message: e.message,
        })
        .collect())
}

// ---------- Error ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct XmlError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    request_id: Option<String>,
}

/// 解析错误响应体。解析不出来时用 HTTP 状态码兜底，不让错误本身变成解析错误。
/// GetObjectAcl 的响应。两家给的形状不一样，所以字段都是可选的。
///
/// AWS 给一串 Grant，每个带 Grantee 和 Permission；阿里云 OSS 只给
/// `<Grant>public-read</Grant>` 一行字。`$text` 让同一个结构体两边都能接住。
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AclPolicy {
    access_control_list: Option<AclList>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AclList {
    #[serde(default)]
    grant: Vec<AclGrant>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AclGrant {
    #[serde(rename = "$text", default)]
    text: Option<String>,
    grantee: Option<AclGrantee>,
    permission: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AclGrantee {
    #[serde(rename = "URI", default)]
    uri: Option<String>,
}

/// 所有人这个组的固定 URI。授权给它就等于公开。
const ALL_USERS: &str = "/groups/global/AllUsers";

pub fn parse_object_acl(xml: &str) -> Result<Acl, S3Error> {
    let policy: AclPolicy = decode(xml)?;
    let grants = policy
        .access_control_list
        .map(|list| list.grant)
        .unwrap_or_default();

    // 阿里云那种一行说完的，认出来就直接返回
    for grant in &grants {
        if let Some(acl) = grant.text.as_deref().and_then(Acl::parse) {
            return Ok(acl);
        }
    }

    // AWS 那种要看有没有授给"所有人"，以及授了什么
    let (mut read, mut write) = (false, false);
    for grant in &grants {
        let public = grant
            .grantee
            .as_ref()
            .and_then(|g| g.uri.as_deref())
            .is_some_and(|uri| uri.ends_with(ALL_USERS));
        if !public {
            continue;
        }
        match grant.permission.as_deref() {
            Some("READ") => read = true,
            Some("WRITE") => write = true,
            // FULL_CONTROL 给了所有人，那就是什么都能干
            Some("FULL_CONTROL") => (read, write) = (true, true),
            _ => {}
        }
    }
    Ok(match (read, write) {
        (true, true) => Acl::PublicReadWrite,
        (true, false) => Acl::PublicRead,
        // 没有公开的授权就是私有。这里不会是 Default——
        // 服务端要是这个意思，上面那段文本形式已经接住了
        _ => Acl::Private,
    })
}

pub fn parse_error(status: u16, body: &str, request_id: Option<&str>) -> S3Error {
    match quick_xml::de::from_str::<XmlError>(body) {
        Ok(e) if !e.code.is_empty() => S3Error::Service {
            status,
            code: e.code,
            message: e.message,
            request_id: e.request_id.or_else(|| request_id.map(str::to_owned)),
        },
        _ => S3Error::Service {
            status,
            code: String::new(),
            message: body.chars().take(500).collect(),
            request_id: request_id.map(str::to_owned),
        },
    }
}

// ---------- 请求体 ----------

/// CompleteMultipartUpload 的请求体。分片必须按编号升序。
pub fn complete_multipart_body(parts: &[(u32, String)]) -> String {
    let mut out = String::from("<CompleteMultipartUpload>");
    for (number, etag) in parts {
        out.push_str("<Part><PartNumber>");
        out.push_str(&number.to_string());
        out.push_str("</PartNumber><ETag>&quot;");
        out.push_str(&quick_xml::escape::escape(strip_quotes(etag).as_str()));
        out.push_str("&quot;</ETag></Part>");
    }
    out.push_str("</CompleteMultipartUpload>");
    out
}

/// DeleteObjects 的请求体。Quiet 模式下只回失败项。
pub fn delete_objects_body(keys: &[String]) -> String {
    let mut out = String::from("<Delete><Quiet>true</Quiet>");
    for key in keys {
        out.push_str("<Object><Key>");
        out.push_str(&quick_xml::escape::escape(key.as_str()));
        out.push_str("</Key></Object>");
    }
    out.push_str("</Delete>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 阿里云 OSS 一行说完，Grant 里直接是权限名。
    #[test]
    fn parses_the_aliyun_shape_of_object_acl() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<AccessControlPolicy>
  <Owner><ID>1234</ID><DisplayName>1234</DisplayName></Owner>
  <AccessControlList><Grant>public-read</Grant></AccessControlList>
</AccessControlPolicy>"#;
        assert_eq!(parse_object_acl(xml).unwrap(), Acl::PublicRead);
    }

    /// OSS 上绝大多数对象是 default（跟着桶走）。认不出来就只能显示成
    /// 「私有」，而那是错的——桶要是公开的，这个对象也是公开的。
    #[test]
    fn default_is_not_the_same_as_private() {
        let xml = r#"<AccessControlPolicy><Owner><ID>1</ID></Owner>
  <AccessControlList><Grant>default</Grant></AccessControlList></AccessControlPolicy>"#;
        assert_eq!(parse_object_acl(xml).unwrap(), Acl::Default);
    }

    /// AWS 那套：看有没有授给"所有人"这个组。
    #[test]
    fn parses_the_aws_shape_of_object_acl() {
        let public = r#"<?xml version="1.0" encoding="UTF-8"?>
<AccessControlPolicy xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Owner><ID>abc</ID></Owner>
  <AccessControlList>
    <Grant>
      <Grantee xsi:type="CanonicalUser"><ID>abc</ID></Grantee>
      <Permission>FULL_CONTROL</Permission>
    </Grant>
    <Grant>
      <Grantee xsi:type="Group"><URI>http://acs.amazonaws.com/groups/global/AllUsers</URI></Grantee>
      <Permission>READ</Permission>
    </Grant>
  </AccessControlList>
</AccessControlPolicy>"#;
        assert_eq!(parse_object_acl(public).unwrap(), Acl::PublicRead);
    }

    /// 只授给自己不算公开。给 owner 的 FULL_CONTROL 不能被当成公开读写。
    #[test]
    fn a_grant_to_the_owner_alone_is_private() {
        let xml = r#"<AccessControlPolicy><Owner><ID>abc</ID></Owner>
  <AccessControlList>
    <Grant>
      <Grantee xsi:type="CanonicalUser"><ID>abc</ID></Grantee>
      <Permission>FULL_CONTROL</Permission>
    </Grant>
  </AccessControlList></AccessControlPolicy>"#;
        assert_eq!(parse_object_acl(xml).unwrap(), Acl::Private);
    }

    #[test]
    fn full_control_to_everyone_is_public_read_write() {
        let xml = r#"<AccessControlPolicy><Owner><ID>abc</ID></Owner>
  <AccessControlList>
    <Grant>
      <Grantee xsi:type="Group"><URI>http://acs.amazonaws.com/groups/global/AllUsers</URI></Grantee>
      <Permission>FULL_CONTROL</Permission>
    </Grant>
  </AccessControlList></AccessControlPolicy>"#;
        assert_eq!(parse_object_acl(xml).unwrap(), Acl::PublicReadWrite);
    }

    #[test]
    fn parses_list_buckets() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListAllMyBucketsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Owner><ID>abc</ID><DisplayName>me</DisplayName></Owner>
  <Buckets>
    <Bucket><Name>photos</Name><CreationDate>2013-05-24T00:00:00.000Z</CreationDate></Bucket>
    <Bucket><Name>backups</Name><CreationDate>2020-01-02T03:04:05.000Z</CreationDate></Bucket>
  </Buckets>
</ListAllMyBucketsResult>"#;
        let buckets = parse_list_buckets(xml).unwrap();
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].name, "photos");
        assert_eq!(buckets[0].created, 1369353600);
        assert_eq!(buckets[1].name, "backups");
    }

    #[test]
    fn parses_empty_bucket_list() {
        let xml = r#"<ListAllMyBucketsResult><Owner><ID>x</ID></Owner><Buckets/></ListAllMyBucketsResult>"#;
        assert!(parse_list_buckets(xml).unwrap().is_empty());
    }

    #[test]
    fn parses_list_objects_with_prefixes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>examplebucket</Name>
  <Prefix>photos/</Prefix>
  <Delimiter>/</Delimiter>
  <KeyCount>2</KeyCount>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>photos/a.jpg</Key>
    <LastModified>2013-05-24T00:00:00.000Z</LastModified>
    <ETag>&quot;d41d8cd98f00b204e9800998ecf8427e&quot;</ETag>
    <Size>1024</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <CommonPrefixes><Prefix>photos/2026/</Prefix></CommonPrefixes>
  <CommonPrefixes><Prefix>photos/2025/</Prefix></CommonPrefixes>
</ListBucketResult>"#;
        let page = parse_list_objects(xml).unwrap();
        assert_eq!(page.objects.len(), 1);
        assert_eq!(page.objects[0].key, "photos/a.jpg");
        assert_eq!(page.objects[0].size, 1024);
        assert_eq!(
            page.objects[0].etag.as_deref(),
            Some("d41d8cd98f00b204e9800998ecf8427e")
        );
        assert_eq!(page.prefixes, vec!["photos/2026/", "photos/2025/"]);
        assert!(!page.is_truncated());
    }

    #[test]
    fn truncated_list_carries_token() {
        let xml = r#"<ListBucketResult><Name>b</Name><IsTruncated>true</IsTruncated>
<NextContinuationToken>1ueGcxLPRx1Tr</NextContinuationToken>
<Contents><Key>a</Key><Size>1</Size><LastModified>2013-05-24T00:00:00.000Z</LastModified></Contents>
</ListBucketResult>"#;
        let page = parse_list_objects(xml).unwrap();
        assert_eq!(page.next_token.as_deref(), Some("1ueGcxLPRx1Tr"));
    }

    #[test]
    fn truncated_without_token_stops_paging() {
        let xml = r#"<ListBucketResult><Name>b</Name><IsTruncated>true</IsTruncated></ListBucketResult>"#;
        assert!(!parse_list_objects(xml).unwrap().is_truncated());
    }

    #[test]
    fn parses_multipart_results() {
        let init = r#"<InitiateMultipartUploadResult><Bucket>b</Bucket><Key>k</Key>
<UploadId>VXBsb2FkIElEIGZvciA2aWWpbmc</UploadId></InitiateMultipartUploadResult>"#;
        assert_eq!(
            parse_initiate_multipart(init).unwrap(),
            "VXBsb2FkIElEIGZvciA2aWWpbmc"
        );

        let done = r#"<CompleteMultipartUploadResult><Location>http://x</Location><Bucket>b</Bucket>
<Key>k</Key><ETag>&quot;3858f62230ac3c915f300c664312c11f-9&quot;</ETag></CompleteMultipartUploadResult>"#;
        assert_eq!(
            parse_complete_multipart(done).unwrap(),
            "3858f62230ac3c915f300c664312c11f-9"
        );
    }

    #[test]
    fn parses_list_parts() {
        let xml = r#"<ListPartsResult><Bucket>b</Bucket><Key>k</Key><UploadId>u</UploadId>
<IsTruncated>false</IsTruncated>
<Part><PartNumber>1</PartNumber><ETag>&quot;aaa&quot;</ETag><Size>8388608</Size></Part>
<Part><PartNumber>2</PartNumber><ETag>&quot;bbb&quot;</ETag><Size>1024</Size></Part>
</ListPartsResult>"#;
        let (parts, next) = parse_list_parts(xml).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].number, 1);
        assert_eq!(parts[0].size, 8388608);
        assert_eq!(parts[0].etag, "aaa");
        assert!(next.is_none());
    }

    #[test]
    fn parses_list_uploads() {
        let xml = r#"<ListMultipartUploadsResult><Bucket>b</Bucket><IsTruncated>false</IsTruncated>
<Upload><Key>big.bin</Key><UploadId>u1</UploadId><Initiated>2026-09-20T01:02:03.000Z</Initiated></Upload>
</ListMultipartUploadsResult>"#;
        let page = parse_list_uploads(xml).unwrap();
        assert_eq!(page.uploads.len(), 1);
        assert_eq!(page.uploads[0].upload_id, "u1");
    }

    #[test]
    fn parses_copy_result() {
        let xml = r#"<CopyObjectResult><LastModified>2013-05-24T00:00:00.000Z</LastModified>
<ETag>&quot;9b2cf535f27731c974343645a3985328&quot;</ETag></CopyObjectResult>"#;
        assert_eq!(
            parse_copy_result(xml).unwrap(),
            "9b2cf535f27731c974343645a3985328"
        );
    }

    #[test]
    fn parses_delete_errors_only() {
        let xml = r#"<DeleteResult><Deleted><Key>ok.txt</Key></Deleted>
<Error><Key>locked.txt</Key><Code>AccessDenied</Code><Message>Access Denied</Message></Error>
</DeleteResult>"#;
        let errors = parse_delete_result(xml).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].key, "locked.txt");
        assert_eq!(errors[0].code, "AccessDenied");
    }

    #[test]
    fn parses_service_error() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message>
<Key>a.txt</Key><RequestId>4442587FB7D0A2F9</RequestId></Error>"#;
        match parse_error(404, xml, None) {
            S3Error::Service {
                status,
                code,
                request_id,
                ..
            } => {
                assert_eq!(status, 404);
                assert_eq!(code, "NoSuchKey");
                assert_eq!(request_id.as_deref(), Some("4442587FB7D0A2F9"));
            }
            other => panic!("期望 Service 错误，得到 {other:?}"),
        }
    }

    #[test]
    fn falls_back_when_error_body_is_not_xml() {
        match parse_error(503, "<html>gateway</html>", Some("req-1")) {
            S3Error::Service {
                status,
                code,
                request_id,
                ..
            } => {
                assert_eq!(status, 503);
                assert!(code.is_empty());
                assert_eq!(request_id.as_deref(), Some("req-1"));
            }
            other => panic!("期望 Service 错误，得到 {other:?}"),
        }
    }

    #[test]
    fn builds_complete_multipart_body() {
        let body = complete_multipart_body(&[(1, "\"aaa\"".into()), (2, "bbb".into())]);
        assert_eq!(
            body,
            "<CompleteMultipartUpload>\
<Part><PartNumber>1</PartNumber><ETag>&quot;aaa&quot;</ETag></Part>\
<Part><PartNumber>2</PartNumber><ETag>&quot;bbb&quot;</ETag></Part>\
</CompleteMultipartUpload>"
        );
    }

    #[test]
    fn escapes_keys_in_delete_body() {
        let body = delete_objects_body(&["a&b.txt".into(), "c<d>.txt".into()]);
        assert!(body.contains("<Key>a&amp;b.txt</Key>"));
        assert!(body.contains("<Key>c&lt;d&gt;.txt</Key>"));
        assert!(body.starts_with("<Delete><Quiet>true</Quiet>"));
    }
}
