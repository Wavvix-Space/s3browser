//! 打真实 S3 兼容服务的集成测试。默认跳过，需要显式打开：
//!
//! ```text
//! set S3B_TEST_ENDPOINT=http://127.0.0.1:9000
//! set S3B_TEST_ACCESS_KEY=...
//! set S3B_TEST_SECRET_KEY=...
//! cargo test -p s3api-reqwest -- --ignored
//! ```
//!
//! 本地起服务见 `docs/testing.md`。

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_executor::block_on;
use futures_util::StreamExt;
use s3api::client::md5_of;
use s3api::{
    Addressing, Body, ByteRange, Credentials, Endpoint, S3Client, S3Error, StaticCredentials,
};
use s3api_reqwest::{ReqwestTransport, TransportOptions};

struct Harness {
    client: S3Client,
    bucket: String,
    /// runtime 必须活到测试结束，否则 spawn 出去的请求会被丢掉。
    _runtime: tokio::runtime::Runtime,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// 没配置环境变量就跳过，不让缺环境变成失败。
fn harness() -> Option<Harness> {
    let endpoint = env("S3B_TEST_ENDPOINT")?;
    let access_key = env("S3B_TEST_ACCESS_KEY")?;
    let secret_key = env("S3B_TEST_SECRET_KEY")?;
    let bucket = env("S3B_TEST_BUCKET").unwrap_or_else(|| "s3browser-test".to_owned());
    let region = env("S3B_TEST_REGION").unwrap_or_else(|| "us-east-1".to_owned());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("建 tokio runtime");

    let options = TransportOptions {
        insecure_tls: env("S3B_TEST_INSECURE_TLS").is_some(),
        ..Default::default()
    };
    let transport =
        ReqwestTransport::new(runtime.handle().clone(), options).expect("建 transport");
    let endpoint = Endpoint::parse(&endpoint, &region)
        .expect("解析 endpoint")
        .with_addressing(match env("S3B_TEST_ADDRESSING").as_deref() {
            Some("virtual_host") => Addressing::VirtualHost,
            Some("auto") => Addressing::Auto,
            // 本地服务基本都是 path style
            _ => Addressing::Path,
        });

    let client = S3Client::new(
        Arc::new(transport),
        Arc::new(StaticCredentials(Credentials::new(access_key, secret_key))),
        endpoint,
    );

    // 桶不存在就建一个，已存在的错误忽略。
    if block_on(client.head_bucket(&bucket)).is_err() {
        let _ = block_on(client.create_bucket(&bucket));
    }

    Some(Harness {
        client,
        bucket,
        _runtime: runtime,
    })
}

macro_rules! harness_or_skip {
    () => {
        match harness() {
            Some(h) => h,
            None => {
                eprintln!("跳过：未设置 S3B_TEST_ENDPOINT 等环境变量");
                return;
            }
        }
    };
}

fn read_all(
    mut body: futures_util::stream::BoxStream<'static, std::io::Result<Bytes>>,
) -> Vec<u8> {
    block_on(async move {
        let mut out = Vec::new();
        while let Some(chunk) = body.next().await {
            out.extend_from_slice(&chunk.expect("读响应体"));
        }
        out
    })
}

/// 每个测试用独立前缀，互不干扰。
fn prefix(name: &str) -> String {
    format!("it/{name}/")
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn put_head_get_delete_round_trip() {
    let h = harness_or_skip!();
    let key = format!("{}hello.txt", prefix("round-trip"));
    let data = b"hello s3browser".to_vec();

    let etag = block_on(h.client.put_object(
        &h.bucket,
        &key,
        Body::from(data.clone()),
        Some(md5_of(&data)),
        Some("text/plain"),
    ))
    .expect("上传");
    assert!(!etag.is_empty());

    let head = block_on(h.client.head_object(&h.bucket, &key)).expect("head");
    assert_eq!(head.size, data.len() as u64);

    let got = block_on(h.client.get_object(&h.bucket, &key, None, None)).expect("下载");
    assert_eq!(read_all(got.body), data);

    block_on(h.client.delete_object(&h.bucket, &key)).expect("删除");
    assert!(block_on(h.client.head_object(&h.bucket, &key)).is_err());
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn get_range_returns_requested_slice() {
    let h = harness_or_skip!();
    let key = format!("{}range.bin", prefix("range"));
    let data: Vec<u8> = (0..=255u8).collect();

    block_on(h.client.put_object(
        &h.bucket,
        &key,
        Body::from(data.clone()),
        Some(md5_of(&data)),
        None,
    ))
    .expect("上传");

    let got = block_on(h.client.get_object(
        &h.bucket,
        &key,
        Some(ByteRange::new(10, 19)),
        None,
    ))
    .expect("按范围下载");
    assert_eq!(read_all(got.body), data[10..20]);

    block_on(h.client.delete_object(&h.bucket, &key)).ok();
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn if_match_rejects_changed_object() {
    let h = harness_or_skip!();
    let key = format!("{}ifmatch.txt", prefix("if-match"));

    block_on(h.client.put_object(&h.bucket, &key, Body::from(b"v1".to_vec()), None, None))
        .expect("上传 v1");
    let head = block_on(h.client.head_object(&h.bucket, &key)).expect("head");
    let etag = head.etag.expect("etag");

    // 同一个 etag 能取到
    block_on(h.client.get_object(&h.bucket, &key, None, Some(&etag))).expect("etag 未变");

    // 改过之后旧 etag 应该被拒
    block_on(h.client.put_object(&h.bucket, &key, Body::from(b"v2 changed".to_vec()), None, None))
        .expect("上传 v2");
    match block_on(h.client.get_object(&h.bucket, &key, None, Some(&etag))) {
        Err(S3Error::Service { status, .. }) => assert_eq!(status, 412),
        other => panic!("期望 412，得到 {other:?}"),
    }

    block_on(h.client.delete_object(&h.bucket, &key)).ok();
}

/// 这个测试盯着传输层：流式体必须带 Content-Length 走普通请求，
/// 一旦退化成 chunked 编码，S3 会直接拒绝。
#[test]
#[ignore = "需要真实 S3 服务"]
fn streaming_body_uploads_with_content_length() {
    let h = harness_or_skip!();
    let key = format!("{}streamed.bin", prefix("stream"));
    let chunks: Vec<Bytes> = (0..4).map(|i| Bytes::from(vec![i as u8; 1024])).collect();
    let total: u64 = chunks.iter().map(|c| c.len() as u64).sum();
    let expected: Vec<u8> = chunks.iter().flat_map(|c| c.to_vec()).collect();

    let stream = futures_util::stream::iter(chunks.into_iter().map(Ok));
    let body = Body::Stream {
        stream: Box::pin(stream),
        len: total,
    };

    block_on(h.client.put_object(&h.bucket, &key, body, None, None)).expect("流式上传");

    let head = block_on(h.client.head_object(&h.bucket, &key)).expect("head");
    assert_eq!(head.size, total);
    let got = block_on(h.client.get_object(&h.bucket, &key, None, None)).expect("下载");
    assert_eq!(read_all(got.body), expected);

    block_on(h.client.delete_object(&h.bucket, &key)).ok();
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn multipart_upload_and_resume_listing() {
    let h = harness_or_skip!();
    let key = format!("{}big.bin", prefix("multipart"));
    // 非最后一个分片必须至少 5 MiB
    let part1 = vec![1u8; 5 * 1024 * 1024];
    let part2 = vec![2u8; 1024];

    let upload_id = block_on(h.client.create_multipart_upload(&h.bucket, &key, None))
        .expect("创建分片上传");

    let etag1 = block_on(h.client.upload_part(
        &h.bucket,
        &key,
        &upload_id,
        1,
        Body::from(part1.clone()),
        Some(md5_of(&part1)),
    ))
    .expect("上传分片 1");

    // 传一半时列一次，这就是续传依赖的信息
    let listed = block_on(h.client.list_parts(&h.bucket, &key, &upload_id)).expect("列分片");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].number, 1);
    assert_eq!(listed[0].size, part1.len() as u64);

    let etag2 = block_on(h.client.upload_part(
        &h.bucket,
        &key,
        &upload_id,
        2,
        Body::from(part2.clone()),
        Some(md5_of(&part2)),
    ))
    .expect("上传分片 2");

    block_on(h.client.complete_multipart_upload(
        &h.bucket,
        &key,
        &upload_id,
        &[(2, etag2), (1, etag1)],
    ))
    .expect("完成");

    let head = block_on(h.client.head_object(&h.bucket, &key)).expect("head");
    assert_eq!(head.size, (part1.len() + part2.len()) as u64);

    block_on(h.client.delete_object(&h.bucket, &key)).ok();
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn abort_multipart_discards_upload() {
    let h = harness_or_skip!();
    let key = format!("{}aborted.bin", prefix("abort"));

    let upload_id = block_on(h.client.create_multipart_upload(&h.bucket, &key, None))
        .expect("创建分片上传");
    block_on(h.client.abort_multipart_upload(&h.bucket, &key, &upload_id)).expect("取消");

    assert!(block_on(h.client.head_object(&h.bucket, &key)).is_err());
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn list_objects_separates_folders_from_files() {
    let h = harness_or_skip!();
    let base = prefix("listing");
    let keys = vec![
        format!("{base}a.txt"),
        format!("{base}b.txt"),
        format!("{base}sub/c.txt"),
    ];
    for key in &keys {
        block_on(h.client.put_object(&h.bucket, key, Body::from(b"x".to_vec()), None, None))
            .expect("上传");
    }

    let page = block_on(h.client.list_objects_v2(&h.bucket, &base, Some("/"), None, 1000))
        .expect("列举");

    let names: Vec<&str> = page.objects.iter().map(|o| o.key.as_str()).collect();
    assert!(names.contains(&format!("{base}a.txt").as_str()), "{names:?}");
    assert!(names.contains(&format!("{base}b.txt").as_str()), "{names:?}");
    // 子目录以 CommonPrefixes 出现，不混在对象里
    assert!(!names.contains(&format!("{base}sub/c.txt").as_str()), "{names:?}");
    assert!(
        page.prefixes.iter().any(|p| p == &format!("{base}sub/")),
        "{:?}",
        page.prefixes
    );

    let errors = block_on(h.client.delete_objects(&h.bucket, &keys)).expect("批量删除");
    assert!(errors.is_empty(), "{errors:?}");
    let after = block_on(h.client.list_objects_v2(&h.bucket, &base, Some("/"), None, 1000))
        .expect("再列举");
    assert!(after.objects.is_empty(), "{:?}", after.objects);
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn list_objects_pages_through_results() {
    let h = harness_or_skip!();
    let base = prefix("paging");
    let keys: Vec<String> = (0..5).map(|i| format!("{base}{i}.txt")).collect();
    for key in &keys {
        block_on(h.client.put_object(&h.bucket, key, Body::from(b"x".to_vec()), None, None))
            .expect("上传");
    }

    let mut seen = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let page = block_on(h.client.list_objects_v2(
            &h.bucket,
            &base,
            Some("/"),
            token.as_deref(),
            2,
        ))
        .expect("列举一页");
        seen.extend(page.objects.iter().map(|o| o.key.clone()));
        match page.next_token {
            Some(t) => token = Some(t),
            None => break,
        }
    }

    assert_eq!(seen.len(), 5, "{seen:?}");
    block_on(h.client.delete_objects(&h.bucket, &keys)).expect("清理");
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn copy_object_duplicates_content() {
    let h = harness_or_skip!();
    let base = prefix("copy");
    let src = format!("{base}src.txt");
    let dst = format!("{base}dst 带空格.txt");
    let data = b"copy me".to_vec();

    block_on(h.client.put_object(&h.bucket, &src, Body::from(data.clone()), None, None))
        .expect("上传源");
    block_on(h.client.copy_object(&h.bucket, &dst, &h.bucket, &src)).expect("复制");

    let got = block_on(h.client.get_object(&h.bucket, &dst, None, None)).expect("下载副本");
    assert_eq!(read_all(got.body), data);

    block_on(h.client.delete_objects(&h.bucket, &[src, dst])).expect("清理");
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn create_folder_shows_up_as_prefix() {
    let h = harness_or_skip!();
    let base = prefix("folder");
    let folder = format!("{base}新建文件夹");

    block_on(h.client.create_folder(&h.bucket, &folder)).expect("新建文件夹");

    let page = block_on(h.client.list_objects_v2(&h.bucket, &base, Some("/"), None, 1000))
        .expect("列举");
    assert!(
        page.prefixes.iter().any(|p| p == &format!("{folder}/")),
        "{:?}",
        page.prefixes
    );

    block_on(h.client.delete_object(&h.bucket, &format!("{folder}/"))).ok();
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn presigned_url_downloads_without_credentials() {
    let h = harness_or_skip!();
    let key = format!("{}shared 文件.txt", prefix("presign"));
    let data = b"presigned content".to_vec();

    block_on(h.client.put_object(&h.bucket, &key, Body::from(data.clone()), None, None))
        .expect("上传");

    let url = block_on(h.client.presign_get(&h.bucket, &key, Duration::from_secs(600)))
        .expect("预签名");

    // 用一个全新的、完全不带凭据的客户端去取
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let body = runtime.block_on(async {
        let resp = reqwest::get(&url).await.expect("请求预签名 URL");
        assert_eq!(resp.status().as_u16(), 200, "url={url}");
        resp.bytes().await.expect("读响应体")
    });
    assert_eq!(body.to_vec(), data);

    block_on(h.client.delete_object(&h.bucket, &key)).ok();
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn missing_object_reports_not_found() {
    let h = harness_or_skip!();

    match block_on(h.client.get_object(&h.bucket, "it/definitely/missing.txt", None, None)) {
        Err(S3Error::Service { status, .. }) => assert_eq!(status, 404),
        other => panic!("期望 404，得到 {other:?}"),
    }
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn wrong_credentials_are_rejected() {
    let h = harness_or_skip!();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let transport =
        ReqwestTransport::new(runtime.handle().clone(), TransportOptions::default()).unwrap();
    let bad = S3Client::new(
        Arc::new(transport),
        Arc::new(StaticCredentials(Credentials::new("wrong", "alsowrong"))),
        h.client.endpoint().clone(),
    );

    match block_on(bad.list_objects_v2(&h.bucket, "", Some("/"), None, 10)) {
        Err(S3Error::Service { status, .. }) => {
            assert!(status == 403 || status == 401, "得到 {status}")
        }
        other => panic!("错误的凭据不该被接受，得到 {other:?}"),
    }
}
