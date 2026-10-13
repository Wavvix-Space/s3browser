//! 用假 ObjectStore 驱动引擎，覆盖真实网络很难复现的那些情况：
//! 服务端已有分片、NoSuchUpload、本地文件被改、412、暂停、取消。

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use s3api::{Body, ByteRange, DeleteError, HeadResult, PartInfo, S3Error};
use transfer::engine::{Engine, EngineConfig, EngineEvent};
use transfer::job::{Batch, Job, JobKind, JobProgress, JobState};
use transfer::object_store::{ObjectStore, StoreResolver, StoreResult};
use transfer::store::{JobStore, SqliteJobStore};

// ---------- 假存储 ----------

#[derive(Default)]
struct Inner {
    /// key -> 内容
    objects: HashMap<String, Vec<u8>>,
    /// upload_id -> (key, 已上传分片)
    uploads: HashMap<String, (String, Vec<PartInfo>)>,
    calls: Vec<String>,
    /// 下一次 upload_part 要失败的分片编号
    fail_part: Option<u32>,
    /// create_multipart 之外的操作是否返回 NoSuchUpload
    no_such_upload: bool,
    /// get_range 是否返回 412
    precondition_failed: bool,
    deleted: Vec<String>,
    /// 每个分片故意慢一点，测试才来得及发暂停命令
    delay_ms: u64,
}

#[derive(Default)]
struct FakeStore {
    inner: Mutex<Inner>,
    next_upload: AtomicU32,
    /// 上传第一个分片时触发，用来在传输中途暂停
    pause_hook: Mutex<Option<Box<dyn Fn() + Send>>>,
    hook_fired: AtomicBool,
}

impl FakeStore {
    fn new() -> Arc<Self> {
        Arc::new(FakeStore::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, call: impl Into<String>) {
        self.lock().calls.push(call.into());
    }

    fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    fn count(&self, prefix: &str) -> usize {
        self.calls().iter().filter(|c| c.starts_with(prefix)).count()
    }

    fn put_object(&self, key: &str, data: Vec<u8>) {
        self.lock().objects.insert(key.to_owned(), data);
    }

    fn object(&self, key: &str) -> Option<Vec<u8>> {
        self.lock().objects.get(key).cloned()
    }

    /// 预置一个进行中的分片上传，模拟上次没传完。
    fn seed_upload(&self, upload_id: &str, key: &str, parts: Vec<PartInfo>) {
        self.lock()
            .uploads
            .insert(upload_id.to_owned(), (key.to_owned(), parts));
    }

    fn set_fail_part(&self, number: u32) {
        self.lock().fail_part = Some(number);
    }

    fn set_no_such_upload(&self, value: bool) {
        self.lock().no_such_upload = value;
    }

    fn set_precondition_failed(&self, value: bool) {
        self.lock().precondition_failed = value;
    }

    fn set_delay(&self, ms: u64) {
        self.lock().delay_ms = ms;
    }

    fn on_first_part(&self, hook: impl Fn() + Send + 'static) {
        *self.pause_hook.lock().unwrap() = Some(Box::new(hook));
    }
}

fn service_error(status: u16, code: &str) -> S3Error {
    S3Error::Service {
        status,
        code: code.to_owned(),
        message: code.to_owned(),
        request_id: None,
    }
}

async fn collect_body(body: Body) -> Vec<u8> {
    use futures_util::StreamExt;
    match body {
        Body::Empty => Vec::new(),
        Body::Bytes(b) => b.to_vec(),
        Body::Stream { mut stream, .. } => {
            let mut out = Vec::new();
            while let Some(chunk) = stream.next().await {
                out.extend_from_slice(&chunk.expect("读请求体"));
            }
            out
        }
    }
}

impl ObjectStore for FakeStore {
    fn head(&self, _bucket: &str, key: &str) -> BoxFuture<'_, StoreResult<HeadResult>> {
        let key = key.to_owned();
        Box::pin(async move {
            self.record(format!("head:{key}"));
            let data = self
                .lock()
                .objects
                .get(&key)
                .cloned()
                .ok_or_else(|| service_error(404, "NoSuchKey"))?;
            Ok(HeadResult {
                size: data.len() as u64,
                modified: 0,
                etag: Some(fake_etag(&data)),
                content_type: None,
                storage_class: None,
            })
        })
    }

    fn get_range(
        &self,
        _bucket: &str,
        key: &str,
        range: Option<ByteRange>,
        _if_match: Option<&str>,
    ) -> BoxFuture<'_, StoreResult<BoxStream<'static, io::Result<Bytes>>>> {
        let key = key.to_owned();
        Box::pin(async move {
            self.record(format!("get:{key}:{:?}", range.map(|r| (r.start, r.end))));
            if self.lock().precondition_failed {
                return Err(service_error(412, "PreconditionFailed"));
            }
            let data = self
                .lock()
                .objects
                .get(&key)
                .cloned()
                .ok_or_else(|| service_error(404, "NoSuchKey"))?;
            let slice = match range {
                Some(r) => {
                    let end = (r.end as usize + 1).min(data.len());
                    data[r.start as usize..end].to_vec()
                }
                None => data,
            };
            // 切成两块发，顺便验证调用方能拼起来。两块之间按 delay_ms 停一下：
            // 调用方能不能在一块的中途收手，要靠这个空档才测得出来。
            let mid = slice.len() / 2;
            let first = Bytes::from(slice[..mid].to_vec());
            let second = Bytes::from(slice[mid..].to_vec());
            let delay = self.lock().delay_ms;
            let stream = futures_util::stream::once(async move { Ok(first) }).chain(
                futures_util::stream::once(async move {
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    Ok(second)
                }),
            );
            Ok(Box::pin(stream) as BoxStream<'static, io::Result<Bytes>>)
        })
    }

    fn put(
        &self,
        _bucket: &str,
        key: &str,
        body: Body,
        _md5: Option<[u8; 16]>,
    ) -> BoxFuture<'_, StoreResult<String>> {
        let key = key.to_owned();
        Box::pin(async move {
            let data = collect_body(body).await;
            self.record(format!("put:{key}:{}", data.len()));
            let etag = fake_etag(&data);
            self.lock().objects.insert(key, data);
            Ok(etag)
        })
    }

    fn create_multipart(&self, _bucket: &str, key: &str) -> BoxFuture<'_, StoreResult<String>> {
        let key = key.to_owned();
        Box::pin(async move {
            let id = format!("up-{}", self.next_upload.fetch_add(1, Ordering::Relaxed));
            self.record(format!("create:{key}:{id}"));
            self.lock().uploads.insert(id.clone(), (key, Vec::new()));
            Ok(id)
        })
    }

    fn upload_part(
        &self,
        _bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: u32,
        body: Body,
        _md5: Option<[u8; 16]>,
    ) -> BoxFuture<'_, StoreResult<String>> {
        let (key, upload_id) = (key.to_owned(), upload_id.to_owned());
        Box::pin(async move {
            let data = collect_body(body).await;
            self.record(format!("part:{key}:{upload_id}:{part_number}:{}", data.len()));

            let delay = self.lock().delay_ms;
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }

            if !self.hook_fired.swap(true, Ordering::Relaxed)
                && let Some(hook) = self.pause_hook.lock().unwrap().as_ref()
            {
                hook();
            }

            if self.lock().fail_part == Some(part_number) {
                return Err(service_error(500, "InternalError"));
            }
            let etag = fake_etag(&data);
            let mut inner = self.lock();
            if let Some((_, parts)) = inner.uploads.get_mut(&upload_id) {
                parts.push(PartInfo {
                    number: part_number,
                    size: data.len() as u64,
                    etag: etag.clone(),
                });
            } else {
                return Err(service_error(404, "NoSuchUpload"));
            }
            Ok(etag)
        })
    }

    fn upload_part_copy(
        &self,
        _bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: u32,
        _src_bucket: &str,
        _src_key: &str,
        _range: Option<ByteRange>,
    ) -> BoxFuture<'_, StoreResult<String>> {
        let (key, upload_id) = (key.to_owned(), upload_id.to_owned());
        Box::pin(async move {
            self.record(format!("partcopy:{key}:{upload_id}:{part_number}"));
            Ok(format!("etag-copy-{part_number}"))
        })
    }

    fn list_parts(
        &self,
        _bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> BoxFuture<'_, StoreResult<Vec<PartInfo>>> {
        let (key, upload_id) = (key.to_owned(), upload_id.to_owned());
        Box::pin(async move {
            self.record(format!("listparts:{key}:{upload_id}"));
            if self.lock().no_such_upload {
                return Err(service_error(404, "NoSuchUpload"));
            }
            match self.lock().uploads.get(&upload_id) {
                Some((_, parts)) => Ok(parts.clone()),
                None => Err(service_error(404, "NoSuchUpload")),
            }
        })
    }

    fn complete_multipart(
        &self,
        _bucket: &str,
        key: &str,
        upload_id: &str,
        parts: Vec<(u32, String)>,
    ) -> BoxFuture<'_, StoreResult<String>> {
        let (key, upload_id) = (key.to_owned(), upload_id.to_owned());
        Box::pin(async move {
            let numbers: Vec<String> = parts.iter().map(|(n, _)| n.to_string()).collect();
            self.record(format!("complete:{key}:{upload_id}:[{}]", numbers.join(",")));
            let mut inner = self.lock();
            let Some((_, stored)) = inner.uploads.remove(&upload_id) else {
                return Err(service_error(404, "NoSuchUpload"));
            };
            let total: u64 = stored.iter().map(|p| p.size).sum();
            inner.objects.insert(key, vec![0u8; total as usize]);
            Ok(format!("final-{}-{}", total, parts.len()))
        })
    }

    fn abort_multipart(
        &self,
        _bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> BoxFuture<'_, StoreResult<()>> {
        let (key, upload_id) = (key.to_owned(), upload_id.to_owned());
        Box::pin(async move {
            self.record(format!("abort:{key}:{upload_id}"));
            self.lock().uploads.remove(&upload_id);
            Ok(())
        })
    }

    fn copy(
        &self,
        _bucket: &str,
        key: &str,
        _src_bucket: &str,
        src_key: &str,
    ) -> BoxFuture<'_, StoreResult<String>> {
        let (key, src_key) = (key.to_owned(), src_key.to_owned());
        Box::pin(async move {
            self.record(format!("copy:{src_key}->{key}"));
            let data = self
                .lock()
                .objects
                .get(&src_key)
                .cloned()
                .ok_or_else(|| service_error(404, "NoSuchKey"))?;
            let etag = fake_etag(&data);
            self.lock().objects.insert(key, data);
            Ok(etag)
        })
    }

    fn list_keys(&self, _bucket: &str, prefix: &str) -> BoxFuture<'_, StoreResult<Vec<String>>> {
        let prefix = prefix.to_owned();
        Box::pin(async move {
            self.record(format!("list:{prefix}"));
            let keys: Vec<String> = self
                .lock()
                .objects
                .keys()
                .filter(|k| k.starts_with(&prefix))
                .cloned()
                .collect();
            Ok(keys)
        })
    }

    fn delete_many(
        &self,
        _bucket: &str,
        keys: Vec<String>,
    ) -> BoxFuture<'_, StoreResult<Vec<DeleteError>>> {
        Box::pin(async move {
            self.record(format!("delete:{}", keys.len()));
            let mut inner = self.lock();
            for key in &keys {
                inner.objects.remove(key);
                inner.deleted.push(key.clone());
            }
            Ok(Vec::new())
        })
    }
}

fn fake_etag(data: &[u8]) -> String {
    format!("{:032x}", data.len())
}

struct OneStore {
    store: Arc<FakeStore>,
    /// 关着时账户查不到，模拟客户端还没建出来的窗口。
    gate: Arc<AtomicBool>,
}

impl StoreResolver for OneStore {
    fn store_for(&self, _profile_id: &str) -> Option<Arc<dyn ObjectStore>> {
        self.gate
            .load(Ordering::Relaxed)
            .then(|| self.store.clone() as Arc<dyn ObjectStore>)
    }
}

// ---------- 测试脚手架 ----------

struct Harness {
    runtime: tokio::runtime::Runtime,
    engine: Engine,
    events: async_channel::Receiver<EngineEvent>,
    store: Arc<FakeStore>,
    jobs: Arc<SqliteJobStore>,
    /// OneStore 那扇门的把手，测试里开关账户可用性。
    gate: Arc<AtomicBool>,
    dir: PathBuf,
}

impl Harness {
    fn new(config: EngineConfig) -> Self {
        Self::with_store(config, FakeStore::new(), Arc::new(SqliteJobStore::in_memory().unwrap()))
    }

    fn with_store(
        config: EngineConfig,
        store: Arc<FakeStore>,
        jobs: Arc<SqliteJobStore>,
    ) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let gate = Arc::new(AtomicBool::new(true));
        let resolver = Arc::new(OneStore {
            store: store.clone(),
            gate: gate.clone(),
        });
        let (engine, events) = Engine::start(
            runtime.handle().clone(),
            jobs.clone() as Arc<dyn JobStore>,
            resolver,
            config,
        );
        let dir = std::env::temp_dir().join(format!("s3b-eng-{}", fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();
        Harness {
            runtime,
            engine,
            events,
            store,
            jobs,
            gate,
            dir,
        }
    }

    /// 等某个任务走到终态。超时就失败，免得测试挂住。
    fn wait_for(&self, job_id: &str, want: &[JobState]) -> JobState {
        self.runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    panic!("等 {job_id} 到达 {want:?} 超时");
                }
                match tokio::time::timeout(remaining, self.events.recv()).await {
                    Ok(Ok(EngineEvent::JobState { id, state, error }))
                        if id == job_id && want.contains(&state) =>
                    {
                        if let Some(error) = error {
                            eprintln!("任务 {id} 失败：{error}");
                        }
                        return state;
                    }
                    Ok(Ok(_)) => continue,
                    Ok(Err(_)) => panic!("事件通道关了"),
                    Err(_) => panic!("等 {job_id} 到达 {want:?} 超时"),
                }
            }
        })
    }

    /// 等一批任务全部落定，把 `BatchDone` 带的账目取出来。
    fn wait_batch_done(&self, batch_id: &str) -> (String, u32, u32, u32) {
        self.runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    panic!("等批次 {batch_id} 结束超时");
                }
                match tokio::time::timeout(remaining, self.events.recv()).await {
                    Ok(Ok(EngineEvent::BatchDone {
                        id,
                        label,
                        done,
                        failed,
                        cancelled,
                        ..
                    })) if id == batch_id => return (label, done, failed, cancelled),
                    Ok(Ok(_)) => continue,
                    Ok(Err(_)) => panic!("事件通道关了"),
                    Err(_) => panic!("等批次 {batch_id} 结束超时"),
                }
            }
        })
    }

    /// 等一条能让 `want` 说"就是它"的快照，把那条任务的进度取出来。
    fn wait_snapshot(
        &self,
        job_id: &str,
        want: impl Fn(&JobProgress) -> bool,
    ) -> JobProgress {
        self.runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    panic!("等 {job_id} 的快照超时");
                }
                match tokio::time::timeout(remaining, self.events.recv()).await {
                    Ok(Ok(EngineEvent::Snapshot { jobs, .. })) => {
                        if let Some(job) = jobs.iter().find(|j| j.id == job_id && want(j)) {
                            return job.clone();
                        }
                    }
                    Ok(Ok(_)) => continue,
                    Ok(Err(_)) => panic!("事件通道关了"),
                    Err(_) => panic!("等 {job_id} 的快照超时"),
                }
            }
        })
    }

    fn local_file(&self, name: &str, data: &[u8]) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, data).unwrap();
        path
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.engine.shutdown();
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn small_config() -> EngineConfig {
    EngineConfig {
        part_size: 1024,
        multipart_threshold: 2048,
        download_chunk: 1024,
        max_connections: 4,
        max_parts_per_job: 2,
        verify_download_md5: false,
        auto_resume: true,
        // 测试用 1 KiB 的分片，不走协议下限
        min_upload_part_size: 1,
        resolve_timeout: Duration::from_secs(60),
    }
}

fn upload_job(batch: &Batch, path: &PathBuf, key: &str, size: u64, mtime: i64) -> Job {
    Job::new(&batch.id, JobKind::Upload, "p1", "bucket", key).with_local(path.clone(), size, mtime)
}

// ---------- 上传 ----------

#[test]
fn small_file_goes_through_put() {
    let h = Harness::new(small_config());
    let path = h.local_file("small.bin", &[7u8; 100]);
    let batch = Batch::new(JobKind::Upload, "上传");
    let job = upload_job(&batch, &path, "dir/small.bin", 100, 1);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    assert_eq!(h.store.object("dir/small.bin").unwrap().len(), 100);
    assert_eq!(h.store.count("create:"), 0, "小文件不该走分片");
    assert_eq!(h.store.count("put:"), 1);
}

/// 传完的任务要记进历史；点「清除记录」后队列和历史一起空。
#[test]
fn finished_job_lands_in_history_until_cleared() {
    let h = Harness::new(small_config());
    let path = h.local_file("hist.bin", &[3u8; 50]);
    let batch = Batch::new(JobKind::Upload, "上传");
    let job = upload_job(&batch, &path, "dir/hist.bin", 50, 1);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    let entries = h.jobs.load_history(10).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key, "dir/hist.bin");
    assert_eq!(entries[0].state, JobState::Done);

    h.engine.clear_finished();
    // 等引擎把清空后的历史推过来，再查库才是确定的
    h.runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "等空历史事件超时");
            match tokio::time::timeout(remaining, h.events.recv()).await {
                Ok(Ok(EngineEvent::History { entries })) if entries.is_empty() => return,
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => panic!("事件通道关了"),
                Err(_) => panic!("等空历史事件超时"),
            }
        }
    });
    assert!(h.jobs.load_history(10).unwrap().is_empty());
    // 任务本身也从队列里走了
    assert!(h.jobs.load_unfinished().unwrap().is_empty());
}

/// 重启后账户的客户端还没建出来：任务在队列里等，别一上来就报
/// 「账户不可用」；客户端注册进来的那一刻要能自己跑起来。
#[test]
fn queued_job_waits_for_the_account_to_register() {
    let h = Harness::new(small_config());
    h.gate.store(false, Ordering::Relaxed);
    let path = h.local_file("wait.bin", &[5u8; 60]);
    let batch = Batch::new(JobKind::Upload, "上传");
    let job = upload_job(&batch, &path, "dir/wait.bin", 60, 1);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    h.runtime
        .block_on(async { tokio::time::sleep(Duration::from_millis(600)).await });
    let p = h.wait_snapshot(&id, |_| true);
    assert_eq!(p.state, JobState::Queued, "账户没就绪时应该还排着队：{p:?}");

    h.gate.store(true, Ordering::Relaxed);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);
    assert_eq!(h.store.object("dir/wait.bin").unwrap().len(), 60);
}

/// 账户真等不到（被删了、STS 一直连不上）：过了超时再报「账户不可用」。
#[test]
fn unresolvable_account_fails_after_the_timeout() {
    let config = EngineConfig {
        resolve_timeout: Duration::from_millis(300),
        ..small_config()
    };
    let h = Harness::new(config);
    h.gate.store(false, Ordering::Relaxed);
    let path = h.local_file("gone.bin", &[5u8; 60]);
    let batch = Batch::new(JobKind::Upload, "上传");
    let job = upload_job(&batch, &path, "dir/gone.bin", 60, 1);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    let p = h.wait_snapshot(&id, |j| j.state == JobState::Failed);
    assert_eq!(p.error.as_deref(), Some("账户不可用"));
    assert_eq!(p.state, JobState::Failed);
}

/// 同一个任务被收两遍不能跑两次。
///
/// 真实场景是启动时从库里捞回来的任务和界面随后送进来的同一批撞上：
/// 第二次入队会把它的 JobControl 换掉，正在跑的那份就再也收不到暂停和取消。
#[test]
fn enqueueing_the_same_job_twice_runs_it_once() {
    let h = Harness::new(small_config());
    let path = h.local_file("dup.bin", &[7u8; 100]);
    let batch = Batch::new(JobKind::Upload, "上传");
    let job = upload_job(&batch, &path, "dir/dup.bin", 100, 1);
    let id = job.id.clone();

    h.engine.enqueue(batch.clone(), vec![job.clone()]);
    h.engine.enqueue(batch, vec![job]);
    assert_eq!(
        h.wait_for(&id, &[JobState::Done, JobState::Failed]),
        JobState::Done
    );
    // 再等一会儿，重复的那份要是会跑，这时候也该露头了
    std::thread::sleep(std::time::Duration::from_millis(300));

    assert_eq!(h.store.count("put:"), 1, "同一个任务不该传两遍");
}

#[test]
fn large_file_is_uploaded_in_parts() {
    let h = Harness::new(small_config());
    let data = vec![3u8; 2500];
    let path = h.local_file("big.bin", &data);
    let batch = Batch::new(JobKind::Upload, "上传");
    let job = upload_job(&batch, &path, "big.bin", data.len() as u64, 1);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    assert_eq!(h.store.count("create:"), 1);
    // 2500 字节按 1024 切成三片
    assert_eq!(h.store.count("part:"), 3);
    assert_eq!(h.store.count("complete:"), 1);
    let complete = h
        .store
        .calls()
        .into_iter()
        .find(|c| c.starts_with("complete:"))
        .unwrap();
    assert!(complete.ends_with("[1,2,3]"), "分片要按编号升序提交：{complete}");
}

#[test]
fn resume_skips_parts_the_server_already_has() {
    let h = Harness::new(small_config());
    let data = vec![3u8; 2500];
    let path = h.local_file("big.bin", &data);
    let (size, mtime) = {
        let meta = std::fs::metadata(&path).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        (meta.len(), mtime)
    };

    // 上次传到一半：服务端已有第 1、2 片
    h.store.seed_upload(
        "up-existing",
        "big.bin",
        vec![
            PartInfo { number: 1, size: 1024, etag: "e1".into() },
            PartInfo { number: 2, size: 1024, etag: "e2".into() },
        ],
    );

    let batch = Batch::new(JobKind::Upload, "上传");
    let mut job = upload_job(&batch, &path, "big.bin", size, mtime);
    job.upload_id = Some("up-existing".into());
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    assert_eq!(h.store.count("create:"), 0, "已有 upload_id 就不该重新创建");
    assert_eq!(h.store.count("listparts:"), 1);
    // 只补最后一片
    assert_eq!(h.store.count("part:"), 1);
    let part_call = h
        .store
        .calls()
        .into_iter()
        .find(|c| c.starts_with("part:"))
        .unwrap();
    assert!(part_call.contains(":3:"), "补传的应该是第 3 片：{part_call}");
}

#[test]
fn stale_upload_id_starts_over() {
    let h = Harness::new(small_config());
    let data = vec![3u8; 2500];
    let path = h.local_file("big.bin", &data);
    let meta = std::fs::metadata(&path).unwrap();
    let mtime = meta
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    h.store.set_no_such_upload(true);
    let batch = Batch::new(JobKind::Upload, "上传");
    let mut job = upload_job(&batch, &path, "big.bin", meta.len(), mtime);
    job.upload_id = Some("up-gone".into());
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    // ListParts 回 NoSuchUpload 之后要重新 create，再把三片都传一遍
    h.store.set_no_such_upload(false);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    assert_eq!(h.store.count("listparts:"), 1);
    assert_eq!(h.store.count("create:"), 1);
    assert_eq!(h.store.count("part:"), 3);
}

#[test]
fn changed_local_file_aborts_the_old_upload() {
    let h = Harness::new(small_config());
    let data = vec![3u8; 2500];
    let path = h.local_file("big.bin", &data);

    h.store.seed_upload(
        "up-old",
        "big.bin",
        vec![PartInfo { number: 1, size: 1024, etag: "e1".into() }],
    );

    let batch = Batch::new(JobKind::Upload, "上传");
    // 任务记录里的大小和现在的文件对不上，说明文件被改过
    let mut job = upload_job(&batch, &path, "big.bin", 9999, 1);
    job.upload_id = Some("up-old".into());
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    assert_eq!(h.store.count("abort:"), 1, "旧的分片上传要取消掉");
    assert_eq!(h.store.count("create:"), 1);
    assert_eq!(h.store.count("part:"), 3, "全部重传");
}

#[test]
fn failed_part_leaves_job_failed_and_keeps_progress() {
    let h = Harness::new(small_config());
    let data = vec![3u8; 2500];
    let path = h.local_file("big.bin", &data);
    h.store.set_fail_part(2);

    let batch = Batch::new(JobKind::Upload, "上传");
    let job = upload_job(&batch, &path, "big.bin", data.len() as u64, 1);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Failed);

    assert_eq!(h.store.count("complete:"), 0, "没传完不能 complete");
    // 成功的分片要落库，重试时才不用重传
    let stored = h.jobs.load_unfinished().unwrap();
    let (_, parts) = stored.into_iter().find(|(j, _)| j.id == id).unwrap();
    assert!(parts.iter().any(|p| p.done), "成功的分片应该记下来了");
}

#[test]
fn pausing_mid_transfer_stops_and_keeps_parts() {
    let h = Harness::new(EngineConfig {
        max_parts_per_job: 1,
        ..small_config()
    });
    let data = vec![3u8; 5000];
    let path = h.local_file("big.bin", &data);
    let batch = Batch::new(JobKind::Upload, "上传");
    let job = upload_job(&batch, &path, "big.bin", data.len() as u64, 1);
    let id = job.id.clone();

    // 第一片刚上传就按暂停
    let engine_id = id.clone();
    let paused = Arc::new(AtomicBool::new(false));
    let flag = paused.clone();
    h.store.on_first_part(move || {
        flag.store(true, Ordering::Relaxed);
        let _ = engine_id;
    });
    h.store.set_delay(250);

    h.engine.enqueue(batch, vec![job]);
    // 等第一片落地再发暂停命令
    h.runtime.block_on(async {
        for _ in 0..100 {
            if paused.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    h.engine.pause(id.clone());

    assert_eq!(
        h.wait_for(&id, &[JobState::Paused, JobState::Done, JobState::Failed]),
        JobState::Paused
    );
    assert_eq!(h.store.count("complete:"), 0, "暂停不该收尾");
    assert!(h.store.count("part:") >= 1);
}

#[test]
fn cancelling_aborts_the_multipart_upload() {
    let h = Harness::new(EngineConfig {
        max_parts_per_job: 1,
        ..small_config()
    });
    let data = vec![3u8; 5000];
    let path = h.local_file("big.bin", &data);
    let batch = Batch::new(JobKind::Upload, "上传");
    let job = upload_job(&batch, &path, "big.bin", data.len() as u64, 1);
    let id = job.id.clone();

    let fired = Arc::new(AtomicBool::new(false));
    let flag = fired.clone();
    h.store.on_first_part(move || flag.store(true, Ordering::Relaxed));
    h.store.set_delay(250);

    h.engine.enqueue(batch, vec![job]);
    h.runtime.block_on(async {
        for _ in 0..100 {
            if fired.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    h.engine.cancel(id.clone());

    assert_eq!(
        h.wait_for(&id, &[JobState::Cancelled, JobState::Done, JobState::Failed]),
        JobState::Cancelled
    );
    assert_eq!(h.store.count("abort:"), 1, "取消要顺手把分片上传废掉");
}

#[test]
fn empty_folder_marker_is_a_zero_byte_object() {
    let h = Harness::new(small_config());
    let batch = Batch::new(JobKind::Upload, "新建文件夹");
    // 和 ops::new_folder_job 一样：新建文件夹没有本地文件，local_path 是空的。
    // 之前这里塞了个目录进去，测试就绕开了真实路径，界面上新建文件夹一直是失败的。
    let job = Job::new(&batch.id, JobKind::Upload, "p1", "bucket", "newdir/");
    assert!(job.local_path.is_none());
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    assert_eq!(h.store.object("newdir/").unwrap().len(), 0);
}

// ---------- 下载 ----------

#[test]
fn download_writes_through_a_temp_file() {
    let h = Harness::new(small_config());
    let data: Vec<u8> = (0..2500).map(|i| (i % 251) as u8).collect();
    h.store.put_object("remote/big.bin", data.clone());

    let target = h.dir.join("downloaded.bin");
    let batch = Batch::new(JobKind::Download, "下载");
    let job = Job::new(&batch.id, JobKind::Download, "p1", "bucket", "remote/big.bin")
        .with_local(target.clone(), 0, 0);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    assert_eq!(std::fs::read(&target).unwrap(), data);
    assert!(
        !target.with_extension("bin.s3b-part").exists(),
        "临时文件要改名掉"
    );
    assert_eq!(h.store.count("get:"), 3, "2500 字节按 1024 分三块");
}

#[test]
fn download_resumes_only_missing_chunks() {
    let store = FakeStore::new();
    let jobs = Arc::new(SqliteJobStore::in_memory().unwrap());
    let data: Vec<u8> = (0..2500).map(|i| (i % 251) as u8).collect();
    store.put_object("remote/big.bin", data.clone());

    let h = Harness::with_store(small_config(), store, jobs);
    let target = h.dir.join("resumed.bin");
    let temp = h.dir.join("resumed.bin.s3b-part");

    // 造出上次下到一半的现场：临时文件已撑满，前两块标记为完成
    std::fs::write(&temp, vec![0u8; data.len()]).unwrap();
    let batch = Batch::new(JobKind::Download, "下载");
    let mut job = Job::new(&batch.id, JobKind::Download, "p1", "bucket", "remote/big.bin")
        .with_local(target.clone(), 0, 0);
    job.size = data.len() as u64;
    let id = job.id.clone();
    h.jobs.insert_batch(&batch, std::slice::from_ref(&job)).unwrap();
    let (_, parts) = transfer::split_parts(data.len() as u64, 1024);
    h.jobs.insert_parts(&id, &parts).unwrap();
    h.jobs.mark_part_done(&id, 0, None).unwrap();
    h.jobs.mark_part_done(&id, 1, None).unwrap();
    // 把已完成的两块内容先写进临时文件，跳过的部分才对得上
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = std::fs::OpenOptions::new().write(true).open(&temp).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&data[..2048]).unwrap();
    }

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    assert_eq!(h.store.count("get:"), 1, "只该补最后一块");
    assert_eq!(std::fs::read(&target).unwrap(), data);
}

#[test]
fn download_fails_when_object_changed_midway() {
    let h = Harness::new(small_config());
    h.store.put_object("remote/x.bin", vec![1u8; 2500]);
    h.store.set_precondition_failed(true);

    let target = h.dir.join("x.bin");
    let batch = Batch::new(JobKind::Download, "下载");
    let job = Job::new(&batch.id, JobKind::Download, "p1", "bucket", "remote/x.bin")
        .with_local(target.clone(), 0, 0);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Failed);
    assert!(!target.exists(), "失败时不该留下目标文件");
}

// ---------- 复制与删除 ----------

#[test]
fn small_copy_uses_copy_object() {
    let h = Harness::new(small_config());
    h.store.put_object("src/a.bin", vec![9u8; 500]);

    let batch = Batch::new(JobKind::Copy, "复制");
    let job = Job::new(&batch.id, JobKind::Copy, "p1", "bucket", "dst/a.bin")
        .with_source("src/a.bin", 500);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    assert_eq!(h.store.count("copy:"), 1);
    assert_eq!(h.store.count("partcopy:"), 0);
    assert_eq!(h.store.object("dst/a.bin").unwrap().len(), 500);
}

#[test]
fn deleting_a_prefix_lists_then_batches() {
    let h = Harness::new(small_config());
    for i in 0..5 {
        h.store.put_object(&format!("dir/f{i}.bin"), vec![1u8; 10]);
    }
    h.store.put_object("other.bin", vec![1u8; 10]);

    let batch = Batch::new(JobKind::Delete, "删除");
    let job = Job::new(&batch.id, JobKind::Delete, "p1", "bucket", "dir/");
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    assert_eq!(h.store.count("list:"), 1);
    assert_eq!(h.store.count("delete:"), 1, "5 个 key 一批就够");
    assert!(h.store.object("dir/f0.bin").is_none());
    assert!(h.store.object("other.bin").is_some(), "别的前缀不能动");
}

#[test]
fn deleting_one_key_skips_listing() {
    let h = Harness::new(small_config());
    h.store.put_object("dir/one.bin", vec![1u8; 10]);

    let batch = Batch::new(JobKind::Delete, "删除");
    let job = Job::new(&batch.id, JobKind::Delete, "p1", "bucket", "dir/one.bin");
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    assert_eq!(h.store.count("list:"), 0, "单个 key 不用列举");
    assert_eq!(h.store.count("delete:"), 1);
}

// ---------- 重启恢复 ----------

/// 上次退出时失败的任务，重启后要能在传输视图里看见。
///
/// 它不会自己跑起来，而快照定时器只在有任务在跑时才转，所以引擎载入之后
/// 必须主动发一次快照——不然界面上一片空白，用户既看不到也没法重试。
#[test]
fn failed_jobs_show_up_after_a_restart() {
    let jobs = Arc::new(SqliteJobStore::in_memory().unwrap());
    let batch = Batch::new(JobKind::Upload, "上传");
    let mut job = Job::new(&batch.id, JobKind::Upload, "p1", "bucket", "gone.bin");
    job.state = JobState::Failed;
    job.error = Some("上次就没传成".to_owned());
    let id = job.id.clone();
    jobs.insert_batch(&batch, std::slice::from_ref(&job)).unwrap();

    let h = Harness::with_store(small_config(), FakeStore::new(), jobs);

    let found = h.runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            match tokio::time::timeout(left, h.events.recv()).await {
                Ok(Ok(EngineEvent::Snapshot { jobs, .. })) => {
                    if jobs.iter().any(|j| j.id == id && j.state == JobState::Failed) {
                        return true;
                    }
                }
                Ok(Ok(_)) => continue,
                _ => return false,
            }
        }
    });

    assert!(found, "重启之后失败的任务要出现在快照里");
}

#[test]
fn unfinished_jobs_are_reloaded_on_start() {
    let jobs = Arc::new(SqliteJobStore::in_memory().unwrap());
    let store = FakeStore::new();
    let data = vec![3u8; 2500];

    let batch = Batch::new(JobKind::Upload, "上传");
    let dir = std::env::temp_dir().join(format!("s3b-reload-{}", fastrand::u32(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.bin");
    std::fs::write(&path, &data).unwrap();

    let mut job = Job::new(&batch.id, JobKind::Upload, "p1", "bucket", "big.bin")
        .with_local(path.clone(), data.len() as u64, 1);
    // 上次进程被杀时这个任务正在跑
    job.state = JobState::Running;
    let id = job.id.clone();
    jobs.insert_batch(&batch, std::slice::from_ref(&job)).unwrap();

    // 新引擎起来后应该把它接着传完
    let h = Harness::with_store(small_config(), store, jobs);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);
    assert_eq!(h.store.count("complete:"), 1);

    std::fs::remove_dir_all(&dir).ok();
}

// ---------- 批次收尾 ----------

/// 界面靠 `BatchDone` 报"传完了"，所以标签和三笔账都得对。
#[test]
fn batch_done_carries_the_label_and_a_tally() {
    let h = Harness::new(small_config());
    let good = h.local_file("ok.bin", &[1u8; 100]);
    let missing = h.dir.join("这个文件不存在.bin");

    let batch = Batch::new(JobKind::Upload, "上传 2 个文件");
    let batch_id = batch.id.clone();
    let jobs = vec![
        upload_job(&batch, &good, "ok.bin", 100, 1),
        upload_job(&batch, &missing, "gone.bin", 100, 1),
    ];

    h.engine.enqueue(batch, jobs);
    let (label, done, failed, cancelled) = h.wait_batch_done(&batch_id);

    assert_eq!(label, "上传 2 个文件");
    assert_eq!((done, failed, cancelled), (1, 1, 0));
}

/// 一批里还有没落定的就不能报结束，不然界面会提前弹"完成"。
///
/// 两个任务不可能同一刻收尾，中间必然有"一个成了一个还没"的时刻，
/// 提前报的话这里收到的第一条 BatchDone 就只有一笔账。
#[test]
fn batch_done_waits_for_every_job() {
    let h = Harness::new(small_config());
    let first = h.local_file("first.bin", &[1u8; 100]);
    let second = h.local_file("second.bin", &[2u8; 100]);

    let batch = Batch::new(JobKind::Upload, "上传 2 个文件");
    let batch_id = batch.id.clone();
    let jobs = vec![
        upload_job(&batch, &first, "first.bin", 100, 1),
        upload_job(&batch, &second, "second.bin", 100, 1),
    ];

    h.engine.enqueue(batch, jobs);
    let (_, done, failed, cancelled) = h.wait_batch_done(&batch_id);

    assert_eq!((done, failed, cancelled), (2, 0, 0));
}

/// 全是用户按取消的，界面不该弹"完成"，所以取消得单独记一笔。
#[test]
fn batch_done_counts_cancelled_separately() {
    let h = Harness::new(EngineConfig {
        max_parts_per_job: 1,
        ..small_config()
    });
    let data = vec![3u8; 5000];
    let path = h.local_file("big.bin", &data);
    let batch = Batch::new(JobKind::Upload, "上传 1 个文件");
    let batch_id = batch.id.clone();
    let job = upload_job(&batch, &path, "big.bin", data.len() as u64, 1);
    let id = job.id.clone();

    // 等第一片落地再取消，不然任务还没跑起来就没得取消了
    let fired = Arc::new(AtomicBool::new(false));
    let flag = fired.clone();
    h.store.on_first_part(move || flag.store(true, Ordering::Relaxed));
    h.store.set_delay(250);

    h.engine.enqueue(batch, vec![job]);
    h.runtime.block_on(async {
        for _ in 0..100 {
            if fired.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    h.engine.cancel(id);

    let (_, done, failed, cancelled) = h.wait_batch_done(&batch_id);
    assert_eq!((done, failed, cancelled), (0, 0, 1));
}

// ---------- 换参数 ----------

/// 调大并发之后新任务能立刻用上多出来的许可。
///
/// 假存储只在上传分片时拖延，所以文件得大到走分片。每个任务一次只跑一片，
/// 于是"同时在传几片"完全由信号量说了算：一个许可时六片串着走，
/// 两个许可时两个任务并排走，用时减半。
#[test]
fn raising_the_connection_limit_takes_effect_at_once() {
    const PART_MS: u64 = 120;
    let narrow = EngineConfig {
        max_connections: 1,
        max_parts_per_job: 1,
        ..small_config()
    };
    let h = Harness::new(narrow.clone());
    h.store.set_delay(PART_MS);

    // 3000 字节按 1024 一片是 3 片，两个任务一共 6 片
    let data = vec![7u8; 3000];
    let a = h.local_file("a.bin", &data);
    let b = h.local_file("b.bin", &data);
    let batch = Batch::new(JobKind::Upload, "上传 2 个文件");
    let batch_id = batch.id.clone();
    let jobs = vec![
        upload_job(&batch, &a, "a.bin", data.len() as u64, 1),
        upload_job(&batch, &b, "b.bin", data.len() as u64, 1),
    ];

    h.engine.reconfigure(EngineConfig {
        max_connections: 2,
        ..narrow
    });
    h.engine.enqueue(batch, jobs);

    let started = std::time::Instant::now();
    let (_, done, failed, _) = h.wait_batch_done(&batch_id);
    let elapsed = started.elapsed();

    assert_eq!((done, failed), (2, 0));
    assert_eq!(h.store.count("part:"), 6);
    // 串行要 6×120=720 ms 起，并行只要 360 ms 出头。取中间当界。
    assert!(
        elapsed < Duration::from_millis(560),
        "两个任务像是串着跑的：{elapsed:?}"
    );
}

/// 参数换过之后，新任务按新的分片大小切。
#[test]
fn a_new_part_size_applies_to_the_next_job() {
    let h = Harness::new(small_config());
    // 2500 字节按 1000 一片是 3 片；换成 2000 一片就只剩 2 片
    h.engine.reconfigure(EngineConfig {
        part_size: 2000,
        min_upload_part_size: 1,
        multipart_threshold: 1000,
        ..small_config()
    });

    let data = vec![9u8; 2500];
    let path = h.local_file("big.bin", &data);
    let batch = Batch::new(JobKind::Upload, "上传");
    let job = upload_job(&batch, &path, "big.bin", data.len() as u64, 1);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);
    assert_eq!(h.store.count("part:"), 2, "该按新的分片大小切");
}

/// 一块传到一半按暂停也该停下来。
///
/// 下载块是按分片大小切的，8 MiB 一块。只在开始一块之前查暂停标记的话，
/// 按下去要等整块传完才有反应——慢链路上就是"点了没用"，还可能在这期间
/// 撞上个网络错误被报成传失败。
#[test]
fn pausing_stops_in_the_middle_of_a_chunk() {
    let h = Harness::new(small_config());
    // 小于 download_chunk，整个文件就一块；假存储把它切成两半发
    let data = vec![5u8; 800];
    h.store.put_object("remote/one.bin", data.clone());
    // 两半之间停 400 毫秒，留出按暂停的时间
    h.store.set_delay(400);

    let target = h.dir.join("one.bin");
    let batch = Batch::new(JobKind::Download, "下载");
    let job = Job::new(&batch.id, JobKind::Download, "p1", "bucket", "remote/one.bin")
        .with_local(target.clone(), 0, 0);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    // 等第一半落地，这时候任务正卡在两半之间
    std::thread::sleep(Duration::from_millis(150));
    h.engine.pause(id.clone());

    assert_eq!(
        h.wait_for(&id, &[JobState::Paused, JobState::Done, JobState::Failed]),
        JobState::Paused
    );
    assert!(!target.exists(), "没传完不该改名成目标文件");

    // 真在块中途停下来的证据：这一块没有被标记完成，续传时会整块重下。
    // 只在块与块之间查暂停的话，这一块会一路传完并标记上，测不出区别。
    let stored = h.jobs.load_unfinished().unwrap();
    let (_, parts) = stored.into_iter().find(|(j, _)| j.id == id).unwrap();
    assert_eq!(parts.len(), 1);
    assert!(!parts[0].done, "半截的块不能算完成");
}

/// 按下暂停之后立刻能从快照里看出"正在暂停"，不用等分片真停下来。
///
/// 界面靠这个给反馈。没有它的话按钮会一直是"暂停"，点了像没点着——
/// 8 MiB 一块的下载能这么僵住十几秒。
#[test]
fn a_running_job_reports_that_it_is_pausing_right_away() {
    let h = Harness::new(small_config());
    let data = vec![5u8; 800];
    h.store.put_object("remote/slow.bin", data);
    // 两半之间停 600 毫秒，这期间任务还在 Running
    h.store.set_delay(600);

    let batch = Batch::new(JobKind::Download, "下载");
    let job = Job::new(&batch.id, JobKind::Download, "p1", "bucket", "remote/slow.bin")
        .with_local(h.dir.join("slow.bin"), 0, 0);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    std::thread::sleep(Duration::from_millis(150));
    h.engine.pause(id.clone());

    let snap = h.wait_snapshot(&id, |j| j.pausing);
    assert_eq!(snap.state, JobState::Running, "分片还没停，状态还是在跑");
    assert!(snap.pausing, "但快照里得看得出正在暂停");

    // 真停下来之后 pausing 就该退场，状态变成 Paused
    assert_eq!(
        h.wait_for(&id, &[JobState::Paused, JobState::Done, JobState::Failed]),
        JobState::Paused
    );
}

// ---------- 整个目录的搬运 ----------

/// 重命名文件夹：底下每个对象换个前缀，子目录的层级原样保住，源清干净。
#[test]
fn moving_a_folder_carries_the_whole_subtree() {
    let h = Harness::new(small_config());
    h.store.put_object("docs/~/a.txt", b"aaa".to_vec());
    h.store.put_object("docs/~/sub/b.txt", b"bbb".to_vec());
    // 文件夹本身那个零字节标记也在里面
    h.store.put_object("docs/~/", Vec::new());
    // 不该被牵连的邻居
    h.store.put_object("docs/other.txt", b"x".to_vec());

    let batch = Batch::new(JobKind::Move, "重命名为 data");
    let job = Job::new(&batch.id, JobKind::Move, "p1", "bucket", "docs/data/")
        .with_source("docs/~/", 0);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    assert_eq!(h.store.object("docs/data/a.txt").as_deref(), Some(&b"aaa"[..]));
    assert_eq!(
        h.store.object("docs/data/sub/b.txt").as_deref(),
        Some(&b"bbb"[..]),
        "子目录的层级要保住"
    );
    assert!(h.store.object("docs/data/").is_some(), "文件夹标记也要跟过去");

    assert!(h.store.object("docs/~/a.txt").is_none(), "源要删干净");
    assert!(h.store.object("docs/~/sub/b.txt").is_none());
    assert!(h.store.object("docs/~/").is_none());
    assert_eq!(
        h.store.object("docs/other.txt").as_deref(),
        Some(&b"x"[..]),
        "同级的邻居不该被牵连"
    );
}

/// 复制文件夹和移动的区别只在留不留源。
#[test]
fn copying_a_folder_leaves_the_source_alone() {
    let h = Harness::new(small_config());
    h.store.put_object("src/a.txt", b"aaa".to_vec());

    let batch = Batch::new(JobKind::Copy, "复制");
    let job = Job::new(&batch.id, JobKind::Copy, "p1", "bucket", "dst/")
        .with_source("src/", 0);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);

    assert_eq!(h.store.object("dst/a.txt").as_deref(), Some(&b"aaa"[..]));
    assert_eq!(h.store.object("src/a.txt").as_deref(), Some(&b"aaa"[..]));
}

/// 空文件夹搬起来不该报错。
#[test]
fn moving_an_empty_folder_is_not_an_error() {
    let h = Harness::new(small_config());
    let batch = Batch::new(JobKind::Move, "重命名");
    let job = Job::new(&batch.id, JobKind::Move, "p1", "bucket", "b/")
        .with_source("a/", 0);
    let id = job.id.clone();

    h.engine.enqueue(batch, vec![job]);
    assert_eq!(h.wait_for(&id, &[JobState::Done, JobState::Failed]), JobState::Done);
}
