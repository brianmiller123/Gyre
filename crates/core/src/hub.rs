//! 进程级消息总线（移植 oh-my-pi `irc/bus` 的进程内子集 + hub 邮箱语义）。
//!
//! 多 Agent 协作的最小可信原语：**注册表 + 定向邮箱**。
//! - [`Hub::register`]：注册一个代理 id，返回其专属收件箱（`mpsc::UnboundedReceiver`）。
//! - [`Hub::send`]：向指定 id 投递消息（接收方尚未注册 → 明确报错，不静默丢弃）。
//! - [`Hub::peers`]：列出全部在册 id（供模型发现可通信对象）。
//! - [`Hub::unregister`]：注销（drop 收件箱）。
//!
//! v1 边界（文档化）：仅进程内、无持久化；子任务生命周期由 supervisor + task 体系
//! 承载（见 `agent-supervisor`），总线侧经 [`HubSupervision`] 端口暴露其只读快照与
//! 取消委托（真实实现由装配层注入）。
//!
//! H41/H43 补齐（对齐 omp `registry/agent-registry.ts` 与 `irc/bus.ts` 的可用子集）：
//! - **注册表带生命周期**：[`AgentStatus`]（`running` / `idle` / `parked`）+ 标签 +
//!   状态迁移（[`Hub::park`] / [`Hub::revive`]）与名册（[`Hub::agents`]）；
//! - **寻址与广播**：[`Hub::broadcast`]（除自己外全部在册代理）+ [`Hub::wake`]
//!   （唤醒 parked 代理：置 running 并触发其唤醒通知）；
//! - **投递回执**：[`Hub::send_tracked`] 返回回执 id，接收方经 [`Hub::ack`] 确认，
//!   发送方 [`Hub::wait_ack`] 等确认（超时/未知 id 明确返回，不静默）。
//! - 与 omp 的差距：跨进程 IPC、持久化名册、插件式 wakeup 回调仍留待后续。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// 消息投递错误。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HubError {
    /// 目标未注册（或已注销）。
    RecipientNotFound(String),
    /// 收件箱已关闭（接收方已退出）。
    RecipientGone(String),
    /// 名册容量已满（H41：`at_capacity` 预拒——在**注册前**拒绝，而不是投递后才失败）。
    AtCapacity {
        /// 容量上限。
        limit: usize,
    },
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RecipientNotFound(id) => write!(f, "收件人 {id:?} 未注册"),
            Self::RecipientGone(id) => write!(f, "收件人 {id:?} 收件箱已关闭"),
            Self::AtCapacity { limit } => {
                write!(f, "名册已满（上限 {limit}）——请先注销空闲代理")
            }
        }
    }
}

/// 代理生命周期状态（H41；对齐 omp 注册表的三态，Gyre 侧只表达「可寻址」语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// 正在跑（可收消息并按轮次边界响应）。
    Running,
    /// 空闲（已注册但无在途工作）。
    Idle,
    /// 已挂起（parked：不主动消费收件箱，需 [`Hub::wake`] 唤醒）。
    Parked,
}

impl AgentStatus {
    /// 线协议名（状态展示与测试用）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Parked => "parked",
        }
    }
}

/// 在册代理的一条名册记录（H41）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// 代理 id（寻址键）。
    pub id: String,
    /// 展示名（缺省等于 id）。
    pub label: String,
    /// 生命周期状态。
    pub status: AgentStatus,
    /// 收件箱积压条数（未消费消息数；`mpsc` 不暴露精确长度时以计数近似）。
    pub inbox_len: usize,
}

/// 一条总线消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubMessage {
    /// 发送方 id。
    pub from: String,
    /// 消息体（纯文本；结构化载荷由调用方自行编码，如 JSON 字符串）。
    pub body: String,
    /// 回执 id（H43；仅 [`Hub::send_tracked`] 投递的消息有值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_id: Option<u64>,
}

/// 投递/回执错误（[`Hub::wait_ack`] 用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AckError {
    /// 未知回执 id（已确认过或从未签发）。
    UnknownAck(u64),
    /// 等待超时（消息可能已投递但接收方未确认）。
    Timeout(u64),
    /// 回执已被发送方撤销（H43 `unwatch`）。
    Revoked(u64),
}

/// 回执状态（H43）：`Pending` → `Acked` / `Revoked`（撤销后等待者立即得到明确错误）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AckState {
    /// 已签发、等待确认。
    Pending,
    /// 已确认。
    Acked,
    /// 已被发送方撤销（`unwatch`）。
    Revoked,
}

/// 单个在册代理的内部记录。
struct Peer {
    tx: tokio::sync::mpsc::UnboundedSender<HubMessage>,
    label: String,
    status: AgentStatus,
    /// 唤醒通知（parked → running 时触发；`notify_waiters` 不积压信号）。
    wake: Arc<tokio::sync::Notify>,
    /// 收件箱积压计数（发送 +1 / 消费 -1）。
    inbox_len: Arc<std::sync::atomic::AtomicUsize>,
}

/// 进程级消息总线（`Arc<Hub>` 共享；线程安全）。
///
/// # 为什么名册不落盘（H41）
///
/// 在册代理的 id / 标签 / 状态描述的是**进程内**的可寻址关系：`main` 是当前前端进程，
/// `task-<n>` 是正在跑的子代理——它们随进程结束一起消失，跨进程恢复一个 `task-3` 只会
/// 得到一个永远收不到消息的幽灵条目。因此 Gyre 有意**不**持久化名册，改为提供
/// [`Hub::set_capacity`] + [`Hub::at_capacity`] 做**注册前**的容量预拒（避免注册表
/// 无界增长后才在投递时失败）。需要「跨重启可见的代理历史」时应由宿主另行记录，
/// 而不是让总线假装那些代理还在。
pub struct Hub {
    peers: RwLock<HashMap<String, Peer>>,
    /// 回执表：ack id → 状态变化通知（`watch` 使「先确认后等待」不丢信号）。
    acks: std::sync::Mutex<HashMap<u64, tokio::sync::watch::Sender<AckState>>>,
    /// 回执 id 计数器。
    next_ack: std::sync::atomic::AtomicU64,
    /// 名册容量上限（0 = 不限；H41 `at_capacity` 预拒依据）。
    capacity: std::sync::atomic::AtomicUsize,
}

/// 一个已注册代理的句柄（收件箱 + 唤醒通知 + 积压计数）。
///
/// 调用方（hub 工具 / 子代理循环）持有它消费消息；`inbox_len` 由 [`HubHandle::take`]
/// 维护，供名册展示积压。
pub struct HubHandle {
    /// 收件箱。
    pub rx: tokio::sync::mpsc::UnboundedReceiver<HubMessage>,
    /// 唤醒通知（parked 被 [`Hub::wake`] 唤醒时触发一次广播）。
    pub wake: Arc<tokio::sync::Notify>,
    /// 收件箱积压计数（与注册表共享）。
    pub inbox_len: Arc<std::sync::atomic::AtomicUsize>,
}

impl HubHandle {
    /// 收一条消息（维护积压计数）。
    pub async fn recv(&mut self) -> Option<HubMessage> {
        let msg = self.rx.recv().await?;
        self.inbox_len
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        Some(msg)
    }

    /// 非阻塞收一条消息（维护积压计数）。
    #[must_use]
    pub fn try_recv(&mut self) -> Option<HubMessage> {
        match self.rx.try_recv() {
            Ok(msg) => {
                self.inbox_len
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                Some(msg)
            }
            Err(_) => None,
        }
    }

    /// 当前积压条数。
    #[must_use]
    pub fn inbox_len(&self) -> usize {
        self.inbox_len.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Hub {
    /// 构造空总线。
    #[must_use]
    pub fn new() -> Self {
        Self {
            peers: RwLock::new(HashMap::new()),
            acks: std::sync::Mutex::new(HashMap::new()),
            next_ack: std::sync::atomic::AtomicU64::new(1),
            capacity: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// 共享句柄。
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// 注册代理，返回其收件箱（重名注册以新代旧，旧收件箱关闭）。
    ///
    /// 等价于 [`Hub::register_with`]（标签 = id、状态 `Running`）。
    pub fn register(&self, id: String) -> tokio::sync::mpsc::UnboundedReceiver<HubMessage> {
        self.register_handle(id, None, AgentStatus::Running).rx
    }

    /// 设置名册容量上限（0 = 不限）。超过上限后 [`Hub::try_register_handle`] 在**注册前**
    /// 返回 [`HubError::AtCapacity`]（`register`/`register_handle` 不受限，供宿主内部与测试使用）。
    pub fn set_capacity(&self, limit: usize) {
        self.capacity
            .store(limit, std::sync::atomic::Ordering::Relaxed);
    }

    /// 当前容量上限（0 = 不限）。
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 是否已达容量（H41：注册前的预拒判定）。
    #[must_use]
    pub fn at_capacity(&self) -> bool {
        let limit = self.capacity();
        if limit == 0 {
            return false;
        }
        self.len() >= limit
    }

    /// 容量感知的注册（H41）：满员时返回 [`HubError::AtCapacity`] 而**不**创建收件箱。
    ///
    /// 重名注册视为「替换」——不占新名额（旧收件箱关闭）。
    ///
    /// # Errors
    /// 名册已达 [`Hub::capacity`]。
    pub fn try_register_handle(
        &self,
        id: String,
        label: Option<String>,
        status: AgentStatus,
    ) -> Result<HubHandle, HubError> {
        if self.at_capacity() && !self.contains(&id) {
            return Err(HubError::AtCapacity {
                limit: self.capacity(),
            });
        }
        Ok(self.register_handle(id, label, status))
    }

    /// 指定代理是否在册。
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(id)
    }

    /// 注册代理并返回完整句柄（H41/H43）：标签、初始状态、唤醒通知与积压计数。
    pub fn register_handle(
        &self,
        id: String,
        label: Option<String>,
        status: AgentStatus,
    ) -> HubHandle {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let wake = Arc::new(tokio::sync::Notify::new());
        let inbox_len = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peer = Peer {
            tx,
            label: label.unwrap_or_else(|| id.clone()),
            status,
            wake: Arc::clone(&wake),
            inbox_len: Arc::clone(&inbox_len),
        };
        let mut g = self
            .peers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.insert(id, peer);
        HubHandle {
            rx,
            wake,
            inbox_len,
        }
    }

    /// 注销代理（重复注销幂等）。
    pub fn unregister(&self, id: &str) {
        let mut g = self
            .peers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.remove(id);
    }

    /// 设置代理生命周期状态（未注册 → `false`，不静默）。
    pub fn set_status(&self, id: &str, status: AgentStatus) -> bool {
        let mut g = self
            .peers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match g.get_mut(id) {
            Some(p) => {
                p.status = status;
                true
            }
            None => false,
        }
    }

    /// 挂起代理（`Parked`；不主动消费收件箱，需 [`Hub::wake`] 唤醒）。
    pub fn park(&self, id: &str) -> bool {
        self.set_status(id, AgentStatus::Parked)
    }

    /// 唤醒代理：置 `Running` 并触发其唤醒通知（无在册 → `false`）。
    ///
    /// `notify_waiters` 不积压信号——若代理当时没在等，醒来后照常看到收件箱里的消息，
    /// 语义等价「有活了」。
    pub fn revive(&self, id: &str) -> bool {
        self.set_status(id, AgentStatus::Running)
    }

    /// 唤醒（别名语义）：触发唤醒通知并置 `Running`。
    pub fn wake(&self, id: &str) -> bool {
        let notify = {
            let g = self
                .peers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match g.get(id) {
                Some(p) => Arc::clone(&p.wake),
                None => return false,
            }
        };
        let _ = self.set_status(id, AgentStatus::Running);
        notify.notify_waiters();
        true
    }

    /// 名册（H41）：全部在册代理的 id/标签/状态/积压，按 id 排序。
    #[must_use]
    pub fn agents(&self) -> Vec<AgentInfo> {
        let g = self
            .peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out: Vec<AgentInfo> = g
            .iter()
            .map(|(id, p)| AgentInfo {
                id: id.clone(),
                label: p.label.clone(),
                status: p.status,
                inbox_len: p.inbox_len.load(std::sync::atomic::Ordering::Relaxed),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// 广播给除 `from` 外的全部在册代理，返回成功投递数（H43）。
    ///
    /// 收件箱已关闭的代理跳过（不视为错误：广播是尽力而为）。
    pub fn broadcast(&self, from: &str, body: impl Into<String>) -> usize {
        let body = body.into();
        let targets: Vec<String> = self
            .agents()
            .into_iter()
            .map(|a| a.id)
            .filter(|id| id != from)
            .collect();
        targets
            .into_iter()
            .filter(|id| self.send(id, from, body.clone()).is_ok())
            .count()
    }

    /// 向指定代理投递消息（不签发回执；需要确认用 [`Hub::send_tracked`]）。
    ///
    /// # Errors
    /// 目标未注册或收件箱已关闭时返回 [`HubError`]。
    pub fn send(&self, to: &str, from: &str, body: impl Into<String>) -> Result<(), HubError> {
        let (tx, inbox_len) = {
            let g = self
                .peers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let p = g
                .get(to)
                .ok_or_else(|| HubError::RecipientNotFound(to.to_string()))?;
            (p.tx.clone(), Arc::clone(&p.inbox_len))
        };
        tx.send(HubMessage {
            from: from.to_string(),
            body: body.into(),
            ack_id: None,
        })
        .map(|()| {
            inbox_len.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        })
        .map_err(|_| HubError::RecipientGone(to.to_string()))
    }

    /// 投递并签发回执 id（H43）：接收方消费消息后应调 [`Hub::ack`]。
    ///
    /// # Errors
    /// 目标未注册或收件箱已关闭时返回 [`HubError`]。
    pub fn send_tracked(
        &self,
        to: &str,
        from: &str,
        body: impl Into<String>,
    ) -> Result<u64, HubError> {
        let ack_id = self
            .next_ack
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, _rx) = tokio::sync::watch::channel(AckState::Pending);
        self.acks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(ack_id, tx);
        let msg = HubMessage {
            from: from.to_string(),
            body: body.into(),
            ack_id: Some(ack_id),
        };
        let (peer_tx, inbox_len) = {
            let g = self
                .peers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match g.get(to) {
                Some(p) => (p.tx.clone(), Arc::clone(&p.inbox_len)),
                None => {
                    self.acks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&ack_id);
                    return Err(HubError::RecipientNotFound(to.to_string()));
                }
            }
        };
        match peer_tx.send(msg) {
            Ok(()) => {
                inbox_len.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(ack_id)
            }
            Err(_) => {
                self.acks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&ack_id);
                Err(HubError::RecipientGone(to.to_string()))
            }
        }
    }

    /// 确认收到消息（H43）：接收方消费后调用；未知/重复 id → `false`（不静默）。
    ///
    /// `watch::send_replace(true)` 记录「已确认」，因此**先确认后等待**也不会丢信号。
    pub fn ack(&self, ack_id: u64) -> bool {
        let tx = self
            .acks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&ack_id)
            .cloned();
        match tx {
            Some(tx) => {
                // 重复确认返回 false（不静默成功）；已撤销的回执不可再确认。
                if *tx.borrow() != AckState::Pending {
                    return false;
                }
                tx.send_replace(AckState::Acked);
                true
            }
            None => false,
        }
    }

    /// 撤销（退订）一个回执（H43 `unwatch`）：从表中移除并返回是否存在。
    ///
    /// 正在 [`Hub::wait_ack`] 的等待者会立即收到 [`AckError::Revoked`]（而不是等到超时），
    /// 因此「发送方放弃等待」不会让接收方误以为消息仍被追踪。
    pub fn revoke_ack(&self, ack_id: u64) -> bool {
        let tx = self
            .acks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&ack_id);
        match tx {
            Some(tx) => {
                // 先通知等待者「已撤销」，再移除表项（等待者在 borrow 里读到 Revoked）。
                tx.send_replace(AckState::Revoked);
                true
            }
            None => false,
        }
    }

    /// **未兑现**的回执数（H43 可观测性：已确认但尚未被 `wait_ack` 消费的不计）。
    #[must_use]
    pub fn pending_acks(&self) -> usize {
        self.acks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|tx| *tx.borrow() == AckState::Pending)
            .count()
    }

    /// 等待某回执被确认（H43）。
    ///
    /// # Errors
    /// 未知 id（从未签发或已被等待消费）→ [`AckError::UnknownAck`]；超时 →
    /// [`AckError::Timeout`]（消息可能已投递但接收方未确认）。
    pub async fn wait_ack(
        &self,
        ack_id: u64,
        timeout: std::time::Duration,
    ) -> Result<(), AckError> {
        let mut rx = {
            let g = self
                .acks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.get(&ack_id).map(tokio::sync::watch::Sender::subscribe)
        }
        .ok_or(AckError::UnknownAck(ack_id))?;
        // 先确认后等待：消费该回执（第二次等待应报 UnknownAck 而非重复成功）。
        if *rx.borrow() == AckState::Acked {
            self.acks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&ack_id);
            return Ok(());
        }
        if *rx.borrow() == AckState::Revoked {
            return Err(AckError::Revoked(ack_id));
        }
        let _ = tokio::time::timeout(timeout, async {
            while *rx.borrow_and_update() == AckState::Pending {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        match *rx.borrow() {
            AckState::Acked => {
                // 等待成功即消费该回执（后续等待同一 id → UnknownAck）。
                self.acks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&ack_id);
                Ok(())
            }
            // 撤销/超时都保留表项：撤销已由 `revoke_ack` 移除（此处读到的是撤销瞬间的副本）。
            AckState::Revoked => Err(AckError::Revoked(ack_id)),
            AckState::Pending => Err(AckError::Timeout(ack_id)),
        }
    }

    /// 全部在册代理 id（排序输出，供展示）。
    #[must_use]
    pub fn peers(&self) -> Vec<String> {
        let g = self
            .peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ids: Vec<String> = g.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// 在册代理数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

/// 单个被监督子任务的快照（hub 工具 `jobs` 面的最小字段集）。
///
/// 字段由监督体系以自有词汇填充：`status` 原样透传（如 `running`/`done`），不做
/// omp `running|idle|parked` 三态转译；监督体系给不出的字段填 `None`，由 hub 工具
/// 如实标 `unknown`，不编造。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HubJobSnapshot {
    /// 子任务 id（hub 工具 `cancel` 的 `ids` 键）。
    pub id: String,
    /// 任务标签（名称）。
    pub label: String,
    /// 生命周期阶段（监督体系自有词汇，原样透传）。
    pub status: String,
    /// 是否在途（未达终态）。
    pub in_flight: bool,
    /// 已完成轮次（未知为 `None`）。
    pub turns: Option<u64>,
    /// 进度 0.0–1.0（未知为 `None`）。
    pub progress: Option<f32>,
}

/// hub 监督句柄：hub 工具 `jobs`/`cancel` 的数据源与执行端（进程内子任务面）。
///
/// 总线本体只有「注册表 + 邮箱」；子任务生命周期由监督体系（`agent-supervisor` +
/// 任务框架）承载。装配层把真实监督句柄桥接为本 trait 后注入 hub 工具：
/// - 未接入：`jobs` 各字段如实标 `unknown`；`cancel` 明确报错（拒绝而非静默 no-op）。
/// - 已接入：`jobs` 展示快照与在途计数；`cancel` 委托句柄（下达取消，不保证已终止）。
#[async_trait::async_trait]
pub trait HubSupervision: Send + Sync {
    /// 全量子任务快照。
    async fn jobs(&self) -> Vec<HubJobSnapshot>;

    /// 请求取消一个子任务；`Ok(())` 表示取消已下达（不保证目标已终止）。
    ///
    /// # Errors
    /// 目标不存在、已终结或监督体系无取消通道时，返回描述性错误文本。
    async fn cancel(&self, id: &str) -> Result<(), String>;
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use std::time::Duration;

    /// H41：名册含标签/状态/积压；`park`/`revive`/`wake` 状态迁移 + 唤醒通知。
    #[tokio::test]
    async fn roster_status_and_wake() {
        let hub = Hub::new().shared();
        let mut h = hub.register_handle("sub-1".into(), Some("Scout".into()), AgentStatus::Running);
        hub.register("main".into());

        let roster = hub.agents();
        assert_eq!(roster.len(), 2);
        let sub = roster.iter().find(|a| a.id == "sub-1").unwrap();
        assert_eq!(sub.label, "Scout");
        assert_eq!(sub.status, AgentStatus::Running);
        assert_eq!(sub.inbox_len, 0);

        // 投递两条 → 积压计数可见；消费后回落。
        hub.send("sub-1", "main", "a").unwrap();
        hub.send("sub-1", "main", "b").unwrap();
        assert_eq!(
            hub.agents()
                .iter()
                .find(|a| a.id == "sub-1")
                .unwrap()
                .inbox_len,
            2
        );
        assert_eq!(h.recv().await.unwrap().body, "a");
        assert_eq!(h.inbox_len(), 1);

        // park → 状态可见；wake → 回 running 并触发唤醒通知。
        assert!(hub.park("sub-1"));
        assert_eq!(
            hub.agents()
                .iter()
                .find(|a| a.id == "sub-1")
                .unwrap()
                .status,
            AgentStatus::Parked
        );
        let notified = tokio::time::timeout(Duration::from_millis(200), h.wake.notified());
        assert!(hub.wake("sub-1"), "wake 应对在册代理返回 true");
        notified.await.expect("wake 应触发通知");
        assert_eq!(
            hub.agents()
                .iter()
                .find(|a| a.id == "sub-1")
                .unwrap()
                .status,
            AgentStatus::Running
        );
        // 未在册 → false（不静默）。
        assert!(!hub.wake("ghost"));
        assert!(!hub.set_status("ghost", AgentStatus::Idle));
        // 收件箱关闭后 send 报 RecipientGone。
        drop(h);
        assert!(matches!(
            hub.send("sub-1", "main", "x"),
            Err(HubError::RecipientGone(_))
        ));
    }

    /// H41：容量预拒——`try_register_handle` 在满员时不创建收件箱；重名替换不占新名额。
    #[test]
    fn capacity_pre_rejects_registration_and_allows_replacement() {
        let hub = Hub::new();
        assert!(!hub.at_capacity(), "默认不限容量");
        hub.set_capacity(2);
        let _a = hub
            .try_register_handle("a".into(), None, AgentStatus::Running)
            .unwrap();
        let _b = hub
            .try_register_handle("b".into(), None, AgentStatus::Running)
            .unwrap();
        assert_eq!(hub.capacity(), 2);
        assert!(hub.at_capacity(), "2/2 应达容量");
        // 满员：新 id 被预拒（错误信息含上限），且名册长度不变。
        match hub.try_register_handle("c".into(), Some("c".into()), AgentStatus::Idle) {
            Err(HubError::AtCapacity { limit }) => assert_eq!(limit, 2),
            Err(other) => panic!("应被容量预拒，得到其它错误 {other:?}"),
            Ok(_) => panic!("应被容量预拒，却注册成功"),
        }
        assert_eq!(hub.len(), 2);
        assert!(!hub.contains("c"));
        // 重名注册是替换，不占新名额。
        let _b2 = hub.try_register_handle("b".into(), Some("b2".into()), AgentStatus::Idle);
        assert!(_b2.is_ok(), "重名替换应允许");
        assert_eq!(hub.len(), 2);
        assert_eq!(
            hub.agents()
                .iter()
                .find(|a| a.id == "b")
                .map(|a| a.label.clone()),
            Some("b2".into())
        );
        // 注销后腾出名额。
        hub.unregister("a");
        assert!(!hub.at_capacity());
        assert!(
            hub.try_register_handle("c".into(), None, AgentStatus::Running)
                .is_ok()
        );
        // 0 = 不限（复位）。
        hub.set_capacity(0);
        assert!(!hub.at_capacity());
    }

    /// H43 `unwatch`：撤销回执后等待者立即得到 `Revoked`（而非等到超时），未兑现计数下降。
    #[tokio::test]
    async fn revoke_ack_wakes_waiter_and_prunes_registry() {
        let hub = Hub::new();
        let _sub = hub.register("sub".into());
        let ack = hub.send_tracked("sub", "main", "hello").unwrap();
        assert_eq!(hub.pending_acks(), 1);
        // 撤销 → 等待者立即拿到 Revoked。
        let hub2 = Arc::new(hub);
        let waiter = {
            let h = Arc::clone(&hub2);
            tokio::spawn(async move { h.wait_ack(ack, std::time::Duration::from_secs(30)).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(hub2.revoke_ack(ack), "撤销应成功");
        match waiter.await.unwrap() {
            Err(AckError::Revoked(id)) => assert_eq!(id, ack),
            other => panic!("应得到 Revoked，得到 {other:?}"),
        }
        assert_eq!(hub2.pending_acks(), 0, "撤销后应移出表项");
        // 撤销后 ack 不再成立；重复撤销返回 false。
        assert!(!hub2.ack(ack));
        assert!(!hub2.revoke_ack(ack));
        // 未确认也不撤销 → 超时（保留表项，可再等或再撤）。
        let ack2 = hub2.send_tracked("sub", "main", "again").unwrap();
        assert!(matches!(
            hub2.wait_ack(ack2, std::time::Duration::from_millis(10)).await,
            Err(AckError::Timeout(id)) if id == ack2
        ));
        assert_eq!(hub2.pending_acks(), 1);
        assert!(hub2.revoke_ack(ack2));
    }

    /// H43：广播给除自己外的全部在册代理。
    #[test]
    fn broadcast_skips_sender_and_dead_inboxes() {
        let hub = Hub::new().shared();
        let mut a = hub.register("a".into());
        let mut b = hub.register("b".into());
        let mut c = hub.register("c".into());
        assert_eq!(hub.broadcast("a", "hello"), 2, "应投给 b/c，跳过 a");
        assert!(a.try_recv().is_err(), "发送方不应收到自己的广播");
        assert_eq!(b.try_recv().unwrap().body, "hello");
        assert_eq!(c.try_recv().unwrap().body, "hello");
        // 一个收件箱关闭 → 只投给剩下的。
        drop(b);
        assert_eq!(hub.broadcast("a", "again"), 1);
    }

    /// H43：回执——先确认后等待、超时、未知 id、重复确认。
    #[tokio::test]
    async fn ack_lifecycle() {
        let hub = Hub::new().shared();
        let mut rx = hub.register("peer".into());
        let ack_id = hub.send_tracked("peer", "main", "work").unwrap();
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.ack_id, Some(ack_id));
        // 正常路径：接收方确认 → 发送方等到。
        assert!(hub.ack(ack_id));
        assert!(
            hub.wait_ack(ack_id, Duration::from_millis(200))
                .await
                .is_ok()
        );
        // 等待消费后同一 id 再等 → UnknownAck（不静默成功）。
        assert_eq!(
            hub.wait_ack(ack_id, Duration::from_millis(10)).await,
            Err(AckError::UnknownAck(ack_id))
        );
        // 重复确认（条目已消费）→ false。
        assert!(!hub.ack(ack_id));

        // 先确认后等待：信号不丢。
        let ack2 = hub.send_tracked("peer", "main", "work2").unwrap();
        let _ = rx.recv().await.unwrap();
        assert!(hub.ack(ack2));
        assert!(hub.wait_ack(ack2, Duration::from_millis(50)).await.is_ok());

        // 未确认 → 超时（不是 UnknownAck）。
        let ack3 = hub.send_tracked("peer", "main", "work3").unwrap();
        let _ = rx.recv().await.unwrap();
        assert_eq!(
            hub.wait_ack(ack3, Duration::from_millis(20)).await,
            Err(AckError::Timeout(ack3))
        );
        // 目标未注册 → RecipientNotFound 且不泄漏回执条目。
        assert!(matches!(
            hub.send_tracked("nobody", "main", "x"),
            Err(HubError::RecipientNotFound(_))
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_send_recv_unregister() {
        let hub = Hub::new().shared();
        let mut rx = hub.register("alpha".into());
        hub.register("beta".into());
        assert_eq!(hub.peers(), vec!["alpha".to_string(), "beta".to_string()]);

        hub.send("alpha", "main", "你好").unwrap();
        hub.send("alpha", "beta", "第二封").unwrap();
        // 未注册目标报错。
        assert_eq!(
            hub.send("nobody", "main", "x"),
            Err(HubError::RecipientNotFound("nobody".into()))
        );

        let m1 = rx.blocking_recv().unwrap();
        assert_eq!(m1.from, "main");
        assert_eq!(m1.body, "你好");
        let m2 = rx.blocking_recv().unwrap();
        assert_eq!(m2.from, "beta");
        assert_eq!(m2.body, "第二封");

        hub.unregister("alpha");
        hub.unregister("alpha"); // 幂等
        assert_eq!(hub.peers(), vec!["beta".to_string()]);
        // 注销后投递 → 未注册错误。
        assert_eq!(
            hub.send("alpha", "main", "x"),
            Err(HubError::RecipientNotFound("alpha".into()))
        );
    }

    #[test]
    fn reregister_replaces_inbox() {
        let hub = Hub::new().shared();
        let mut rx1 = hub.register("a".into());
        // 重名注册：旧收件箱关闭（新收件箱存活才能收到消息）。
        let _rx2 = hub.register("a".into());
        assert!(hub.send("a", "x", "1").is_ok());
        assert!(rx1.try_recv().is_err(), "旧收件箱应被替换");
    }

    #[tokio::test]
    async fn send_after_drop_reports_gone() {
        let hub = Hub::new().shared();
        {
            let _rx = hub.register("t".into());
        }
        // receiver drop → unbounded_sender.send 报 RecipientGone。
        match hub.send("t", "x", "1") {
            Err(HubError::RecipientGone(id)) => assert_eq!(id, "t"),
            other => panic!("期望 RecipientGone，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn supervision_port_dispatches() {
        struct Fake;
        #[async_trait::async_trait]
        impl HubSupervision for Fake {
            async fn jobs(&self) -> Vec<HubJobSnapshot> {
                vec![HubJobSnapshot {
                    id: "sub-1".into(),
                    label: "Demo".into(),
                    status: "running".into(),
                    in_flight: true,
                    turns: Some(2),
                    progress: Some(0.5),
                }]
            }
            async fn cancel(&self, id: &str) -> Result<(), String> {
                if id == "sub-1" {
                    Ok(())
                } else {
                    Err(format!("{id} 不存在"))
                }
            }
        }
        let s: Arc<dyn HubSupervision> = Arc::new(Fake);
        let jobs = s.jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, "running");
        assert!(jobs[0].in_flight);
        assert!(s.cancel("sub-1").await.is_ok());
        assert!(s.cancel("nope").await.is_err());
    }
}
