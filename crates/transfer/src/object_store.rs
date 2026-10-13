//! 引擎访问对象存储的边界。
//!
//! 引擎只认这个 trait，不认 `S3Client`，所以测试里能注入超时、412、NoSuchUpload
//! 这些真实网络很难复现的情况。

use std::io;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use s3api::{Body, ByteRange, DeleteError, HeadResult, PartInfo, S3Client, S3Error};

pub type StoreResult<T> = Result<T, S3Error>;

pub trait ObjectStore: Send + Sync {
    fn head(&self, bucket: &str, key: &str) -> BoxFuture<'_, StoreResult<HeadResult>>;

    /// `if_match` 传上次拿到的 ETag，对象在续传期间被换掉时服务端回 412。
    fn get_range(
        &self,
        bucket: &str,
        key: &str,
        range: Option<ByteRange>,
        if_match: Option<&str>,
    ) -> BoxFuture<'_, StoreResult<BoxStream<'static, io::Result<Bytes>>>>;

    fn put(
        &self,
        bucket: &str,
        key: &str,
        body: Body,
        md5: Option<[u8; 16]>,
    ) -> BoxFuture<'_, StoreResult<String>>;

    fn create_multipart(&self, bucket: &str, key: &str) -> BoxFuture<'_, StoreResult<String>>;

    fn upload_part(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: u32,
        body: Body,
        md5: Option<[u8; 16]>,
    ) -> BoxFuture<'_, StoreResult<String>>;

    fn upload_part_copy(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: u32,
        src_bucket: &str,
        src_key: &str,
        range: Option<ByteRange>,
    ) -> BoxFuture<'_, StoreResult<String>>;

    fn list_parts(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> BoxFuture<'_, StoreResult<Vec<PartInfo>>>;

    fn complete_multipart(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        parts: Vec<(u32, String)>,
    ) -> BoxFuture<'_, StoreResult<String>>;

    fn abort_multipart(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> BoxFuture<'_, StoreResult<()>>;

    fn copy(
        &self,
        bucket: &str,
        key: &str,
        src_bucket: &str,
        src_key: &str,
    ) -> BoxFuture<'_, StoreResult<String>>;

    /// 递归列出前缀下所有 key。删除文件夹时先用它展开。
    fn list_keys(&self, bucket: &str, prefix: &str) -> BoxFuture<'_, StoreResult<Vec<String>>>;

    /// 一次调用最多 1000 个 key，分批由调用方负责。
    fn delete_many(
        &self,
        bucket: &str,
        keys: Vec<String>,
    ) -> BoxFuture<'_, StoreResult<Vec<DeleteError>>>;
}

/// 按账户找到对应的存储。账户配置变了就换一个新的实现进来。
pub trait StoreResolver: Send + Sync {
    fn store_for(&self, profile_id: &str) -> Option<Arc<dyn ObjectStore>>;
}

/// 把 `S3Client` 接到 `ObjectStore` 上。
pub struct S3ObjectStore {
    client: Arc<S3Client>,
}

impl S3ObjectStore {
    pub fn new(client: Arc<S3Client>) -> Self {
        S3ObjectStore { client }
    }
}

impl ObjectStore for S3ObjectStore {
    fn head(&self, bucket: &str, key: &str) -> BoxFuture<'_, StoreResult<HeadResult>> {
        let (bucket, key) = (bucket.to_owned(), key.to_owned());
        Box::pin(async move { self.client.head_object(&bucket, &key).await })
    }

    fn get_range(
        &self,
        bucket: &str,
        key: &str,
        range: Option<ByteRange>,
        if_match: Option<&str>,
    ) -> BoxFuture<'_, StoreResult<BoxStream<'static, io::Result<Bytes>>>> {
        let (bucket, key) = (bucket.to_owned(), key.to_owned());
        let if_match = if_match.map(str::to_owned);
        Box::pin(async move {
            let result = self
                .client
                .get_object(&bucket, &key, range, if_match.as_deref())
                .await?;
            Ok(result.body)
        })
    }

    fn put(
        &self,
        bucket: &str,
        key: &str,
        body: Body,
        md5: Option<[u8; 16]>,
    ) -> BoxFuture<'_, StoreResult<String>> {
        let (bucket, key) = (bucket.to_owned(), key.to_owned());
        Box::pin(async move { self.client.put_object(&bucket, &key, body, md5, None).await })
    }

    fn create_multipart(&self, bucket: &str, key: &str) -> BoxFuture<'_, StoreResult<String>> {
        let (bucket, key) = (bucket.to_owned(), key.to_owned());
        Box::pin(async move { self.client.create_multipart_upload(&bucket, &key, None).await })
    }

    fn upload_part(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: u32,
        body: Body,
        md5: Option<[u8; 16]>,
    ) -> BoxFuture<'_, StoreResult<String>> {
        let (bucket, key, upload_id) = (bucket.to_owned(), key.to_owned(), upload_id.to_owned());
        Box::pin(async move {
            self.client
                .upload_part(&bucket, &key, &upload_id, part_number, body, md5)
                .await
        })
    }

    fn upload_part_copy(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: u32,
        src_bucket: &str,
        src_key: &str,
        range: Option<ByteRange>,
    ) -> BoxFuture<'_, StoreResult<String>> {
        let (bucket, key, upload_id) = (bucket.to_owned(), key.to_owned(), upload_id.to_owned());
        let (src_bucket, src_key) = (src_bucket.to_owned(), src_key.to_owned());
        Box::pin(async move {
            self.client
                .upload_part_copy(
                    &bucket,
                    &key,
                    &upload_id,
                    part_number,
                    &src_bucket,
                    &src_key,
                    range,
                )
                .await
        })
    }

    fn list_parts(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> BoxFuture<'_, StoreResult<Vec<PartInfo>>> {
        let (bucket, key, upload_id) = (bucket.to_owned(), key.to_owned(), upload_id.to_owned());
        Box::pin(async move { self.client.list_parts(&bucket, &key, &upload_id).await })
    }

    fn complete_multipart(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        parts: Vec<(u32, String)>,
    ) -> BoxFuture<'_, StoreResult<String>> {
        let (bucket, key, upload_id) = (bucket.to_owned(), key.to_owned(), upload_id.to_owned());
        Box::pin(async move {
            self.client
                .complete_multipart_upload(&bucket, &key, &upload_id, &parts)
                .await
        })
    }

    fn abort_multipart(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> BoxFuture<'_, StoreResult<()>> {
        let (bucket, key, upload_id) = (bucket.to_owned(), key.to_owned(), upload_id.to_owned());
        Box::pin(async move {
            self.client
                .abort_multipart_upload(&bucket, &key, &upload_id)
                .await
        })
    }

    fn copy(
        &self,
        bucket: &str,
        key: &str,
        src_bucket: &str,
        src_key: &str,
    ) -> BoxFuture<'_, StoreResult<String>> {
        let (bucket, key) = (bucket.to_owned(), key.to_owned());
        let (src_bucket, src_key) = (src_bucket.to_owned(), src_key.to_owned());
        Box::pin(async move {
            self.client
                .copy_object(&bucket, &key, &src_bucket, &src_key)
                .await
        })
    }

    fn list_keys(&self, bucket: &str, prefix: &str) -> BoxFuture<'_, StoreResult<Vec<String>>> {
        let (bucket, prefix) = (bucket.to_owned(), prefix.to_owned());
        Box::pin(async move {
            let mut keys = Vec::new();
            let mut token: Option<String> = None;
            loop {
                let page = self
                    .client
                    .list_objects_v2(&bucket, &prefix, None, token.as_deref(), 1000)
                    .await?;
                keys.extend(page.objects.into_iter().map(|o| o.key));
                match page.next_token {
                    Some(next) => token = Some(next),
                    None => break,
                }
            }
            Ok(keys)
        })
    }

    fn delete_many(
        &self,
        bucket: &str,
        keys: Vec<String>,
    ) -> BoxFuture<'_, StoreResult<Vec<DeleteError>>> {
        let bucket = bucket.to_owned();
        Box::pin(async move { self.client.delete_objects(&bucket, &keys).await })
    }
}

/// 服务端说这个分片上传已经不存在了，只能从头来。
pub fn is_no_such_upload(error: &S3Error) -> bool {
    matches!(
        error,
        S3Error::Service { code, .. } if code == "NoSuchUpload"
    )
}

/// 对象在续传期间被改过。重试只能从零开始，不能接着传。
pub fn is_precondition_failed(error: &S3Error) -> bool {
    matches!(error, S3Error::Service { status: 412, .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(status: u16, code: &str) -> S3Error {
        S3Error::Service {
            status,
            code: code.to_owned(),
            message: String::new(),
            request_id: None,
        }
    }

    #[test]
    fn recognises_restart_conditions() {
        assert!(is_no_such_upload(&service(404, "NoSuchUpload")));
        assert!(!is_no_such_upload(&service(404, "NoSuchKey")));

        assert!(is_precondition_failed(&service(412, "PreconditionFailed")));
        assert!(!is_precondition_failed(&service(404, "NoSuchKey")));
    }
}
