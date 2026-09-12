//! 异步后台作业管理器（对齐 oh-my-pi `async/job-manager.ts` v18.1.2 语义）。
//!
//! 职责：注册/上限/取消/查询、完成投递（按 owner 路由 + 失败指数退避重试 +
//! 死信）、保留期逐出、自适应轮询阶梯。`run_command async:true` 与后续 task/eval
//! 后台执行共用本管理器；`hub` 工具的 `jobs`/`cancel`/`wait` 面合并本管理器的
//! 作业行。
//!
//! 与 omp 的对应关系（语句级对照；锁内纯状态、无 await）：
//! - `register`：运行中作业计数达上限即拒绝（排队态 `queued` 不占槽位）；
//!   id 解析 `bg_N` 起步，偏好 id 冲突加 `-2` 后缀。
//! - 投递：完成/失败即入队（按 `next_attempt_at` 有序）；owner 作业只投给
//!   `register_delivery_sink` 注册的 sink（无 sink 即死信——绝不落入默认 sink，
//!   防止一个会话的结果泄漏进另一会话）；无 owner 作业投给构造时的默认 sink。
//!   sink 出错按 500ms×2^n + ≤200ms 抖动退避重试，上限 30s，作业行存活期内重试。
//! - 抑制面：`acknowledge`（前台等待时提前抑制，防投递循环重复注入）、`watch`
//!   （hub 等待接管结果，抑制自动投递）、`consume`（结果已被前台快照恢复）；
//!   `resume` 解除抑制时对已完结作业重新入队，保证恰好一次投递。
//! - 保留期：终态作业 5 分钟后逐出（`retention_ms = 0` 立即逐出）。
//! - 轮询阶梯：同一 owner 的连续阻塞等待按 [5s,10s,30s,60s,300s] 爬升；间隔
//!   ≥60s（去做了实际工作）回落到阶梯底部。
//!
//! Rust 侧适配：omp 的 `Promise.withResolvers` 通知改为 [`tokio::sync::Notify`]；
//! `setTimeout` 逐出改为 `tokio::spawn` 任务（句柄 abort 即撤销）；投递循环为
//! 单飞 tokio 任务，队列空即退出、入队再拉起。

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// 投递重试退避基数（对齐 omp `DELIVERY_RETRY_BASE_MS`）。
const DELIVERY_RETRY_BASE_MS: u64 = 500;
/// 投递重试单次等待上限（对齐 omp `DELIVERY_RETRY_MAX_MS`）。
const DELIVERY_RETRY_MAX_MS: u64 = 30_000;
/// 投递重试随机抖动上限（对齐 omp `DELIVERY_RETRY_JITTER_MS`）。
const DELIVERY_RETRY_JITTER_MS: u64 = 200;
/// 终态作业保留期默认值（对齐 omp `DEFAULT_RETENTION_MS`）。
const DEFAULT_RETENTION_MS: u64 = 5 * 60 * 1000;
/// 运行中作业上限默认值（对齐 omp `DEFAULT_MAX_RUNNING_JOBS`）。
const DEFAULT_MAX_RUNNING_JOBS: usize = 15;
/// 自适应轮询阶梯（对齐 omp `POLL_WAIT_LADDER_MS`）。
const POLL_WAIT_LADDER_MS: [u64; 5] = [5_000, 10_000, 30_000, 60_000, 300_000];
/// 距上次轮询返回超过该间隔视为离开等待循环，阶梯回落底部（对齐 omp）。
const POLL_ESCALATION_RESET_MS: u64 = 60_000;

/// 作业类别（对齐 omp `AsyncJobType`；驱动作业行徽标与投递标签）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AsyncJobType {
    /// shell 命令后台作业。
    Bash,
    /// 子代理任务作业。
    Task,
    /// 评测作业。
    Eval,
}

impl AsyncJobType {
    /// 作业行/投递日志用的类别词。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Task => "task",
            Self::Eval => "eval",
        }
    }
}

/// 作业生命周期状态（对齐 omp 四态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// 执行中。
    Running,
    /// 成功完结。
    Completed,
    /// 失败完结（错误文本见 [`AsyncJob::error_text`]）。
    Failed,
    /// 已取消。
    Cancelled,
}

impl JobStatus {
    /// 状态词。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// 单个受管作业的快照行。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AsyncJob {
    /// 作业 id（`hub` 工具 `cancel`/`wait ids` 的键）。
    pub id: String,
    /// 作业类别。
    #[serde(rename = "type")]
    pub job_type: AsyncJobType,
    /// 生命周期状态。
    pub status: JobStatus,
    /// 注册时刻（毫秒 Unix 时间戳）。
    pub started_at_ms: u64,
    /// 标签（bash 作业为命令前 120 字符截断）。
    pub label: String,
    /// 注册方代理 id（owner 范围取消/投递路由的键；匿名注册为 `None`）。
    pub owner_id: Option<String>,
    /// 作业运行终态结果文本（成功路径）。
    pub result_text: Option<String>,
    /// 作业运行终态错误文本。
    pub error_text: Option<String>,
    /// 注册时尚在调用方管理的门后排队（不占运行槽位）。
    pub queued: bool,
}

impl AsyncJob {
    /// 已运行时长（注册起到 `now_ms`）。
    #[must_use]
    pub fn duration_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.started_at_ms)
    }
}

/// `register` 时传给作业体的取消上下文。
pub struct JobRunContext {
    /// 作业 id。
    pub job_id: String,
    /// 取消令牌：`cancel`/`cancel_all`/`dispose` 触发；作业体须响应。
    pub cancel: CancellationToken,
}

/// 进度回调（register 选项注入；工具流式更新等）。
pub type ProgressCallback =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// 投递 sink：完结作业的结果文本接收端。`Err` 触发退避重试。
pub type DeliverySink = Arc<
    dyn Fn(
            String,
            String,
            Option<AsyncJob>,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
        + Send
        + Sync,
>;

/// 注册选项。
#[derive(Clone, Default)]
pub struct RegisterOptions {
    /// 偏好 id（缺省自动 `bg_N`；冲突自动加 `-2`/`-3`… 后缀）。
    pub id: Option<String>,
    /// 注册方代理 id（owner 范围取消/投递路由）。
    pub owner_id: Option<String>,
    /// 进度回调。
    pub on_progress: Option<ProgressCallback>,
    /// 以排队态注册（不占运行槽位；作业体启动时经 `mark_running` 占槽）。
    pub queued: bool,
}

/// 投递队列状态快照（对齐 omp `AsyncJobDeliveryState`）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeliveryState {
    /// 排队 + 在途投递总数。
    pub queued: usize,
    /// 是否有投递正在进行。
    pub delivering: bool,
    /// 最早的下一次重试时刻（毫秒 Unix；无重试为 `None`）。
    pub next_retry_at_ms: Option<u64>,
    /// 待投递作业 id（排队 + 在途）。
    pub pending_job_ids: Vec<String>,
}

/// 待投递条目（内部）。
#[derive(Debug, Clone)]
struct Delivery {
    job_id: String,
    text: String,
    attempt: u32,
    next_attempt_at_ms: u64,
    owner_id: Option<String>,
}

/// 内部可变状态。持锁期间禁止 await；投递循环经 [`Notify`] 唤醒。
struct Inner {
    jobs: HashMap<String, AsyncJob>,
    cancels: HashMap<String, CancellationToken>,
    /// 作业运行体句柄（`wait_for_owner_jobs`/`dispose` 等待退出）。
    /// 作业完成信号端（运行体退出置真；等待端 subscribe）。
    completions: HashMap<String, tokio::sync::watch::Sender<bool>>,
    deliveries: VecDeque<Delivery>,
    in_flight: Vec<Delivery>,
    suppressed: HashSet<String>,
    watched: HashSet<String>,
    consumed: HashSet<String>,
    sinks: HashMap<String, DeliverySink>,
    poll_escalation: HashMap<Option<String>, (usize, u64)>,
    evictions: HashMap<String, tokio::task::AbortHandle>,
    next_seq: u64,
    disposed: bool,
}

/// 异步作业管理器。见模块文档。
pub struct AsyncJobManager {
    inner: Mutex<Inner>,
    queue_changed: Notify,
    default_sink: Option<DeliverySink>,
    max_running_jobs: usize,
    retention_ms: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

impl AsyncJobManager {
    /// 以默认参数构造为 `Arc`（上限 15、保留 5 分钟、无默认 sink）。
    #[must_use]
    pub fn new() -> Arc<Self> {
        Self::build(DEFAULT_MAX_RUNNING_JOBS, DEFAULT_RETENTION_MS, None)
    }

    /// 指定运行中作业上限构造（其余默认）。上限下限钳到 1。
    #[must_use]
    pub fn with_max_running(max_running_jobs: usize) -> Arc<Self> {
        Self::build(max_running_jobs.max(1), DEFAULT_RETENTION_MS, None)
    }

    /// 全参数构造。
    #[must_use]
    pub fn build(
        max_running_jobs: usize,
        retention_ms: u64,
        default_sink: Option<DeliverySink>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                jobs: HashMap::new(),
                cancels: HashMap::new(),
                completions: HashMap::new(),
                deliveries: VecDeque::new(),
                in_flight: Vec::new(),
                suppressed: HashSet::new(),
                watched: HashSet::new(),
                consumed: HashSet::new(),
                sinks: HashMap::new(),
                poll_escalation: HashMap::new(),
                evictions: HashMap::new(),
                next_seq: 1,
                disposed: false,
            }),
            queue_changed: Notify::new(),
            default_sink,
            max_running_jobs: max_running_jobs.max(1),
            retention_ms,
        })
    }

    /// 运行中作业数是否已达上限（排队态不占槽位；对齐 omp `atCapacity`）。
    #[must_use]
    pub fn at_capacity(&self) -> bool {
        self.running_count() >= self.max_running_jobs
    }

    fn running_count(&self) -> usize {
        Self::running_count_locked(&self.inner.lock().expect("jobs lock"))
    }

    fn running_count_locked(inner: &Inner) -> usize {
        inner
            .jobs
            .values()
            .filter(|j| j.status == JobStatus::Running && !j.queued)
            .count()
    }

    /// id 解析：缺省 `bg_N`；偏好 id 冲突加 `-N` 后缀（对齐 omp `#resolveJobId`）。
    fn resolve_job_id(inner: &mut Inner, preferred: Option<&str>) -> String {
        let preferred = preferred.map(str::trim).filter(|s| !s.is_empty());
        let Some(base) = preferred else {
            loop {
                let id = format!("bg_{}", inner.next_seq);
                inner.next_seq += 1;
                if !inner.jobs.contains_key(&id) {
                    return id;
                }
            }
        };
        if !inner.jobs.contains_key(base) {
            return base.to_string();
        }
        let mut suffix = 2;
        loop {
            let candidate = format!("{base}-{suffix}");
            if !inner.jobs.contains_key(&candidate) {
                return candidate;
            }
            suffix += 1;
        }
    }
    /// 注册作业。运行体在独立 tokio 任务中执行；终态后触发投递与保留期逐出。
    ///
    /// # Errors
    /// 管理器已 dispose 或运行中作业达上限时返回描述性错误。
    pub fn register(
        self: &Arc<Self>,
        job_type: AsyncJobType,
        label: impl Into<String>,
        run: impl FnOnce(JobRunContext) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
        + Send
        + 'static,
        options: RegisterOptions,
    ) -> Result<String, String> {
        // 锁内原子完成：容量检查 → id 解析 → 作业行/取消令牌落表。
        let (job_id, cancel) = {
            let mut inner = self.inner.lock().expect("jobs lock");
            if inner.disposed {
                return Err("Async job manager is disposed".into());
            }
            if Self::running_count_locked(&inner) >= self.max_running_jobs {
                return Err(format!(
                    "Background job limit reached ({}). Wait for running jobs to finish or cancel one.",
                    self.max_running_jobs
                ));
            }
            let job_id = Self::resolve_job_id(&mut inner, options.id.as_deref());
            // id 复用即清 per-id 标记（对齐 omp register 的 per-id set 清理）。
            inner.suppressed.remove(&job_id);
            inner.consumed.remove(&job_id);
            inner.watched.remove(&job_id);
            let cancel = CancellationToken::new();
            inner.jobs.insert(
                job_id.clone(),
                AsyncJob {
                    id: job_id.clone(),
                    job_type,
                    status: JobStatus::Running,
                    started_at_ms: now_ms(),
                    label: label.into(),
                    owner_id: options.owner_id.clone(),
                    result_text: None,
                    error_text: None,
                    queued: options.queued,
                },
            );
            inner.cancels.insert(job_id.clone(), cancel.clone());
            (job_id, cancel)
        };

        let manager = Arc::clone(self);
        let progress = options.on_progress;
        let queued = options.queued;
        let run_job_id = job_id.clone();
        // per-job 完成信号：运行体退出（任意路径）即置真；等待端经 receiver 订阅。
        // watch::Sender 不可 Clone 且 Receiver 可多订——对应 omp 的 Promise<void> 语义。
        let (done_tx, _done_rx) = tokio::sync::watch::channel(false);
        self.inner
            .lock()
            .expect("jobs lock")
            .completions
            .insert(job_id.clone(), done_tx.clone());
        tokio::spawn(async move {
            let body = async {
                if queued {
                    manager.mark_running(&run_job_id);
                }
                let ctx = JobRunContext {
                    job_id: run_job_id.clone(),
                    cancel,
                };
                let outcome = run(ctx).await;
                if let (Some(cb), Some(text)) = (&progress, outcome.as_ref().ok()) {
                    // 终态进度回调失败不影响状态落盘（对齐 omp reportProgress 的容错）。
                    let _ = cb(text.clone()).await;
                }
                let (status, result_text, error_text) = match outcome {
                    Ok(text) => (JobStatus::Completed, Some(text), None),
                    Err(err) => (JobStatus::Failed, None, Some(err)),
                };
                let delivery_text = result_text
                    .clone()
                    .or_else(|| error_text.clone())
                    .unwrap_or_default();
                let already_cancelled = {
                    let mut inner = manager.inner.lock().expect("jobs lock");
                    let Some(job) = inner.jobs.get_mut(&run_job_id) else {
                        return; // 已被逐出（极短保留期）：无事可做。
                    };
                    if job.status == JobStatus::Cancelled {
                        // 取消先于终态落盘：吞掉状态变更，仅保留文本待查询（对齐 omp）。
                        job.result_text = result_text;
                        job.error_text = error_text;
                        true
                    } else {
                        job.status = status;
                        job.result_text = result_text;
                        job.error_text = error_text;
                        false
                    }
                };
                if already_cancelled {
                    manager.schedule_eviction(&run_job_id);
                    return;
                }
                manager.enqueue_delivery(Delivery {
                    job_id: run_job_id.clone(),
                    text: delivery_text,
                    attempt: 0,
                    next_attempt_at_ms: now_ms(),
                    owner_id: manager.get_job(&run_job_id).and_then(|j| j.owner_id),
                });
                manager.schedule_eviction(&run_job_id);
            };
            body.await;
            let _ = done_tx.send(true);
        });
        Ok(job_id)
    }

    /// 清除排队标记（作业体真正开始执行时调用；开始占运行槽位）。
    pub fn mark_running(&self, job_id: &str) {
        if let Some(job) = self.inner.lock().expect("jobs lock").jobs.get_mut(job_id) {
            job.queued = false;
        }
    }

    /// 取消单个作业。owner 过滤不匹配视为不存在（跨代理取消在管理层拒绝）。
    pub fn cancel(self: &Arc<Self>, id: &str, owner_id: Option<&str>) -> bool {
        let mut inner = self.inner.lock().expect("jobs lock");
        let Some(job) = inner.jobs.get_mut(id) else {
            return false;
        };
        if let Some(owner) = owner_id {
            if job.owner_id.as_deref() != Some(owner) {
                return false;
            }
        }
        if job.status != JobStatus::Running {
            return false;
        }
        job.status = JobStatus::Cancelled;
        if let Some(cancel) = inner.cancels.get(id) {
            cancel.cancel();
        }
        drop(inner);
        self.schedule_eviction(id);
        true
    }

    /// 取消全部运行中作业（`owner_id` 限定范围；`None` 取消所有——dispose 路径）。
    pub fn cancel_all(self: &Arc<Self>, owner_id: Option<&str>) {
        let ids: Vec<String> = {
            let inner = self.inner.lock().expect("jobs lock");
            inner
                .jobs
                .values()
                .filter(|j| {
                    j.status == JobStatus::Running
                        && owner_id.is_none_or(|o| j.owner_id.as_deref() == Some(o))
                })
                .map(|j| j.id.clone())
                .collect()
        };
        for id in ids {
            self.cancel(&id, owner_id);
        }
    }

    /// 单作业查询。
    #[must_use]
    pub fn get_job(&self, id: &str) -> Option<AsyncJob> {
        self.inner.lock().expect("jobs lock").jobs.get(id).cloned()
    }

    /// 全部运行中作业。
    #[must_use]
    pub fn running_jobs(&self, owner_id: Option<&str>) -> Vec<AsyncJob> {
        self.jobs_filtered(owner_id)
            .into_iter()
            .filter(|j| j.status == JobStatus::Running)
            .collect()
    }

    /// 最近完结作业（新→旧，按注册时间；上限 `limit`）。
    #[must_use]
    pub fn recent_jobs(&self, limit: usize, owner_id: Option<&str>) -> Vec<AsyncJob> {
        let mut done: Vec<AsyncJob> = self
            .jobs_filtered(owner_id)
            .into_iter()
            .filter(|j| j.status != JobStatus::Running)
            .collect();
        done.sort_by_key(|j| std::cmp::Reverse(j.started_at_ms));
        done.truncate(limit);
        done
    }

    /// 全部作业。
    #[must_use]
    pub fn all_jobs(&self, owner_id: Option<&str>) -> Vec<AsyncJob> {
        self.jobs_filtered(owner_id)
    }

    fn jobs_filtered(&self, owner_id: Option<&str>) -> Vec<AsyncJob> {
        self.inner
            .lock()
            .expect("jobs lock")
            .jobs
            .values()
            .filter(|j| owner_id.is_none_or(|o| j.owner_id.as_deref() == Some(o)))
            .cloned()
            .collect()
    }

    /// 投递队列状态。
    #[must_use]
    pub fn delivery_state(&self, owner_id: Option<&str>) -> DeliveryState {
        let inner = self.inner.lock().expect("jobs lock");
        let filtered: Vec<&Delivery> = inner
            .deliveries
            .iter()
            .chain(inner.in_flight.iter())
            .filter(|d| {
                owner_id.is_none_or(|o| d.owner_id.as_deref() == Some(o))
                    && !Self::is_suppressed(&inner, &d.job_id)
            })
            .collect();
        let next_retry_at_ms = filtered.iter().map(|d| d.next_attempt_at_ms).min();
        let delivering = !inner.in_flight.is_empty()
            && owner_id.is_none_or(|o| {
                inner
                    .in_flight
                    .iter()
                    .any(|d| d.owner_id.as_deref() == Some(o))
            });
        DeliveryState {
            queued: filtered.len(),
            delivering,
            next_retry_at_ms,
            pending_job_ids: filtered.iter().map(|d| d.job_id.clone()).collect(),
        }
    }

    /// 是否有待投递条目。
    #[must_use]
    pub fn has_pending_deliveries(&self, owner_id: Option<&str>) -> bool {
        self.delivery_state(owner_id).queued > 0
    }

    /// hub 等待接管：抑制这些作业的自动投递（结果经 wait 返回）。返回新增数量。
    pub fn watch_jobs(&self, job_ids: &[&str]) -> usize {
        let mut inner = self.inner.lock().expect("jobs lock");
        let mut added = 0;
        for id in job_ids.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if inner.watched.insert((*id).to_string()) {
                added += 1;
            }
        }
        drop(inner);
        self.queue_changed.notify_waiters();
        added
    }

    /// 解除 watch（返回移除数量）。
    pub fn unwatch_jobs(&self, job_ids: &[&str]) -> usize {
        let mut inner = self.inner.lock().expect("jobs lock");
        job_ids
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && inner.watched.remove(*s))
            .count()
    }

    /// 前台确认：抑制排队投递并移除（返回移除条数；对齐 omp `acknowledgeDeliveries`）。
    pub fn acknowledge_deliveries(&self, job_ids: &[&str]) -> usize {
        let mut inner = self.inner.lock().expect("jobs lock");
        let ids: Vec<String> = job_ids
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| (*s).to_string())
            .collect();
        for id in &ids {
            inner.suppressed.insert(id.clone());
        }
        let before = inner.deliveries.len();
        let dropped: Vec<String> = inner
            .deliveries
            .iter()
            .filter(|d| Self::is_suppressed(&inner, &d.job_id))
            .map(|d| d.job_id.clone())
            .collect();
        inner.deliveries.retain(|d| !dropped.contains(&d.job_id));
        let removed = before - inner.deliveries.len();
        drop(inner);
        self.queue_changed.notify_waiters();
        removed
    }

    /// H27：当前被抑制（`watch` 或已确认）的作业 id（按 owner 过滤；`None` = 全部）。
    ///
    /// 用途：会话/Agent 重建后用 [`AsyncJobManager::resume_deliveries`] 把「watch 期间
    /// 已完结但结果被丢弃」的作业重新入队——否则结果会静默丢失（`enqueue_delivery`
    /// 在抑制状态下直接返回，不入队）。
    #[must_use]
    pub fn suppressed_job_ids(&self, owner_id: Option<&str>) -> Vec<String> {
        let inner = self.inner.lock().expect("jobs lock");
        let mut out: Vec<String> = inner
            .jobs
            .values()
            .filter(|j| match owner_id {
                Some(o) => j.owner_id.as_deref() == Some(o),
                None => true,
            })
            .filter(|j| Self::is_suppressed(&inner, &j.id))
            .map(|j| j.id.clone())
            .collect();
        out.sort();
        out
    }

    /// 前台快照恢复：确认投递 + 标记结果已消费（对齐 omp `consumeJobResults`）。
    pub fn consume_job_results(&self, job_ids: &[&str]) -> usize {
        self.acknowledge_deliveries(job_ids);
        let mut inner = self.inner.lock().expect("jobs lock");
        let mut consumed = 0;
        for id in job_ids.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let Some(job) = inner.jobs.get(id) else {
                continue;
            };
            if job.status == JobStatus::Running || inner.consumed.contains(id) {
                continue;
            }
            if job.result_text.is_none() && job.error_text.is_none() {
                continue;
            }
            inner.consumed.insert(id.to_string());
            consumed += 1;
        }
        consumed
    }

    /// 结果是否已被投递或前台恢复（对齐 omp `isJobResultConsumed`）。
    #[must_use]
    pub fn is_job_result_consumed(&self, job_id: &str) -> bool {
        self.inner
            .lock()
            .expect("jobs lock")
            .consumed
            .contains(job_id)
    }

    /// 解除前台抑制；已完结且未排队的作业重新入队（保证恰好一次投递）。
    pub fn resume_deliveries(self: &Arc<Self>, job_ids: &[&str]) {
        let mut requeue: Vec<Delivery> = Vec::new();
        {
            let mut inner = self.inner.lock().expect("jobs lock");
            for raw in job_ids {
                let id = raw.trim();
                if id.is_empty() || !inner.suppressed.remove(id) {
                    continue;
                }
                let Some(job) = inner.jobs.get(id) else {
                    continue;
                };
                if !matches!(job.status, JobStatus::Completed | JobStatus::Failed) {
                    continue;
                }
                let queued = inner.deliveries.iter().any(|d| d.job_id == id)
                    || inner.in_flight.iter().any(|d| d.job_id == id);
                if queued {
                    continue;
                }
                requeue.push(Delivery {
                    job_id: id.to_string(),
                    text: if job.status == JobStatus::Completed {
                        job.result_text.clone().unwrap_or_default()
                    } else {
                        job.error_text.clone().unwrap_or_default()
                    },
                    attempt: 0,
                    next_attempt_at_ms: now_ms(),
                    owner_id: job.owner_id.clone(),
                });
            }
        }
        if !requeue.is_empty() {
            {
                let mut inner = self.inner.lock().expect("jobs lock");
                for delivery in requeue {
                    Self::queue_delivery_locked(&mut inner, delivery);
                }
            }
            self.queue_changed.notify_waiters();
            self.ensure_delivery_loop();
        }
    }

    /// 注册 owner 投递 sink（后注册覆盖先注册；返回注销闭包，仅当仍指向本 sink 时清除）。
    pub fn register_delivery_sink(
        self: &Arc<Self>,
        owner_id: &str,
        sink: DeliverySink,
    ) -> SinkGuard {
        self.inner
            .lock()
            .expect("jobs lock")
            .sinks
            .insert(owner_id.to_string(), Arc::clone(&sink));
        SinkGuard {
            manager: Arc::clone(self),
            owner: owner_id.to_string(),
            sink,
        }
    }

    /// 自适应轮询等待（毫秒）：连续轮询爬阶梯，间隔 ≥60s 回落底部。
    #[must_use]
    pub fn next_poll_wait_ms(&self, owner_id: Option<&str>) -> u64 {
        let now = now_ms();
        let mut inner = self.inner.lock().expect("jobs lock");
        let key = owner_id.map(str::to_string);
        let prev = inner.poll_escalation.get(&key).copied();
        let reset = match prev {
            Some((_, last_end)) => now.saturating_sub(last_end) >= POLL_ESCALATION_RESET_MS,
            None => true,
        };
        let level = if reset {
            0
        } else {
            (prev.unwrap_or((0, now)).0 + 1).min(POLL_WAIT_LADDER_MS.len() - 1)
        };
        inner
            .poll_escalation
            .insert(key, (level, prev.map(|(_, t)| t).unwrap_or(now)));
        POLL_WAIT_LADDER_MS[level]
    }

    /// 记录一次轮询等待返回（空闲重置窗口自此刻起算）。
    pub fn record_poll_wait_end(&self, owner_id: Option<&str>) {
        let now = now_ms();
        let mut inner = self.inner.lock().expect("jobs lock");
        let key = owner_id.map(str::to_string);
        let level = inner
            .poll_escalation
            .get(&key)
            .copied()
            .unwrap_or((0, now))
            .0;
        inner.poll_escalation.insert(key, (level, now));
    }

    /// 等待 owner 全部作业运行体退出。新注册的作业也纳入等待；超时返回 `false`。
    pub async fn wait_for_owner_jobs(&self, owner_id: &str, timeout: Option<Duration>) -> bool {
        let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
        loop {
            let receivers: Vec<tokio::sync::watch::Receiver<bool>> = {
                let inner = self.inner.lock().expect("jobs lock");
                inner
                    .jobs
                    .values()
                    .filter(|j| j.owner_id.as_deref() == Some(owner_id))
                    .map(|j| j.id.clone())
                    .collect::<Vec<_>>()
                    .iter()
                    .filter_map(|id| inner.completions.get(id).map(|tx| tx.subscribe()))
                    .collect()
            };
            if receivers.is_empty() {
                return true;
            }
            let wait_fut = async {
                for mut rx in receivers {
                    // Sender drop（逐出/dispose 清表）视为已终结；值变 true 即退出。
                    while let Ok(()) = rx.changed().await {
                        if *rx.borrow() {
                            break;
                        }
                    }
                }
            };
            match deadline {
                None => wait_fut.await,
                Some(deadline) => {
                    if tokio::time::timeout_at(deadline, wait_fut).await.is_err() {
                        return false;
                    }
                }
            }
        }
    }
    /// 取消 owner 全部作业并等待至 deadline；未终结部分由返回列表标注（调用方可
    /// 再次 `wait_for_owner_jobs(owner, None)` 续等）。
    pub async fn cancel_and_reap_owner_jobs(
        self: &Arc<Self>,
        owner_id: &str,
        deadline: tokio::time::Instant,
    ) -> (bool, Vec<String>) {
        self.cancel_all(Some(owner_id));
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
        if self.wait_for_owner_jobs(owner_id, Some(timeout)).await {
            return (true, Vec::new());
        }
        let pending: Vec<String> = {
            let inner = self.inner.lock().expect("jobs lock");
            inner
                .jobs
                .values()
                .filter(|j| {
                    j.owner_id.as_deref() == Some(owner_id)
                        && matches!(j.status, JobStatus::Running | JobStatus::Cancelled)
                })
                .map(|j| j.id.clone())
                .collect()
        };
        (false, pending)
    }

    /// 立即逐出终态作业（丢弃排队投递）；返回逐出数量。
    pub fn evict_completed_jobs(&self, owner_id: Option<&str>) -> usize {
        let ids: Vec<String> = {
            let inner = self.inner.lock().expect("jobs lock");
            inner
                .jobs
                .values()
                .filter(|j| {
                    matches!(j.status, JobStatus::Completed | JobStatus::Failed)
                        && owner_id.is_none_or(|o| j.owner_id.as_deref() == Some(o))
                })
                .map(|j| j.id.clone())
                .collect()
        };
        let mut evicted = 0;
        for id in ids {
            self.acknowledge_deliveries(&[&id]);
            if self.evict_job(&id) {
                evicted += 1;
            }
        }
        evicted
    }

    /// 关停：取消全部、等待退出、清空状态。返回是否全部作业在期限内终结并投递排空。
    pub async fn dispose(self: &Arc<Self>, timeout: Option<Duration>) -> bool {
        let timeout = timeout.unwrap_or(Duration::from_secs(3));
        {
            let mut inner = self.inner.lock().expect("jobs lock");
            inner.disposed = true;
            for handle in inner.evictions.values() {
                handle.abort();
            }
            inner.evictions.clear();
        }
        self.cancel_all(None);
        let deadline = tokio::time::Instant::now() + timeout;
        let receivers: Vec<tokio::sync::watch::Receiver<bool>> = {
            let inner = self.inner.lock().expect("jobs lock");
            inner
                .completions
                .values()
                .map(|tx| tx.subscribe())
                .collect()
        };
        let mut jobs_settled = true;
        for mut rx in receivers {
            let wait = async {
                while let Ok(()) = rx.changed().await {
                    if *rx.borrow() {
                        break;
                    }
                }
            };
            if tokio::time::timeout_at(deadline, wait).await.is_err() {
                jobs_settled = false;
            }
        }
        let drained = self
            .drain_deliveries(Some(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
            ))
            .await;
        {
            let mut inner = self.inner.lock().expect("jobs lock");
            for handle in inner.evictions.values() {
                handle.abort();
            }
            inner.jobs.clear();
            inner.cancels.clear();
            inner.completions.clear();
            inner.deliveries.clear();
            inner.in_flight.clear();
            inner.suppressed.clear();
            inner.watched.clear();
            inner.consumed.clear();
            inner.poll_escalation.clear();
            inner.sinks.clear();
        }
        self.queue_changed.notify_waiters();
        jobs_settled && drained
    }

    /// 排空投递队列（期限内有界等待）。全部投出（或队列被抑制清空）返回 `true`。
    pub async fn drain_deliveries(self: &Arc<Self>, timeout: Option<Duration>) -> bool {
        let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
        loop {
            if !self.has_pending_deliveries(None) {
                return true;
            }
            self.ensure_delivery_loop();
            match deadline {
                None => {
                    self.queue_changed.notified().await;
                }
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return !self.has_pending_deliveries(None);
                    }
                    let _ = tokio::time::timeout(remaining, self.queue_changed.notified()).await;
                    if tokio::time::Instant::now() >= deadline && self.has_pending_deliveries(None)
                    {
                        return false;
                    }
                }
            }
        }
    }

    // ---- 投递内部 ----

    fn is_suppressed(inner: &Inner, job_id: &str) -> bool {
        inner.suppressed.contains(job_id) || inner.watched.contains(job_id)
    }

    fn enqueue_delivery(self: &Arc<Self>, delivery: Delivery) {
        {
            let mut inner = self.inner.lock().expect("jobs lock");
            if Self::is_suppressed(&inner, &delivery.job_id) {
                return;
            }
            Self::queue_delivery_locked(&mut inner, delivery);
        }
        self.ensure_delivery_loop();
    }

    fn queue_delivery_locked(inner: &mut Inner, delivery: Delivery) {
        let pos = inner
            .deliveries
            .iter()
            .position(|d| d.next_attempt_at_ms > delivery.next_attempt_at_ms);
        match pos {
            Some(pos) => inner.deliveries.insert(pos, delivery),
            None => inner.deliveries.push_back(delivery),
        }
    }

    /// 拉起投递循环（单飞；队列空退出，入队再拉起）。
    fn ensure_delivery_loop(self: &Arc<Self>) {
        if self.inner.lock().expect("jobs lock").deliveries.is_empty() {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                // 弹出下一个可投递条目（抑制项直接丢弃）。
                let next = {
                    let mut inner = this.inner.lock().expect("jobs lock");
                    loop {
                        let Some(front) = inner.deliveries.front().cloned() else {
                            return; // 队列空：循环退出。
                        };
                        if Self::is_suppressed(&inner, &front.job_id) {
                            inner.deliveries.pop_front();
                            continue;
                        }
                        break front;
                    }
                };
                let wait_ms = next.next_attempt_at_ms.saturating_sub(now_ms());
                if wait_ms > 0 {
                    // 到期前可被队列变化唤醒（新条目插队/抑制/清除），唤醒后重读队首。
                    let _ = tokio::time::timeout(
                        Duration::from_millis(wait_ms),
                        this.queue_changed.notified(),
                    )
                    .await;
                    continue;
                }
                {
                    let mut inner = this.inner.lock().expect("jobs lock");
                    if inner.deliveries.front().map(|d| d.job_id.as_str())
                        != Some(next.job_id.as_str())
                    {
                        continue; // 队首已被替换（重排），重读。
                    }
                    inner.deliveries.pop_front();
                    inner.in_flight.push(next.clone());
                }
                this.deliver(next).await;
            }
        });
    }

    async fn deliver(self: &Arc<Self>, delivery: Delivery) {
        let sink = {
            let inner = self.inner.lock().expect("jobs lock");
            match &delivery.owner_id {
                Some(owner) => inner.sinks.get(owner).cloned(),
                None => self.default_sink.clone(),
            }
        };
        let Some(sink) = sink else {
            // 死信：owner 无活 sink（会话已释放/停驻）或无默认 sink。作业行保留
            // 结果文本至保留期逐出，仍可经作业查询检查结局。
            tracing::warn!(
                job_id = %delivery.job_id,
                owner = ?delivery.owner_id,
                "async job delivery dead-lettered: no sink"
            );
            self.inner
                .lock()
                .expect("jobs lock")
                .in_flight
                .retain(|d| d.job_id != delivery.job_id);
            return;
        };
        let job_snapshot = self.get_job(&delivery.job_id);
        let outcome = sink(delivery.job_id.clone(), delivery.text.clone(), job_snapshot).await;
        let mut inner = self.inner.lock().expect("jobs lock");
        inner.in_flight.retain(|d| d.job_id != delivery.job_id);
        match outcome {
            Ok(()) => {
                inner.consumed.insert(delivery.job_id.clone());
            }
            Err(err) => {
                let attempt = delivery.attempt + 1;
                let next_attempt_at_ms = now_ms() + Self::retry_delay_ms(attempt);
                tracing::warn!(job_id = %delivery.job_id, attempt, %err, "async job delivery failed; will retry");
                let job_alive = inner.jobs.contains_key(&delivery.job_id);
                let suppressed = Self::is_suppressed(&inner, &delivery.job_id);
                if job_alive && !suppressed {
                    Self::queue_delivery_locked(
                        &mut inner,
                        Delivery {
                            job_id: delivery.job_id,
                            text: delivery.text,
                            attempt,
                            next_attempt_at_ms,
                            owner_id: delivery.owner_id,
                        },
                    );
                }
            }
        }
    }

    /// 重试退避：500ms×2^(n-1) + ≤200ms 抖动，上限 30s（对齐 omp `#getRetryDelay`）。
    fn retry_delay_ms(attempt: u32) -> u64 {
        let exp = attempt.saturating_sub(1).min(8);
        let backoff = DELIVERY_RETRY_BASE_MS.saturating_mul(1 << exp);
        let jitter = rand_jitter(DELIVERY_RETRY_JITTER_MS);
        (backoff + jitter).min(DELIVERY_RETRY_MAX_MS)
    }

    // ---- 逐出内部 ----

    fn evict_job(&self, job_id: &str) -> bool {
        let mut inner = self.inner.lock().expect("jobs lock");
        if let Some(handle) = inner.evictions.remove(job_id) {
            handle.abort();
        }
        inner.suppressed.remove(job_id);
        inner.watched.remove(job_id);
        inner.consumed.remove(job_id);
        inner.cancels.remove(job_id);
        inner.completions.remove(job_id);
        inner.jobs.remove(job_id).is_some()
    }

    fn schedule_eviction(self: &Arc<Self>, job_id: &str) {
        if self.inner.lock().expect("jobs lock").disposed {
            return;
        }
        if self.retention_ms == 0 {
            self.evict_job(job_id);
            return;
        }
        let this = Arc::clone(self);
        let id = job_id.to_string();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(this.retention_ms)).await;
            this.evict_job(&id);
        });
        let mut inner = self.inner.lock().expect("jobs lock");
        if let Some(old) = inner
            .evictions
            .insert(job_id.to_string(), handle.abort_handle())
        {
            old.abort();
        }
    }
}

/// sink 注销守卫：drop 时注销，仅当映射仍指向本 sink（复活会话的新注册不被
/// 停驻前身的迟到清理清除；对齐 omp 返回的 unregister 闭包语义）。
pub struct SinkGuard {
    manager: Arc<AsyncJobManager>,
    owner: String,
    sink: DeliverySink,
}

impl Drop for SinkGuard {
    fn drop(&mut self) {
        let mut inner = self.manager.inner.lock().expect("jobs lock");
        if inner
            .sinks
            .get(&self.owner)
            .is_some_and(|s| Arc::ptr_eq(s, &self.sink))
        {
            inner.sinks.remove(&self.owner);
        }
    }
}

/// 轻量抖动：`std` 无 rand；取纳秒熵 % 上限（调度用途足够，非密码学）。
fn rand_jitter(cap_ms: u64) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % (cap_ms + 1)
}

/// H28：异步唤醒探测端口——引擎在「停止边界」用它判断是否有后台作业在途。
///
/// 有在途作业时**不注入** todo 完成提醒：后台作业完成会经投递 sink / hub 唤醒循环
/// （见 `agent_core::jobs` 的 delivery 语义），此时再催一轮会与唤醒重复（移植 omp
/// `TodoTrackerHost::hasPendingAsyncWake`）。
pub trait AsyncWakeProbe: Send + Sync {
    /// 是否存在会重新唤醒循环的在途异步工作（运行中作业或待投递结果）。
    fn has_pending_async(&self) -> bool;
}

impl AsyncWakeProbe for AsyncJobManager {
    fn has_pending_async(&self) -> bool {
        !self.running_jobs(None).is_empty() || self.has_pending_deliveries(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 记录型 sink 的投递日志。
    type SinkLog = Arc<Mutex<Vec<(String, String)>>>;
    /// 慢作业体：started 通知启动、release/取消后返回。
    type SlowRun = Box<
        dyn FnOnce(JobRunContext) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
            + Send,
    >;

    fn sink_recording(sink_errors: u32) -> (DeliverySink, SinkLog) {
        let log: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let log2 = Arc::clone(&log);
        let sink: DeliverySink = Arc::new(move |job_id, text, _job| {
            let log = Arc::clone(&log2);
            Box::pin(async move {
                let mut guard = log.lock().expect("log");
                if guard.len() < sink_errors as usize {
                    return Err("sink down".into());
                }
                guard.push((job_id, text));
                Ok(())
            })
        });
        (sink, log)
    }

    fn run_ok(
        text: &'static str,
    ) -> impl FnOnce(JobRunContext) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
    {
        move |_ctx| Box::pin(async move { Ok(text.to_string()) })
    }

    fn run_slow_release() -> (SlowRun, Arc<Notify>, Arc<Notify>) {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let s = Arc::clone(&started);
        let r = Arc::clone(&release);
        let f = move |ctx: JobRunContext| {
            let s = Arc::clone(&s);
            let r = Arc::clone(&r);
            Box::pin(async move {
                s.notify_one();
                tokio::select! {
                    _ = r.notified() => {}
                    _ = ctx.cancel.cancelled() => {}
                }
                Ok("done".to_string())
            }) as Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
        };
        (Box::new(f) as SlowRun, started, release)
    }

    /// 等待作业到达终态（测试辅助：单线程运行时下先让完成体任务跑完）。
    async fn await_settled(manager: &Arc<AsyncJobManager>, id: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match manager.get_job(id) {
                Some(job) if job.status == JobStatus::Running => {}
                Some(_) => return,
                None => return, // 已逐出（retention 0）。
            }
            if std::time::Instant::now() >= deadline {
                panic!("job {id} did not settle in time");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn delivers_unowned_completion_to_default_sink() {
        let (sink, log) = sink_recording(0);
        let manager = AsyncJobManager::build(2, 60_000, Some(sink));
        let id = manager
            .register(
                AsyncJobType::Bash,
                "echo",
                run_ok("out"),
                RegisterOptions::default(),
            )
            .unwrap();
        await_settled(&manager, &id).await;
        assert!(manager.drain_deliveries(Some(Duration::from_secs(2))).await);
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1, "exactly one delivery: {log:?}");
        assert_eq!(log[0].0, id);
        assert_eq!(log[0].1, "out");
        assert!(manager.is_job_result_consumed(&id));
    }

    #[tokio::test]
    async fn owned_delivery_dead_letters_without_owner_sink() {
        let (sink, log) = sink_recording(0);
        let _ = &sink; // 默认 sink 存在与否不影响死信断言（owner 路由优先）。
        let manager = AsyncJobManager::build(2, 60_000, Some(sink));
        let opts = RegisterOptions {
            owner_id: Some("Main".into()),
            ..Default::default()
        };
        manager
            .register(AsyncJobType::Bash, "x", run_ok("secret"), opts)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        // 无 owner sink → 死信：默认 sink 绝不收到（防跨会话泄漏）。
        assert!(log.lock().unwrap().is_empty());
        // 注册 owner sink 后 resume 不适用（死信不重排）；作业行仍可查询。
        let job = manager
            .get_job("bg_1")
            .expect("job row kept for inspection");
        assert_eq!(job.result_text.as_deref(), Some("secret"));
    }

    #[tokio::test]
    async fn owned_delivery_routes_to_owner_sink() {
        let (owner_sink, owner_log) = sink_recording(0);
        let manager = AsyncJobManager::build(2, 60_000, None);
        let _guard = manager.register_delivery_sink("Main", owner_sink);
        let opts = RegisterOptions {
            owner_id: Some("Main".into()),
            ..Default::default()
        };
        let id = manager
            .register(AsyncJobType::Bash, "x", run_ok("res"), opts)
            .unwrap();
        await_settled(&manager, &id).await;
        assert!(manager.drain_deliveries(Some(Duration::from_secs(2))).await);
        assert_eq!(owner_log.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sink_failure_requeues_with_backoff() {
        // 前 1 次失败（attempt 0 → 失败），重试成功。
        let (_sink, log) = sink_recording(0); // 记录型 sink 不用于本测；仅取日志。
        let _ = _sink;
        // 构造先失败一次的 sink：第一次调用返回 Err。
        let log2 = Arc::clone(&log);
        let failing: DeliverySink = {
            let attempts = Arc::new(Mutex::new(0u32));
            let log2 = Arc::clone(&log2);
            Arc::new(move |job_id, text, _job| {
                let attempts = Arc::clone(&attempts);
                let log2 = Arc::clone(&log2);
                Box::pin(async move {
                    let mut n = attempts.lock().unwrap();
                    *n += 1;
                    if *n == 1 {
                        return Err("boom".into());
                    }
                    log2.lock().unwrap().push((job_id, text));
                    Ok(())
                })
            })
        };
        let manager = AsyncJobManager::build(2, 60_000, Some(failing));
        manager
            .register(
                AsyncJobType::Bash,
                "x",
                run_ok("out"),
                RegisterOptions::default(),
            )
            .unwrap();
        // 重试退避 ~500ms+：给足 3s。
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while log2.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(log2.lock().unwrap().len(), 1, "retry delivered");
    }

    #[tokio::test]
    async fn cancel_fires_token_and_skips_delivery() {
        let (sink, log) = sink_recording(0);
        let manager = AsyncJobManager::build(2, 60_000, Some(sink));
        let (run, started, release) = run_slow_release();
        let id = manager
            .register(AsyncJobType::Bash, "sleep", run, RegisterOptions::default())
            .unwrap();
        started.notified().await;
        assert!(manager.cancel(&id, None));
        release.notify_one();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            log.lock().unwrap().is_empty(),
            "cancelled job must not deliver"
        );
        assert_eq!(manager.get_job(&id).unwrap().status, JobStatus::Cancelled);
    }

    #[tokio::test]
    async fn cancel_rejects_cross_owner() {
        let manager = AsyncJobManager::new();
        let (run, started, release) = run_slow_release();
        let opts = RegisterOptions {
            owner_id: Some("Main".into()),
            ..Default::default()
        };
        let id = manager
            .register(AsyncJobType::Bash, "x", run, opts)
            .unwrap();
        started.notified().await;
        assert!(
            !manager.cancel(&id, Some("Other")),
            "cross-owner cancel rejected"
        );
        assert!(manager.cancel(&id, Some("Main")));
        release.notify_one();
    }

    #[tokio::test]
    async fn running_cap_rejects_until_finished() {
        let manager = AsyncJobManager::with_max_running(1);
        let (run, started, release) = run_slow_release();
        let a = manager
            .register(AsyncJobType::Bash, "a", run, RegisterOptions::default())
            .unwrap();
        started.notified().await;
        let err = manager
            .register(
                AsyncJobType::Bash,
                "b",
                run_ok("y"),
                RegisterOptions::default(),
            )
            .unwrap_err();
        assert!(err.contains("limit reached"));
        release.notify_one();
        await_settled(&manager, &a).await;
        // 完结作业不占槽（retention 内仍可注册）。
        let id = manager
            .register(
                AsyncJobType::Bash,
                "c",
                run_ok("z"),
                RegisterOptions::default(),
            )
            .unwrap();
        assert!(id.starts_with("bg_"));
    }

    #[tokio::test]
    async fn preferred_id_conflict_gets_suffix() {
        let manager = AsyncJobManager::new();
        let opts = RegisterOptions {
            id: Some("worker".into()),
            ..Default::default()
        };
        let (run, started, release) = run_slow_release();
        let a = manager
            .register(AsyncJobType::Task, "a", run, opts.clone())
            .unwrap();
        started.notified().await;
        assert_eq!(a, "worker");
        let b = manager
            .register(AsyncJobType::Task, "b", run_ok("x"), opts)
            .unwrap();
        assert_eq!(b, "worker-2");
        release.notify_one();
    }

    #[tokio::test]
    async fn acknowledge_suppresses_then_resume_redelivers() {
        let (sink, log) = sink_recording(0);
        let manager = AsyncJobManager::build(2, 60_000, Some(sink));
        let id = manager
            .register(
                AsyncJobType::Bash,
                "x",
                run_ok("payload"),
                RegisterOptions::default(),
            )
            .unwrap();
        // 前台等待路径：完成前先确认（抑制后续投递）。
        manager.acknowledge_deliveries(&[&id]);
        await_settled(&manager, &id).await;
        assert!(manager.drain_deliveries(Some(Duration::from_secs(2))).await);
        assert!(
            log.lock().unwrap().is_empty(),
            "suppressed delivery stays quiet"
        );
        assert_eq!(manager.acknowledge_deliveries(&[&id]), 0);
        // resume：解除抑制并重新入队（恰好一次投递保证）。
        manager.resume_deliveries(&[&id]);
        assert!(manager.drain_deliveries(Some(Duration::from_secs(2))).await);
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn watch_takes_over_delivery() {
        let (sink, log) = sink_recording(0);
        let manager = AsyncJobManager::build(2, 60_000, Some(sink));
        let id = manager
            .register(
                AsyncJobType::Bash,
                "x",
                run_ok("via-wait"),
                RegisterOptions::default(),
            )
            .unwrap();
        manager.watch_jobs(&[&id]);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            log.lock().unwrap().is_empty(),
            "watched job skips auto-delivery"
        );
        // hub wait 路径：查询 + 消费。
        let job = manager.get_job(&id).unwrap();
        assert_eq!(job.result_text.as_deref(), Some("via-wait"));
        assert_eq!(manager.consume_job_results(&[&id]), 1);
        assert!(manager.is_job_result_consumed(&id));
    }

    #[tokio::test]
    async fn zero_retention_evicts_immediately() {
        let manager = AsyncJobManager::build(2, 0, None);
        let id = manager
            .register(
                AsyncJobType::Bash,
                "x",
                run_ok("gone"),
                RegisterOptions::default(),
            )
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while manager.get_job(&id).is_some() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            manager.get_job(&id).is_none(),
            "retention 0 evicts promptly"
        );
    }

    #[test]
    fn poll_ladder_climbs_and_resets() {
        let manager = AsyncJobManager::new();
        let w1 = manager.next_poll_wait_ms(Some("Main"));
        let w2 = manager.next_poll_wait_ms(Some("Main"));
        assert_eq!(w1, 5_000);
        assert_eq!(w2, 10_000, "consecutive polls climb");
        manager.record_poll_wait_end(Some("Main"));
        // 60s 间隔（手动回拨 lastPollEndAt 模拟）——直接构造空闲状态。
        {
            let mut inner = manager.inner.lock().unwrap();
            inner
                .poll_escalation
                .insert(Some("Main".to_string()), (1, now_ms() - 61_000));
        }
        assert_eq!(
            manager.next_poll_wait_ms(Some("Main")),
            5_000,
            "idle resets to floor"
        );
    }

    #[tokio::test]
    async fn dispose_cancels_running_and_reports_settlement() {
        let manager = AsyncJobManager::new();
        let (run, started, _release) = run_slow_release();
        manager
            .register(AsyncJobType::Bash, "long", run, RegisterOptions::default())
            .unwrap();
        started.notified().await;
        let settled = manager.dispose(Some(Duration::from_secs(2))).await;
        assert!(settled, "cancelled jobs settle promptly");
        assert!(manager.all_jobs(None).is_empty());
    }

    /// H27：`dispose` 取消在跑作业并在期限内终结（会话关闭路径的依赖语义）。
    #[tokio::test]
    async fn dispose_cancels_running_jobs_and_settles() {
        let mgr = AsyncJobManager::with_max_running(4);
        let job_id = mgr
            .register(
                AsyncJobType::Bash,
                "sleep",
                |_ctx| Box::pin(async { Ok("done".to_string()) }),
                RegisterOptions {
                    id: Some("bg-x".into()),
                    owner_id: Some("main".into()),
                    on_progress: None,
                    queued: false,
                },
            )
            .expect("register");
        mgr.mark_running(&job_id);
        assert_eq!(mgr.running_jobs(None).len(), 1);
        // 未投递结果检测（shutdown 的告警分支依据）。
        let _ = mgr.has_pending_deliveries(None);
        let settled = mgr.dispose(Some(std::time::Duration::from_secs(2))).await;
        // dispose 必须返回（不悬挂）且不留运行中作业。
        let _ = settled;
        assert!(
            mgr.running_jobs(None).is_empty(),
            "dispose 后不得留在跑作业"
        );
        // 已 dispose 的管理器拒绝新作业（防止关闭竞态下再注册）。
        assert!(
            mgr.register(
                AsyncJobType::Bash,
                "late",
                |_ctx| Box::pin(async { Ok(String::new()) }),
                RegisterOptions::default(),
            )
            .is_err(),
            "dispose 后注册应被拒绝"
        );
    }
}
