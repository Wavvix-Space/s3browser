//! 分片上传操作。分片编号从 1 开始，最多 10000 个。

use crate::client::{Request, S3Client};
use crate::model::{ByteRange, ListUploadsPage, PartInfo};
use crate::ops::object::copy_source;
use crate::transport::Body;
use crate::types::S3Error;
use crate::xml;

/// S3 协议规定的分片数量上限。
pub const MAX_PARTS: u32 = 10000;
/// ListParts 每页的上限。阿里云 OSS 对超过 1000 的值直接回 InvalidArgument，
/// 分片总数靠翻页取全。
pub const MAX_PARTS_PER_PAGE: u32 = 1000;

impl S3Client {
    pub async fn create_multipart_upload(
        &self,
        bucket: &str,
        key: &str,
        content_type: Option<&str>,
    ) -> Result<String, S3Error> {
        let req = Request::post()
            .bucket(bucket)
            .key(key)
            .flag("uploads")
            .maybe_header("content-type", content_type);

        let response = self.execute(req).await?;
        let text = self.read_text(response).await?;
        xml::parse_initiate_multipart(&text)
    }

    pub async fn upload_part(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: u32,
        body: Body,
        md5: Option<[u8; 16]>,
    ) -> Result<String, S3Error> {
        let req = Request::put()
            .bucket(bucket)
            .key(key)
            .query("partNumber", part_number)
            .query("uploadId", upload_id)
            .body_with_md5(body, md5);

        let response = self.execute(req).await?;
        Ok(response
            .header("etag")
            .map(|e| e.trim_matches('"').to_owned())
            .unwrap_or_default())
    }

    /// 从已有对象复制一段作为分片，用于超过单次 CopyObject 上限的大对象。
    pub async fn upload_part_copy(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: u32,
        src_bucket: &str,
        src_key: &str,
        range: Option<ByteRange>,
    ) -> Result<String, S3Error> {
        let req = Request::put()
            .bucket(bucket)
            .key(key)
            .query("partNumber", part_number)
            .query("uploadId", upload_id)
            .header("x-amz-copy-source", &copy_source(src_bucket, src_key))
            .maybe_header(
                "x-amz-copy-source-range",
                range.map(|r| r.header_value()).as_deref(),
            );

        let response = self.execute(req).await?;
        let text = self.read_text(response).await?;
        xml::parse_copy_result(&text)
    }

    /// 完成上传。`parts` 会按编号升序排序后提交。
    pub async fn complete_multipart_upload(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, S3Error> {
        let mut sorted = parts.to_vec();
        sorted.sort_by_key(|(n, _)| *n);
        let body = xml::complete_multipart_body(&sorted);

        let req = Request::post()
            .bucket(bucket)
            .key(key)
            .query("uploadId", upload_id)
            .header("content-type", "application/xml")
            .body(Body::from(body));

        let response = self.execute(req).await?;
        let text = self.read_text(response).await?;
        // 完成请求可能返回 200 加一个错误体，这种情况按失败处理。
        if text.contains("<Error>") {
            return Err(xml::parse_error(200, &text, None));
        }
        xml::parse_complete_multipart(&text)
    }

    pub async fn abort_multipart_upload(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> Result<(), S3Error> {
        self.execute(
            Request::delete()
                .bucket(bucket)
                .key(key)
                .query("uploadId", upload_id),
        )
        .await?;
        Ok(())
    }

    /// 列出已上传的分片，自动翻页。续传时以此为准。
    pub async fn list_parts(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> Result<Vec<PartInfo>, S3Error> {
        let mut all = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let mut req = Request::get()
                .bucket(bucket)
                .key(key)
                .query("uploadId", upload_id)
                .query("max-parts", MAX_PARTS_PER_PAGE);
            if let Some(m) = &marker {
                req = req.query("part-number-marker", m);
            }

            let response = self.execute(req).await?;
            let text = self.read_text(response).await?;
            let (parts, next) = xml::parse_list_parts(&text)?;
            all.extend(parts);
            match next {
                Some(m) => marker = Some(m),
                None => break,
            }
        }
        all.sort_by_key(|p| p.number);
        Ok(all)
    }

    /// 列出未完成的分片上传，用于清理和续传发现。
    pub async fn list_multipart_uploads(
        &self,
        bucket: &str,
        prefix: &str,
        key_marker: Option<&str>,
        upload_id_marker: Option<&str>,
    ) -> Result<ListUploadsPage, S3Error> {
        let mut req = Request::get().bucket(bucket).flag("uploads");
        if !prefix.is_empty() {
            req = req.query("prefix", prefix);
        }
        if let Some(m) = key_marker {
            req = req.query("key-marker", m);
        }
        if let Some(m) = upload_id_marker {
            req = req.query("upload-id-marker", m);
        }

        let response = self.execute(req).await?;
        let text = self.read_text(response).await?;
        xml::parse_list_uploads(&text)
    }
}
