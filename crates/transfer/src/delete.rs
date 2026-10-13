//! 批量删除。key 以 `/` 结尾时先递归列举，然后每 1000 个一批。
//!
//! 不做续传：删一半中断了，重跑一遍就是了，已经删掉的再删也不报错。

use crate::driver::TaskContext;
use crate::engine::{JobOutcome, JobRun};

/// DeleteObjects 一次最多 1000 个 key。
const BATCH: usize = 1000;

pub(crate) async fn run(run: JobRun, ctx: TaskContext) -> JobOutcome {
    let JobRun { job, control } = run;

    let keys = if job.key.ends_with('/') {
        let Some(_permit) = ctx.connection().await else {
            return JobOutcome::Paused;
        };
        match ctx.store.list_keys(&job.bucket, &job.key).await {
            Ok(keys) => keys,
            Err(e) => return JobOutcome::Failed(format!("列举要删的对象失败：{e}")),
        }
    } else {
        vec![job.key.clone()]
    };

    if keys.is_empty() {
        return JobOutcome::Done { etag: None };
    }

    // 进度按 key 的个数算，一个 key 记一个单位
    let total = keys.len() as u64;
    let mut deleted = 0u64;
    let mut failures: Vec<String> = Vec::new();

    for chunk in keys.chunks(BATCH) {
        if control.is_cancelled() {
            return JobOutcome::Cancelled;
        }
        if control.is_paused() {
            return JobOutcome::Paused;
        }
        let Some(_permit) = ctx.connection().await else {
            return JobOutcome::Paused;
        };

        match ctx.store.delete_many(&job.bucket, chunk.to_vec()).await {
            Ok(errors) => {
                for error in &errors {
                    failures.push(format!("{}：{}", error.key, error.message));
                }
                deleted += chunk.len() as u64;
                control.set_transferred(deleted.min(total));
            }
            Err(e) => return JobOutcome::Failed(e.to_string()),
        }
    }

    if failures.is_empty() {
        JobOutcome::Done { etag: None }
    } else {
        // 只报前几条，免得错误信息长得没法看
        let shown: Vec<String> = failures.iter().take(3).cloned().collect();
        let more = failures.len().saturating_sub(shown.len());
        let mut message = format!("{} 个对象没删掉：{}", failures.len(), shown.join("；"));
        if more > 0 {
            message.push_str(&format!("；还有 {more} 个"));
        }
        JobOutcome::Failed(message)
    }
}
