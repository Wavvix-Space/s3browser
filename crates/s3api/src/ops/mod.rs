//! 操作按对象归组：桶级、对象级、分片上传。都是 `S3Client` 的方法。

pub mod acl;
pub mod bucket;
pub mod multipart;
pub mod object;
