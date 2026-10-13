//! 对象级操作。

use std::time::Duration;

use crate::client::{Request, S3Client, md5_of};
use crate::encode::encode_path;
use crate::model::{ByteRange, DeleteError, GetResult, HeadResult};
use crate::transport::{Body, HttpResponse};
use crate::types::S3Error;
use crate::xml;

/// DeleteObjects 单次请求的 key 数量上限。
pub const MAX_DELETE_BATCH: usize = 1000;

impl S3Client {
    pub async fn head_object(&self, bucket: &str, key: &str) -> Result<HeadResult, S3Error> {
        let response = self.execute(Request::head().bucket(bucket).key(key)).await?;
        Ok(HeadResult {
            size: content_length(&response).unwrap_or(0),
            modified: last_modified(&response),
            etag: etag_of(&response),
            content_type: response.header("content-type").map(str::to_owned),
            storage_class: response.header("x-amz-storage-class").map(str::to_owned),
        })
    }

    /// 下载对象。`if_match` 传上次拿到的 ETag，对象在续传期间被改过时服务端回 412。
    pub async fn get_object(
        &self,
        bucket: &str,
        key: &str,
        range: Option<ByteRange>,
        if_match: Option<&str>,
    ) -> Result<GetResult, S3Error> {
        let req = Request::get()
            .bucket(bucket)
            .key(key)
            .maybe_header("range", range.map(|r| r.header_value()).as_deref())
            .maybe_header("if-match", if_match);

        let response = self.execute(req).await?;
        Ok(GetResult {
            size: content_length(&response).unwrap_or(0),
            etag: etag_of(&response),
            content_type: response.header("content-type").map(str::to_owned),
            body: response.body,
        })
    }

    /// 整体上传。`md5` 非空时服务端会校验请求体。
    pub async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        body: Body,
        md5: Option<[u8; 16]>,
        content_type: Option<&str>,
    ) -> Result<String, S3Error> {
        let req = Request::put()
            .bucket(bucket)
            .key(key)
            .maybe_header("content-type", content_type)
            .body_with_md5(body, md5);

        let response = self.execute(req).await?;
        Ok(etag_of(&response).unwrap_or_default())
    }

    /// 新建文件夹就是上传一个以 `/` 结尾的空对象。
    pub async fn create_folder(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
        let key = if key.ends_with('/') {
            key.to_owned()
        } else {
            format!("{key}/")
        };
        self.put_object(bucket, &key, Body::Empty, Some(md5_of(&[])), None)
            .await?;
        Ok(())
    }

    pub async fn delete_object(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
        self.execute(Request::delete().bucket(bucket).key(key))
            .await?;
        Ok(())
    }

    /// 批量删除，一次最多 1000 个。返回逐个 key 的失败原因，整体成功时为空。
    pub async fn delete_objects(
        &self,
        bucket: &str,
        keys: &[String],
    ) -> Result<Vec<DeleteError>, S3Error> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let body = xml::delete_objects_body(keys);
        let bytes = body.into_bytes();
        let md5 = md5_of(&bytes);

        let req = Request::post()
            .bucket(bucket)
            .flag("delete")
            .header("content-type", "application/xml")
            .body_with_md5(Body::from(bytes), Some(md5));

        let response = self.execute(req).await?;
        let text = self.read_text(response).await?;
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        xml::parse_delete_result(&text)
    }

    /// 服务端复制。对象超过 5 GiB 时阿里云 OSS 会拒绝，改用 UploadPartCopy。
    pub async fn copy_object(
        &self,
        bucket: &str,
        key: &str,
        src_bucket: &str,
        src_key: &str,
    ) -> Result<String, S3Error> {
        let req = Request::put()
            .bucket(bucket)
            .key(key)
            .header("x-amz-copy-source", &copy_source(src_bucket, src_key));

        let response = self.execute(req).await?;
        let text = self.read_text(response).await?;
        xml::parse_copy_result(&text)
    }

    /// 生成预签名下载链接，有效期上限 7 天。
    pub async fn presign_get(
        &self,
        bucket: &str,
        key: &str,
        expires: Duration,
    ) -> Result<String, S3Error> {
        self.presign(bucket, key, expires.as_secs()).await
    }
}

/// `x-amz-copy-source` 的值：`/桶/编码后的 key`。
pub(crate) fn copy_source(bucket: &str, key: &str) -> String {
    format!("/{bucket}/{}", encode_path(key))
}

fn content_length(response: &HttpResponse) -> Option<u64> {
    response.header("content-length")?.parse().ok()
}

fn etag_of(response: &HttpResponse) -> Option<String> {
    response
        .header("etag")
        .map(|e| e.trim_matches('"').to_owned())
        .filter(|e| !e.is_empty())
}

/// `Last-Modified` 是 RFC 2822 格式的 HTTP 日期。
fn last_modified(response: &HttpResponse) -> i64 {
    response
        .header("last-modified")
        .and_then(|v| chrono::DateTime::parse_from_rfc2822(v).ok())
        .map(|t| t.timestamp())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_source_encodes_key() {
        assert_eq!(copy_source("b", "a/b c.txt"), "/b/a/b%20c.txt");
        assert_eq!(copy_source("b", "x$y"), "/b/x%24y");
    }
}
