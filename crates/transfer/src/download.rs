//! 下载。写临时文件，全部分块到齐后再原子改名，中途中断不会留下半个文件。

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use s3api::ByteRange;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::driver::{PartsOutcome, TaskContext, md5_of_file, settle_parts, temp_path};
use crate::engine::{JobControl, JobOutcome, JobRun};
use crate::job::{Job, Part, split_parts};
use crate::object_store::is_precondition_failed;
use crate::store::JobPatch;

pub(crate) async fn run(run: JobRun, ctx: TaskContext) -> JobOutcome {
    let JobRun { job, control } = run;
    let Some(target) = job.local_path.clone() else {
        return JobOutcome::Failed("任务没有本地路径".to_owned());
    };

    let head = match ctx.store.head(&job.bucket, &job.key).await {
        Ok(head) => head,
        Err(e) => return JobOutcome::Failed(e.to_string()),
    };
    let size = head.size;
    let etag = head.etag.clone();

    // 对象在上次中断后被换过，之前下的字节都不能要了
    let object_changed = job.etag.is_some() && job.etag != etag;
    if object_changed {
        tracing::info!("{} 在服务端被改过，重新下载", job.key);
    }

    if let Some(dir) = target.parent()
        && let Err(e) = tokio::fs::create_dir_all(dir).await
    {
        return JobOutcome::Failed(format!("建目录失败：{e}"));
    }

    let temp = temp_path(&target);
    let (part_size, mut parts) = split_parts(size, ctx.config.download_chunk);

    // 续传只信数据库里的完成标记：写盘和落库之间崩掉的块重下一遍
    let resumable = !object_changed
        && job.size == size
        && tokio::fs::try_exists(&temp).await.unwrap_or(false);
    if resumable {
        let stored = ctx
            .job_store
            .load_unfinished()
            .ok()
            .and_then(|all| all.into_iter().find(|(j, _)| j.id == job.id))
            .map(|(_, parts)| parts)
            .unwrap_or_default();
        for part in parts.iter_mut() {
            if stored
                .iter()
                .any(|s| s.idx == part.idx && s.len == part.len && s.done)
            {
                part.done = true;
            }
        }
    }

    if let Err(e) = prepare_temp_file(&temp, size, resumable).await {
        return JobOutcome::Failed(format!("准备临时文件失败：{e}"));
    }

    let _ = ctx.job_store.update_job(
        &job.id,
        JobPatch::default()
            .with_size(size)
            .with_part_size(part_size)
            .with_etag(etag.clone()),
    );
    let _ = ctx.job_store.insert_parts(&job.id, &parts);

    control.set_transferred(parts.iter().filter(|p| p.done).map(|p| p.len).sum());

    let pending: Vec<Part> = parts.iter().filter(|p| !p.done).cloned().collect();
    let concurrency = ctx.config.max_parts_per_job.max(1) as usize;
    let single_shot = size <= ctx.config.download_chunk;

    let results: Vec<ChunkResult> = futures_util::stream::iter(pending)
        .map(|part| {
            let ctx = &ctx;
            let job = &job;
            let control = control.clone();
            let temp = temp.clone();
            let etag = etag.clone();
            async move {
                if control.should_stop() {
                    return ChunkResult::Stopped;
                }
                let Some(_permit) = ctx.connection().await else {
                    return ChunkResult::Stopped;
                };
                if control.should_stop() {
                    return ChunkResult::Stopped;
                }
                download_one(ctx, job, &control, &temp, etag.as_deref(), part, single_shot).await
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let mut stopped = false;
    let mut failure = None;
    for result in results {
        match result {
            ChunkResult::Done => {}
            ChunkResult::Stopped => stopped = true,
            ChunkResult::Failed(message) => {
                failure.get_or_insert(message);
            }
        }
    }

    match settle_parts(&control, stopped, failure, &job.id) {
        PartsOutcome::Cancelled => {
            let _ = tokio::fs::remove_file(&temp).await;
            return JobOutcome::Cancelled;
        }
        PartsOutcome::Paused => return JobOutcome::Paused,
        PartsOutcome::Failed(message) => return JobOutcome::Failed(message),
        PartsOutcome::AllDone => {}
    }

    if let Err(e) = finish(&ctx, &temp, &target, size, etag.as_deref()).await {
        return JobOutcome::Failed(e);
    }
    control.set_transferred(size);
    JobOutcome::Done { etag }
}

/// 续传时保留已有内容，否则重建。预先撑到目标大小，后面各块按偏移写。
async fn prepare_temp_file(temp: &Path, size: u64, resume: bool) -> std::io::Result<()> {
    if !resume {
        let _ = tokio::fs::remove_file(temp).await;
    }
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(temp)
        .await?;
    file.set_len(size).await?;
    Ok(())
}

enum ChunkResult {
    Done,
    Stopped,
    Failed(String),
}

async fn download_one(
    ctx: &TaskContext,
    job: &Job,
    control: &JobControl,
    temp: &Path,
    etag: Option<&str>,
    part: Part,
    single_shot: bool,
) -> ChunkResult {
    // 整个文件就一块时不带 Range，省掉一次 206 往返
    let range = if single_shot {
        None
    } else {
        Some(ByteRange::new(part.offset, part.end()))
    };

    let mut stream = match ctx
        .store
        .get_range(&job.bucket, &job.key, range, etag)
        .await
    {
        Ok(stream) => stream,
        Err(e) if is_precondition_failed(&e) => {
            return ChunkResult::Failed("对象在下载过程中被改过，请重新下载".to_owned());
        }
        Err(e) => return ChunkResult::Failed(e.to_string()),
    };

    let mut file = match tokio::fs::OpenOptions::new().write(true).open(temp).await {
        Ok(file) => file,
        Err(e) => return ChunkResult::Failed(format!("打开临时文件失败：{e}")),
    };
    if let Err(e) = file.seek(std::io::SeekFrom::Start(part.offset)).await {
        return ChunkResult::Failed(format!("定位失败：{e}"));
    }

    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        // 每读一段就看一眼。只在开始这一块之前查的话，8 MiB 一块意味着按下暂停
        // 之后半天没动静，还可能在这期间撞上个网络错误被当成传失败。
        // 半截数据不要紧：这一块没标记完成，下次整块重下。
        if control.should_stop() {
            return ChunkResult::Stopped;
        }
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => return ChunkResult::Failed(format!("读响应失败：{e}")),
        };
        if let Err(e) = file.write_all(&chunk).await {
            return ChunkResult::Failed(format!("写文件失败：{e}"));
        }
        written += chunk.len() as u64;
        control.add_transferred(chunk.len() as u64);
    }
    if let Err(e) = file.flush().await {
        return ChunkResult::Failed(format!("刷盘失败：{e}"));
    }

    if written != part.len {
        return ChunkResult::Failed(format!(
            "第 {} 块只收到 {written} 字节，应为 {}",
            part.number(),
            part.len
        ));
    }

    // 先落盘再落库：反过来的话崩溃后会以为这块好了
    let _ = ctx.job_store.mark_part_done(&job.id, part.idx, None);
    ChunkResult::Done
}

async fn finish(
    ctx: &TaskContext,
    temp: &Path,
    target: &Path,
    size: u64,
    etag: Option<&str>,
) -> Result<(), String> {
    {
        // 必须用写模式打开：Windows 上对只读句柄调 FlushFileBuffers 会返回拒绝访问
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(temp)
            .await
            .map_err(|e| format!("打开临时文件失败：{e}"))?;
        file.sync_all().await.map_err(|e| format!("同步失败：{e}"))?;
    }

    let actual = tokio::fs::metadata(temp)
        .await
        .map(|m| m.len())
        .unwrap_or_default();
    if actual != size {
        return Err(format!("下载不完整：{actual} 字节，应为 {size}"));
    }

    // 分片上传的对象 ETag 形如 "<md5>-<片数>"，那不是内容的 MD5，跳过校验
    if ctx.config.verify_download_md5
        && let Some(etag) = etag
        && is_plain_md5(etag)
    {
        let digest = md5_of_file(temp.to_path_buf())
            .await
            .map_err(|e| format!("校验失败：{e}"))?;
        if hex_lower(&digest) != etag.to_ascii_lowercase() {
            return Err("下载内容与服务端 MD5 不符".to_owned());
        }
    }

    tokio::fs::rename(temp, target)
        .await
        .map_err(|e| format!("改名失败：{e}"))
}

fn is_plain_md5(etag: &str) -> bool {
    etag.len() == 32 && etag.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 目标文件已存在时按 Finder 的规矩取一个不冲突的名字。
pub fn unique_path(target: &Path) -> PathBuf {
    if !target.exists() {
        return target.to_path_buf();
    }
    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = target.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 2..1000 {
        let name = match &ext {
            Some(ext) => format!("{stem} {n}.{ext}"),
            None => format!("{stem} {n}"),
        };
        let candidate = target.with_file_name(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    target.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_plain_md5_etags() {
        assert!(is_plain_md5("d41d8cd98f00b204e9800998ecf8427e"));
        // 分片上传的 ETag 带片数后缀，不能拿来比对内容
        assert!(!is_plain_md5("3858f62230ac3c915f300c664312c11f-9"));
        assert!(!is_plain_md5("短的"));
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex_lower(&[0x0a, 0xff, 0x00]), "0aff00");
    }

    #[test]
    fn unique_path_appends_numbers() {
        let dir = std::env::temp_dir().join(format!("s3b-dl-{}", fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();

        let target = dir.join("report.pdf");
        assert_eq!(unique_path(&target), target);

        std::fs::write(&target, b"x").unwrap();
        assert_eq!(unique_path(&target), dir.join("report 2.pdf"));

        std::fs::write(dir.join("report 2.pdf"), b"x").unwrap();
        assert_eq!(unique_path(&target), dir.join("report 3.pdf"));

        // 没有扩展名也要能处理
        let noext = dir.join("LICENSE");
        std::fs::write(&noext, b"x").unwrap();
        assert_eq!(unique_path(&noext), dir.join("LICENSE 2"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
