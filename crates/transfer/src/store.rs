//! 任务的持久化。队列以数据库为准，进程崩了也能接着传。

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use crate::job::{Batch, HistoryEntry, Job, JobKind, JobState, Part};

/// 历史最多留多少条。超了就从最老的删起，库才不会无限长。
pub const HISTORY_KEEP: u32 = 2000;

/// 对任务记录的部分更新。`None` 表示这个字段不动。
#[derive(Debug, Clone, Default)]
pub struct JobPatch {
    pub state: Option<JobState>,
    pub size: Option<u64>,
    pub part_size: Option<u64>,
    /// 外层 Some 表示要改，内层 None 表示改成空。
    pub upload_id: Option<Option<String>>,
    pub etag: Option<Option<String>>,
    pub error: Option<Option<String>>,
    pub local_mtime: Option<i64>,
}

impl JobPatch {
    pub fn state(state: JobState) -> Self {
        JobPatch {
            state: Some(state),
            ..Default::default()
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        JobPatch {
            state: Some(JobState::Failed),
            error: Some(Some(message.into())),
            ..Default::default()
        }
    }

    pub fn with_state(mut self, state: JobState) -> Self {
        self.state = Some(state);
        self
    }

    pub fn with_size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    pub fn with_part_size(mut self, part_size: u64) -> Self {
        self.part_size = Some(part_size);
        self
    }

    pub fn with_upload_id(mut self, upload_id: Option<String>) -> Self {
        self.upload_id = Some(upload_id);
        self
    }

    pub fn with_etag(mut self, etag: Option<String>) -> Self {
        self.etag = Some(etag);
        self
    }

    pub fn clear_error(mut self) -> Self {
        self.error = Some(None);
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("数据库错误：{0}")]
    Sqlite(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e.to_string())
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// 引擎只通过这个接口落盘，测试可以换成内存实现。
pub trait JobStore: Send + Sync {
    /// 启动时加载所有没跑完的任务。
    fn load_unfinished(&self) -> Result<Vec<(Job, Vec<Part>)>>;
    fn insert_batch(&self, batch: &Batch, jobs: &[Job]) -> Result<()>;
    fn insert_parts(&self, job_id: &str, parts: &[Part]) -> Result<()>;
    fn mark_part_done(&self, job_id: &str, idx: u32, etag: Option<&str>) -> Result<()>;
    fn update_job(&self, job_id: &str, patch: JobPatch) -> Result<()>;
    fn delete_job(&self, job_id: &str) -> Result<()>;
    /// 清掉已完成和已取消的任务。
    fn clear_finished(&self) -> Result<()>;
    /// 记一条传输历史。任务落定时调用。
    fn insert_history(&self, entry: &HistoryEntry) -> Result<()>;
    /// 按落定时间倒序取最近的历史。
    fn load_history(&self, limit: u32) -> Result<Vec<HistoryEntry>>;
    /// 只留最近 `keep` 条历史，老的删掉。
    fn prune_history(&self, keep: u32) -> Result<()>;
    /// 历史整个清空。界面上「清除记录」按的就是这个。
    fn clear_history(&self) -> Result<()>;
}

pub struct SqliteJobStore {
    conn: Mutex<Connection>,
}

impl SqliteJobStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        Self::prepare(&conn)?;
        Ok(SqliteJobStore {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::prepare(&conn)?;
        Ok(SqliteJobStore {
            conn: Mutex::new(conn),
        })
    }

    fn prepare(conn: &Connection) -> Result<()> {
        // WAL 让读写不互相阻塞；NORMAL 在崩溃时最多丢最后一次提交，
        // 而任务状态本来就能靠 ListParts 重建，不值得为它付出每次 fsync 的代价。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS batches(
                id TEXT PRIMARY KEY, kind INTEGER, label TEXT, created_at INTEGER);
             CREATE TABLE IF NOT EXISTS jobs(
                id TEXT PRIMARY KEY, batch_id TEXT, kind INTEGER, profile_id TEXT,
                bucket TEXT, key TEXT, src_key TEXT, local_path TEXT,
                size INTEGER, part_size INTEGER, state INTEGER,
                upload_id TEXT, etag TEXT, local_mtime INTEGER,
                error TEXT, created_at INTEGER, updated_at INTEGER);
             CREATE TABLE IF NOT EXISTS parts(
                job_id TEXT, idx INTEGER, offset INTEGER, len INTEGER,
                done INTEGER, etag TEXT, PRIMARY KEY(job_id, idx));
             CREATE TABLE IF NOT EXISTS history(
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id TEXT, kind INTEGER, bucket TEXT, key TEXT, size INTEGER,
                state INTEGER, error TEXT, created_at INTEGER, finished_at INTEGER);
             CREATE INDEX IF NOT EXISTS jobs_by_state ON jobs(state);
             CREATE INDEX IF NOT EXISTS jobs_by_batch ON jobs(batch_id);
             CREATE INDEX IF NOT EXISTS history_by_finished ON history(finished_at DESC);",
        )?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let local_path: Option<String> = row.get("local_path")?;
    Ok(Job {
        id: row.get("id")?,
        batch_id: row.get("batch_id")?,
        kind: JobKind::from_i64(row.get("kind")?),
        profile_id: row.get("profile_id")?,
        bucket: row.get("bucket")?,
        key: row.get("key")?,
        src_key: row.get("src_key")?,
        local_path: local_path.filter(|p| !p.is_empty()).map(Into::into),
        size: row.get::<_, i64>("size")? as u64,
        part_size: row.get::<_, i64>("part_size")? as u64,
        state: JobState::from_i64(row.get("state")?),
        upload_id: row.get("upload_id")?,
        etag: row.get("etag")?,
        local_mtime: row.get("local_mtime")?,
        error: row.get("error")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

impl JobStore for SqliteJobStore {
    fn load_unfinished(&self) -> Result<Vec<(Job, Vec<Part>)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT * FROM jobs WHERE state NOT IN (?1, ?2) ORDER BY created_at, id",
        )?;
        let jobs: Vec<Job> = stmt
            .query_map(
                params![JobState::Done.as_i64(), JobState::Cancelled.as_i64()],
                row_to_job,
            )?
            .collect::<rusqlite::Result<_>>()?;

        let mut parts_stmt = conn
            .prepare("SELECT idx, offset, len, done, etag FROM parts WHERE job_id = ?1 ORDER BY idx")?;
        let mut out = Vec::with_capacity(jobs.len());
        for job in jobs {
            let parts: Vec<Part> = parts_stmt
                .query_map(params![job.id], |row| {
                    Ok(Part {
                        idx: row.get::<_, i64>(0)? as u32,
                        offset: row.get::<_, i64>(1)? as u64,
                        len: row.get::<_, i64>(2)? as u64,
                        done: row.get::<_, i64>(3)? != 0,
                        etag: row.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            out.push((job, parts));
        }
        Ok(out)
    }

    fn insert_batch(&self, batch: &Batch, jobs: &[Job]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO batches(id, kind, label, created_at) VALUES(?1, ?2, ?3, ?4)",
            params![batch.id, batch.kind.as_i64(), batch.label, batch.created_at],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO jobs(
                    id, batch_id, kind, profile_id, bucket, key, src_key, local_path,
                    size, part_size, state, upload_id, etag, local_mtime, error,
                    created_at, updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            )?;
            for job in jobs {
                stmt.execute(params![
                    job.id,
                    job.batch_id,
                    job.kind.as_i64(),
                    job.profile_id,
                    job.bucket,
                    job.key,
                    job.src_key,
                    job.local_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    job.size as i64,
                    job.part_size as i64,
                    job.state.as_i64(),
                    job.upload_id,
                    job.etag,
                    job.local_mtime,
                    job.error,
                    job.created_at,
                    job.updated_at,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn insert_parts(&self, job_id: &str, parts: &[Part]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO parts(job_id, idx, offset, len, done, etag)
                 VALUES(?1,?2,?3,?4,?5,?6)",
            )?;
            for part in parts {
                stmt.execute(params![
                    job_id,
                    part.idx as i64,
                    part.offset as i64,
                    part.len as i64,
                    part.done as i64,
                    part.etag,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn mark_part_done(&self, job_id: &str, idx: u32, etag: Option<&str>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE parts SET done = 1, etag = ?3 WHERE job_id = ?1 AND idx = ?2",
            params![job_id, idx as i64, etag],
        )?;
        Ok(())
    }

    fn update_job(&self, job_id: &str, patch: JobPatch) -> Result<()> {
        let conn = self.lock();
        let mut sets: Vec<&str> = Vec::new();
        let mut values: Vec<rusqlite::types::Value> = Vec::new();

        if let Some(state) = patch.state {
            sets.push("state");
            values.push(state.as_i64().into());
        }
        if let Some(size) = patch.size {
            sets.push("size");
            values.push((size as i64).into());
        }
        if let Some(part_size) = patch.part_size {
            sets.push("part_size");
            values.push((part_size as i64).into());
        }
        if let Some(upload_id) = patch.upload_id {
            sets.push("upload_id");
            values.push(upload_id.into());
        }
        if let Some(etag) = patch.etag {
            sets.push("etag");
            values.push(etag.into());
        }
        if let Some(error) = patch.error {
            sets.push("error");
            values.push(error.into());
        }
        if let Some(mtime) = patch.local_mtime {
            sets.push("local_mtime");
            values.push(mtime.into());
        }
        if sets.is_empty() {
            return Ok(());
        }

        let assignments: Vec<String> = sets
            .iter()
            .enumerate()
            .map(|(ix, name)| format!("{name} = ?{}", ix + 1))
            .collect();
        let sql = format!(
            "UPDATE jobs SET {}, updated_at = ?{} WHERE id = ?{}",
            assignments.join(", "),
            sets.len() + 1,
            sets.len() + 2
        );
        values.push(crate::job::now().into());
        values.push(job_id.to_owned().into());

        conn.execute(&sql, rusqlite::params_from_iter(values))?;
        Ok(())
    }

    fn delete_job(&self, job_id: &str) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM parts WHERE job_id = ?1", params![job_id])?;
        tx.execute("DELETE FROM jobs WHERE id = ?1", params![job_id])?;
        tx.commit()?;
        Ok(())
    }

    fn clear_finished(&self) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM parts WHERE job_id IN (SELECT id FROM jobs WHERE state IN (?1, ?2))",
            params![JobState::Done.as_i64(), JobState::Cancelled.as_i64()],
        )?;
        tx.execute(
            "DELETE FROM jobs WHERE state IN (?1, ?2)",
            params![JobState::Done.as_i64(), JobState::Cancelled.as_i64()],
        )?;
        tx.execute(
            "DELETE FROM batches WHERE id NOT IN (SELECT DISTINCT batch_id FROM jobs)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn insert_history(&self, entry: &HistoryEntry) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO history(job_id, kind, bucket, key, size, state, error, created_at, finished_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                entry.job_id,
                entry.kind.as_i64(),
                entry.bucket,
                entry.key,
                entry.size as i64,
                entry.state.as_i64(),
                entry.error,
                entry.created_at,
                entry.finished_at,
            ],
        )?;
        Ok(())
    }

    fn load_history(&self, limit: u32) -> Result<Vec<HistoryEntry>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT job_id, kind, bucket, key, size, state, error, created_at, finished_at
             FROM history ORDER BY finished_at DESC, seq DESC LIMIT ?1",
        )?;
        let entries = stmt
            .query_map(params![limit as i64], |row| {
                Ok(HistoryEntry {
                    job_id: row.get(0)?,
                    kind: JobKind::from_i64(row.get(1)?),
                    bucket: row.get(2)?,
                    key: row.get(3)?,
                    size: row.get::<_, i64>(4)? as u64,
                    state: JobState::from_i64(row.get(5)?),
                    error: row.get(6)?,
                    created_at: row.get(7)?,
                    finished_at: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(entries)
    }

    fn prune_history(&self, keep: u32) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM history WHERE seq NOT IN (
                SELECT seq FROM history ORDER BY finished_at DESC, seq DESC LIMIT ?1)",
            params![keep as i64],
        )?;
        Ok(())
    }

    fn clear_history(&self) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM history", [])?;
        Ok(())
    }
}

impl SqliteJobStore {
    /// 只在测试里用：按 id 读一条任务。
    pub fn job(&self, job_id: &str) -> Result<Option<Job>> {
        let conn = self.lock();
        let job = conn
            .query_row("SELECT * FROM jobs WHERE id = ?1", params![job_id], row_to_job)
            .optional()?;
        Ok(job)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::split_parts;

    fn sample_job(batch: &Batch) -> Job {
        Job::new(&batch.id, JobKind::Upload, "p1", "bucket", "dir/file.bin")
            .with_local("C:/tmp/file.bin".into(), 20, 1700)
    }

    #[test]
    fn round_trips_a_batch_with_parts() {
        let store = SqliteJobStore::in_memory().unwrap();
        let batch = Batch::new(JobKind::Upload, "上传 1 个文件");
        let job = sample_job(&batch);
        let (part_size, parts) = split_parts(job.size, 8);

        store.insert_batch(&batch, std::slice::from_ref(&job)).unwrap();
        store
            .update_job(&job.id, JobPatch::default().with_part_size(part_size))
            .unwrap();
        store.insert_parts(&job.id, &parts).unwrap();

        let loaded = store.load_unfinished().unwrap();
        assert_eq!(loaded.len(), 1);
        let (loaded_job, loaded_parts) = &loaded[0];
        assert_eq!(loaded_job.id, job.id);
        assert_eq!(loaded_job.key, "dir/file.bin");
        assert_eq!(loaded_job.part_size, 8);
        assert_eq!(loaded_job.local_path.as_deref(), Some(Path::new("C:/tmp/file.bin")));
        assert_eq!(loaded_parts.len(), 3);
        assert!(loaded_parts.iter().all(|p| !p.done));
    }

    #[test]
    fn marking_a_part_done_survives_reload() {
        let store = SqliteJobStore::in_memory().unwrap();
        let batch = Batch::new(JobKind::Upload, "x");
        let job = sample_job(&batch);
        let (_, parts) = split_parts(job.size, 8);
        store.insert_batch(&batch, std::slice::from_ref(&job)).unwrap();
        store.insert_parts(&job.id, &parts).unwrap();

        store.mark_part_done(&job.id, 1, Some("etag-1")).unwrap();

        let loaded = store.load_unfinished().unwrap();
        let parts = &loaded[0].1;
        assert!(!parts[0].done);
        assert!(parts[1].done);
        assert_eq!(parts[1].etag.as_deref(), Some("etag-1"));
    }

    #[test]
    fn patch_only_touches_named_fields() {
        let store = SqliteJobStore::in_memory().unwrap();
        let batch = Batch::new(JobKind::Upload, "x");
        let job = sample_job(&batch);
        store.insert_batch(&batch, std::slice::from_ref(&job)).unwrap();

        store
            .update_job(&job.id, JobPatch::default().with_upload_id(Some("up-1".into())))
            .unwrap();
        store.update_job(&job.id, JobPatch::state(JobState::Running)).unwrap();

        let stored = store.job(&job.id).unwrap().unwrap();
        assert_eq!(stored.upload_id.as_deref(), Some("up-1"));
        assert_eq!(stored.state, JobState::Running);
        assert_eq!(stored.key, "dir/file.bin");

        // 失败带上原因，重试时再清掉
        store.update_job(&job.id, JobPatch::failed("网络断了")).unwrap();
        let stored = store.job(&job.id).unwrap().unwrap();
        assert_eq!(stored.state, JobState::Failed);
        assert_eq!(stored.error.as_deref(), Some("网络断了"));

        store
            .update_job(&job.id, JobPatch::state(JobState::Queued).clear_error())
            .unwrap();
        let stored = store.job(&job.id).unwrap().unwrap();
        assert!(stored.error.is_none());
    }

    #[test]
    fn finished_jobs_are_not_loaded_and_can_be_cleared() {
        let store = SqliteJobStore::in_memory().unwrap();
        let batch = Batch::new(JobKind::Upload, "x");
        let done = sample_job(&batch);
        let running = sample_job(&batch);
        store.insert_batch(&batch, &[done.clone(), running.clone()]).unwrap();
        store.insert_parts(&done.id, &split_parts(20, 8).1).unwrap();
        store.update_job(&done.id, JobPatch::state(JobState::Done)).unwrap();

        let loaded = store.load_unfinished().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0.id, running.id);

        store.clear_finished().unwrap();
        assert!(store.job(&done.id).unwrap().is_none());
        assert!(store.job(&running.id).unwrap().is_some());
    }

    #[test]
    fn deleting_a_job_takes_its_parts() {
        let store = SqliteJobStore::in_memory().unwrap();
        let batch = Batch::new(JobKind::Upload, "x");
        let job = sample_job(&batch);
        store.insert_batch(&batch, std::slice::from_ref(&job)).unwrap();
        store.insert_parts(&job.id, &split_parts(20, 8).1).unwrap();

        store.delete_job(&job.id).unwrap();

        assert!(store.job(&job.id).unwrap().is_none());
        assert!(store.load_unfinished().unwrap().is_empty());
    }

    #[test]
    fn reopening_the_same_file_keeps_jobs() {
        let dir = std::env::temp_dir().join(format!("s3b-store-{}", fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transfers.db");

        let batch = Batch::new(JobKind::Upload, "x");
        let job = {
            let store = SqliteJobStore::open(&path).unwrap();
            let job = sample_job(&batch);
            store.insert_batch(&batch, std::slice::from_ref(&job)).unwrap();
            store.insert_parts(&job.id, &split_parts(20, 8).1).unwrap();
            store.mark_part_done(&job.id, 0, Some("e0")).unwrap();
            job
        };

        let store = SqliteJobStore::open(&path).unwrap();
        let loaded = store.load_unfinished().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0.id, job.id);
        assert!(loaded[0].1[0].done);

        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn history_entry(job: &Job, state: JobState, finished_at: i64) -> HistoryEntry {
        HistoryEntry {
            job_id: job.id.clone(),
            kind: job.kind,
            bucket: job.bucket.clone(),
            key: job.key.clone(),
            size: job.size,
            state,
            error: None,
            created_at: job.created_at,
            finished_at,
        }
    }

    #[test]
    fn history_survives_reopen_and_loads_newest_first() {
        let dir = std::env::temp_dir().join(format!("s3b-hist-{}", fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transfers.db");

        let batch = Batch::new(JobKind::Upload, "x");
        let job = sample_job(&batch);
        {
            let store = SqliteJobStore::open(&path).unwrap();
            store.insert_history(&history_entry(&job, JobState::Done, 100)).unwrap();
            store.insert_history(&history_entry(&job, JobState::Failed, 300)).unwrap();
            store.insert_history(&history_entry(&job, JobState::Done, 200)).unwrap();
        }

        // 重新打开还在，按落定时间倒序
        let store = SqliteJobStore::open(&path).unwrap();
        let entries = store.load_history(10).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].finished_at, 300);
        assert_eq!(entries[0].state, JobState::Failed);
        assert_eq!(entries[1].finished_at, 200);
        assert_eq!(entries[2].finished_at, 100);
        assert_eq!(entries[0].display_name(), "file.bin");

        store.prune_history(2).unwrap();
        assert_eq!(store.load_history(10).unwrap().len(), 2);
        // 删的是最老的
        assert_eq!(store.load_history(10).unwrap()[1].finished_at, 200);

        // 清空是全删，不分新旧
        store.clear_history().unwrap();
        assert!(store.load_history(10).unwrap().is_empty());

        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }
}
