//! 对象的访问权限。
//!
//! 只做对象级，不碰桶级——桶的权限一般不是在文件浏览器里改的，
//! 改错了影响面也大得多。

use crate::client::{Request, S3Client};
use crate::model::Acl;
use crate::types::S3Error;
use crate::xml;

impl S3Client {
    pub async fn get_object_acl(&self, bucket: &str, key: &str) -> Result<Acl, S3Error> {
        let response = self
            .execute(Request::get().bucket(bucket).key(key).flag("acl"))
            .await?;
        let text = self.read_text(response).await?;
        xml::parse_object_acl(&text)
    }

    /// 改对象的访问权限。
    ///
    /// 两个头都发：走 S3 协议的服务端认 `x-amz-acl`，阿里云 OSS 的
    /// `default`（跟着桶走）是它自己的说法，只有 `x-oss-object-acl` 收。
    /// 多发一个头对不认识它的服务端没有副作用。
    pub async fn put_object_acl(&self, bucket: &str, key: &str, acl: Acl) -> Result<(), S3Error> {
        let value = acl.header_value();
        self.execute(
            Request::put()
                .bucket(bucket)
                .key(key)
                .flag("acl")
                .header("x-amz-acl", value)
                .header("x-oss-object-acl", value),
        )
        .await?;
        Ok(())
    }
}
