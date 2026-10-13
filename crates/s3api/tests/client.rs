//! 用假 transport 回放响应，验证客户端构造的请求和对响应的处理。

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_executor::block_on;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use http::HeaderMap;
use s3api::{
    Addressing, Body, Credentials, Endpoint, HttpRequest, HttpResponse, HttpTransport, S3Client,
    S3Error, StaticCredentials,
};

// ---------- 假 transport ----------

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Reply {
    fn ok(body: &str) -> Self {
        Reply {
            status: 200,
            headers: Vec::new(),
            body: body.to_owned(),
        }
    }

    fn status(status: u16, body: &str) -> Self {
        Reply {
            status,
            headers: Vec::new(),
            body: body.to_owned(),
        }
    }

    fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }
}

struct Recorded {
    method: String,
    url: String,
    headers: HeaderMap,
    body: Option<Bytes>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    fn authorization(&self) -> &str {
        self.header("authorization").unwrap_or_default()
    }
}

#[derive(Default)]
struct MockTransport {
    replies: Mutex<VecDeque<Reply>>,
    sent: Mutex<Vec<Recorded>>,
    slept: Mutex<Vec<Duration>>,
}

impl MockTransport {
    fn with(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(MockTransport {
            replies: Mutex::new(replies.into()),
            ..Default::default()
        })
    }

    fn sent(&self) -> std::sync::MutexGuard<'_, Vec<Recorded>> {
        self.sent.lock().unwrap()
    }

    fn call_count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }
}

fn stream_of(data: Bytes) -> BoxStream<'static, io::Result<Bytes>> {
    Box::pin(futures_util::stream::once(async move { Ok(data) }))
}

impl HttpTransport for MockTransport {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, s3api::TransportError>> {
        let body = match req.body {
            Body::Bytes(b) => Some(b),
            _ => None,
        };
        self.sent.lock().unwrap().push(Recorded {
            method: req.method.to_string(),
            url: req.url,
            headers: req.headers,
            body,
        });

        let reply = self.replies.lock().unwrap().pop_front();
        Box::pin(async move {
            let reply = reply.expect("假 transport 的响应用完了");
            let mut headers = HeaderMap::new();
            for (name, value) in reply.headers {
                headers.insert(name, value.parse().unwrap());
            }
            Ok(HttpResponse {
                status: reply.status,
                headers,
                body: stream_of(Bytes::from(reply.body)),
            })
        })
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        self.slept.lock().unwrap().push(duration);
        Box::pin(async {})
    }
}

fn client(transport: Arc<MockTransport>) -> S3Client {
    let creds = Credentials::new("AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
    S3Client::new(
        transport,
        Arc::new(StaticCredentials(creds)),
        Endpoint::parse("https://s3.example.com", "us-east-1").unwrap(),
    )
}

/// path-style 的本地服务，对应 MinIO 那种部署。
fn path_style_client(transport: Arc<MockTransport>) -> S3Client {
    let creds = Credentials::new("minioadmin", "minioadmin");
    S3Client::new(
        transport,
        Arc::new(StaticCredentials(creds)),
        Endpoint::parse("http://127.0.0.1:9000", "us-east-1")
            .unwrap()
            .with_addressing(Addressing::Path),
    )
}

// ---------- 测试 ----------

#[test]
fn list_buckets_parses_response() {
    let xml = r#"<ListAllMyBucketsResult><Owner><ID>x</ID></Owner><Buckets>
<Bucket><Name>photos</Name><CreationDate>2013-05-24T00:00:00.000Z</CreationDate></Bucket>
</Buckets></ListAllMyBucketsResult>"#;
    let transport = MockTransport::with(vec![Reply::ok(xml)]);
    let client = client(transport.clone());

    let buckets = block_on(client.list_buckets()).unwrap();

    assert_eq!(buckets.len(), 1);
    assert_eq!(buckets[0].name, "photos");
    let sent = transport.sent();
    assert_eq!(sent[0].method, "GET");
    assert_eq!(sent[0].url, "https://s3.example.com/");
    assert!(sent[0].authorization().starts_with("AWS4-HMAC-SHA256 Credential="));
}

#[test]
fn list_objects_builds_expected_query() {
    let xml = r#"<ListBucketResult><Name>b</Name><IsTruncated>false</IsTruncated></ListBucketResult>"#;
    let transport = MockTransport::with(vec![Reply::ok(xml)]);
    let client = client(transport.clone());

    block_on(client.list_objects_v2("photos", "a b/", Some("/"), Some("tok/en"), 1000)).unwrap();

    let sent = transport.sent();
    let url = &sent[0].url;
    // virtual-host 寻址，查询参数按键排序且严格编码
    assert!(url.starts_with("https://photos.s3.example.com/?"), "{url}");
    assert!(url.contains("list-type=2"), "{url}");
    assert!(url.contains("delimiter=%2F"), "{url}");
    assert!(url.contains("prefix=a%20b%2F"), "{url}");
    assert!(url.contains("continuation-token=tok%2Fen"), "{url}");
    let query = url.split_once('?').unwrap().1;
    let keys: Vec<&str> = query
        .split('&')
        .map(|p| p.split_once('=').unwrap().0)
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "查询参数必须按键排序");
}

#[test]
fn path_style_puts_bucket_in_path() {
    let xml = r#"<ListBucketResult><Name>b</Name><IsTruncated>false</IsTruncated></ListBucketResult>"#;
    let transport = MockTransport::with(vec![Reply::ok(xml)]);
    let client = path_style_client(transport.clone());

    block_on(client.list_objects_v2("photos", "", Some("/"), None, 1000)).unwrap();

    let sent = transport.sent();
    assert!(
        sent[0].url.starts_with("http://127.0.0.1:9000/photos?"),
        "{}",
        sent[0].url
    );
    assert_eq!(sent[0].header("host").unwrap(), "127.0.0.1:9000");
}

#[test]
fn head_object_reads_metadata_headers() {
    let transport = MockTransport::with(vec![
        Reply::ok("")
            .header("content-length", "1234")
            .header("etag", "\"d41d8cd98f00b204e9800998ecf8427e\"")
            .header("last-modified", "Fri, 24 May 2013 00:00:00 GMT")
            .header("content-type", "image/jpeg"),
    ]);
    let client = client(transport.clone());

    let head = block_on(client.head_object("photos", "a.jpg")).unwrap();

    assert_eq!(head.size, 1234);
    assert_eq!(head.modified, 1369353600);
    assert_eq!(head.etag.as_deref(), Some("d41d8cd98f00b204e9800998ecf8427e"));
    assert_eq!(head.content_type.as_deref(), Some("image/jpeg"));
    assert_eq!(transport.sent()[0].method, "HEAD");
}

#[test]
fn head_on_missing_object_reports_not_found_without_body() {
    let transport = MockTransport::with(vec![Reply::status(404, "")]);
    let client = client(transport);

    match block_on(client.head_object("photos", "missing.jpg")) {
        Err(S3Error::Service { status, code, .. }) => {
            assert_eq!(status, 404);
            assert_eq!(code, "NotFound");
        }
        other => panic!("期望 404，得到 {other:?}"),
    }
}

#[test]
fn get_object_sends_range_and_if_match() {
    let transport = MockTransport::with(vec![
        Reply::status(206, "0123456789").header("content-length", "10"),
    ]);
    let client = client(transport.clone());

    let result = block_on(client.get_object(
        "photos",
        "a.jpg",
        Some(s3api::ByteRange::new(0, 9)),
        Some("etag-1"),
    ))
    .unwrap();

    assert_eq!(result.size, 10);
    let sent = transport.sent();
    assert_eq!(sent[0].header("range").unwrap(), "bytes=0-9");
    assert_eq!(sent[0].header("if-match").unwrap(), "etag-1");
    // range 与 if-match 必须参与签名
    assert!(sent[0].authorization().contains("if-match;range"), "{}", sent[0].authorization());
}

#[test]
fn put_object_sends_content_md5() {
    let transport = MockTransport::with(vec![Reply::ok("").header("etag", "\"abc\"")]);
    let client = client(transport.clone());
    let data = b"hello".to_vec();
    let md5 = s3api::client::md5_of(&data);

    let etag = block_on(client.put_object("photos", "a.txt", Body::from(data), Some(md5), None))
        .unwrap();

    assert_eq!(etag, "abc");
    let sent = transport.sent();
    assert_eq!(sent[0].header("content-md5").unwrap(), "XUFAKrxLKna5cZ2REBfFkg==");
    assert!(sent[0].authorization().contains("content-md5"));
}

#[test]
fn create_folder_puts_empty_object_with_trailing_slash() {
    let transport = MockTransport::with(vec![Reply::ok("")]);
    let client = client(transport.clone());

    block_on(client.create_folder("photos", "2026")).unwrap();

    let sent = transport.sent();
    assert_eq!(sent[0].url, "https://photos.s3.example.com/2026/");
    assert_eq!(sent[0].body.as_ref().map(|b| b.len()).unwrap_or(0), 0);
}

#[test]
fn delete_objects_sends_quiet_body_with_md5() {
    let transport = MockTransport::with(vec![Reply::ok("<DeleteResult/>")]);
    let client = client(transport.clone());

    let errors = block_on(client.delete_objects("photos", &["a.txt".into(), "b&c.txt".into()]))
        .unwrap();

    assert!(errors.is_empty());
    let sent = transport.sent();
    assert_eq!(sent[0].method, "POST");
    assert!(sent[0].url.ends_with("/?delete="), "{}", sent[0].url);
    assert!(sent[0].header("content-md5").is_some());
    let body = String::from_utf8(sent[0].body.clone().unwrap().to_vec()).unwrap();
    assert!(body.contains("<Quiet>true</Quiet>"));
    assert!(body.contains("<Key>b&amp;c.txt</Key>"));
}

#[test]
fn delete_objects_reports_per_key_failures() {
    let xml = r#"<DeleteResult><Error><Key>locked</Key><Code>AccessDenied</Code>
<Message>no</Message></Error></DeleteResult>"#;
    let transport = MockTransport::with(vec![Reply::ok(xml)]);
    let client = client(transport);

    let errors = block_on(client.delete_objects("photos", &["locked".into()])).unwrap();

    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, "AccessDenied");
}

#[test]
fn copy_object_encodes_source() {
    let xml = r#"<CopyObjectResult><ETag>&quot;abc&quot;</ETag></CopyObjectResult>"#;
    let transport = MockTransport::with(vec![Reply::ok(xml)]);
    let client = client(transport.clone());

    let etag = block_on(client.copy_object("dst", "new name.txt", "src", "old name.txt")).unwrap();

    assert_eq!(etag, "abc");
    let sent = transport.sent();
    assert_eq!(
        sent[0].header("x-amz-copy-source").unwrap(),
        "/src/old%20name.txt"
    );
    assert!(sent[0].authorization().contains("x-amz-copy-source"));
}

#[test]
fn multipart_upload_round_trip() {
    let init = r#"<InitiateMultipartUploadResult><UploadId>up-1</UploadId></InitiateMultipartUploadResult>"#;
    let done = r#"<CompleteMultipartUploadResult><ETag>&quot;final-9&quot;</ETag></CompleteMultipartUploadResult>"#;
    let transport = MockTransport::with(vec![
        Reply::ok(init),
        Reply::ok("").header("etag", "\"part1\""),
        Reply::ok(done),
    ]);
    let client = client(transport.clone());

    let upload_id = block_on(client.create_multipart_upload("photos", "big.bin", None)).unwrap();
    let part = block_on(client.upload_part(
        "photos",
        "big.bin",
        &upload_id,
        1,
        Body::from(vec![0u8; 16]),
        None,
    ))
    .unwrap();
    let etag = block_on(client.complete_multipart_upload(
        "photos",
        "big.bin",
        &upload_id,
        &[(2, "part2".into()), (1, part.clone())],
    ))
    .unwrap();

    assert_eq!(upload_id, "up-1");
    assert_eq!(part, "part1");
    assert_eq!(etag, "final-9");

    let sent = transport.sent();
    assert!(sent[0].url.ends_with("/big.bin?uploads="), "{}", sent[0].url);
    assert!(sent[1].url.contains("partNumber=1"), "{}", sent[1].url);
    assert!(sent[1].url.contains("uploadId=up-1"), "{}", sent[1].url);
    // 分片必须按编号升序提交
    let body = String::from_utf8(sent[2].body.clone().unwrap().to_vec()).unwrap();
    assert!(
        body.find("<PartNumber>1<").unwrap() < body.find("<PartNumber>2<").unwrap(),
        "{body}"
    );
}

#[test]
fn complete_with_error_body_and_200_status_is_a_failure() {
    let body = r#"<Error><Code>InternalError</Code><Message>retry</Message></Error>"#;
    let transport = MockTransport::with(vec![Reply::ok(body)]);
    let client = client(transport);

    match block_on(client.complete_multipart_upload("b", "k", "u", &[(1, "e".into())])) {
        Err(S3Error::Service { code, .. }) => assert_eq!(code, "InternalError"),
        other => panic!("期望失败，得到 {other:?}"),
    }
}

#[test]
fn list_parts_follows_pagination() {
    let page1 = r#"<ListPartsResult><IsTruncated>true</IsTruncated>
<NextPartNumberMarker>1</NextPartNumberMarker>
<Part><PartNumber>1</PartNumber><ETag>&quot;a&quot;</ETag><Size>8</Size></Part>
</ListPartsResult>"#;
    let page2 = r#"<ListPartsResult><IsTruncated>false</IsTruncated>
<Part><PartNumber>2</PartNumber><ETag>&quot;b&quot;</ETag><Size>4</Size></Part>
</ListPartsResult>"#;
    let transport = MockTransport::with(vec![Reply::ok(page1), Reply::ok(page2)]);
    let client = client(transport.clone());

    let parts = block_on(client.list_parts("b", "k", "u")).unwrap();

    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].number, 1);
    assert_eq!(parts[1].number, 2);
    assert!(
        transport.sent()[1].url.contains("part-number-marker=1"),
        "{}",
        transport.sent()[1].url
    );
}

#[test]
fn abort_multipart_sends_delete() {
    let transport = MockTransport::with(vec![Reply::status(204, "")]);
    let client = client(transport.clone());

    block_on(client.abort_multipart_upload("b", "k", "u-1")).unwrap();

    let sent = transport.sent();
    assert_eq!(sent[0].method, "DELETE");
    assert!(sent[0].url.contains("uploadId=u-1"));
}

#[test]
fn retries_server_errors_then_succeeds() {
    let xml = r#"<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>"#;
    let transport = MockTransport::with(vec![
        Reply::status(503, "<Error><Code>SlowDown</Code></Error>"),
        Reply::status(500, "<Error><Code>InternalError</Code></Error>"),
        Reply::ok(xml),
    ]);
    let client = client(transport.clone());

    block_on(client.list_buckets()).unwrap();

    assert_eq!(transport.call_count(), 3);
    let slept = transport.slept.lock().unwrap();
    assert_eq!(slept.len(), 2);
    // 退避递增
    assert!(slept[1] > slept[0], "{slept:?}");
}

#[test]
fn gives_up_after_max_attempts() {
    let replies = (0..5)
        .map(|_| Reply::status(503, "<Error><Code>SlowDown</Code><Message>slow</Message></Error>"))
        .collect();
    let transport = MockTransport::with(replies);
    let client = client(transport.clone());

    match block_on(client.list_buckets()) {
        Err(S3Error::Service { status, code, .. }) => {
            assert_eq!(status, 503);
            assert_eq!(code, "SlowDown");
        }
        other => panic!("期望 503，得到 {other:?}"),
    }
    assert_eq!(transport.call_count(), 5);
}

#[test]
fn does_not_retry_client_errors() {
    let transport = MockTransport::with(vec![Reply::status(
        403,
        "<Error><Code>AccessDenied</Code><Message>no</Message></Error>",
    )]);
    let client = client(transport.clone());

    assert!(block_on(client.list_buckets()).is_err());
    assert_eq!(transport.call_count(), 1);
}

#[test]
fn does_not_retry_streaming_bodies() {
    let transport = MockTransport::with(vec![Reply::status(503, "<Error><Code>SlowDown</Code></Error>")]);
    let client = client(transport.clone());
    let body = Body::Stream {
        stream: stream_of(Bytes::from_static(b"data")),
        len: 4,
    };

    assert!(block_on(client.put_object("b", "k", body, None, None)).is_err());
    // 流读过就没了，客户端不能替调用方重试
    assert_eq!(transport.call_count(), 1);
}

#[test]
fn follows_region_redirect_once() {
    let xml = r#"<ListBucketResult><Name>b</Name><IsTruncated>false</IsTruncated></ListBucketResult>"#;
    let transport = MockTransport::with(vec![
        Reply::status(301, "").header("x-amz-bucket-region", "eu-west-1"),
        Reply::ok(xml),
    ]);
    let client = client(transport.clone());

    block_on(client.list_objects_v2("photos", "", Some("/"), None, 1000)).unwrap();

    assert_eq!(transport.call_count(), 2);
    let sent = transport.sent();
    assert!(sent[0].authorization().contains("/us-east-1/s3/"), "{}", sent[0].authorization());
    assert!(sent[1].authorization().contains("/eu-west-1/s3/"), "{}", sent[1].authorization());
    // 修正后的 region 记住了，后续请求不用再跳一次
    assert_eq!(client.region(), "eu-west-1");
}

#[test]
fn presigned_url_has_signature_and_expiry() {
    let transport = MockTransport::with(vec![]);
    let client = client(transport);

    let url = block_on(client.presign_get("photos", "a b.jpg", Duration::from_secs(3600))).unwrap();

    assert!(url.starts_with("https://photos.s3.example.com/a%20b.jpg?"), "{url}");
    assert!(url.contains("X-Amz-Expires=3600"), "{url}");
    assert!(url.contains("X-Amz-SignedHeaders=host"), "{url}");
    assert!(url.contains("&X-Amz-Signature="), "{url}");
}

#[test]
fn session_token_reaches_the_wire() {
    let xml = r#"<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>"#;
    let transport = MockTransport::with(vec![Reply::ok(xml)]);
    let creds = Credentials::new("ak", "sk").with_token("sts-token");
    let client = S3Client::new(
        transport.clone(),
        Arc::new(StaticCredentials(creds)),
        Endpoint::parse("https://s3.oss-cn-shenzhen.aliyuncs.com", "cn-shenzhen").unwrap(),
    );

    block_on(client.list_buckets()).unwrap();

    let sent = transport.sent();
    assert_eq!(sent[0].header("x-amz-security-token").unwrap(), "sts-token");
    assert!(sent[0].authorization().contains("x-amz-security-token"));
}
