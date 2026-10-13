//! 引擎打真实 S3 兼容服务的端到端测试：上传一个大到要分片的文件，再下回来比对。
//!
//! 默认跳过，环境变量和 `docs/testing.md` 里的一样：
//!
//! ```text
//! cargo test -p transfer -- --ignored
//! ```

use std::sync::Arc;
use std::time::Duration;

use s3api::{Credentials, Endpoint, S3Client, StaticCredentials};
use s3api_reqwest::{ReqwestTransport, TransportOptions};
use transfer::engine::{Engine, EngineConfig, EngineEvent};
use transfer::job::{Batch, Job, JobKind, JobState};
use transfer::object_store::{ObjectStore, S3ObjectStore, StoreResolver};
use transfer::store::{JobStore, SqliteJobStore};

struct OneStore(Arc<dyn ObjectStore>);

impl StoreResolver for OneStore {
    fn store_for(&self, _profile_id: &str) -> Option<Arc<dyn ObjectStore>> {
        Some(self.0.clone())
    }
}

struct Live {
    runtime: tokio::runtime::Runtime,
    engine: Engine,
    events: async_channel::Receiver<EngineEvent>,
    client: Arc<S3Client>,
    /// 留着给需要另起一个引擎的用例（比如按出厂配置测内存）。
    store: Arc<dyn ObjectStore>,
    bucket: String,
    dir: std::path::PathBuf,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn live() -> Option<Live> {
    let endpoint = env("S3B_TEST_ENDPOINT")?;
    let access_key = env("S3B_TEST_ACCESS_KEY")?;
    let secret_key = env("S3B_TEST_SECRET_KEY")?;
    let bucket = env("S3B_TEST_BUCKET").unwrap_or_else(|| "s3browser-test".to_owned());
    let region = env("S3B_TEST_REGION").unwrap_or_else(|| "us-east-1".to_owned());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .ok()?;
    let transport =
        ReqwestTransport::new(runtime.handle().clone(), TransportOptions::default()).ok()?;
    let endpoint = Endpoint::parse(&endpoint, &region)
        .ok()?
        .with_addressing(s3api::Addressing::Path);
    let client = Arc::new(S3Client::new(
        Arc::new(transport),
        Arc::new(StaticCredentials(Credentials::new(access_key, secret_key))),
        endpoint,
    ));

    let store: Arc<dyn ObjectStore> = Arc::new(S3ObjectStore::new(client.clone()));
    let jobs: Arc<dyn JobStore> = Arc::new(SqliteJobStore::in_memory().ok()?);
    let store_for_engine = store.clone();
    // 分片按协议下限 5 MiB，阈值压低好让测试文件真的走分片上传
    let config = EngineConfig {
        part_size: 5 * 1024 * 1024,
        multipart_threshold: 5 * 1024 * 1024,
        download_chunk: 5 * 1024 * 1024,
        max_connections: 4,
        max_parts_per_job: 2,
        verify_download_md5: true,
        auto_resume: true,
        ..EngineConfig::default()
    };
    let (engine, events) = Engine::start(
        runtime.handle().clone(),
        jobs,
        Arc::new(OneStore(store_for_engine)),
        config,
    );

    let dir = std::env::temp_dir().join(format!("s3b-live-{}", fastrand::u32(..)));
    std::fs::create_dir_all(&dir).ok()?;

    Some(Live {
        runtime,
        engine,
        events,
        client,
        store,
        bucket,
        dir,
    })
}

impl Live {
    fn wait(&self, job_id: &str) -> (JobState, Option<String>) {
        self.runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            loop {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                assert!(!left.is_zero(), "等 {job_id} 结束超时");
                match tokio::time::timeout(left, self.events.recv()).await {
                    Ok(Ok(EngineEvent::JobState { id, state, error }))
                        if id == job_id
                            && matches!(
                                state,
                                JobState::Done | JobState::Failed | JobState::Cancelled
                            ) =>
                    {
                        return (state, error);
                    }
                    Ok(Ok(_)) => continue,
                    Ok(Err(_)) => panic!("事件通道关了"),
                    Err(_) => panic!("等 {job_id} 结束超时"),
                }
            }
        })
    }
}

impl Live {
    /// 另起一个引擎，用来按出厂配置测量。返回的事件通道是它自己的。
    fn engine_with(
        &self,
        config: EngineConfig,
    ) -> (Engine, async_channel::Receiver<EngineEvent>) {
        let jobs: Arc<dyn JobStore> = Arc::new(SqliteJobStore::in_memory().expect("建库"));
        Engine::start(
            self.runtime.handle().clone(),
            jobs,
            Arc::new(OneStore(self.store.clone())),
            config,
        )
    }

    /// 等某个任务结束，用调用方给的事件通道。
    fn wait_on(
        &self,
        events: &async_channel::Receiver<EngineEvent>,
        job_id: &str,
    ) -> (JobState, Option<String>) {
        self.runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
            loop {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                assert!(!left.is_zero(), "等 {job_id} 结束超时");
                match tokio::time::timeout(left, events.recv()).await {
                    Ok(Ok(EngineEvent::JobState { id, state, error }))
                        if id == job_id
                            && matches!(
                                state,
                                JobState::Done | JobState::Failed | JobState::Cancelled
                            ) =>
                    {
                        return (state, error);
                    }
                    Ok(Ok(_)) => continue,
                    Ok(Err(_)) => panic!("事件通道关了"),
                    Err(_) => panic!("等 {job_id} 结束超时"),
                }
            }
        })
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.engine.shutdown();
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

macro_rules! live_or_skip {
    () => {
        match live() {
            Some(live) => live,
            None => {
                eprintln!("跳过：未设置 S3B_TEST_ENDPOINT 等环境变量");
                return;
            }
        }
    };
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn multipart_upload_then_download_round_trips() {
    let h = live_or_skip!();
    // 12 MiB，按 5 MiB 分片就是三片
    let data: Vec<u8> = (0..12 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let source = h.dir.join("upload.bin");
    std::fs::write(&source, &data).unwrap();
    let meta = std::fs::metadata(&source).unwrap();
    let mtime = meta
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let key = format!("engine-live/{}.bin", fastrand::u32(..));

    // 上传
    let batch = Batch::new(JobKind::Upload, "上传");
    let job = Job::new(&batch.id, JobKind::Upload, "p1", &h.bucket, &key)
        .with_local(source.clone(), meta.len(), mtime);
    let upload_id = job.id.clone();
    h.engine.enqueue(batch, vec![job]);

    let (state, error) = h.wait(&upload_id);
    assert_eq!(state, JobState::Done, "上传失败：{error:?}");

    let head = h
        .runtime
        .block_on(h.client.head_object(&h.bucket, &key))
        .expect("上传后 head");
    assert_eq!(head.size, data.len() as u64);

    // 下载回来
    let target = h.dir.join("downloaded.bin");
    let batch = Batch::new(JobKind::Download, "下载");
    let job = Job::new(&batch.id, JobKind::Download, "p1", &h.bucket, &key)
        .with_local(target.clone(), 0, 0);
    let download_id = job.id.clone();
    h.engine.enqueue(batch, vec![job]);

    let (state, error) = h.wait(&download_id);
    assert_eq!(state, JobState::Done, "下载失败：{error:?}");
    assert_eq!(std::fs::read(&target).unwrap(), data, "下回来的内容对不上");
    assert!(
        !h.dir.join("downloaded.bin.s3b-part").exists(),
        "临时文件该改名掉"
    );

    h.runtime
        .block_on(h.client.delete_object(&h.bucket, &key))
        .ok();
}

#[test]
#[ignore = "需要真实 S3 服务"]
fn upload_resumes_after_the_engine_restarts() {
    let h = live_or_skip!();
    let data: Vec<u8> = (0..12 * 1024 * 1024).map(|i| (i % 241) as u8).collect();
    let source = h.dir.join("resume.bin");
    std::fs::write(&source, &data).unwrap();
    let meta = std::fs::metadata(&source).unwrap();
    let mtime = meta
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let key = format!("engine-live/{}-resume.bin", fastrand::u32(..));

    // 先手动传两片上去，模拟上次传到一半
    let upload_id = h
        .runtime
        .block_on(h.client.create_multipart_upload(&h.bucket, &key, None))
        .expect("创建分片上传");
    for number in 1..=2u32 {
        let start = (number as usize - 1) * 5 * 1024 * 1024;
        let chunk = data[start..start + 5 * 1024 * 1024].to_vec();
        let md5 = s3api::client::md5_of(&chunk);
        h.runtime
            .block_on(h.client.upload_part(
                &h.bucket,
                &key,
                &upload_id,
                number,
                s3api::Body::from(chunk),
                Some(md5),
            ))
            .expect("预先上传分片");
    }

    // 引擎接手：应该只补最后一片
    let batch = Batch::new(JobKind::Upload, "续传");
    let mut job = Job::new(&batch.id, JobKind::Upload, "p1", &h.bucket, &key)
        .with_local(source.clone(), meta.len(), mtime);
    job.upload_id = Some(upload_id.clone());
    let job_id = job.id.clone();
    h.engine.enqueue(batch, vec![job]);

    let (state, error) = h.wait(&job_id);
    assert_eq!(state, JobState::Done, "续传失败：{error:?}");

    let head = h
        .runtime
        .block_on(h.client.head_object(&h.bucket, &key))
        .expect("续传后 head");
    assert_eq!(head.size, data.len() as u64, "续传出来的大小要对");

    h.runtime
        .block_on(h.client.delete_object(&h.bucket, &key))
        .ok();
}

/// 传输期间进程多占多少内存。设计文档第 10 节的预算是额外不超过 10 MB。
///
/// 上传和下载都是流式的：上传从文件按分片读，下载按 Range 写临时文件，
/// 两头都不该把整个对象读进内存。这里按**出厂配置**（8 MiB 分片、单任务 4 片、
/// 8 条连接）传一个 120 MB 的文件再下回来，期间每 50 ms 采一次工作集。
#[test]
#[ignore = "需要本地或远程 S3 服务"]
fn transfer_stays_within_the_memory_budget() {
    const SIZE: usize = 120 * 1024 * 1024;
    const BUDGET_MB: f64 = 10.0;

    let h = live_or_skip!();
    // 用出厂配置，测出来的数才是用户真实会碰到的
    let (engine, events) = h.engine_with(EngineConfig::default());

    let key = format!("s3browser-mem-{}.bin", fastrand::u32(..));
    let source = h.dir.join("mem.bin");
    let target = h.dir.join("mem-back.bin");

    // 写个不可压缩的文件，免得哪一层顺手压掉了
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&source).expect("建测试文件");
        let mut block = vec![0u8; 1024 * 1024];
        for i in 0..SIZE / block.len() {
            for (j, b) in block.iter_mut().enumerate() {
                *b = ((i * 31 + j * 17) % 251) as u8;
            }
            file.write_all(&block).expect("写测试文件");
        }
    }

    let baseline = working_set_mb();
    let sampler = Sampler::start();

    let up = Batch::new(JobKind::Upload, "内存测量：上传");
    let up_job = Job::new(&up.id, JobKind::Upload, "p1", &h.bucket, &key)
        .with_local(source.clone(), SIZE as u64, 0);
    let up_id = up_job.id.clone();
    engine.enqueue(up, vec![up_job]);
    let (state, error) = h.wait_on(&events, &up_id);
    assert_eq!(state, JobState::Done, "上传失败：{error:?}");
    let after_upload = sampler.peak();

    let down = Batch::new(JobKind::Download, "内存测量：下载");
    let down_job = Job::new(&down.id, JobKind::Download, "p1", &h.bucket, &key)
        .with_local(target.clone(), 0, 0);
    let down_id = down_job.id.clone();
    engine.enqueue(down, vec![down_job]);
    let (state, error) = h.wait_on(&events, &down_id);
    assert_eq!(state, JobState::Done, "下载失败：{error:?}");

    let peak = sampler.stop();
    engine.shutdown();

    assert_eq!(
        std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0),
        SIZE as u64,
        "下回来的大小不对"
    );

    let extra = peak - baseline;
    eprintln!(
        "传输前 {baseline:.1} MB，上传峰值 {after_upload:.1} MB，全程峰值 {peak:.1} MB，额外 {extra:.1} MB（预算 {BUDGET_MB} MB）"
    );

    h.runtime
        .block_on(h.client.delete_object(&h.bucket, &key))
        .ok();
    std::fs::remove_file(&source).ok();
    std::fs::remove_file(&target).ok();

    assert!(
        extra < BUDGET_MB,
        "传输期间多占了 {extra:.1} MB，超过预算 {BUDGET_MB} MB"
    );
}

/// 后台每 50 ms 采一次工作集，记下峰值。
struct Sampler {
    peak: Arc<std::sync::atomic::AtomicU64>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Sampler {
    fn start() -> Self {
        let peak = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = {
            let (peak, stop) = (peak.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    // 存成百分之一 MB 的整数，好用原子取最大值
                    let mb = (working_set_mb() * 100.0) as u64;
                    peak.fetch_max(mb, std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };
        Sampler {
            peak,
            stop,
            handle: Some(handle),
        }
    }

    fn peak(&self) -> f64 {
        self.peak.load(std::sync::atomic::Ordering::Relaxed) as f64 / 100.0
    }

    fn stop(mut self) -> f64 {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.join().ok();
        }
        self.peak()
    }
}

/// 当前进程的工作集，单位 MB。只在 Windows 上有实现，别的平台返回 0 跳过判断。
#[cfg(windows)]
fn working_set_mb() -> f64 {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
    }

    let mut counters = Counters {
        cb: std::mem::size_of::<Counters>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<Counters>() as u32,
        )
    };
    if ok == 0 {
        return 0.0;
    }
    counters.working_set_size as f64 / 1048576.0
}

#[cfg(not(windows))]
fn working_set_mb() -> f64 {
    0.0
}
