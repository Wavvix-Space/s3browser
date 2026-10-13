//! 任务模型：一种队列，四种任务。

use std::path::PathBuf;

/// 任务 id。短随机串够本机用。
pub type JobId = String;
pub type BatchId = String;

pub fn new_id() -> String {
    format!("{:016x}", fastrand::u64(..))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Upload,
    Download,
    /// 服务端复制。
    Copy,
    /// 移动：复制成功后删掉源对象。重命名就是同目录内的移动。
    Move,
    /// 批量删除，一个任务管一批 key。
    Delete,
}

impl JobKind {
    pub fn as_i64(self) -> i64 {
        match self {
            JobKind::Upload => 0,
            JobKind::Download => 1,
            JobKind::Copy => 2,
            JobKind::Delete => 3,
            JobKind::Move => 4,
        }
    }

    pub fn from_i64(value: i64) -> Self {
        match value {
            1 => JobKind::Download,
            2 => JobKind::Copy,
            3 => JobKind::Delete,
            4 => JobKind::Move,
            _ => JobKind::Upload,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            JobKind::Upload => "上传",
            JobKind::Download => "下载",
            JobKind::Copy => "复制",
            JobKind::Move => "移动",
            JobKind::Delete => "删除",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Done,
    /// 可重试的失败，错误原因在 `Job::error`。
    Failed,
    Cancelled,
}

impl JobState {
    pub fn as_i64(self) -> i64 {
        match self {
            JobState::Queued => 0,
            JobState::Running => 1,
            JobState::Paused => 2,
            JobState::Done => 3,
            JobState::Failed => 4,
            JobState::Cancelled => 5,
        }
    }

    pub fn from_i64(value: i64) -> Self {
        match value {
            1 => JobState::Running,
            2 => JobState::Paused,
            3 => JobState::Done,
            4 => JobState::Failed,
            5 => JobState::Cancelled,
            _ => JobState::Queued,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, JobState::Done | JobState::Cancelled)
    }

    /// 还需要占用连接的状态。
    pub fn is_active(self) -> bool {
        matches!(self, JobState::Queued | JobState::Running)
    }
}

/// 一批任务，界面上按批次汇总进度。
#[derive(Debug, Clone)]
pub struct Batch {
    pub id: BatchId,
    pub kind: JobKind,
    pub label: String,
    pub created_at: i64,
}

impl Batch {
    pub fn new(kind: JobKind, label: impl Into<String>) -> Self {
        Batch {
            id: new_id(),
            kind,
            label: label.into(),
            created_at: now(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: JobId,
    pub batch_id: BatchId,
    pub kind: JobKind,
    pub profile_id: String,
    pub bucket: String,
    /// 目标 key。删除任务用它存第一个 key，完整列表在 `delete_keys`。
    pub key: String,
    /// 复制任务的源 key。
    pub src_key: String,
    /// 上传下载的本地路径。
    pub local_path: Option<PathBuf>,
    pub size: u64,
    pub part_size: u64,
    pub state: JobState,
    pub upload_id: Option<String>,
    pub etag: Option<String>,
    /// 上传任务记下的本地文件修改时间，续传时比对。
    pub local_mtime: i64,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Job {
    pub fn new(batch_id: &str, kind: JobKind, profile_id: &str, bucket: &str, key: &str) -> Self {
        let ts = now();
        Job {
            id: new_id(),
            batch_id: batch_id.to_owned(),
            kind,
            profile_id: profile_id.to_owned(),
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            src_key: String::new(),
            local_path: None,
            size: 0,
            part_size: 0,
            state: JobState::Queued,
            upload_id: None,
            etag: None,
            local_mtime: 0,
            error: None,
            created_at: ts,
            updated_at: ts,
        }
    }

    pub fn with_local(mut self, path: PathBuf, size: u64, mtime: i64) -> Self {
        self.local_path = Some(path);
        self.size = size;
        self.local_mtime = mtime;
        self
    }

    pub fn with_source(mut self, src_key: &str, size: u64) -> Self {
        self.src_key = src_key.to_owned();
        self.size = size;
        self
    }

    /// 界面上显示的名字：key 的最后一段。
    pub fn display_name(&self) -> &str {
        let key = self.key.trim_end_matches('/');
        match key.rfind('/') {
            Some(ix) => &key[ix + 1..],
            None => key,
        }
    }
}

/// 一个分片。上传是文件分片，下载是 Range 分块，复制是 UploadPartCopy 的一段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub idx: u32,
    pub offset: u64,
    pub len: u64,
    pub done: bool,
    /// 分片上传完成后服务端回的 ETag，CompleteMultipartUpload 要用。
    pub etag: Option<String>,
}

impl Part {
    /// S3 的分片编号从 1 开始。
    pub fn number(&self) -> u32 {
        self.idx + 1
    }

    pub fn end(&self) -> u64 {
        self.offset + self.len.saturating_sub(1)
    }
}

/// S3 规定分片上传时除最后一片外每片至少 5 MiB，小于这个值服务端回 EntityTooSmall。
/// 下载分块没有这个限制。
pub const MIN_UPLOAD_PART_SIZE: u64 = 5 * 1024 * 1024;

/// 按总大小和分片大小切分。分片数超过 10000 时自动放大分片。
pub fn split_parts(size: u64, part_size: u64) -> (u64, Vec<Part>) {
    const MAX_PARTS: u64 = 10_000;
    const MIB: u64 = 1024 * 1024;

    let mut part_size = part_size.max(1);
    if size.div_ceil(part_size) > MAX_PARTS {
        // 放大到刚好能装下，向上取整到 MiB
        part_size = size.div_ceil(MAX_PARTS).div_ceil(MIB) * MIB;
    }

    let mut parts = Vec::new();
    let mut offset = 0u64;
    let mut idx = 0u32;
    while offset < size {
        let len = part_size.min(size - offset);
        parts.push(Part {
            idx,
            offset,
            len,
            done: false,
            etag: None,
        });
        offset += len;
        idx += 1;
    }
    (part_size, parts)
}

/// 界面用的任务进度快照。
#[derive(Debug, Clone)]
pub struct JobProgress {
    pub id: JobId,
    pub batch_id: BatchId,
    pub kind: JobKind,
    pub name: String,
    pub state: JobState,
    /// 任务入队的时刻（unix 秒）。传输列表按它显示时间、排新在上。
    pub created_at: i64,
    /// 用户按了暂停，但手上的分片还没停下来。
    ///
    /// 这时候 `state` 还是 `Running`——真停下来了才会变成 `Paused`。
    /// 界面靠它立刻给个反馈，不然按下去半天没动静，像是没点着。
    pub pausing: bool,
    pub transferred: u64,
    pub total: u64,
    pub rate_bps: u64,
    pub error: Option<String>,
}

impl JobProgress {
    pub fn percent(&self) -> f32 {
        if self.total == 0 {
            return if self.state == JobState::Done { 1.0 } else { 0.0 };
        }
        (self.transferred as f32 / self.total as f32).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone)]
pub struct BatchProgress {
    pub id: BatchId,
    pub label: String,
    pub kind: JobKind,
    pub total_jobs: u32,
    pub done_jobs: u32,
    pub failed_jobs: u32,
    pub transferred: u64,
    pub total: u64,
}

/// 一条传输历史：任务落定（成功、失败或取消）时记下，跨启动保留。
///
/// 和 `jobs` 表分开存——那张表是给断点续传用的，完成的行会被清掉；
/// 历史只管"什么时候传过什么"，不参与恢复。
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub job_id: JobId,
    pub kind: JobKind,
    pub bucket: String,
    /// 完整 key，界面取最后一段当名字，和任务列表一致。
    pub key: String,
    pub size: u64,
    pub state: JobState,
    pub error: Option<String>,
    /// 任务入队时刻（unix 秒）。
    pub created_at: i64,
    /// 落定时刻（unix 秒），历史按它倒序。
    pub finished_at: i64,
}

impl HistoryEntry {
    /// 界面上显示的名字：key 的最后一段。
    pub fn display_name(&self) -> &str {
        let key = self.key.trim_end_matches('/');
        match key.rfind('/') {
            Some(ix) => &key[ix + 1..],
            None => key,
        }
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_evenly_with_remainder() {
        let (size, parts) = split_parts(20, 8);
        assert_eq!(size, 8);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], Part { idx: 0, offset: 0, len: 8, done: false, etag: None });
        assert_eq!(parts[2], Part { idx: 2, offset: 16, len: 4, done: false, etag: None });
        assert_eq!(parts[0].number(), 1);
        assert_eq!(parts[2].end(), 19);
    }

    #[test]
    fn empty_file_has_no_parts() {
        let (_, parts) = split_parts(0, 8);
        assert!(parts.is_empty());
    }

    #[test]
    fn exact_multiple_has_no_trailing_part() {
        let (_, parts) = split_parts(16, 8);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].len, 8);
    }

    #[test]
    fn grows_part_size_past_ten_thousand_parts() {
        const MIB: u64 = 1024 * 1024;
        // 8 MiB 分片装 200 GiB 要两万多片，必须放大
        let size = 200 * 1024 * MIB;
        let (part_size, parts) = split_parts(size, 8 * MIB);

        assert!(parts.len() <= 10_000, "{} 片太多", parts.len());
        assert!(part_size > 8 * MIB);
        assert_eq!(part_size % MIB, 0, "放大后要对齐到 MiB");
        assert_eq!(parts.iter().map(|p| p.len).sum::<u64>(), size);
    }

    #[test]
    fn display_name_takes_last_segment() {
        let job = Job::new("b", JobKind::Upload, "p", "bucket", "a/b/c.txt");
        assert_eq!(job.display_name(), "c.txt");

        let folder = Job::new("b", JobKind::Upload, "p", "bucket", "a/b/");
        assert_eq!(folder.display_name(), "b");

        let top = Job::new("b", JobKind::Upload, "p", "bucket", "top.txt");
        assert_eq!(top.display_name(), "top.txt");
    }

    #[test]
    fn state_round_trips_through_int() {
        for state in [
            JobState::Queued,
            JobState::Running,
            JobState::Paused,
            JobState::Done,
            JobState::Failed,
            JobState::Cancelled,
        ] {
            assert_eq!(JobState::from_i64(state.as_i64()), state);
        }
        for kind in [
            JobKind::Upload,
            JobKind::Download,
            JobKind::Copy,
            JobKind::Move,
            JobKind::Delete,
        ] {
            assert_eq!(JobKind::from_i64(kind.as_i64()), kind);
        }
    }

    #[test]
    fn progress_percent_handles_zero_size() {
        let mut p = JobProgress {
            id: "j".into(),
            batch_id: "b".into(),
            kind: JobKind::Upload,
            name: "x".into(),
            state: JobState::Running,
            created_at: 0,
            pausing: false,
            transferred: 0,
            total: 0,
            rate_bps: 0,
            error: None,
        };
        assert_eq!(p.percent(), 0.0);
        p.state = JobState::Done;
        assert_eq!(p.percent(), 1.0);

        p.total = 100;
        p.transferred = 50;
        assert_eq!(p.percent(), 0.5);
    }
}
