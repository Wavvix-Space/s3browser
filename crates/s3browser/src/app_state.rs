//! 账户表与惰性建出来的 S3 客户端。
//!
//! 这里是组合根：把配置里的账户拼成 `S3Client`（transport + 凭据 + endpoint），
//! 视图只管要一个客户端，不关心它怎么来的。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use s3api::{Credentials, Endpoint, S3Client, StaticCredentials};
use s3api_reqwest::{ReqwestTransport, TransportOptions};
use transfer::engine::{Engine, EngineConfig, EngineEvent};
use transfer::object_store::{ObjectStore, S3ObjectStore, StoreResolver};
use transfer::store::{JobStore, SqliteJobStore};

use s3browser::buckets::BucketList;
use s3browser::config::{Config, Profile, TransferConfig, config_path, data_dir};

/// 引擎跑在 tokio 线程上，够不着界面的 `AppState`，所以客户端另外放一份在这里。
#[derive(Default)]
pub struct SharedStores {
    inner: Mutex<HashMap<String, Arc<dyn ObjectStore>>>,
}

impl SharedStores {
    fn register(&self, profile_id: &str, store: Arc<dyn ObjectStore>) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(profile_id.to_owned(), store);
    }

    fn forget(&self, profile_id: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(profile_id);
    }
}

impl StoreResolver for SharedStores {
    fn store_for(&self, profile_id: &str) -> Option<Arc<dyn ObjectStore>> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(profile_id)
            .cloned()
    }
}

pub struct AppState {
    pub config: Config,
    config_path: std::path::PathBuf,
    /// HTTP 都跑在这个 runtime 上。必须保活，否则请求会被丢掉。
    runtime: tokio::runtime::Runtime,
    clients: HashMap<String, Arc<S3Client>>,
    pub buckets: HashMap<String, BucketList>,
    /// 侧边栏当前展开的账户。
    pub expanded: HashMap<String, bool>,
    /// 哪些账户正开着"显示隐藏的桶"。
    ///
    /// 故意不落盘：这是"我现在要改配置"的临时模式，不是偏好。
    /// 重启该回到干净视图，否则藏桶就白藏了。
    pub show_hidden: HashSet<String>,
    stores: Arc<SharedStores>,
    pub engine: Engine,
    /// 引擎的事件流，主窗口起一个任务一直读它。
    pub engine_events: Option<async_channel::Receiver<EngineEvent>>,
}

impl AppState {
    pub fn new() -> Self {
        let path = config_path();
        let config = Config::load(&path);
        // 两个工作线程足够跑满几条 S3 连接，再多只是白占内存。
        // 阻塞线程池必须自己封顶：tokio 默认放到 512，而这里只有读写本地文件和
        // 算 MD5 会用到它，并发上限本来就是连接数，给 4 个够了。
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(4)
            .enable_all()
            .thread_name("s3browser-io")
            .build()
            .expect("建 tokio runtime");

        let stores = Arc::new(SharedStores::default());
        let db = data_dir().join("transfers.db");
        let job_store: Arc<dyn JobStore> = match SqliteJobStore::open(&db) {
            Ok(store) => Arc::new(store),
            Err(e) => {
                // 落不了盘也得让程序能用，只是重启后接不上
                tracing::error!("打开 {} 失败，改用内存队列：{e}", db.display());
                Arc::new(SqliteJobStore::in_memory().expect("内存队列"))
            }
        };
        let engine_config = engine_config(&config.transfer);
        let (engine, engine_events) = Engine::start(
            runtime.handle().clone(),
            job_store,
            stores.clone(),
            engine_config,
        );
        prewarm_stores(&config, &stores, &runtime);

        AppState {
            config,
            config_path: path,
            runtime,
            clients: HashMap::new(),
            buckets: HashMap::new(),
            expanded: HashMap::new(),
            show_hidden: HashSet::new(),
            stores,
            engine,
            engine_events: Some(engine_events),
        }
    }

    /// 设置改完之后把新参数送给引擎。
    ///
    /// 正在跑的任务不受影响，新的从下一个任务开始算；并发上限调大立刻生效，
    /// 调小要等手上的连接还回来。
    pub fn apply_transfer_config(&mut self) {
        self.config.transfer = std::mem::take(&mut self.config.transfer).clamped();
        self.engine
            .reconfigure(engine_config(&self.config.transfer));
        self.save_config();
    }

    pub fn save_config(&self) {
        if let Err(e) = self.config.save(&self.config_path) {
            tracing::error!("保存配置失败：{e}");
        }
    }

    /// 界面上零散的异步活儿（检查更新、下载新版）跑在这个 runtime 上。
    /// reqwest 离不开 tokio，别把它们扔到 GPUI 自己的执行器上。
    pub fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    /// 取账户的客户端，第一次用到时才建。
    pub fn client(&mut self, profile_id: &str) -> Option<Arc<S3Client>> {
        if let Some(client) = self.clients.get(profile_id) {
            return Some(client.clone());
        }
        let profile = self.config.profile(profile_id)?.clone();
        let client = build_client(&profile, self.runtime.handle().clone())?;
        let client = Arc::new(client);
        self.clients.insert(profile_id.to_owned(), client.clone());
        // 引擎那边也要能拿到同一个客户端
        self.stores
            .register(profile_id, Arc::new(S3ObjectStore::new(client.clone())));
        Some(client)
    }

    /// 已经建好的客户端，只读。渲染里想知道 endpoint 时用它，不会顺手建新的。
    pub fn peek_client(&self, profile_id: &str) -> Option<&Arc<S3Client>> {
        self.clients.get(profile_id)
    }

    /// 账户改过之后要把缓存的客户端丢掉，下次用新配置重建。
    pub fn invalidate(&mut self, profile_id: &str) {
        self.clients.remove(profile_id);
        self.buckets.remove(profile_id);
        self.stores.forget(profile_id);
    }

    /// 给账户对话框的"测试连接"用，不进缓存。
    pub fn probe_client(&self, profile: &Profile) -> Option<S3Client> {
        build_client(profile, self.runtime.handle().clone())
    }

    pub fn is_expanded(&self, profile_id: &str) -> bool {
        self.expanded.get(profile_id).copied().unwrap_or(false)
    }

    pub fn bucket_list(&self, profile_id: &str) -> BucketList {
        self.buckets.get(profile_id).cloned().unwrap_or_default()
    }

    pub fn shows_hidden(&self, profile_id: &str) -> bool {
        self.show_hidden.contains(profile_id)
    }
}

/// 启动时把所有账户的客户端先建出来注册进引擎的账户表。
///
/// 客户端原本是界面用到才建，但重启后引擎会立刻捡起没传完的任务——直接去
/// 传输页点「继续」时账户往往还没注册，任务只能干等。静态 S3 客户端不碰网络，
/// 当场建好进表。
fn prewarm_stores(config: &Config, stores: &Arc<SharedStores>, runtime: &tokio::runtime::Runtime) {
    for profile in &config.profiles {
        let Some(client) = build_client(profile, runtime.handle().clone()) else {
            continue;
        };
        stores.register(&profile.id, Arc::new(S3ObjectStore::new(Arc::new(client))));
    }
}

fn build_client(profile: &Profile, handle: tokio::runtime::Handle) -> Option<S3Client> {
    let endpoint = match Endpoint::parse(&profile.endpoint, &profile.region) {
        Ok(e) => e.with_addressing(profile.addressing.into()),
        Err(e) => {
            tracing::error!("账户 {} 的 endpoint 无效：{e}", profile.name);
            return None;
        }
    };
    let options = TransportOptions {
        insecure_tls: profile.insecure_tls,
        ..Default::default()
    };
    let transport = match ReqwestTransport::new(handle, options) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("账户 {} 建传输层失败：{e}", profile.name);
            return None;
        }
    };
    let credentials = Credentials::new(profile.access_key.clone(), profile.secret_key.clone());

    Some(
        S3Client::new(
            Arc::new(transport),
            Arc::new(StaticCredentials(credentials)),
            endpoint,
        )
        .with_payload_signing(profile.payload_signing.into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use s3browser::config::Addressing;

    fn profile(endpoint: &str) -> Profile {
        Profile {
            id: "p1".into(),
            name: "测试".into(),
            endpoint: endpoint.into(),
            region: "us-east-1".into(),
            access_key: "ak".into(),
            secret_key: "sk".into(),
            addressing: Addressing::Path,
            ..Default::default()
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn builds_client_from_profile() {
        let rt = runtime();
        let p = profile("http://127.0.0.1:9000");

        let client = build_client(&p, rt.handle().clone()).expect("应该建得出来");
        assert_eq!(client.region(), "us-east-1");
        assert_eq!(client.endpoint().host, "127.0.0.1");
        assert!(!client.endpoint().uses_virtual_host("anything"));
    }

    #[test]
    fn rejects_broken_endpoint() {
        let rt = runtime();
        let p = profile("");
        assert!(build_client(&p, rt.handle().clone()).is_none());
    }
}

/// 界面上那套以 MiB 为单位的设置，换算成引擎要的字节数。
fn engine_config(transfer: &TransferConfig) -> EngineConfig {
    const MIB: u64 = 1024 * 1024;
    // 两个框各自独立，字段之间的关系在这儿取齐：
    //   阈值比分片还小没意义——那等于所有文件都走分片，而装不满一片的文件
    //   分片上传纯属绕远路；
    //   一个任务同时跑的分片数也不可能超过总连接数。
    let threshold = transfer.multipart_threshold_mib.max(transfer.part_size_mib);
    let parts = transfer.max_parts_per_job.min(transfer.max_connections);
    EngineConfig {
        part_size: transfer.part_size_mib * MIB,
        multipart_threshold: threshold * MIB,
        // 下载没有 5 MiB 下限，但一份设置管两头更好解释
        download_chunk: transfer.part_size_mib * MIB,
        max_connections: transfer.max_connections,
        max_parts_per_job: parts,
        verify_download_md5: transfer.verify_download_md5,
        auto_resume: transfer.auto_resume,
        ..EngineConfig::default()
    }
}
