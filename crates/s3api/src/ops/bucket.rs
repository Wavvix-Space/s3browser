//! 桶级操作。

use crate::client::{Request, S3Client};
use crate::model::{BucketInfo, ListPage};
use crate::types::S3Error;
use crate::xml;

/// 每页条目数上限，S3 协议规定最大 1000。
pub const MAX_KEYS: u32 = 1000;

impl S3Client {
    pub async fn list_buckets(&self) -> Result<Vec<BucketInfo>, S3Error> {
        let response = self.execute(Request::get()).await?;
        let text = self.read_text(response).await?;
        xml::parse_list_buckets(&text)
    }

    /// 只用于账户对话框的“测试连接”。成功返回 Ok，桶不存在或无权限时返回对应错误。
    pub async fn head_bucket(&self, bucket: &str) -> Result<(), S3Error> {
        self.execute(Request::head().bucket(bucket)).await?;
        Ok(())
    }

    /// 界面不暴露建桶，但集成测试需要它来准备环境。
    pub async fn create_bucket(&self, bucket: &str) -> Result<(), S3Error> {
        self.execute(Request::put().bucket(bucket)).await?;
        Ok(())
    }

    /// 列举一页对象。目录浏览传 `delimiter = Some("/")`，递归列举传 None。
    pub async fn list_objects_v2(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        continuation_token: Option<&str>,
        max_keys: u32,
    ) -> Result<ListPage, S3Error> {
        let mut req = Request::get()
            .bucket(bucket)
            .query("list-type", 2)
            .query("max-keys", max_keys.clamp(1, MAX_KEYS));
        if !prefix.is_empty() {
            req = req.query("prefix", prefix);
        }
        if let Some(d) = delimiter {
            req = req.query("delimiter", d);
        }
        if let Some(t) = continuation_token {
            req = req.query("continuation-token", t);
        }

        let response = self.execute(req).await?;
        let text = self.read_text(response).await?;
        xml::parse_list_objects(&text)
    }
}
