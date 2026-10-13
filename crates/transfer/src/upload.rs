//! 上传。小文件一次 PutObject，大文件分片；续传以服务端的 ListParts 为准。

use std::path::PathBuf;

use futures_util::StreamExt;
use s3api::Body;

use crate::driver::{
    PartsOutcome, TaskContext, file_part_stream, local_stamp, md5_of_range, settle_parts,
};
use crate::engine::{JobControl, JobOutcome, JobRun};
use crate::job::{Job, Part, split_parts};
use crate::object_store::is_no_such_upload;
use crate::store::JobPatch;

pub(crate) async fn run(run: JobRun, ctx: TaskContext) -> JobOutcome {
    let JobRun { job, control } = run;

    // key 以 / 结尾的是空文件夹标记，传一个零字节对象就行。
    // 这一步必须排在本地路径检查前面：新建文件夹压根没有本地文件。
    if job.key.ends_with('/') {
        return match ctx
            .store
            .put(&job.bucket, &job.key, Body::Empty, Some(md5_empty()))
            .await
        {
            Ok(etag) => JobOutcome::Done { etag: Some(etag) },
            Err(e) => JobOutcome::Failed(e.to_string()),
        };
    }

    let Some(path) = job.local_path.clone() else {
        return JobOutcome::Failed("任务没有本地路径".to_owned());
    };

    let (size, mtime) = match local_stamp(&path).await {
        Ok(stamp) => stamp,
        Err(e) => return JobOutcome::Failed(format!("读不到本地文件：{e}")),
    };

    if size <= ctx.config.multipart_threshold {
        return upload_whole(&job, &control, &ctx, path, size).await;
    }
    upload_parts(job, control, ctx, path, size, mtime).await
}

/// 小文件：算一次 MD5，流式发一次。失败了下次重来，不记分片。
async fn upload_whole(
    job: &Job,
    control: &JobControl,
    ctx: &TaskContext,
    path: PathBuf,
    size: u64,
) -> JobOutcome {
    let md5 = match md5_of_range(path.clone(), 0, size).await {
        Ok(md5) => md5,
        Err(e) => return JobOutcome::Failed(format!("算 MD5 失败：{e}")),
    };
    let Some(_permit) = ctx.connection().await else {
        return JobOutcome::Paused;
    };
    let body = Body::Stream {
        stream: file_part_stream(path, 0, size),
        len: size,
    };
    match ctx.store.put(&job.bucket, &job.key, body, Some(md5)).await {
        Ok(etag) => {
            control.set_transferred(size);
            JobOutcome::Done { etag: Some(etag) }
        }
        Err(e) => JobOutcome::Failed(e.to_string()),
    }
}

/// 大文件：分片上传，中断后能接着传。
async fn upload_parts(
    job: Job,
    control: std::sync::Arc<JobControl>,
    ctx: TaskContext,
    path: PathBuf,
    size: u64,
    mtime: i64,
) -> JobOutcome {
    // 配置里可以把分片调小，但小于协议下限服务端会回 EntityTooSmall，这里兜底抬上去
    let (part_size, mut parts) =
        split_parts(size, ctx.config.part_size.max(ctx.config.min_upload_part_size));

    // 本地文件在两次运行之间被改过的话，之前传上去的分片都对不上了
    let local_changed = job.size != 0 && (job.size != size || job.local_mtime != mtime);
    let mut upload_id = job.upload_id.clone();

    if local_changed && let Some(old) = upload_id.take() {
        tracing::info!("{} 本地文件变了，放弃之前的分片上传", job.key);
        let _ = ctx
            .store
            .abort_multipart(&job.bucket, &job.key, &old)
            .await;
    }

    // 续传：服务端说有哪些分片就认哪些，本地记录不作数
    if let Some(id) = &upload_id {
        match ctx.store.list_parts(&job.bucket, &job.key, id).await {
            Ok(remote) => {
                for part in parts.iter_mut() {
                    let matched = remote
                        .iter()
                        .find(|r| r.number == part.number() && r.size == part.len);
                    if let Some(remote) = matched {
                        part.done = true;
                        part.etag = Some(remote.etag.clone());
                    }
                }
            }
            Err(e) if is_no_such_upload(&e) => {
                tracing::info!("{} 的分片上传已失效，重新开始", job.key);
                upload_id = None;
            }
            Err(e) => return JobOutcome::Failed(e.to_string()),
        }
    }

    let upload_id = match upload_id {
        Some(id) => id,
        None => match ctx.store.create_multipart(&job.bucket, &job.key).await {
            Ok(id) => id,
            Err(e) => return JobOutcome::Failed(e.to_string()),
        },
    };

    // upload_id 和分片表立刻落库，这样进程突然没了也能接着传
    let _ = ctx.job_store.update_job(
        &job.id,
        JobPatch::default()
            .with_size(size)
            .with_part_size(part_size)
            .with_upload_id(Some(upload_id.clone())),
    );
    let _ = ctx.job_store.insert_parts(&job.id, &parts);

    let done_bytes: u64 = parts.iter().filter(|p| p.done).map(|p| p.len).sum();
    control.set_transferred(done_bytes);

    let pending: Vec<Part> = parts.iter().filter(|p| !p.done).cloned().collect();
    let concurrency = ctx.config.max_parts_per_job.max(1) as usize;

    let results: Vec<PartResult> = futures_util::stream::iter(pending)
        .map(|part| {
            let ctx = &ctx;
            let job = &job;
            let control = control.clone();
            let path = path.clone();
            let upload_id = upload_id.clone();
            async move {
                if control.should_stop() {
                    return PartResult::Stopped;
                }
                let Some(_permit) = ctx.connection().await else {
                    return PartResult::Stopped;
                };
                // 拿到许可的瞬间可能已经被暂停了，再看一眼
                if control.should_stop() {
                    return PartResult::Stopped;
                }
                upload_one(ctx, job, &control, &path, &upload_id, part).await
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let mut etags: Vec<(u32, String)> = parts
        .iter()
        .filter(|p| p.done)
        .filter_map(|p| p.etag.clone().map(|e| (p.number(), e)))
        .collect();
    let mut stopped = false;
    let mut failure = None;
    for result in results {
        match result {
            PartResult::Done { number, etag } => etags.push((number, etag)),
            PartResult::Stopped => stopped = true,
            PartResult::Failed(message) => {
                failure.get_or_insert(message);
            }
        }
    }

    match settle_parts(&control, stopped, failure, &job.id) {
        PartsOutcome::Cancelled => {
            let _ = ctx
                .store
                .abort_multipart(&job.bucket, &job.key, &upload_id)
                .await;
            return JobOutcome::Cancelled;
        }
        PartsOutcome::Paused => return JobOutcome::Paused,
        PartsOutcome::Failed(message) => return JobOutcome::Failed(message),
        PartsOutcome::AllDone => {}
    }

    // 并发上传回来的顺序是乱的，CompleteMultipartUpload 要求按编号升序。
    // s3api 那层也会排一次，但引擎不该指望下游替自己兜底。
    etags.sort_by_key(|(number, _)| *number);

    match ctx
        .store
        .complete_multipart(&job.bucket, &job.key, &upload_id, etags)
        .await
    {
        Ok(etag) => {
            control.set_transferred(size);
            JobOutcome::Done { etag: Some(etag) }
        }
        Err(e) => JobOutcome::Failed(e.to_string()),
    }
}

enum PartResult {
    Done { number: u32, etag: String },
    Stopped,
    Failed(String),
}

async fn upload_one(
    ctx: &TaskContext,
    job: &Job,
    control: &JobControl,
    path: &PathBuf,
    upload_id: &str,
    part: Part,
) -> PartResult {
    let md5 = match md5_of_range(path.clone(), part.offset, part.len).await {
        Ok(md5) => md5,
        Err(e) => return PartResult::Failed(format!("算分片 MD5 失败：{e}")),
    };
    let body = Body::Stream {
        stream: file_part_stream(path.clone(), part.offset, part.len),
        len: part.len,
    };

    match ctx
        .store
        .upload_part(
            &job.bucket,
            &job.key,
            upload_id,
            part.number(),
            body,
            Some(md5),
        )
        .await
    {
        Ok(etag) => {
            let _ = ctx
                .job_store
                .mark_part_done(&job.id, part.idx, Some(&etag));
            control.add_transferred(part.len);
            PartResult::Done {
                number: part.number(),
                etag,
            }
        }
        Err(e) => PartResult::Failed(e.to_string()),
    }
}

/// 空内容的 MD5。
fn md5_empty() -> [u8; 16] {
    use md5::Digest as _;
    md5::Md5::digest([]).into()
}
