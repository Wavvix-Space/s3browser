//! 服务端复制。不超过 5 GiB 一次 CopyObject，更大的走 UploadPartCopy 分片。
//!
//! key 以 `/` 结尾时是整个目录：像删除那样先递归列举，再逐个搬。一个目录
//! 一条任务，不是每个对象一条——十万个对象的目录不该在队列里铺十万行。

use futures_util::StreamExt;
use s3api::ByteRange;

use crate::driver::{PartsOutcome, TaskContext, settle_parts};
use crate::engine::{JobControl, JobOutcome, JobRun};
use crate::job::{Job, JobKind, Part, split_parts};
use crate::object_store::is_no_such_upload;
use crate::store::JobPatch;

/// 阿里云 OSS 的 CopyObject 上限，超过就得分片复制。
const COPY_LIMIT: u64 = 5 * 1024 * 1024 * 1024;
/// 分片复制时每片的大小。
const COPY_PART_SIZE: u64 = 1024 * 1024 * 1024;
/// DeleteObjects 一次最多 1000 个 key。
const DELETE_BATCH: usize = 1000;

pub(crate) async fn run(run: JobRun, ctx: TaskContext) -> JobOutcome {
    let JobRun { job, control } = run;

    // 目标 key 带尾斜杠就是整个目录，底下每个对象都要搬一遍
    if job.key.ends_with('/') {
        return run_folder(&job, &control, &ctx).await;
    }

    if job.size <= COPY_LIMIT {
        let Some(_permit) = ctx.connection().await else {
            return JobOutcome::Paused;
        };
        return match ctx
            .store
            .copy(&job.bucket, &job.key, &job.bucket, &job.src_key)
            .await
        {
            Ok(etag) => {
                control.set_transferred(job.size);
                finish_move(&ctx, &job, etag).await
            }
            Err(e) => JobOutcome::Failed(e.to_string()),
        };
    }

    let (part_size, mut parts) = split_parts(job.size, COPY_PART_SIZE);
    let mut upload_id = job.upload_id.clone();

    // 续传逻辑和上传一样，以服务端的 ListParts 为准
    if let Some(id) = &upload_id {
        match ctx.store.list_parts(&job.bucket, &job.key, id).await {
            Ok(remote) => {
                for part in parts.iter_mut() {
                    if let Some(found) = remote
                        .iter()
                        .find(|r| r.number == part.number() && r.size == part.len)
                    {
                        part.done = true;
                        part.etag = Some(found.etag.clone());
                    }
                }
            }
            Err(e) if is_no_such_upload(&e) => upload_id = None,
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

    let _ = ctx.job_store.update_job(
        &job.id,
        JobPatch::default()
            .with_part_size(part_size)
            .with_upload_id(Some(upload_id.clone())),
    );
    let _ = ctx.job_store.insert_parts(&job.id, &parts);
    control.set_transferred(parts.iter().filter(|p| p.done).map(|p| p.len).sum());

    let pending: Vec<Part> = parts.iter().filter(|p| !p.done).cloned().collect();
    let concurrency = ctx.config.max_parts_per_job.max(1) as usize;

    let results: Vec<Result<(u32, String), Option<String>>> = futures_util::stream::iter(pending)
        .map(|part| {
            let ctx = &ctx;
            let job = &job;
            let control = control.clone();
            let upload_id = upload_id.clone();
            async move {
                if control.should_stop() {
                    return Err(None);
                }
                let Some(_permit) = ctx.connection().await else {
                    return Err(None);
                };
                match ctx
                    .store
                    .upload_part_copy(
                        &job.bucket,
                        &job.key,
                        &upload_id,
                        part.number(),
                        &job.bucket,
                        &job.src_key,
                        Some(ByteRange::new(part.offset, part.end())),
                    )
                    .await
                {
                    Ok(etag) => {
                        let _ = ctx.job_store.mark_part_done(&job.id, part.idx, Some(&etag));
                        control.add_transferred(part.len);
                        Ok((part.number(), etag))
                    }
                    Err(e) => Err(Some(e.to_string())),
                }
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
            Ok(entry) => etags.push(entry),
            Err(None) => stopped = true,
            Err(Some(message)) => {
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

    match ctx
        .store
        .complete_multipart(&job.bucket, &job.key, &upload_id, etags)
        .await
    {
        Ok(etag) => {
            control.set_transferred(job.size);
            finish_move(&ctx, &job, etag).await
        }
        Err(e) => JobOutcome::Failed(e.to_string()),
    }
}

/// 移动任务在复制成功后才删源，复制没成就什么都不动。
/// 整个目录的复制或移动。
///
/// 先列一遍源前缀底下的对象，逐个服务端复制到新前缀，移动的话全部复制完
/// 再删源——中途失败源还在，重跑一遍就是（已经复制过的会被覆盖成一样的内容）。
///
/// 进度按对象个数算：逐个 HEAD 一遍才知道字节数，为了个进度条多打一轮
/// 请求不值。删除那边也是这么记的。
async fn run_folder(job: &Job, control: &JobControl, ctx: &TaskContext) -> JobOutcome {
    let keys = {
        let Some(_permit) = ctx.connection().await else {
            return JobOutcome::Paused;
        };
        match ctx.store.list_keys(&job.bucket, &job.src_key).await {
            Ok(keys) => keys,
            Err(e) => return JobOutcome::Failed(format!("列举要搬的对象失败：{e}")),
        }
    };

    if keys.is_empty() {
        return JobOutcome::Done { etag: None };
    }

    let mut moved = 0u64;
    for src in &keys {
        if control.is_cancelled() {
            return JobOutcome::Cancelled;
        }
        if control.is_paused() {
            return JobOutcome::Paused;
        }
        let Some(_permit) = ctx.connection().await else {
            return JobOutcome::Paused;
        };

        // src 一定以 src_key 打头，剩下那截原样接到新前缀后面，
        // 子目录的层级就这么保住了
        let rest = src.strip_prefix(&job.src_key).unwrap_or(src.as_str());
        let dst = format!("{}{rest}", job.key);
        if let Err(e) = ctx.store.copy(&job.bucket, &dst, &job.bucket, src).await {
            return JobOutcome::Failed(format!("{src}：{e}"));
        }
        moved += 1;
        control.set_transferred(moved);
    }

    if job.kind != JobKind::Move {
        return JobOutcome::Done { etag: None };
    }

    // 全都复制好了才删源
    let mut failures = 0usize;
    for chunk in keys.chunks(DELETE_BATCH) {
        let Some(_permit) = ctx.connection().await else {
            return JobOutcome::Paused;
        };
        match ctx.store.delete_many(&job.bucket, chunk.to_vec()).await {
            Ok(errors) => failures += errors.len(),
            Err(e) => return JobOutcome::Failed(format!("搬完了但删源失败：{e}")),
        }
    }
    if failures > 0 {
        return JobOutcome::Failed(format!("搬完了但有 {failures} 个源对象没删掉"));
    }
    JobOutcome::Done { etag: None }
}

async fn finish_move(ctx: &TaskContext, job: &Job, etag: String) -> JobOutcome {
    if job.kind != JobKind::Move {
        return JobOutcome::Done { etag: Some(etag) };
    }
    match ctx
        .store
        .delete_many(&job.bucket, vec![job.src_key.clone()])
        .await
    {
        Ok(errors) if errors.is_empty() => JobOutcome::Done { etag: Some(etag) },
        Ok(errors) => JobOutcome::Failed(format!(
            "复制成功但源对象没删掉：{}",
            errors
                .first()
                .map(|e| e.message.clone())
                .unwrap_or_default()
        )),
        Err(e) => JobOutcome::Failed(format!("复制成功但删源失败：{e}")),
    }
}
