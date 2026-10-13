//! 断点续传引擎：上传、下载、服务端复制、批量删除。
//!
//! 队列以 SQLite 为准，进程崩了重启能接着传。引擎通过 `ObjectStore` 访问对象存储，
//! 不直接依赖 `S3Client`，方便注入故障做测试。

mod copy;
mod delete;
mod download;
mod driver;
mod upload;

pub mod engine;
pub mod job;
pub mod object_store;
pub mod store;

pub use download::unique_path;
pub use engine::{Engine, EngineCommand, EngineConfig, EngineEvent, JobControl, JobOutcome};
pub use job::{
    Batch, BatchId, BatchProgress, Job, JobId, JobKind, JobProgress, JobState,
    MIN_UPLOAD_PART_SIZE, Part, split_parts,
};
pub use object_store::{ObjectStore, S3ObjectStore, StoreResolver, StoreResult};
pub use store::{JobPatch, JobStore, SqliteJobStore, StoreError};
