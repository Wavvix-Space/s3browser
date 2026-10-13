//! 任务驱动器：按类型分发，并提供各类任务共用的文件读写和许可获取。

use std::io::{self, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use futures_util::stream::BoxStream;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::engine::{EngineConfig, JobControl, JobOutcome, JobRun};
use crate::job::JobKind;
use crate::object_store::ObjectStore;
use crate::store::JobStore;

/// 流式发送时的缓冲。设计上传输期间的额外内存就是它乘以并发数。
pub(crate) const STREAM_BUFFER: usize = 64 * 1024;

pub(crate) async fn run_job(
    run: JobRun,
    store: Arc<dyn ObjectStore>,
    job_store: Arc<dyn JobStore>,
    config: EngineConfig,
    permits: Arc<Semaphore>,
) -> JobOutcome {
    let ctx = TaskContext {
        store,
        job_store,
        permits,
        config,
    };

    match run.job.kind {
        JobKind::Upload => crate::upload::run(run, ctx).await,
        JobKind::Download => crate::download::run(run, ctx).await,
        JobKind::Copy | JobKind::Move => crate::copy::run(run, ctx).await,
        JobKind::Delete => crate::delete::run(run, ctx).await,
    }
}

/// 一个任务执行期间要用到的东西。
///
/// 任务内的并发用 `buffer_unordered(max_parts_per_job)` 控制，
/// 跨任务的总连接数用这个全局信号量控制。
pub(crate) struct TaskContext {
    pub store: Arc<dyn ObjectStore>,
    pub job_store: Arc<dyn JobStore>,
    pub config: EngineConfig,
    pub permits: Arc<Semaphore>,
}

impl TaskContext {
    /// 占一条全局连接。拿不到说明引擎在关，调用方直接收手。
    pub async fn connection(&self) -> Option<OwnedSemaphorePermit> {
        self.permits.clone().acquire_owned().await.ok()
    }
}

/// 一批分片跑完之后该怎么收场。
pub(crate) enum PartsOutcome {
    /// 全都传完了，可以收尾。
    AllDone,
    /// 用户按了取消。收尾（abort、删临时文件）由调用方自己做。
    Cancelled,
    /// 停下来了，没传完的下次接着。
    Paused,
    /// 真出错了。
    Failed(String),
}

/// 三种任务的分片循环长得不一样，收场的判断却是同一套。
///
/// 要点是**用户的意思优先于报错**：按了取消或暂停之后冒出来的错，多半是被
/// 我们自己打断的（连接掐了、请求发不出去），报成"失败"会让人以为传坏了。
/// 没传完的分片本来就不算数，下次从头再来，所以按用户的意思收场就行。
pub(crate) fn settle_parts(
    control: &JobControl,
    stopped: bool,
    failure: Option<String>,
    job_id: &str,
) -> PartsOutcome {
    if control.is_cancelled() {
        if let Some(message) = failure {
            tracing::debug!(job = job_id, "取消期间有分片出错，按取消处理：{message}");
        }
        return PartsOutcome::Cancelled;
    }
    // 该传的都传完了才按下暂停：白扔掉太可惜，收尾就完事了。
    // 取消不走这条——那是要把东西删掉，已经传完也一样。
    if !stopped && failure.is_none() {
        return PartsOutcome::AllDone;
    }
    if control.is_paused() {
        if let Some(message) = failure {
            tracing::debug!(job = job_id, "暂停期间有分片出错，按暂停处理：{message}");
        }
        return PartsOutcome::Paused;
    }
    if let Some(message) = failure {
        return PartsOutcome::Failed(message);
    }
    if stopped {
        // 没人按暂停也停了，多半是引擎在关。当暂停处理，下次接着传。
        return PartsOutcome::Paused;
    }
    PartsOutcome::AllDone
}

/// 把文件的一段包成流，按 64 KiB 读，不整块进内存。
pub(crate) fn file_part_stream(
    path: PathBuf,
    offset: u64,
    len: u64,
) -> BoxStream<'static, io::Result<Bytes>> {
    struct State {
        file: Option<tokio::fs::File>,
        path: PathBuf,
        offset: u64,
        remaining: u64,
    }

    let state = State {
        file: None,
        path,
        offset,
        remaining: len,
    };

    Box::pin(futures_util::stream::unfold(state, |mut state| async move {
        if state.remaining == 0 {
            return None;
        }
        // 第一次拉取时才开文件，并定位到分片起点
        if state.file.is_none() {
            match tokio::fs::File::open(&state.path).await {
                Ok(mut file) => {
                    if let Err(e) = file.seek(SeekFrom::Start(state.offset)).await {
                        state.remaining = 0;
                        return Some((Err(e), state));
                    }
                    state.file = Some(file);
                }
                Err(e) => {
                    state.remaining = 0;
                    return Some((Err(e), state));
                }
            }
        }

        let want = state.remaining.min(STREAM_BUFFER as u64) as usize;
        let mut buf = vec![0u8; want];
        let file = state.file.as_mut().expect("文件已打开");
        match file.read_exact(&mut buf).await {
            Ok(_) => {
                state.remaining -= want as u64;
                Some((Ok(Bytes::from(buf)), state))
            }
            Err(e) => {
                state.remaining = 0;
                Some((Err(e), state))
            }
        }
    }))
}

/// 顺序读一遍算这一段的 MD5。放在 blocking 池里跑，别占住异步线程。
pub(crate) async fn md5_of_range(path: PathBuf, offset: u64, len: u64) -> io::Result<[u8; 16]> {
    tokio::task::spawn_blocking(move || {
        use md5::Digest as _;
        use std::io::{Read, Seek};

        let mut file = std::fs::File::open(&path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut hasher = md5::Md5::new();
        let mut buf = vec![0u8; STREAM_BUFFER];
        let mut remaining = len;
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            file.read_exact(&mut buf[..want])?;
            hasher.update(&buf[..want]);
            remaining -= want as u64;
        }
        Ok(hasher.finalize().into())
    })
    .await
    .map_err(|e| io::Error::other(e.to_string()))?
}

/// 整个文件的 MD5，下载完成后校验用。
pub(crate) async fn md5_of_file(path: PathBuf) -> io::Result<[u8; 16]> {
    let len = tokio::fs::metadata(&path).await?.len();
    md5_of_range(path, 0, len).await
}

/// 文件当前的大小与修改时间，用来判断续传期间本地文件有没有被改过。
pub(crate) async fn local_stamp(path: &Path) -> io::Result<(u64, i64)> {
    let meta = tokio::fs::metadata(path).await?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok((meta.len(), mtime))
}

/// 下载写入用的临时文件名。
pub(crate) fn temp_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".s3b-part");
    target.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn control(paused: bool, cancelled: bool) -> JobControl {
        let control = JobControl::for_test();
        control.set_paused(paused);
        control.set_cancelled(cancelled);
        control
    }

    #[test]
    fn everything_done_is_done() {
        assert!(matches!(
            settle_parts(&control(false, false), false, None, "j"),
            PartsOutcome::AllDone
        ));
    }

    #[test]
    fn a_real_failure_is_reported() {
        let out = settle_parts(&control(false, false), false, Some("炸了".into()), "j");
        assert!(matches!(out, PartsOutcome::Failed(m) if m == "炸了"));
    }

    /// 按了暂停之后冒出来的错多半是被我们自己打断的，不该报成失败——
    /// 界面上会显示"请求无效: error sending request"，看着像传坏了。
    #[test]
    fn a_failure_while_pausing_counts_as_paused() {
        let out = settle_parts(&control(true, false), true, Some("error sending request".into()), "j");
        assert!(matches!(out, PartsOutcome::Paused));
    }

    #[test]
    fn a_failure_while_cancelling_counts_as_cancelled() {
        let out = settle_parts(&control(false, true), true, Some("error sending request".into()), "j");
        assert!(matches!(out, PartsOutcome::Cancelled));
    }

    /// 取消压过暂停：两个都按了就按取消算，别把该删的临时文件留下。
    #[test]
    fn cancelling_wins_over_pausing() {
        let out = settle_parts(&control(true, true), true, None, "j");
        assert!(matches!(out, PartsOutcome::Cancelled));
    }

    /// 谁也没按却停了，多半是引擎在关，当暂停处理，下次接着传。
    #[test]
    fn stopping_without_a_reason_is_a_pause() {
        assert!(matches!(
            settle_parts(&control(false, false), true, None, "j"),
            PartsOutcome::Paused
        ));
    }

    /// 分片全都传完了才按下暂停，别把成果扔掉——收个尾就完事了。
    #[test]
    fn a_pause_that_arrives_after_everything_finished_still_finishes() {
        assert!(matches!(
            settle_parts(&control(true, false), false, None, "j"),
            PartsOutcome::AllDone
        ));
    }

    /// 取消不一样：它是要把东西删掉，传完了也照删。
    #[test]
    fn a_late_cancel_still_cancels() {
        assert!(matches!(
            settle_parts(&control(false, true), false, None, "j"),
            PartsOutcome::Cancelled
        ));
    }

    /// tokio::fs 要在 tokio runtime 里跑。
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn temp_file(contents: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("s3b-drv-{}", fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.bin");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn streams_only_the_requested_range() {
        let data: Vec<u8> = (0..=255u8).collect();
        let path = temp_file(&data);

        let chunks: Vec<Bytes> = block_on(async {
            file_part_stream(path.clone(), 10, 20)
                .map(|c| c.unwrap())
                .collect()
                .await
        });

        let joined: Vec<u8> = chunks.iter().flat_map(|c| c.to_vec()).collect();
        assert_eq!(joined, data[10..30]);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn streams_in_buffer_sized_chunks() {
        let data = vec![7u8; STREAM_BUFFER * 2 + 100];
        let path = temp_file(&data);

        let sizes: Vec<usize> = block_on(async {
            file_part_stream(path.clone(), 0, data.len() as u64)
                .map(|c| c.unwrap().len())
                .collect()
                .await
        });

        assert_eq!(sizes, vec![STREAM_BUFFER, STREAM_BUFFER, 100]);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn empty_range_yields_nothing() {
        let path = temp_file(b"abc");
        let count = block_on(async { file_part_stream(path.clone(), 0, 0).count().await });
        assert_eq!(count, 0);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn temp_path_appends_suffix() {
        let target = Path::new("C:/down/report.pdf");
        assert_eq!(
            temp_path(target),
            PathBuf::from("C:/down/report.pdf.s3b-part")
        );
    }
}
