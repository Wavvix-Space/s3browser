//! 极简 S3 协议层：只管签名、编码和请求构造，不绑定 HTTP 库与异步运行时。
//!
//! 具体的 HTTP 发送由调用方通过 [`transport::HttpTransport`] 注入，
//! 凭据来源通过 [`transport::CredentialsProvider`] 注入。

pub mod client;
pub mod encode;
pub mod model;
pub mod ops;
pub mod sigv4;
pub mod transport;
pub mod types;
pub mod xml;

pub use client::{RetryConfig, S3Client};
pub use model::{
    Acl, BucketInfo, ByteRange, DeleteError, GetResult, HeadResult, ListPage, ListUploadsPage,
    ObjectMeta, PartInfo, PayloadSigning, UploadInfo,
};
pub use transport::{
    Body, CredentialsError, CredentialsProvider, HttpRequest, HttpResponse, HttpTransport,
    StaticCredentials, TransportError,
};
pub use types::{Addressing, Credentials, Endpoint, EndpointError, S3Error, Scheme};
