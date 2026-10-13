//! 协调器：排队、分配连接、汇总进度。
//!
//! 每个任务由一个驱动器负责。全局连接数用信号量控制，任务内的并发分片数
//! 由驱动器自己用 `buffer_unordered` 限制。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::Semaphore;

use crate::job::{
    Batch, BatchId, BatchProgress, HistoryEntry, Job, JobId, JobKind, JobProgress, JobState, now,
};
use crate::object_store::StoreResolver;
use crate::store::{HISTORY_KEEP, JobPatch, JobStore};

/// 进度快照的间隔。只在有活动任务时才发。
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(250);
/// 速率用 1 秒窗口算滑动平均。
const RATE_WINDOW: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub part_size: u64,
    pub multipart_threshold: u64,
    pub download_chunk: u64,
    pub max_connections: u32,
    pub max_parts_per_job: u32,
    pub verify_download_md5: bool,
    /// 上次退出时正在跑的任务，启动后是否自动接着传。
    pub auto_resume: bool,
    /// 分片上传的最小分片。S3 要求除最后一片外每片至少 5 MiB，
    /// 配置里填更小的值会被抬到这个数。测试里可以调小。
    pub min_upload_part_size: u64,
    /// 账户的客户端还没注册时，任务在队列里最多等多久才报「账户不可用」。
    /// 界面是惰性建客户端的，重启后引擎往往先到——得给它建出来的时间。
    pub resolve_timeout: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        const MIB: u64 = 1024 * 1024;
        EngineConfig {
            part_size: 8 * MIB,
            multipart_threshold: 16 * MIB,
            download_chunk: 8 * MIB,
            max_connections: 8,
            max_parts_per_job: 4,
            verify_download_md5: true,
            auto_resume: true,
            min_upload_part_size: crate::job::MIN_UPLOAD_PART_SIZE,
            resolve_timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Debug)]
pub enum EngineCommand {
    Enqueue {
        batch: Batch,
        jobs: Vec<Job>,
    },
    Pause(JobId),
    Resume(JobId),
    Cancel(JobId),
    Retry(JobId),
    PauseAll,
    ResumeAll,
    RetryFailed,
    ClearFinished,
    /// 换一套传输参数。设置改完立刻送过来。
    Reconfigure(Box<EngineConfig>),
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    Snapshot {
        jobs: Vec<JobProgress>,
        batches: Vec<BatchProgress>,
        total_rate_bps: u64,
    },
    JobState {
        id: JobId,
        state: JobState,
        error: Option<String>,
    },
    /// 一批任务全部落定（成功、失败或取消）。界面靠它报完成。
    BatchDone {
        id: BatchId,
        kind: JobKind,
        /// 批次标签，界面直接拿去当提示文案。
        label: String,
        done: u32,
        failed: u32,
        cancelled: u32,
    },
    /// 任务没入队——多半是任务库写入失败。之前这条路只写日志，
    /// 界面上无声无息，用户以为传上了其实什么都没发生。
    EnqueueFailed {
        error: String,
    },
    /// 启动时载入的全部历史，新到旧排好。
    History {
        entries: Vec<HistoryEntry>,
    },
    /// 一个任务刚落定，历史里添了一条。
    HistoryAppended {
        entry: HistoryEntry,
    },
}

/// 一个任务的运行期控制块。驱动器每跑完一个分片就看一眼这里。
pub struct JobControl {
    paused: AtomicBool,
    cancelled: AtomicBool,
    transferred: AtomicU64,
}

impl JobControl {
    fn new(transferred: u64) -> Self {
        JobControl {
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            transferred: AtomicU64::new(transferred),
        }
    }

    /// 测试里要现造一个处于某种状态的控制块。
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::new(0)
    }

    #[cfg(test)]
    pub(crate) fn set_paused(&self, value: bool) {
        self.paused.store(value, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn set_cancelled(&self, value: bool) {
        self.cancelled.store(value, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// 暂停或取消都该让驱动器停下来。
    pub fn should_stop(&self) -> bool {
        self.is_paused() || self.is_cancelled()
    }

    pub fn add_transferred(&self, bytes: u64) {
        self.transferred.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn transferred(&self) -> u64 {
        self.transferred.load(Ordering::Relaxed)
    }

    pub fn set_transferred(&self, bytes: u64) {
        self.transferred.store(bytes, Ordering::Relaxed);
    }
}

/// 驱动器跑完一个任务后回报的结果。
#[derive(Debug)]
pub enum JobOutcome {
    Done { etag: Option<String> },
    Paused,
    Cancelled,
    Failed(String),
}

pub(crate) struct JobRun {
    pub job: Job,
    pub control: Arc<JobControl>,
}

/// 引擎句柄。命令走通道，界面拿到的是事件流。
pub struct Engine {
    commands: async_channel::Sender<EngineCommand>,
}

impl Engine {
    /// 启动协调器。返回句柄和事件接收端。
    pub fn start(
        handle: Handle,
        store: Arc<dyn JobStore>,
        resolver: Arc<dyn StoreResolver>,
        config: EngineConfig,
    ) -> (Engine, async_channel::Receiver<EngineEvent>) {
        let (command_tx, command_rx) = async_channel::unbounded();
        // 事件用有界通道，界面卡住时不至于把内存吃光；满了就丢快照。
        let (event_tx, event_rx) = async_channel::bounded(64);

        let coordinator = Coordinator::new(store, resolver, config, event_tx, handle.clone());
        handle.spawn(coordinator.run(command_rx));

        (
            Engine {
                commands: command_tx,
            },
            event_rx,
        )
    }

    fn send(&self, command: EngineCommand) {
        // 协调器还没起来或已经退出时丢弃命令，不阻塞界面。
        if let Err(e) = self.commands.try_send(command) {
            tracing::warn!("传输命令没送到：{e}");
        }
    }

    pub fn enqueue(&self, batch: Batch, jobs: Vec<Job>) {
        self.send(EngineCommand::Enqueue { batch, jobs });
    }

    pub fn pause(&self, id: JobId) {
        self.send(EngineCommand::Pause(id));
    }

    pub fn resume(&self, id: JobId) {
        self.send(EngineCommand::Resume(id));
    }

    pub fn cancel(&self, id: JobId) {
        self.send(EngineCommand::Cancel(id));
    }

    pub fn retry(&self, id: JobId) {
        self.send(EngineCommand::Retry(id));
    }

    pub fn pause_all(&self) {
        self.send(EngineCommand::PauseAll);
    }

    pub fn resume_all(&self) {
        self.send(EngineCommand::ResumeAll);
    }

    pub fn retry_failed(&self) {
        self.send(EngineCommand::RetryFailed);
    }

    pub fn clear_finished(&self) {
        self.send(EngineCommand::ClearFinished);
    }

    /// 换一套传输参数。
    ///
    /// 已经在跑的任务不受影响——它们按老的分片大小切好了，中途换算法就续不上了。
    /// 新参数从下一个任务开始算数；并发上限是例外，见 `Coordinator::reconfigure`。
    pub fn reconfigure(&self, config: EngineConfig) {
        self.send(EngineCommand::Reconfigure(Box::new(config)));
    }

    pub fn shutdown(&self) {
        self.send(EngineCommand::Shutdown);
    }
}

/// 协调器里每个任务的运行期状态。
struct Tracked {
    job: Job,
    control: Arc<JobControl>,
    running: bool,
    /// 账户客户端还没注册时的等待期限。`None` 表示还没开始等。
    resolve_deadline: Option<std::time::Instant>,
    /// 上一次算速率时的已传字节和时刻。
    last_bytes: u64,
    last_at: std::time::Instant,
    rate_bps: u64,
}

struct Coordinator {
    store: Arc<dyn JobStore>,
    resolver: Arc<dyn StoreResolver>,
    config: EngineConfig,
    events: async_channel::Sender<EngineEvent>,
    handle: Handle,
    permits: Arc<Semaphore>,
    /// 并发上限调小之后还没收回来的许可数。见 `reconfigure`。
    permit_debt: usize,
    jobs: Vec<JobId>,
    tracked: HashMap<JobId, Tracked>,
    batches: HashMap<BatchId, Batch>,
    /// 驱动器跑完后把结果送回这里。
    outcomes: (
        async_channel::Sender<(JobId, JobOutcome)>,
        async_channel::Receiver<(JobId, JobOutcome)>,
    ),
}

impl Coordinator {
    fn new(
        store: Arc<dyn JobStore>,
        resolver: Arc<dyn StoreResolver>,
        config: EngineConfig,
        events: async_channel::Sender<EngineEvent>,
        handle: Handle,
    ) -> Self {
        let permits = Arc::new(Semaphore::new(config.max_connections.max(1) as usize));
        let permit_debt = 0;
        Coordinator {
            store,
            resolver,
            config,
            events,
            handle,
            permits,
            permit_debt,
            jobs: Vec::new(),
            tracked: HashMap::new(),
            batches: HashMap::new(),
            outcomes: async_channel::unbounded(),
        }
    }

    async fn run(mut self, commands: async_channel::Receiver<EngineCommand>) {
        self.load_from_store();
        self.load_history();
        // 上次退出时失败或暂停的任务不会自己跑起来，而快照定时器只在有任务在跑时
        // 才参与 select。不在这里主动发一次，传输视图就看不到它们，
        // 也就没法重试或者继续。
        self.emit_snapshot();

        let mut ticker = tokio::time::interval(SNAPSHOT_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            self.schedule();
            // 并发上限刚调小过的话，看看有没有空闲许可能收回来了
            self.settle_permit_debt();

            let has_active = self.tracked.values().any(|t| t.running);
            // 排着队但账户还没就绪的任务也得让 ticker 醒着：客户端注册进来的
            // 那一刻要靠下一轮 schedule 把它们捞起来，没人叫就永远睡过去了。
            let has_waiting = self
                .tracked
                .values()
                .any(|t| t.job.state == JobState::Queued);
            tokio::select! {
                command = commands.recv() => {
                    match command {
                        Ok(EngineCommand::Shutdown) | Err(_) => break,
                        Ok(command) => self.handle_command(command),
                    }
                }
                outcome = self.outcomes.1.recv() => {
                    if let Ok((id, outcome)) = outcome {
                        self.finish_job(id, outcome);
                    }
                }
                // 队列空闲（既没在跑也没在等账户）时这个分支不参与 select
                _ = ticker.tick(), if has_active || has_waiting => {
                    self.emit_snapshot();
                }
            }
        }
    }

    fn load_from_store(&mut self) {
        let loaded = match self.store.load_unfinished() {
            Ok(loaded) => loaded,
            Err(e) => {
                tracing::error!("加载未完成任务失败：{e}");
                return;
            }
        };
        for (mut job, parts) in loaded {
            let transferred: u64 = parts.iter().filter(|p| p.done).map(|p| p.len).sum();
            // 上次退出时在跑的任务：按配置决定接着传还是停在暂停态
            if job.state == JobState::Running {
                job.state = if self.config.auto_resume {
                    JobState::Queued
                } else {
                    JobState::Paused
                };
                let _ = self.store.update_job(&job.id, JobPatch::state(job.state));
            }
            let control = Arc::new(JobControl::new(transferred));
            if job.state == JobState::Paused {
                control.paused.store(true, Ordering::Relaxed);
            }
            self.jobs.push(job.id.clone());
            self.tracked.insert(
                job.id.clone(),
                Tracked {
                    job,
                    control,
                    running: false,
                    resolve_deadline: None,
                    last_bytes: transferred,
                    last_at: std::time::Instant::now(),
                    rate_bps: 0,
                },
            );
        }
        if !self.jobs.is_empty() {
            tracing::info!("载入 {} 个未完成任务", self.jobs.len());
        }
    }

    /// 启动时把历史捞出来给界面，顺手清掉超出保留量的老记录。
    fn load_history(&mut self) {
        if let Err(e) = self.store.prune_history(HISTORY_KEEP) {
            tracing::warn!("清理传输历史失败：{e}");
        }
        match self.store.load_history(HISTORY_KEEP) {
            Ok(entries) => {
                let _ = self.events.try_send(EngineEvent::History { entries });
            }
            Err(e) => tracing::warn!("载入传输历史失败：{e}"),
        }
    }

    fn handle_command(&mut self, command: EngineCommand) {
        match command {
            EngineCommand::Enqueue { batch, jobs } => self.enqueue(batch, jobs),
            EngineCommand::Pause(id) => self.set_paused(&id, true),
            EngineCommand::Resume(id) => self.set_paused(&id, false),
            EngineCommand::Cancel(id) => self.cancel(&id),
            EngineCommand::Retry(id) => self.retry(&id),
            EngineCommand::PauseAll => {
                for id in self.jobs.clone() {
                    self.set_paused(&id, true);
                }
            }
            EngineCommand::ResumeAll => {
                for id in self.jobs.clone() {
                    self.set_paused(&id, false);
                }
            }
            EngineCommand::RetryFailed => {
                let failed: Vec<JobId> = self
                    .tracked
                    .values()
                    .filter(|t| t.job.state == JobState::Failed)
                    .map(|t| t.job.id.clone())
                    .collect();
                for id in failed {
                    self.retry(&id);
                }
            }
            EngineCommand::ClearFinished => {
                // 「清除记录」：落定的任务从队列里走，历史整个清掉。
                // 失败的留着——还能重试，不算记录。
                let _ = self.store.clear_finished();
                let _ = self.store.clear_history();
                self.tracked
                    .retain(|_, t| !t.job.state.is_terminal() && t.job.state != JobState::Failed);
                self.jobs.retain(|id| self.tracked.contains_key(id));
                self.emit_snapshot();
                // 顺带把清空后的历史再推一份，历史视图当场变空
                self.load_history();
            }
            EngineCommand::Reconfigure(config) => self.reconfigure(*config),
            EngineCommand::Shutdown => {}
        }
    }

    fn enqueue(&mut self, batch: Batch, jobs: Vec<Job>) {
        if let Err(e) = self.store.insert_batch(&batch, &jobs) {
            // 只写日志的话界面上无声无息，用户以为传上了。报给界面弹个提示。
            tracing::error!("任务入库失败：{e}");
            let _ = self.events.try_send(EngineEvent::EnqueueFailed {
                error: e.to_string(),
            });
            return;
        }
        self.batches.insert(batch.id.clone(), batch);
        // 新任务插到队头：传输列表按这份顺序渲染，最新的永远在第一行，
        // 不用滚到底去找刚传的那个。
        let mut new_ids = Vec::with_capacity(jobs.len());
        for job in jobs {
            // 已经在跟的任务不能再收一遍：那会把它的 JobControl 换掉，
            // 正在跑的那个任务就再也收不到暂停和取消，schedule 还可能把它又起一份。
            // 启动时从库里捞回来的任务和随后送进来的同一批任务就会撞上这条。
            if self.tracked.contains_key(&job.id) {
                tracing::debug!(id = %job.id, "任务已在队列里，忽略重复入队");
                continue;
            }
            let id = job.id.clone();
            let control = Arc::new(JobControl::new(0));
            self.tracked.insert(
                id.clone(),
                Tracked {
                    job,
                    control,
                    running: false,
                    resolve_deadline: None,
                    last_bytes: 0,
                    last_at: std::time::Instant::now(),
                    rate_bps: 0,
                },
            );
            new_ids.push(id);
        }
        self.jobs.splice(0..0, new_ids);
        self.emit_snapshot();
    }

    fn set_paused(&mut self, id: &str, paused: bool) {
        let Some(tracked) = self.tracked.get_mut(id) else {
            return;
        };
        if tracked.job.state.is_terminal() {
            return;
        }
        tracked.control.paused.store(paused, Ordering::Relaxed);
        if paused {
            // 正在跑的分片停下来之后驱动器才回报 Paused。那之前状态还是
            // Running，界面靠快照里的 `pausing` 立刻给个反馈。
            if !tracked.running {
                tracked.job.state = JobState::Paused;
                let _ = self.store.update_job(id, JobPatch::state(JobState::Paused));
                let state = tracked.job.state;
                self.emit_state(id, state, None);
            }
        } else if tracked.job.state == JobState::Paused {
            tracked.job.state = JobState::Queued;
            let _ = self.store.update_job(id, JobPatch::state(JobState::Queued));
            self.emit_state(id, JobState::Queued, None);
        }
        // 立刻推一份快照：不然要等下一个 250 毫秒的滴答，按钮才会变
        self.emit_snapshot();
    }

    fn cancel(&mut self, id: &str) {
        let Some(tracked) = self.tracked.get_mut(id) else {
            return;
        };
        tracked.control.cancelled.store(true, Ordering::Relaxed);
        if !tracked.running {
            // 没在跑就直接收尾，正在跑的等驱动器回报
            tracked.job.state = JobState::Cancelled;
            let _ = self
                .store
                .update_job(id, JobPatch::state(JobState::Cancelled));
            self.emit_state(id, JobState::Cancelled, None);
        }
    }

    fn retry(&mut self, id: &str) {
        let Some(tracked) = self.tracked.get_mut(id) else {
            return;
        };
        if tracked.job.state != JobState::Failed {
            return;
        }
        tracked.job.state = JobState::Queued;
        tracked.job.error = None;
        tracked.control.paused.store(false, Ordering::Relaxed);
        tracked.control.cancelled.store(false, Ordering::Relaxed);
        // 重试给一轮全新的账户等待期——上次超时不代表这次也等不到
        tracked.resolve_deadline = None;
        let _ = self
            .store
            .update_job(id, JobPatch::state(JobState::Queued).clear_error());
        self.emit_state(id, JobState::Queued, None);
    }

    /// 按创建顺序启动排队中的任务，直到没有空闲连接。
    fn schedule(&mut self) {
        let ids: Vec<JobId> = self.jobs.clone();
        for id in ids {
            if self.permits.available_permits() == 0 {
                break;
            }
            let Some(tracked) = self.tracked.get_mut(&id) else {
                continue;
            };
            if tracked.running || tracked.job.state != JobState::Queued {
                continue;
            }
            if tracked.control.should_stop() {
                continue;
            }
            let Some(store) = self.resolver.store_for(&tracked.job.profile_id) else {
                // 账户的客户端还没注册。界面上客户端是惰性建的，重启后引擎
                // 往往先到：留在队列里等它，别一上来就判死。真等不到
                // （账户被删了、endpoint 连不上）超时再报不迟。
                let deadline = tracked
                    .resolve_deadline
                    .get_or_insert_with(|| std::time::Instant::now() + self.config.resolve_timeout);
                if std::time::Instant::now() < *deadline {
                    continue;
                }
                tracked.job.state = JobState::Failed;
                let message = "账户不可用".to_owned();
                tracked.job.error = Some(message.clone());
                let _ = self.store.update_job(&id, JobPatch::failed(message.clone()));
                self.emit_state(&id, JobState::Failed, Some(message));
                self.emit_snapshot();
                continue;
            };
            tracked.resolve_deadline = None;

            tracked.running = true;
            tracked.job.state = JobState::Running;
            tracked.last_at = std::time::Instant::now();
            tracked.last_bytes = tracked.control.transferred();
            let run = JobRun {
                job: tracked.job.clone(),
                control: tracked.control.clone(),
            };

            let _ = self
                .store
                .update_job(&id, JobPatch::state(JobState::Running));
            self.emit_state(&id, JobState::Running, None);

            let outcomes = self.outcomes.0.clone();
            let job_store = self.store.clone();
            let config = self.config.clone();
            let permits = self.permits.clone();
            let job_id = id.clone();

            self.handle.spawn(async move {
                let outcome = crate::driver::run_job(run, store, job_store, config, permits).await;
                let _ = outcomes.send((job_id, outcome)).await;
            });
        }
    }

    fn finish_job(&mut self, id: JobId, outcome: JobOutcome) {
        let Some(tracked) = self.tracked.get_mut(&id) else {
            return;
        };
        tracked.running = false;
        tracked.rate_bps = 0;

        let (state, error) = match outcome {
            JobOutcome::Done { etag } => {
                if let Some(etag) = etag {
                    let _ = self
                        .store
                        .update_job(&id, JobPatch::default().with_etag(Some(etag)));
                }
                // 进度条收口到 100%
                tracked.control.set_transferred(tracked.job.size);
                (JobState::Done, None)
            }
            JobOutcome::Paused => (JobState::Paused, None),
            JobOutcome::Cancelled => (JobState::Cancelled, None),
            JobOutcome::Failed(message) => (JobState::Failed, Some(message)),
        };

        tracked.job.state = state;
        tracked.job.error = error.clone();
        tracked.job.updated_at = now();
        let patch = match &error {
            Some(message) => JobPatch::failed(message.clone()),
            None => JobPatch::state(state).clear_error(),
        };
        let _ = self.store.update_job(&id, patch);
        let batch_id = tracked.job.batch_id.clone();
        self.record_history(&id, state);
        self.emit_state(&id, state, error);
        self.check_batch_done(&batch_id);
        self.emit_snapshot();
    }

    /// 落定的任务记进历史。历史库写坏了不拦着传输本身，记条日志就算。
    fn record_history(&self, id: &str, state: JobState) {
        if !matches!(state, JobState::Done | JobState::Failed | JobState::Cancelled) {
            return;
        }
        let Some(tracked) = self.tracked.get(id) else {
            return;
        };
        let entry = HistoryEntry {
            job_id: tracked.job.id.clone(),
            kind: tracked.job.kind,
            bucket: tracked.job.bucket.clone(),
            key: tracked.job.key.clone(),
            size: tracked.job.size,
            state,
            error: tracked.job.error.clone(),
            created_at: tracked.job.created_at,
            finished_at: now(),
        };
        if let Err(e) = self.store.insert_history(&entry) {
            tracing::warn!("写传输历史失败：{e}");
            return;
        }
        let _ = self.events.try_send(EngineEvent::HistoryAppended { entry });
    }

    /// 换一套参数。
    ///
    /// 调大并发立刻生效。调小只收得走当下空闲的许可：正在跑的那些是任务
    /// 自己攥着的，抢不回来，差额记在账上，等它们还回来再收。所以"从 8 调到 2"
    /// 是慢慢收敛到 2，不是立刻掐断正在传的东西。
    fn reconfigure(&mut self, config: EngineConfig) {
        let old = self.config.max_connections.max(1) as usize;
        let new = config.max_connections.max(1) as usize;
        match new.cmp(&old) {
            std::cmp::Ordering::Greater => self.permits.add_permits(new - old),
            std::cmp::Ordering::Less => {
                self.permit_debt += old - new;
                self.settle_permit_debt();
            }
            std::cmp::Ordering::Equal => {}
        }
        tracing::info!(
            connections = new,
            part_size = config.part_size,
            "换了一套传输参数"
        );
        self.config = config;
    }

    /// 把欠着的许可收回来。每轮循环都试一次，正在跑的任务还回许可就能收掉。
    fn settle_permit_debt(&mut self) {
        if self.permit_debt == 0 {
            return;
        }
        self.permit_debt -= self.permits.forget_permits(self.permit_debt);
    }

    fn check_batch_done(&mut self, batch_id: &str) {
        let mut total = 0u32;
        let mut done = 0u32;
        let mut failed = 0u32;
        let mut cancelled = 0u32;
        let mut kind = None;
        for tracked in self.tracked.values() {
            if tracked.job.batch_id != batch_id {
                continue;
            }
            total += 1;
            kind.get_or_insert(tracked.job.kind);
            match tracked.job.state {
                JobState::Done => done += 1,
                JobState::Failed => failed += 1,
                JobState::Cancelled => cancelled += 1,
                _ => {}
            }
        }
        if total == 0 || done + failed + cancelled < total {
            return;
        }
        // 批次记录里的 kind 最准；续传回来的任务可能还没建批次记录，
        // 那就退回用任务自己的 kind
        let batch = self.batches.get(batch_id);
        let kind = batch.map(|b| b.kind).or(kind).unwrap_or(JobKind::Upload);
        let label = batch
            .map(|b| b.label.clone())
            .unwrap_or_else(|| kind.label().to_owned());
        let _ = self.events.try_send(EngineEvent::BatchDone {
            id: batch_id.to_owned(),
            kind,
            label,
            done,
            failed,
            cancelled,
        });
    }

    fn emit_state(&self, id: &str, state: JobState, error: Option<String>) {
        let _ = self.events.try_send(EngineEvent::JobState {
            id: id.to_owned(),
            state,
            error,
        });
    }

    fn emit_snapshot(&mut self) {
        let elapsed_ok = |at: std::time::Instant| at.elapsed() >= RATE_WINDOW;
        let mut total_rate = 0u64;

        for tracked in self.tracked.values_mut() {
            if !tracked.running {
                tracked.rate_bps = 0;
                continue;
            }
            let bytes = tracked.control.transferred();
            if elapsed_ok(tracked.last_at) {
                let secs = tracked.last_at.elapsed().as_secs_f64().max(0.001);
                let delta = bytes.saturating_sub(tracked.last_bytes);
                tracked.rate_bps = (delta as f64 / secs) as u64;
                tracked.last_bytes = bytes;
                tracked.last_at = std::time::Instant::now();
            }
            total_rate += tracked.rate_bps;
        }

        let jobs: Vec<JobProgress> = self
            .jobs
            .iter()
            .filter_map(|id| self.tracked.get(id))
            .map(|t| JobProgress {
                id: t.job.id.clone(),
                batch_id: t.job.batch_id.clone(),
                kind: t.job.kind,
                name: t.job.display_name().to_owned(),
                state: t.job.state,
                created_at: t.job.created_at,
                // 按了暂停但分片还在跑
                pausing: t.running && t.control.is_paused(),
                transferred: t.control.transferred(),
                total: t.job.size,
                rate_bps: t.rate_bps,
                error: t.job.error.clone(),
            })
            .collect();

        let mut batches: HashMap<BatchId, BatchProgress> = HashMap::new();
        for tracked in self.tracked.values() {
            let batch = batches
                .entry(tracked.job.batch_id.clone())
                .or_insert_with(|| BatchProgress {
                    id: tracked.job.batch_id.clone(),
                    label: self
                        .batches
                        .get(&tracked.job.batch_id)
                        .map(|b| b.label.clone())
                        .unwrap_or_default(),
                    kind: tracked.job.kind,
                    total_jobs: 0,
                    done_jobs: 0,
                    failed_jobs: 0,
                    transferred: 0,
                    total: 0,
                });
            batch.total_jobs += 1;
            batch.total += tracked.job.size;
            batch.transferred += tracked.control.transferred();
            match tracked.job.state {
                JobState::Done => batch.done_jobs += 1,
                JobState::Failed => batch.failed_jobs += 1,
                _ => {}
            }
        }

        let _ = self.events.try_send(EngineEvent::Snapshot {
            jobs,
            batches: batches.into_values().collect(),
            total_rate_bps: total_rate,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_reports_stop_on_pause_or_cancel() {
        let control = JobControl::new(0);
        assert!(!control.should_stop());

        control.paused.store(true, Ordering::Relaxed);
        assert!(control.should_stop());
        control.paused.store(false, Ordering::Relaxed);
        assert!(!control.should_stop());

        control.cancelled.store(true, Ordering::Relaxed);
        assert!(control.should_stop());
    }

    #[test]
    fn control_accumulates_progress() {
        let control = JobControl::new(100);
        assert_eq!(control.transferred(), 100);
        control.add_transferred(50);
        assert_eq!(control.transferred(), 150);
        control.set_transferred(400);
        assert_eq!(control.transferred(), 400);
    }

    #[test]
    fn default_config_matches_the_spec() {
        const MIB: u64 = 1024 * 1024;
        let config = EngineConfig::default();
        assert_eq!(config.part_size, 8 * MIB);
        assert_eq!(config.multipart_threshold, 16 * MIB);
        assert_eq!(config.download_chunk, 8 * MIB);
        assert_eq!(config.max_connections, 8);
        assert_eq!(config.max_parts_per_job, 4);
        assert_eq!(config.min_upload_part_size, 5 * MIB);
    }
}
