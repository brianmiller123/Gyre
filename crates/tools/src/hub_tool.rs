//! `hub` 工具：进程内消息总线 + 子任务监督面（移植 oh-my-pi hub 工具的进程内子集）。
//!
//! 经 [`agent_core::hub::Hub`] 与在册代理互发消息：`send`（定向投递）、`recv`
//! （拉取自己的收件箱）、`list`（在册代理）、`wait`（阻塞等下一条消息）、`inbox`
//! （列队中消息，`peek` 不消费）；监督面经 [`agent_core::hub::HubSupervision`] 句柄：
//! `jobs`（子任务状态快照）、`cancel`（取消在途子任务；句柄未接入时明确报错而非
//! no-op）。装配层把本工具 id 注册为 `"main"`，子 Agent（task 委派）经
//! [`agent_core::hub::Hub::register`] 以 `"task-<n>"` 入册后可被父模型定向消息。
//!
//! 进程托管面（对齐 omp hub launch）：`start` / `ps` / `logs` / `stop` /
//! `restart` / `describe`，`send` / `wait` 入参含 `name` 时分流到进程语义
//! （与 `to` / `from` 互斥）。经 [`agent_supervisor::ProcessManager`] 进程内实现，
//! 由装配层 [`HubTool::with_processes`] 注入；未接入时明确报错（对齐 `cancel`
//! 先例）。有意偏差：无 broker 子进程、无磁盘持久化、无 detached/persist、
//! 无 PTY（管道 stdin/stdout）；进程退出不主动推消息，用 `wait`（name +
//! `for=exit`）或 `logs`（`follow=true`）轮询。

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_core::hub::{HubMessage, HubSupervision};
use agent_core::{CapabilityTier, ToolError, ToolResult};
use agent_supervisor::{ProcessManager, ProcessSpec, ReadySpec, RestartPolicy, Signal};
use async_trait::async_trait;
use serde_json::json;

use crate::{Concurrency, Tool, ToolContext};

/// `wait` 默认超时（毫秒）；对齐 omp `DEFAULT_IRC_TIMEOUT_MS`。
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 120_000;

/// 当前代理在总线上的注册 id（装配层注入）。
#[derive(Clone)]
pub struct HubIdentity {
    id: String,
}

impl HubIdentity {
    /// 构造。
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }

    /// 当前代理 id。
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// `hub`：进程内消息总线、子任务监督面与进程托管工具。
pub struct HubTool {
    hub: Arc<agent_core::hub::Hub>,
    identity: HubIdentity,
    // execute 签名为 &self（Tool trait），收件箱须可共享——tokio Mutex 包裹；
    // wait 会跨 await 持锁，同一代理的 hub 调用因此天然串行（单消费者邮箱）。
    inbox: tokio::sync::Mutex<agent_core::hub::HubHandle>,
    // peek 语义需要「通道 → 待取队列」单向搬运：mpsc 通道只能取不能窥，
    // pump 进待取队列后 peek 只读队列、消费路径才出队。
    pending: tokio::sync::Mutex<VecDeque<HubMessage>>,
    // 监督句柄（可选）：jobs 快照与 cancel 委托；未接入时 jobs 如实标 unknown、
    // cancel 明确报错（拒绝而非静默 no-op）。
    supervision: Option<Arc<dyn HubSupervision>>,
    // 进程托管（可选）：start/ps/logs/stop/restart/describe 与 name 分流的
    // send/wait 委托 ProcessManager；未接入时明确报错（拒绝而非静默 no-op）。
    processes: Option<Arc<ProcessManager>>,
    /// 异步后台作业管理器（可选）：`jobs`/`cancel`/`wait ids` 合并 `bg_*` 作业行；
    /// watch 语义抑制自动投递（结果经 wait 返回，恰好一次）。
    jobs: Option<Arc<agent_core::jobs::AsyncJobManager>>,
}

impl HubTool {
    /// 构造并注册当前代理（收件箱随工具持有；监督句柄与进程托管未接入）。
    #[must_use]
    pub fn register(hub: Arc<agent_core::hub::Hub>, id: impl Into<String>) -> Self {
        let identity = HubIdentity::new(id);
        // H41：以 HubHandle 注册（带标签/状态/积压计数），名册因此能显示真实积压。
        let inbox = hub.register_handle(
            identity.id.clone(),
            Some(identity.id.clone()),
            agent_core::hub::AgentStatus::Running,
        );
        Self {
            hub,
            identity,
            inbox: tokio::sync::Mutex::new(inbox),
            pending: tokio::sync::Mutex::new(VecDeque::new()),
            supervision: None,
            processes: None,
            jobs: None,
        }
    }

    /// 从既有构造（测试用：已注册的收件箱）。
    #[must_use]
    pub fn from_parts(
        hub: Arc<agent_core::hub::Hub>,
        identity: HubIdentity,
        inbox: agent_core::hub::HubHandle,
    ) -> Self {
        Self {
            hub,
            identity,
            inbox: tokio::sync::Mutex::new(inbox),
            pending: tokio::sync::Mutex::new(VecDeque::new()),
            supervision: None,
            processes: None,
            jobs: None,
        }
    }

    /// 注入监督句柄（builder）：接入后 `jobs` 展示子任务快照、`cancel` 委托句柄。
    #[must_use]
    pub fn with_supervision(mut self, supervision: Arc<dyn HubSupervision>) -> Self {
        self.supervision = Some(supervision);
        self
    }

    /// 注入进程托管管理器（builder）：接入后 `start`/`ps`/`logs`/`stop`/
    /// `restart`/`describe` 与带 `name` 的 `send`/`wait` 委托 [`ProcessManager`]。
    #[must_use]
    pub fn with_processes(mut self, processes: Arc<ProcessManager>) -> Self {
        self.processes = Some(processes);
        self
    }

    /// 注入异步后台作业管理器（builder）：接入后 `jobs` 展示 `bg_*` 作业行、
    /// `cancel` 可取消后台作业、`wait` 携带 `ids` 时等待作业完结并取回结果
    ///（watch 抑制自动投递，结果经 wait 恰好一次交付）。
    #[must_use]
    pub fn with_jobs(mut self, jobs: Arc<agent_core::jobs::AsyncJobManager>) -> Self {
        self.jobs = Some(jobs);
        self
    }

    /// 取异步作业管理器句柄；未接入返回 `None`（作业面属可选能力）。
    #[must_use]
    pub fn job_manager(&self) -> Option<&Arc<agent_core::jobs::AsyncJobManager>> {
        self.jobs.as_ref()
    }

    /// [`with_jobs`] 的可选变体（装配层按配置开关传入）。
    #[must_use]
    pub fn with_jobs_option(
        mut self,
        jobs: Option<Arc<agent_core::jobs::AsyncJobManager>>,
    ) -> Self {
        self.jobs = jobs;
        self
    }

    /// 把通道中当前可读的消息全部搬入待取队列（锁序恒为 inbox → pending）。
    async fn pump(&self) {
        let mut rx = self.inbox.lock().await;
        let mut pending = self.pending.lock().await;
        while let Some(m) = rx.try_recv() {
            pending.push_back(m);
        }
    }

    /// 取出第一条匹配 `from` 的待取消息（`from` 为 `None` 取队首）。
    async fn take_matching(&self, from: Option<&str>) -> Option<HubMessage> {
        self.pump().await;
        let mut pending = self.pending.lock().await;
        let idx = match from {
            None => {
                if pending.is_empty() {
                    None
                } else {
                    Some(0)
                }
            }
            Some(f) => pending.iter().position(|m| m.from == f),
        }?;
        let msg = pending.remove(idx)?;
        // H43：消息交到模型手里即视为「已读」→ 回执确认（发送方的 wait_ack 随之返回）。
        if let Some(ack_id) = msg.ack_id {
            self.hub.ack(ack_id);
        }
        Some(msg)
    }
}

impl Drop for HubTool {
    fn drop(&mut self) {
        self.hub.unregister(&self.identity.id);
    }
}

#[async_trait]
impl Tool for HubTool {
    fn name(&self) -> &'static str {
        "hub"
    }
    fn description(&self) -> &'static str {
        "进程内消息总线、子任务监督面与进程托管：代理消息（send 定向投递 / recv 拉取\
收件箱 / list 在册代理 / agents 名册（含状态与积压）/ broadcast 广播给除自己外全部 / \
wait 阻塞等下一条消息 / inbox 列队中消息，peek 不消费）、回执（send tracked=true 签发 \
ack_id；acks 查看未兑现数 / ack 确认 / unwatch 撤销，撤销后等待方立即得到明确错误）、\
子任务观测（jobs 快照 / cancel 取消，需装配层接入监督句柄）、长驻进程托管（start 拉起 / \
ps / logs / stop / restart / describe；send / wait 入参含 name 时作用于进程，与 to / \
from 互斥）。进程托管为进程内最小实现，有意偏差：无 broker 子进程、无磁盘持久化、\
不做 detached/persist 生命周期、无 PTY（stdin/stdout 为管道）；进程退出不主动推送\
消息，用 wait（name + for=exit）或 logs（follow=true）轮询。消息、回执与进程记录不跨\
会话保留；监督句柄/进程托管未接入时相应 op 明确报错而非静默忽略。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "op": { "type": "string",
                        "enum": ["send", "recv", "list", "agents", "broadcast", "wait", "inbox", "jobs", "cancel",
                                 "ack", "unwatch", "acks",
                                 "start", "ps", "logs", "stop", "restart", "describe"],
                        "default": "list",
                        "description": "send 投递 / recv 拉取收件箱（drain）/ list 在册代理 / agents 名册（含状态与积压，H41）/ broadcast 广播给除自己外全部 / wait 阻塞等下一条消息 / inbox 列队中消息 / jobs 子任务快照 / cancel 取消在途子任务；进程托管：start 拉起 / ps 快照 / logs 读日志 / stop 停止 / restart 重启 / describe 规格+状态" },
                "to": { "type": "string", "description": "send：目标代理 id（list 可查）" },
                "message": { "type": "string", "description": "send：消息体（纯文本）" },
                "from": { "type": "string", "description": "wait：只接受来自该代理 id 的消息（其余消息缓存不丢）" },
                "timeout_ms": { "type": "integer", "minimum": 0,
                                "description": "wait：超时毫秒（默认 120000；0=无限等待）" },
                "peek": { "type": "boolean", "description": "inbox：true 时仅列出队中消息，不消费" },
                "ack_id": { "type": "integer", "minimum": 1,
                            "description": "ack：确认某回执（send 带 tracked=true 时返回 id）；unwatch：撤销该回执" },
                "tracked": { "type": "boolean",
                             "description": "send：true 时签发回执 id（接收方消费后自动确认，可用 wait_ack 语义核对）" },
                "ids": { "type": "array", "items": { "type": "string" },
                         "description": "cancel：要取消的子任务 id 列表（jobs 可查）" },
                "name": { "type": "string", "description": "进程名；send/wait 携带 name 时分流到进程语义（与 to/from 互斥）" },
                "application": { "type": "string", "description": "start：可执行文件" },
                "args": { "type": "array", "items": { "type": "string" },
                          "description": "start：参数列表" },
                "env": { "type": "object", "additionalProperties": { "type": "string" },
                         "description": "start：附加环境变量" },
                "cwd": { "type": "string", "description": "start：工作目录（默认当前目录）" },
                "ready": { "type": "object",
                           "properties": {
                               "log": { "type": "string", "description": "就绪日志正则（Rust regex）" },
                               "port": { "type": "integer", "minimum": 1, "maximum": 65535, "description": "就绪端口（对 127.0.0.1 探测）" },
                               "timeout": { "type": "number", "description": "就绪超时秒数（默认 30）" }
                           },
                           "description": "start：就绪条件（log/port 至少其一；超时→状态 failed 但进程保留可 stop）" },
                "restart": { "type": "string", "enum": ["no", "on-failure", "always"],
                             "description": "start：重启策略（退避 500ms→8s，最多 5 次；显式 stop 不算失败）" },
                "lines": { "type": "integer", "minimum": 1, "maximum": 1000,
                           "description": "logs：行数（默认 100，上限 1000）" },
                "head": { "type": "boolean", "description": "logs：从头取（默认取尾部）" },
                "grep": { "type": "string", "description": "logs：行过滤正则（Rust regex）" },
                "follow": { "type": "boolean", "description": "logs：等待新输出（配合 timeout，超时即返回当前结果）" },
                "cursor": { "type": "integer", "minimum": 0,
                            "description": "logs：起始行序号（上次返回的 cursor）" },
                "for": { "type": "string", "enum": ["ready", "exit"],
                         "description": "wait(name)：等待就绪或退出（默认 exit）" },
                "pattern": { "type": "string", "description": "wait(name)：等待输出匹配该正则（follow 语义）" },
                "text": { "type": "string", "description": "send(name)：写入 stdin 的文本" },
                "enter": { "type": "boolean", "description": "send(name)：文本后追加换行（默认 true）" },
                "keys": { "type": "array", "items": { "type": "string" },
                          "description": "send(name)：按键序列（ENTER TAB ESCAPE CTRL_C CTRL_D UP DOWN LEFT RIGHT）" },
                "signal": { "type": "string", "enum": ["SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT", "SIGKILL"],
                            "description": "send(name)：向进程组发送信号（给出 signal 时忽略 text/keys）" },
                "timeout": { "type": "number", "description": "进程 op 超时秒数（wait/logs follow 默认 30；stop 默认 5）" }
            },
            "required": ["op"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        // 仅进程内消息与子任务观测/取消委托，不触工作区——read 级审批（对齐 omp）。
        CapabilityTier::ReadOnly
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let op = input
            .get("op")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("list");
        match op {
            "send" => {
                let name = input
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty());
                let to = input
                    .get("to")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|t| !t.is_empty());
                if name.is_some() && to.is_some() {
                    return Err(ToolError::InvalidArgs(
                        "send 的 name（进程）与 to（代理）互斥，只能二选一".into(),
                    ));
                }
                if let Some(n) = name {
                    return self.proc_send(n, &input).await;
                }
                let to = to.ok_or_else(|| ToolError::InvalidArgs("send 需要 `to` 参数".into()))?;
                let message = input
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .ok_or_else(|| ToolError::InvalidArgs("send 需要 `message` 参数".into()))?;
                // H43：`tracked=true` 签发回执（接收方消费即自动确认）；默认不签发。
                let tracked = input
                    .get("tracked")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if tracked {
                    let ack_id = self
                        .hub
                        .send_tracked(to, &self.identity.id, message)
                        .map_err(|e| ToolError::Execution(format!("hub send 失败: {e}")))?;
                    return Ok(ToolResult::text(format!(
                        "已投递给 {to}（来自 {}；回执 ack_id={ack_id}，可用 unwatch 撤销）",
                        self.identity.id
                    )));
                }
                self.hub
                    .send(to, &self.identity.id, message)
                    .map_err(|e| ToolError::Execution(format!("hub send 失败: {e}")))?;
                Ok(ToolResult::text(format!(
                    "已投递给 {to}（来自 {}）",
                    self.identity.id
                )))
            }
            "recv" => {
                let mut msgs = Vec::new();
                while let Some(m) = self.take_matching(None).await {
                    msgs.push(format!("<{}> {}", m.from, m.body));
                }
                if msgs.is_empty() {
                    Ok(ToolResult::text("（收件箱为空）"))
                } else {
                    Ok(ToolResult::text(msgs.join("\n")))
                }
            }
            "wait" => {
                let name = input
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty());
                let from = input
                    .get("from")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|f| !f.is_empty());
                if name.is_some() && from.is_some() {
                    return Err(ToolError::InvalidArgs(
                        "wait 的 name（进程）与 from（代理）互斥，只能二选一".into(),
                    ));
                }
                if let Some(n) = name {
                    return self.proc_wait(n, &input).await;
                }
                self.wait(&input, ctx).await
            }
            "inbox" => {
                let peek = input
                    .get("peek")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                self.pump().await;
                let mut pending = self.pending.lock().await;
                if pending.is_empty() {
                    return Ok(ToolResult::text("（收件箱为空）"));
                }
                let header = if peek {
                    format!("未读消息（{} 条，peek 未消费）：", pending.len())
                } else {
                    format!("收件箱（{} 条）：", pending.len())
                };
                let lines: Vec<String> = pending
                    .iter()
                    .map(|m| format!("- <{}> {}", m.from, m.body))
                    .collect();
                if !peek {
                    pending.clear();
                }
                Ok(ToolResult::text(
                    [header]
                        .into_iter()
                        .chain(lines)
                        .collect::<Vec<_>>()
                        .join("\n"),
                ))
            }
            "agents" => {
                // H41：名册（id / 标签 / 生命周期状态 / 收件箱积压）。
                let roster = self.hub.agents();
                if roster.is_empty() {
                    return Ok(ToolResult::text("名册为空（无在册代理）"));
                }
                let mut lines = vec![format!("在册代理（{}）：", roster.len())];
                for a in &roster {
                    let self_mark = if a.id == self.identity.id {
                        "（自己）"
                    } else {
                        ""
                    };
                    lines.push(format!(
                        "- {}{} [{}] 标签={} 积压={}",
                        a.id,
                        self_mark,
                        a.status.as_str(),
                        a.label,
                        a.inbox_len
                    ));
                }
                Ok(ToolResult::text(lines.join("\n")))
            }
            "ack" => {
                let ack_id = input
                    .get("ack_id")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| ToolError::InvalidArgs("ack 需要 `ack_id` 参数".into()))?;
                if self.hub.ack(ack_id) {
                    Ok(ToolResult::text(format!("回执 {ack_id} 已确认")))
                } else {
                    Err(ToolError::InvalidArgs(format!(
                        "回执 {ack_id} 不存在（已确认或从未签发）"
                    )))
                }
            }
            "unwatch" => {
                let ack_id = input
                    .get("ack_id")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| ToolError::InvalidArgs("unwatch 需要 `ack_id` 参数".into()))?;
                if self.hub.revoke_ack(ack_id) {
                    Ok(ToolResult::text(format!(
                        "已撤销回执 {ack_id}（等待方会立即收到已撤销错误，不再等到超时）"
                    )))
                } else {
                    Err(ToolError::InvalidArgs(format!("回执 {ack_id} 不存在")))
                }
            }
            "acks" => Ok(ToolResult::text(format!(
                "未兑现回执：{} 条",
                self.hub.pending_acks()
            ))),
            "broadcast" => {
                // H43：广播给除自己外的全部在册代理，返回投递数。
                let message = input
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .ok_or_else(|| {
                        ToolError::InvalidArgs("broadcast 需要 `message` 参数".into())
                    })?;
                let n = self.hub.broadcast(&self.identity.id, message);
                Ok(ToolResult::text(format!("已广播给 {n} 个代理")))
            }
            "list" => {
                let peers = self.hub.peers();
                if peers.is_empty() {
                    Ok(ToolResult::text("（总线上无在册代理）"))
                } else {
                    Ok(ToolResult::text(format!(
                        "在册代理（{}）：{}",
                        peers.len(),
                        peers.join(", ")
                    )))
                }
            }
            "jobs" => self.jobs().await,
            "cancel" => self.cancel(&input).await,
            "start" => self.proc_start(&input).await,
            "ps" => self.proc_ps().await,
            "logs" => self.proc_logs(&input).await,
            "stop" => self.proc_stop(&input).await,
            "restart" => self.proc_restart(&input).await,
            "describe" => self.proc_describe(&input).await,
            other => Err(ToolError::InvalidArgs(format!("未知 op：{other}"))),
        }
    }
}

impl HubTool {
    /// `wait`：阻塞等待下一条消息（先清待取队列，再挂起等通道），等待窗口内任一
    /// 事件（消息到达 / 超时 / steering 取消）即返回；`from` 过滤时其余消息缓存不丢。
    async fn wait(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let from = input
            .get("from")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|f| !f.is_empty());
        let timeout_ms = match input.get("timeout_ms") {
            None | Some(serde_json::Value::Null) => DEFAULT_WAIT_TIMEOUT_MS,
            Some(v) => v.as_u64().ok_or_else(|| {
                ToolError::InvalidArgs(format!("wait 的 timeout_ms 必须是非负整数，得到 {v}"))
            })?,
        };
        // 已缓冲的匹配消息立即返回（对齐 omp drainPendingInbox，不空挂一轮）。
        if let Some(m) = self.take_matching(from).await {
            return Ok(ToolResult::text(format!("<{}> {}", m.from, m.body)));
        }
        // `ids` + 作业管理器 → 作业等待面：watch 抑制自动投递，任一作业完结即
        // 返回其结果（恰好一次）；等待同时仍响应消息到达与 steering 取消。
        let wait_ids: Vec<String> = input
            .get("ids")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !wait_ids.is_empty() {
            if let Some(jobs) = self.jobs.as_ref() {
                return self
                    .wait_jobs(input, ctx, jobs, &wait_ids, timeout_ms)
                    .await;
            }
            return Err(ToolError::Execution(
                "wait 携带 ids 需要异步作业管理器（装配层未接入）".into(),
            ));
        }
        let mut rx = self.inbox.lock().await;
        let deadline = (timeout_ms != 0)
            .then(|| tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms));
        loop {
            let msg = tokio::select! {
                biased;
                () = ctx.cancel.cancelled() => {
                    return Err(ToolError::Execution("hub wait 被取消".into()));
                }
                m = rx.recv() => m,
                _ = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    let note = from.map_or_else(String::new, |f| format!("（过滤：from {f}）"));
                    return Ok(ToolResult::text(format!(
                        "等待 {}ms 内无消息{note}",
                        timeout_ms
                    )));
                }
            };
            match msg {
                Some(m) if from.is_none_or(|f| m.from == f) => {
                    return Ok(ToolResult::text(format!("<{}> {}", m.from, m.body)));
                }
                Some(m) => self.pending.lock().await.push_back(m),
                None => {
                    return Err(ToolError::Execution(
                        "hub 收件箱已关闭（代理已注销）".into(),
                    ));
                }
            }
        }
    }

    /// `wait ids`：等待异步作业完结（omp hub wait 的 watch 语义）。
    ///
    /// watch 抑制自动投递；作业完结 → consume → 返回结果文本（恰好一次）。
    /// 等待窗口内消息到达 / steering 取消同样返回。轮询阶梯防紧贴空转：
    /// 同 owner 连续等待按 [5s,10s,30s,60s,300s] 爬升，间隔 ≥60s 回落底部。
    async fn wait_jobs(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
        jobs: &Arc<agent_core::jobs::AsyncJobManager>,
        ids: &[String],
        timeout_ms: u64,
    ) -> Result<ToolResult, ToolError> {
        // H27：任何返回路径都必须释放 watch——`watch` 期间完结的作业结果会在入队时被
        // 丢弃（`enqueue_delivery` 抑制直接返回），不释放就永久静默丢失。释放 = 解除
        // watch（未来完结正常入队）+ 把「已完结但被抑制」的结果重新入队。
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let out = self
            .wait_jobs_inner(input, ctx, jobs, ids, timeout_ms)
            .await;
        jobs.unwatch_jobs(&id_refs);
        jobs.resume_deliveries(&id_refs);
        out
    }

    async fn wait_jobs_inner(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
        jobs: &Arc<agent_core::jobs::AsyncJobManager>,
        ids: &[String],
        timeout_ms: u64,
    ) -> Result<ToolResult, ToolError> {
        let from = input
            .get("from")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|f| !f.is_empty());
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        jobs.watch_jobs(&id_refs);
        let user_deadline = (timeout_ms != 0)
            .then(|| tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms));
        let mut rx = self.inbox.lock().await;
        loop {
            // 完结且未消费的作业：consume 后返回结果（此后自动投递不会再发）。
            if let Some(job) = Self::take_settled_result(jobs, ids) {
                let text = if job.status == agent_core::jobs::JobStatus::Completed {
                    job.result_text.clone().unwrap_or_default()
                } else {
                    job.error_text.clone().unwrap_or_default()
                };
                return Ok(ToolResult::text(format!(
                    "作业 {}（{}，{}）：\n{text}",
                    job.id,
                    job.job_type.as_str(),
                    job.status.as_str()
                )));
            }
            let any_running = ids.iter().any(|id| {
                jobs.get_job(id).is_none_or(|j| {
                    j.status == agent_core::jobs::JobStatus::Running
                        || !jobs.is_job_result_consumed(id)
                })
            });
            if !any_running {
                return Ok(ToolResult::text(
                    "所有指定作业均已完结且结果已被消费（无新事件）".to_string(),
                ));
            }
            // 阶梯等待：连续轮询爬升，间隔 ≥60s 回落（对齐 omp smart poll）。
            let ladder_ms = jobs.next_poll_wait_ms(Some(&self.identity.id));
            let iter_deadline =
                tokio::time::Instant::now() + std::time::Duration::from_millis(ladder_ms);
            let iter_deadline = match user_deadline {
                Some(d) if d < iter_deadline => d,
                _ => iter_deadline,
            };
            let msg = tokio::select! {
                biased;
                () = ctx.cancel.cancelled() => {
                    return Err(ToolError::Execution("hub wait 被取消".into()));
                }
                m = rx.recv() => m,
                _ = tokio::time::sleep_until(iter_deadline) => {
                    jobs.record_poll_wait_end(Some(&self.identity.id));
                    let timed_out = user_deadline
                        .is_some_and(|d| tokio::time::Instant::now() >= d);
                    if timed_out {
                        // 期限到达：最后抓取一次已完结结果（睡醒间隙完结的作业）。
                        if let Some(job) = Self::take_settled_result(jobs, ids) {
                            let text = if job.status == agent_core::jobs::JobStatus::Completed {
                                job.result_text.clone().unwrap_or_default()
                            } else {
                                job.error_text.clone().unwrap_or_default()
                            };
                            return Ok(ToolResult::text(format!(
                                "作业 {}（{}，{}）：\n{text}",
                                job.id,
                                job.job_type.as_str(),
                                job.status.as_str()
                            )));
                        }
                        let mut lines = vec![format!(
                            "等待 {}ms 内指定作业未全部完结：",
                            timeout_ms
                        )];
                        for id in ids {
                            match jobs.get_job(id) {
                                Some(j) => lines.push(format!(
                                    "- {id}：{}（{}）",
                                    j.status.as_str(),
                                    j.label
                                )),
                                None => lines.push(format!("- {id}：未找到")),
                            }
                        }
                        lines.push(
                            "提示：watch 已释放——作业完成后结果会以异步通知投递（也可再次 `wait ids` 取回，先到者消费、不会重复）。"
                                .into(),
                        );
                        return Ok(ToolResult::text(lines.join("\n")));
                    }
                    continue;
                }
            };
            match msg {
                Some(m) if from.is_none_or(|f| m.from == f) => {
                    return Ok(ToolResult::text(format!("<{}> {}", m.from, m.body)));
                }
                Some(m) => self.pending.lock().await.push_back(m),
                None => {
                    return Err(ToolError::Execution(
                        "hub 收件箱已关闭（代理已注销）".into(),
                    ));
                }
            }
        }
    }

    /// 取第一个「已完结且未消费」的作业：consume 标记后交还调用方（恰一次交付）。
    fn take_settled_result(
        jobs: &Arc<agent_core::jobs::AsyncJobManager>,
        ids: &[String],
    ) -> Option<agent_core::jobs::AsyncJob> {
        for id in ids {
            let Some(job) = jobs.get_job(id) else {
                continue; // 未知 id：保留等待（可能是刚注册前的竞态）。
            };
            if job.status == agent_core::jobs::JobStatus::Running {
                continue;
            }
            if !jobs.is_job_result_consumed(id) {
                jobs.consume_job_results(&[id]);
                return Some(job);
            }
        }
        None
    }

    /// `jobs`：在册代理 + 子任务状态快照。监督句柄未接入时字段如实标 `unknown`。
    async fn jobs(&self) -> Result<ToolResult, ToolError> {
        let peers = self.hub.peers();
        let sup_jobs = match self.supervision.as_ref() {
            Some(s) => Some(s.jobs().await),
            None => None,
        };
        let mut lines = Vec::new();
        if peers.is_empty() {
            lines.push("在册代理：无".to_string());
        } else {
            lines.push(format!("在册代理（{}）：", peers.len()));
            for p in &peers {
                let self_mark = if *p == self.identity.id {
                    "（自己）"
                } else {
                    ""
                };
                let status = sup_jobs
                    .as_ref()
                    .and_then(|js| js.iter().find(|j| &j.id == p))
                    .map_or_else(|| "unknown".to_string(), |j| j.status.clone());
                lines.push(format!("- {p}{self_mark}：状态 {status}"));
            }
        }
        match &sup_jobs {
            Some(js) => {
                let inflight = js.iter().filter(|j| j.in_flight).count();
                lines.push(format!("子任务（{} 个，在途 {inflight}）：", js.len()));
                for j in js {
                    let mut row = format!("- {} {}：{}", j.id, j.label, j.status);
                    if let Some(t) = j.turns {
                        row.push_str(&format!("，轮次 {t}"));
                    }
                    if let Some(p) = j.progress {
                        row.push_str(&format!("，进度 {:.0}%", (p * 100.0).round()));
                    }
                    if !j.in_flight {
                        row.push_str("（已终结）");
                    }
                    lines.push(row);
                }
            }
            None => {
                lines.push("子任务：监督句柄未接入（unknown）".to_string());
                lines.push("在途子任务：unknown".to_string());
            }
        }
        if let Some(jobs) = self.jobs.as_ref() {
            let all = jobs.all_jobs(Some(&self.identity.id));
            if all.is_empty() {
                lines.push("后台作业：无".to_string());
            } else {
                let running = all
                    .iter()
                    .filter(|j| j.status == agent_core::jobs::JobStatus::Running)
                    .count();
                lines.push(format!("后台作业（{} 个，运行中 {running}）：", all.len()));
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or_default();
                for j in &all {
                    let mut row = format!(
                        "- {} {} {}：{}",
                        j.id,
                        j.job_type.as_str(),
                        j.label,
                        j.status.as_str()
                    );
                    row.push_str(&format!("，已运行 {}s", j.duration_ms(now) / 1000));
                    if let Some(t) = &j.result_text {
                        let preview: String = t.chars().take(80).collect();
                        row.push_str(&format!("，结果预览：{preview}"));
                    }
                    if let Some(e) = &j.error_text {
                        let preview: String = e.chars().take(80).collect();
                        row.push_str(&format!("，错误预览：{preview}"));
                    }
                    lines.push(row);
                }
                lines.push(
                    "提示：wait ids 可取回完结作业结果（恰一次）；cancel ids 可取消运行中作业。"
                        .to_string(),
                );
            }
        }
        Ok(ToolResult::text(lines.join("\n")))
    }

    /// `cancel`：经监督句柄逐 id 下达取消；未在册 id 按 not_found 上报、不透传句柄；
    /// 句柄未接入时明确报错（拒绝而非 no-op）。
    async fn cancel(&self, input: &serde_json::Value) -> Result<ToolResult, ToolError> {
        let ids: Vec<String> = input
            .get("ids")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if ids.is_empty() {
            return Err(ToolError::InvalidArgs(
                "cancel 需要非空的 `ids` 参数（字符串数组）".into(),
            ));
        }
        // omp 语义（hub/jobs.ts `cancelAgentRegistration`）：先查注册表再动手——
        // 不在册的 id 按 not_found 上报，不透传句柄（对句柄的无谓调用即谎报状态）。
        // `bg_*` 异步作业优先经管理器取消（owner 限定，跨代理拒绝在管理层）。
        let known: HashSet<String> = match self.supervision.as_ref() {
            Some(sup) => sup.jobs().await.into_iter().map(|j| j.id).collect(),
            None => HashSet::new(),
        };
        let mut lines = vec!["取消请求处理结果：".to_string()];
        for id in ids {
            if id == self.identity.id {
                lines.push(format!("- {id}：拒绝（不能取消自己）"));
                continue;
            }
            let is_async_job = self.jobs.as_ref().is_some_and(|j| j.get_job(&id).is_some());
            if is_async_job {
                let jobs = self.jobs.as_ref().expect("checked above");
                if jobs.cancel(&id, Some(&self.identity.id)) {
                    lines.push(format!("- {id}：已下达取消"));
                } else {
                    lines.push(format!("- {id}：失败（作业已终结或非本人所有）"));
                }
                continue;
            }
            if !known.contains(&id) {
                lines.push(format!("- {id}：未找到（非本代理派生的子任务或不存在）"));
                continue;
            }
            let Some(sup) = self.supervision.as_ref() else {
                lines.push(format!("- {id}：未找到（监督句柄未接入）"));
                continue;
            };
            match sup.cancel(&id).await {
                Ok(()) => lines.push(format!("- {id}：已下达取消")),
                Err(e) => lines.push(format!("- {id}：失败（{e}）")),
            }
        }
        Ok(ToolResult::text(lines.join("\n")))
    }
}

// ───────────────────────────── 进程托管（launch 面）─────────────────────────────

impl HubTool {
    /// 取进程托管句柄；未接入时明确报错（对齐 cancel 先例，拒绝而非静默 no-op）。
    fn processes(&self) -> Result<&Arc<ProcessManager>, ToolError> {
        self.processes.as_ref().ok_or_else(|| {
            ToolError::Execution(
                "进程托管未接入（装配层未注入 ProcessManager），拒绝执行而非静默忽略".into(),
            )
        })
    }

    /// `start`：按参数构造规格并拉起进程，阻塞至就绪/超时。
    async fn proc_start(&self, input: &serde_json::Value) -> Result<ToolResult, ToolError> {
        let pm = self.processes()?;
        let name = required_str(input, "name", "start 需要 `name` 参数")?;
        let application = required_str(input, "application", "start 需要 `application` 参数")?;
        let args: Vec<String> = str_vec(input.get("args")).unwrap_or_default();
        let env: BTreeMap<String, String> = input
            .get("env")
            .and_then(serde_json::Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let cwd: Option<PathBuf> = input
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from);
        let ready = match input.get("ready") {
            None | Some(serde_json::Value::Null) => ReadySpec::default(),
            Some(r) => {
                let log = r
                    .get("log")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                let port = match r.get("port").and_then(serde_json::Value::as_u64) {
                    None => None,
                    Some(p) => match u16::try_from(p) {
                        Ok(0) => {
                            return Err(ToolError::InvalidArgs("ready.port 须为 1-65535".into()));
                        }
                        Ok(p) => Some(p),
                        Err(_) => {
                            return Err(ToolError::InvalidArgs("ready.port 须为 1-65535".into()));
                        }
                    },
                };
                if log.is_none() && port.is_none() {
                    return Err(ToolError::InvalidArgs(
                        "ready 需要 log 或 port 至少其一".into(),
                    ));
                }
                let timeout_secs = r
                    .get("timeout")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(30);
                ReadySpec {
                    log,
                    port,
                    timeout_secs,
                }
            }
        };
        let restart = match input.get("restart").and_then(serde_json::Value::as_str) {
            None | Some("no") => RestartPolicy::No,
            Some("on-failure") => RestartPolicy::OnFailure,
            Some("always") => RestartPolicy::Always,
            Some(other) => {
                return Err(ToolError::InvalidArgs(format!(
                    "restart 仅支持 no/on-failure/always，得到 {other}"
                )));
            }
        };
        let spec = ProcessSpec {
            name,
            application,
            args,
            cwd,
            env,
            ready,
            restart,
        };
        let info = pm
            .start(spec)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        let state_line = pm
            .ps()
            .await
            .into_iter()
            .find(|s| s.name == info.name)
            .map_or_else(
                || "状态：unknown".to_string(),
                |s| format!("状态：{}", render_state(&s.state)),
            );
        let pid = info
            .pid
            .map_or_else(String::new, |p| format!("（pid {p}）"));
        Ok(ToolResult::text(format!(
            "已启动 {}{pid}\n{state_line}",
            info.name
        )))
    }

    /// `ps`：全部托管进程快照。
    async fn proc_ps(&self) -> Result<ToolResult, ToolError> {
        let pm = self.processes()?;
        let procs = pm.ps().await;
        if procs.is_empty() {
            return Ok(ToolResult::text("（无托管进程）"));
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let lines: Vec<String> = procs
            .iter()
            .map(|p| {
                let uptime = now_ms.saturating_sub(p.started_at);
                let pid = p.pid.map_or_else(|| "—".to_string(), |v| v.to_string());
                format!(
                    "- {}：{}，pid {pid}，已运行 {}，自动重启 {} 次",
                    p.name,
                    render_state(&p.state),
                    humanize(uptime),
                    p.restarts
                )
            })
            .collect();
        Ok(ToolResult::text(format!(
            "托管进程（{}）：\n{}",
            procs.len(),
            lines.join("\n")
        )))
    }

    /// `logs`：读进程日志（游标翻页 / grep / head / follow）。
    async fn proc_logs(&self, input: &serde_json::Value) -> Result<ToolResult, ToolError> {
        let pm = self.processes()?;
        let name = required_str(input, "name", "logs 需要 `name` 参数")?;
        let cursor = input.get("cursor").and_then(serde_json::Value::as_u64);
        let grep = input
            .get("grep")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let head = input
            .get("head")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let lines = usize::try_from(
            input
                .get("lines")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        )
        .unwrap_or(0);
        let follow = input
            .get("follow")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let timeout_secs = input
            .get("timeout")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(30);
        let page = pm
            .logs(&name, cursor, grep, head, lines, follow, timeout_secs)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        if page.lines.is_empty() {
            if follow {
                return Ok(ToolResult::text(format!(
                    "follow {timeout_secs}s 内无新输出（cursor={}）",
                    page.cursor
                )));
            }
            return Ok(ToolResult::text(format!(
                "（无日志行）cursor={}",
                page.cursor
            )));
        }
        let body: Vec<String> = page
            .lines
            .iter()
            .map(|l| format!("[{}][{}] {}", l.seq, l.stream.label(), l.text))
            .collect();
        Ok(ToolResult::text(format!(
            "{}\ncursor={}（下次传 cursor={} 续读）",
            body.join("\n"),
            page.cursor,
            page.cursor
        )))
    }

    /// `stop`：SIGTERM → 宽限 → SIGKILL（unix 杀整组）。
    async fn proc_stop(&self, input: &serde_json::Value) -> Result<ToolResult, ToolError> {
        let pm = self.processes()?;
        let name = required_str(input, "name", "stop 需要 `name` 参数")?;
        let timeout_secs = input
            .get("timeout")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(5);
        let status = pm
            .stop(&name, timeout_secs)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(ToolResult::text(format!(
            "已请求停止 {name}；最终状态：{}",
            render_state(&status.state)
        )))
    }

    /// `restart`：复用存档规格重启，阻塞至就绪/超时。
    async fn proc_restart(&self, input: &serde_json::Value) -> Result<ToolResult, ToolError> {
        let pm = self.processes()?;
        let name = required_str(input, "name", "restart 需要 `name` 参数")?;
        let info = pm
            .restart(&name)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        let state_line = pm
            .ps()
            .await
            .into_iter()
            .find(|s| s.name == info.name)
            .map_or_else(
                || "状态：unknown".to_string(),
                |s| format!("状态：{}", render_state(&s.state)),
            );
        let pid = info
            .pid
            .map_or_else(String::new, |p| format!("（pid {p}）"));
        Ok(ToolResult::text(format!(
            "已重启 {}{pid}\n{state_line}",
            info.name
        )))
    }

    /// `describe`：启动规格 + 当前状态（JSON）。
    async fn proc_describe(&self, input: &serde_json::Value) -> Result<ToolResult, ToolError> {
        let pm = self.processes()?;
        let name = required_str(input, "name", "describe 需要 `name` 参数")?;
        let v = pm
            .describe(&name)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        let pretty = serde_json::to_string_pretty(&v)
            .map_err(|e| ToolError::Execution(format!("describe 序列化失败：{e}")))?;
        Ok(ToolResult::text(pretty))
    }

    /// `send`（name 分流）：写 stdin（text/enter/keys）或发信号。
    async fn proc_send(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let pm = self.processes()?;
        if let Some(sig) = input.get("signal").and_then(serde_json::Value::as_str) {
            let s = Signal::parse(sig).ok_or_else(|| {
                ToolError::InvalidArgs(format!(
                    "未知信号 {sig}（支持 SIGINT SIGTERM SIGHUP SIGQUIT SIGKILL）"
                ))
            })?;
            pm.signal(name, s)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;
            return Ok(ToolResult::text(format!("已向 {name} 进程组发送 {sig}")));
        }
        let text = input
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let enter = input
            .get("enter")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let keys = str_vec(input.get("keys")).unwrap_or_default();
        pm.send(name, text, enter, &keys)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(ToolResult::text(format!(
            "已写入 {name} stdin（text {} 字符，keys {} 个，enter {enter}）",
            text.chars().count(),
            keys.len()
        )))
    }

    /// `wait`（name 分流）：`pattern` 等输出匹配（follow 语义）；否则按
    /// `for=ready|exit` 轮询状态直至满足或超时。
    async fn proc_wait(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let pm = self.processes()?;
        let timeout_secs = input
            .get("timeout")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(30);
        if let Some(pattern) = input.get("pattern").and_then(serde_json::Value::as_str) {
            let page = pm
                .logs(
                    name,
                    None,
                    Some(pattern.to_string()),
                    false,
                    1,
                    true,
                    timeout_secs,
                )
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;
            return match page.lines.last() {
                Some(l) => Ok(ToolResult::text(format!(
                    "输出已匹配 {pattern}：[{}][{}] {}",
                    l.seq,
                    l.stream.label(),
                    l.text
                ))),
                None => Ok(ToolResult::text(format!(
                    "等待 {timeout_secs}s 内输出未匹配 {pattern}"
                ))),
            };
        }
        let for_ = input
            .get("for")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("exit");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            match pm.ps().await.into_iter().find(|s| s.name == name) {
                Some(s) => {
                    let satisfied = match for_ {
                        "ready" => s.state == agent_supervisor::ProcessState::Running,
                        "exit" => s.state.is_terminal(),
                        other => {
                            return Err(ToolError::InvalidArgs(format!(
                                "wait(name) 的 for 仅支持 ready/exit，得到 {other}"
                            )));
                        }
                    };
                    if satisfied {
                        return Ok(ToolResult::text(format!(
                            "进程 {name}：{}",
                            render_state(&s.state)
                        )));
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Ok(ToolResult::text(format!(
                            "等待 {timeout_secs}s 内进程 {name} 未满足 for={for_}（当前：{}）",
                            render_state(&s.state)
                        )));
                    }
                }
                None => {
                    return Err(ToolError::Execution(format!("进程 {name} 不存在")));
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// 取必填字符串参数。
fn required_str(input: &serde_json::Value, key: &str, err: &str) -> Result<String, ToolError> {
    input
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ToolError::InvalidArgs(err.into()))
}

/// 取字符串数组参数。
fn str_vec(v: Option<&serde_json::Value>) -> Option<Vec<String>> {
    v.and_then(serde_json::Value::as_array).map(|a| {
        a.iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

/// 状态的人类可读渲染。
fn render_state(state: &agent_supervisor::ProcessState) -> String {
    use agent_supervisor::ProcessState as S;
    match state {
        S::Starting => "starting".to_string(),
        S::Running => "running".to_string(),
        S::Exited { code } => code.map_or_else(
            || "exited（信号终止）".to_string(),
            |c| format!("exited code={c}"),
        ),
        S::Failed { reason } => format!("failed：{reason}"),
        S::Stopped => "stopped".to_string(),
    }
}

/// 毫秒时长的紧凑人类可读（`42s` / `3m12s` / `2h05m`）。
fn humanize(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::hub::HubJobSnapshot;
    use agent_core::{ApprovalDecision, ApprovalPolicy, ApprovalRequest, AskMessage, AskResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct NoopApproval;
    #[async_trait::async_trait]
    impl ApprovalPolicy for NoopApproval {
        fn decide(&self, _r: &ApprovalRequest<'_>) -> ApprovalDecision {
            ApprovalDecision::Allow
        }
        async fn prompt(&self, _a: &AskMessage) -> Result<AskResponse, ToolError> {
            Ok(AskResponse::Yes)
        }
    }

    fn ctx<'a>(
        ws: &'a agent_core::Workspace,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> ToolContext<'a> {
        ToolContext {
            workspace: ws,
            approval: &NoopApproval,
            cancel,
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
            update_tx: None,
            conflicts: None,
            pending_rewrites: None,
            context: None,
            snapshots: None,
            tool_call_id: None,
        }
    }

    /// 假监督句柄：jobs 返回固定快照；cancel 计数并按 `cancel_ok` 给出结果。
    struct FakeSup {
        jobs: Vec<HubJobSnapshot>,
        cancel_ok: bool,
        cancels: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl HubSupervision for FakeSup {
        async fn jobs(&self) -> Vec<HubJobSnapshot> {
            self.jobs.clone()
        }
        async fn cancel(&self, _id: &str) -> Result<(), String> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            if self.cancel_ok {
                Ok(())
            } else {
                Err("监督注册表仅观测，暂无取消通道".into())
            }
        }
    }

    fn fake_sup(cancel_ok: bool) -> (Arc<FakeSup>, Arc<AtomicUsize>) {
        let cancels = Arc::new(AtomicUsize::new(0));
        let sup = Arc::new(FakeSup {
            jobs: vec![
                HubJobSnapshot {
                    id: "sub-1".into(),
                    label: "BuildRel".into(),
                    status: "running".into(),
                    in_flight: true,
                    turns: Some(3),
                    progress: Some(0.45),
                },
                HubJobSnapshot {
                    id: "sub-2".into(),
                    label: "Docs".into(),
                    status: "done".into(),
                    in_flight: false,
                    turns: Some(9),
                    progress: Some(1.0),
                },
            ],
            cancel_ok,
            cancels: Arc::clone(&cancels),
        });
        (sup, cancels)
    }

    #[tokio::test]
    async fn send_recv_list_roundtrip() {
        let hub = agent_core::hub::Hub::new().shared();
        // 子代理入册（持有收件箱但不在本测试消费——用 from_parts 保留其生命周期）。
        let sub_inbox = hub.register("task-1".into());
        let _keep = sub_inbox;

        let tool = HubTool::register(Arc::clone(&hub), "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);

        // list 看到两个在册代理。
        let out = tool
            .execute(serde_json::json!({"op": "list"}), c)
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(t.contains("task-1") && t.contains("main"), "{t}");

        // send：main → task-1。
        let out = tool
            .execute(
                serde_json::json!({"op": "send", "to": "task-1", "message": "开始任务"}),
                c,
            )
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("已投递"),
            "{}",
            out.to_llm_text()
        );

        // 未注册目标报错。
        let err = tool
            .execute(
                serde_json::json!({"op": "send", "to": "nobody", "message": "x"}),
                c,
            )
            .await;
        assert!(err.is_err());

        // main 的收件箱：task-1 反投。
        hub.send("main", "task-1", "收到").unwrap();
        let out = tool
            .execute(serde_json::json!({"op": "recv"}), c)
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("<task-1> 收到"),
            "{}",
            out.to_llm_text()
        );
        // 空箱。
        let out = tool
            .execute(serde_json::json!({"op": "recv"}), c)
            .await
            .unwrap();
        assert!(out.to_llm_text().contains("收件箱为空"));

        // drop 注销。
        drop(tool);
        assert_eq!(hub.peers(), vec!["task-1".to_string()]);
    }

    #[tokio::test]
    async fn missing_args_rejected() {
        let hub = agent_core::hub::Hub::new().shared();
        let tool = HubTool::register(hub, "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let err = tool
            .execute(serde_json::json!({"op": "send"}), &ctx(&ws, &cancel))
            .await;
        assert!(err.is_err());
        let err = tool
            .execute(serde_json::json!({"op": "unknown"}), &ctx(&ws, &cancel))
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn wait_returns_buffered_message_immediately() {
        let hub = agent_core::hub::Hub::new().shared();
        let tool = HubTool::register(Arc::clone(&hub), "main");
        hub.send("main", "task-1", "先到").unwrap();
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let out = tool
            .execute(
                serde_json::json!({"op": "wait", "timeout_ms": 1000}),
                &ctx(&ws, &cancel),
            )
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("<task-1> 先到"),
            "{}",
            out.to_llm_text()
        );
    }

    #[tokio::test]
    async fn wait_times_out_cleanly() {
        let hub = agent_core::hub::Hub::new().shared();
        let tool = HubTool::register(hub, "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        // 超时是正常结果（非错误），对齐 omp「No message within X」。
        let out = tool
            .execute(
                serde_json::json!({"op": "wait", "timeout_ms": 30}),
                &ctx(&ws, &cancel),
            )
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(t.contains("无消息"), "{t}");
    }

    #[tokio::test]
    async fn wait_from_filter_buffers_mismatched_messages() {
        let hub = agent_core::hub::Hub::new().shared();
        let tool = HubTool::register(Arc::clone(&hub), "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);

        // 不匹配的消息先到；匹配的消息稍后异步到达。
        hub.send("main", "other", "插队").unwrap();
        let h = Arc::clone(&hub);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            h.send("main", "task-1", "定向").unwrap();
        });
        let out = tool
            .execute(
                serde_json::json!({"op": "wait", "from": "task-1", "timeout_ms": 2000}),
                c,
            )
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("<task-1> 定向"),
            "{}",
            out.to_llm_text()
        );

        // 不匹配的消息被缓存而非丢弃：inbox 消费仍能看到。
        let out = tool
            .execute(serde_json::json!({"op": "inbox"}), c)
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("<other> 插队"),
            "{}",
            out.to_llm_text()
        );
    }

    #[tokio::test]
    async fn wait_zero_waits_until_message_arrives() {
        let hub = agent_core::hub::Hub::new().shared();
        let tool = HubTool::register(Arc::clone(&hub), "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let h = Arc::clone(&hub);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            h.send("main", "task-1", "迟到").unwrap();
        });
        // timeout_ms = 0：无限等待，消息到达即返回。
        let out = tool
            .execute(
                serde_json::json!({"op": "wait", "timeout_ms": 0}),
                &ctx(&ws, &cancel),
            )
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("<task-1> 迟到"),
            "{}",
            out.to_llm_text()
        );
    }

    #[tokio::test]
    async fn inbox_peek_does_not_consume() {
        let hub = agent_core::hub::Hub::new().shared();
        let tool = HubTool::register(Arc::clone(&hub), "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);
        hub.send("main", "a", "一").unwrap();
        hub.send("main", "b", "二").unwrap();

        // peek：列出且不消费（两次 peek 结果一致）。
        for _ in 0..2 {
            let out = tool
                .execute(serde_json::json!({"op": "inbox", "peek": true}), c)
                .await
                .unwrap();
            let t = out.to_llm_text();
            assert!(
                t.contains("未读消息（2 条") && t.contains("<a> 一") && t.contains("<b> 二"),
                "{t}"
            );
        }
        // 消费：drain 全部，随后为空。
        let out = tool
            .execute(serde_json::json!({"op": "inbox"}), c)
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("收件箱（2 条）"),
            "{}",
            out.to_llm_text()
        );
        let out = tool
            .execute(serde_json::json!({"op": "inbox"}), c)
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("收件箱为空"),
            "{}",
            out.to_llm_text()
        );
    }

    #[tokio::test]
    async fn jobs_reports_unknown_without_handle_and_snapshot_with_fake() {
        let hub = agent_core::hub::Hub::new().shared();
        let _keep = hub.register("task-1".into());
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);

        // 无句柄：peer 状态与子任务面如实标 unknown。
        let tool = HubTool::register(Arc::clone(&hub), "main");
        let out = tool
            .execute(serde_json::json!({"op": "jobs"}), c)
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(t.contains("task-1：状态 unknown"), "{t}");
        assert!(t.contains("监督句柄未接入"), "{t}");
        assert!(t.contains("在途子任务：unknown"), "{t}");
        drop(tool);

        // 假句柄：快照与在途计数如实呈现；未匹配 peer 仍标 unknown。
        let (sup, _cancels) = fake_sup(true);
        let tool = HubTool::register(Arc::clone(&hub), "main").with_supervision(sup);
        let out = tool
            .execute(serde_json::json!({"op": "jobs"}), c)
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(t.contains("main（自己）"), "{t}");
        assert!(t.contains("task-1：状态 unknown"), "{t}");
        assert!(
            t.contains("sub-1 BuildRel：running，轮次 3，进度 45%"),
            "{t}"
        );
        assert!(
            t.contains("sub-2 Docs：done") && t.contains("（已终结）"),
            "{t}"
        );
        assert!(t.contains("子任务（2 个，在途 1）"), "{t}");
    }

    #[tokio::test]
    async fn cancel_without_handle_reports_not_found_per_id() {
        let hub = agent_core::hub::Hub::new().shared();
        let tool = HubTool::register(Arc::clone(&hub), "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);
        // 句柄未接入且非后台作业：逐 id 如实上报未找到（拒绝而非静默 no-op）。
        let out = tool
            .execute(serde_json::json!({"op": "cancel", "ids": ["sub-1"]}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(out.contains("未找到"), "{out}");
        // ids 缺失：参数错误。
        let (sup, _) = fake_sup(true);
        let _tool = HubTool::register(Arc::clone(&hub), "main").with_supervision(sup);
    }

    #[tokio::test]
    async fn cancel_delegates_and_rejects_self() {
        let hub = agent_core::hub::Hub::new().shared();
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);

        // 假句柄：逐 id 委托；自己被拒绝且不触发句柄。
        let (sup, cancels) = fake_sup(true);
        let tool = HubTool::register(Arc::clone(&hub), "main").with_supervision(sup);
        let out = tool
            .execute(
                serde_json::json!({"op": "cancel", "ids": ["sub-1", "main", "ghost"]}),
                c,
            )
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(t.contains("- sub-1：已下达取消"), "{t}");
        assert!(t.contains("- main：拒绝（不能取消自己）"), "{t}");
        assert_eq!(cancels.load(Ordering::SeqCst), 1, "只有 sub-1 到达句柄");

        // 句柄报错 → 逐 id 呈现失败原因（非 no-op）。
        let (sup, cancels) = fake_sup(false);
        let tool = HubTool::register(hub, "main").with_supervision(sup);
        let out = tool
            .execute(serde_json::json!({"op": "cancel", "ids": ["sub-1"]}), c)
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(
            t.contains("- sub-1：失败（监督注册表仅观测，暂无取消通道）"),
            "{t}"
        );
        assert_eq!(cancels.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn wait_respects_cancellation() {
        let hub = agent_core::hub::Hub::new().shared();
        let tool = HubTool::register(hub, "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            c.cancel();
        });
        // timeout_ms=0（无限等待），但 steering 取消在 20ms 后触发 → 报错而非悬挂。
        let err = tool
            .execute(
                serde_json::json!({"op": "wait", "timeout_ms": 0}),
                &ctx(&ws, &cancel),
            )
            .await;
        assert!(err.is_err());
    }

    // ─────────────────── 名册 / 广播（H41 续 · H43）───────────────────

    #[tokio::test]
    async fn agents_reports_roster_status_and_backlog() {
        let hub = agent_core::hub::Hub::new().shared();
        // task-1：自定义标签、running、两条未读 → 积压 2。
        let _t1 = hub.register_handle(
            "task-1".into(),
            Some("Scout".into()),
            agent_core::hub::AgentStatus::Running,
        );
        // task-2：parked、无标签（标签缺省等于 id）。
        let _t2 = hub.register_handle("task-2".into(), None, agent_core::hub::AgentStatus::Parked);
        hub.send("task-1", "main", "一").unwrap();
        hub.send("task-1", "main", "二").unwrap();

        let tool = HubTool::register(Arc::clone(&hub), "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let out = tool
            .execute(serde_json::json!({"op": "agents"}), &ctx(&ws, &cancel))
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(t.contains("在册代理（3）："), "{t}");
        // 名册按 id 排序；自己带标记；状态/标签/积压逐项如实。
        assert!(
            t.contains("- main（自己） [running] 标签=main 积压=0"),
            "{t}"
        );
        assert!(t.contains("- task-1 [running] 标签=Scout 积压=2"), "{t}");
        assert!(t.contains("- task-2 [parked] 标签=task-2 积压=0"), "{t}");
        let (i1, i2) = (
            t.find("task-1").expect("task-1 应在名册"),
            t.find("task-2").expect("task-2 应在名册"),
        );
        assert!(i1 < i2, "名册应按 id 排序：{t}");
    }

    /// H27：`wait ids` 超时后**必须释放 watch**——否则 watch 期间完结的作业结果会在入队
    /// 时被丢弃而永久静默丢失。释放 = 解除 watch + 把已完结被抑制的结果重新入队。
    #[tokio::test]
    async fn wait_ids_timeout_releases_watch_and_requeues_result() {
        let hub = agent_core::hub::Hub::new().shared();
        let jobs = agent_core::jobs::AsyncJobManager::with_max_running(4);
        // 记录投递（sink 拿到的就是被重新入队的结果）。
        let delivered: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink: agent_core::jobs::DeliverySink = {
            let seen = Arc::clone(&delivered);
            Arc::new(move |job_id, _text, _job| {
                seen.lock().push(job_id.to_string());
                Box::pin(async { Ok(()) })
            })
        };
        // 守卫必须持有：`SinkGuard` drop 即注销（否则投递死信）。
        let _sink_guard = jobs.register_delivery_sink("main", sink);
        let tool =
            HubTool::register(Arc::clone(&hub), "main").with_jobs_option(Some(Arc::clone(&jobs)));
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);

        // 注册一个「永远运行」的作业（结果尚未产生），wait ids 会在超时后返回。
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let id = jobs
            .register(
                agent_core::jobs::AsyncJobType::Bash,
                "sleep forever",
                move |_ctx| {
                    Box::pin(async move {
                        let _ = rx.await; // 直到测试结束才释放
                        Ok("done".to_string())
                    })
                },
                agent_core::jobs::RegisterOptions {
                    owner_id: Some("main".into()),
                    ..Default::default()
                },
            )
            .expect("注册作业");
        jobs.mark_running(&id);
        let out = tool
            .execute(
                serde_json::json!({"op": "wait", "ids": [id.clone()], "timeout_ms": 30}),
                c,
            )
            .await
            .unwrap();
        let text = out.to_llm_text();
        assert!(
            text.contains("未全部完结") || text.contains("等待"),
            "{text}"
        );
        assert!(text.contains("watch 已释放"), "应提示 watch 已释放: {text}");
        // 释放后：作业不在抑制集合里（再完结可正常入队）。
        assert!(
            jobs.suppressed_job_ids(Some("main")).is_empty(),
            "超时返回后不得仍处于 watch 抑制状态"
        );
        // 作业现在完结 → 结果进入投递队列并被 sink 收到（不再静默丢失）。
        let _ = tx.send(());
        for _ in 0..50 {
            if !delivered.lock().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            delivered.lock().as_slice(),
            std::slice::from_ref(&id),
            "释放 watch 后完结的结果应被投递"
        );
        assert!(jobs.is_job_result_consumed(&id), "投递成功应标记已消费");
    }

    /// H43：`send tracked=true` → `acks` 计数 → `ack`/`unwatch` 两条处置路径。
    #[tokio::test]
    async fn tracked_send_ack_and_unwatch_ops() {
        let hub = agent_core::hub::Hub::new().shared();
        let mut sub = hub.register("task-1".into());
        let tool = HubTool::register(Arc::clone(&hub), "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);

        // 默认不签发回执。
        let out = tool
            .execute(
                serde_json::json!({"op": "send", "to": "task-1", "message": "普通"}),
                c,
            )
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("已投递"),
            "{}",
            out.to_llm_text()
        );
        assert_eq!(hub.pending_acks(), 0);
        let _ = sub.try_recv();

        // tracked=true：返回 ack_id，`acks` 能看到未兑现计数。
        let out = tool
            .execute(
                serde_json::json!({"op": "send", "to": "task-1", "message": "要回执", "tracked": true}),
                c,
            )
            .await
            .unwrap();
        let text = out.to_llm_text();
        assert!(text.contains("ack_id="), "{text}");
        let ack_id: u64 = text
            .split("ack_id=")
            .nth(1)
            .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|s| s.parse().ok())
            .expect("应带数字 ack_id");
        let out = tool
            .execute(serde_json::json!({"op": "acks"}), c)
            .await
            .unwrap();
        assert!(out.to_llm_text().contains("1 条"), "{}", out.to_llm_text());

        // `ack` 兑现；重复 ack 报错（不静默）。
        let out = tool
            .execute(serde_json::json!({"op": "ack", "ack_id": ack_id}), c)
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("已确认"),
            "{}",
            out.to_llm_text()
        );
        assert!(
            tool.execute(serde_json::json!({"op": "ack", "ack_id": ack_id}), c)
                .await
                .is_err()
        );

        // 第二条：用 `unwatch` 撤销（等待方会立即得到已撤销错误）。
        let out = tool
            .execute(
                serde_json::json!({"op": "send", "to": "task-1", "message": "撤销我", "tracked": true}),
                c,
            )
            .await
            .unwrap();
        let ack2: u64 = out
            .to_llm_text()
            .split("ack_id=")
            .nth(1)
            .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|s| s.parse().ok())
            .unwrap();
        let out = tool
            .execute(serde_json::json!({"op": "unwatch", "ack_id": ack2}), c)
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("已撤销"),
            "{}",
            out.to_llm_text()
        );
        assert_eq!(hub.pending_acks(), 0);
        assert!(
            tool.execute(serde_json::json!({"op": "unwatch", "ack_id": ack2}), c)
                .await
                .is_err(),
            "重复撤销应报错"
        );
        // 缺参数报错。
        assert!(
            tool.execute(serde_json::json!({"op": "ack"}), c)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn broadcast_skips_sender_and_dead_inboxes() {
        let hub = agent_core::hub::Hub::new().shared();
        let mut t1 =
            hub.register_handle("task-1".into(), None, agent_core::hub::AgentStatus::Running);
        let t2 = hub.register_handle("task-2".into(), None, agent_core::hub::AgentStatus::Idle);
        drop(t2); // 收件箱已关闭：广播尽力而为，跳过而非报错。

        let tool = HubTool::register(Arc::clone(&hub), "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);

        // 缺 message → 参数错误（不静默广播空消息）。
        let err = tool
            .execute(serde_json::json!({"op": "broadcast"}), c)
            .await;
        assert!(err.is_err());

        let out = tool
            .execute(
                serde_json::json!({"op": "broadcast", "message": "全体注意"}),
                c,
            )
            .await
            .unwrap();
        assert_eq!(out.to_llm_text(), "已广播给 1 个代理");

        // 存活代理收到；发送者自己不在收件人内。
        let got = t1.try_recv().expect("task-1 应收到广播");
        assert_eq!(got.from, "main");
        assert_eq!(got.body, "全体注意");
        let out = tool
            .execute(serde_json::json!({"op": "inbox"}), c)
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("收件箱为空"),
            "{}",
            out.to_llm_text()
        );
        assert_eq!(t1.inbox_len(), 0, "消费后积压应归零");
    }

    // ─────────────────────── 进程托管（launch 面）───────────────────────

    /// 构造接好 ProcessManager 的工具。
    fn proc_tool() -> HubTool {
        let hub = agent_core::hub::Hub::new().shared();
        HubTool::register(hub, "main")
            .with_processes(Arc::new(agent_supervisor::ProcessManager::new()))
    }

    #[tokio::test]
    async fn process_ops_rejected_without_manager() {
        let hub = agent_core::hub::Hub::new().shared();
        let tool = HubTool::register(hub, "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);
        for op in ["start", "ps", "logs", "stop", "restart", "describe"] {
            let err = tool
                .execute(serde_json::json!({"op": op, "name": "x"}), c)
                .await
                .err()
                .unwrap_or_else(|| panic!("{op} 应报错"));
            assert!(err.to_string().contains("进程托管未接入"), "{op}: {err}");
        }
        // name 分流的 send/wait 同样明确报错。
        let err = tool
            .execute(
                serde_json::json!({"op": "send", "name": "x", "text": "y"}),
                c,
            )
            .await;
        assert!(err.is_err());
        let err = tool
            .execute(serde_json::json!({"op": "wait", "name": "x"}), c)
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn send_name_and_to_are_mutually_exclusive() {
        let hub = agent_core::hub::Hub::new().shared();
        let tool = HubTool::register(hub, "main")
            .with_processes(Arc::new(agent_supervisor::ProcessManager::new()));
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);
        let err = tool
            .execute(
                serde_json::json!({"op": "send", "name": "p", "to": "q", "text": "x"}),
                c,
            )
            .await;
        assert!(err.is_err(), "name 与 to 互斥");
        let err = tool
            .execute(
                serde_json::json!({"op": "wait", "name": "p", "from": "q"}),
                c,
            )
            .await;
        assert!(err.is_err(), "name 与 from 互斥");
    }

    #[cfg(unix)]
    fn sh_start_args(script: &str) -> serde_json::Value {
        serde_json::json!({
            "op": "start",
            "name": "hubsrv",
            "application": "/bin/sh",
            "args": ["-c", script]
        })
    }

    /// 轮询直到输出中包含目标片段（避免 flaky sleep 断言）。
    async fn wait_output_contains(tool: &HubTool, c: &ToolContext<'_>, needle: &str) -> String {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let out = tool
                .execute(
                    serde_json::json!({"op": "logs", "name": "hubsrv", "lines": 100}),
                    c,
                )
                .await
                .expect("logs");
            let t = out.to_llm_text();
            if t.contains(needle) {
                return t;
            }
            assert!(tokio::time::Instant::now() < deadline, "等待日志超时：{t}");
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_ps_describe_stop_roundtrip() {
        let tool = proc_tool();
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);

        // start：缺 application 报参数错误。
        let err = tool
            .execute(serde_json::json!({"op": "start", "name": "hubsrv"}), c)
            .await;
        assert!(err.is_err());

        // start 常驻进程 → ps 可见 → describe 出 JSON。
        let out = tool
            .execute(sh_start_args("echo up; sleep 30"), c)
            .await
            .expect("start");
        let t = out.to_llm_text();
        assert!(t.contains("已启动 hubsrv"), "{t}");
        let out = tool
            .execute(serde_json::json!({"op": "ps"}), c)
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(t.contains("- hubsrv：running"), "{t}");
        let out = tool
            .execute(serde_json::json!({"op": "describe", "name": "hubsrv"}), c)
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(
            t.contains("\"application\": \"/bin/sh\"") && t.contains("\"name\": \"hubsrv\""),
            "{t}"
        );

        // stop → 状态收敛 stopped。
        let out = tool
            .execute(
                serde_json::json!({"op": "stop", "name": "hubsrv", "timeout": 5}),
                c,
            )
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(t.contains("最终状态：stopped"), "{t}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_name_writes_stdin_and_wait_name_exits() {
        let tool = proc_tool();
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);

        // 常驻进程：读一行回显后退出。
        tool.execute(sh_start_args("read line; echo got:$line"), c)
            .await
            .expect("start");

        // send（name 分流）：写 stdin。
        let out = tool
            .execute(
                serde_json::json!({"op": "send", "name": "hubsrv", "text": "hello"}),
                c,
            )
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("已写入 hubsrv stdin"),
            "{}",
            out.to_llm_text()
        );
        let t = wait_output_contains(&tool, c, "got:hello").await;
        assert!(t.contains("[1][stdout] got:hello"), "{t}");

        // wait（name 分流）：for=exit 直到进程退出。
        let out = tool
            .execute(
                serde_json::json!({"op": "wait", "name": "hubsrv", "for": "exit", "timeout": 5}),
                c,
            )
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(t.contains("exited code=0"), "{t}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn logs_cursor_and_grep_through_hub() {
        let tool = proc_tool();
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);
        tool.execute(sh_start_args("printf 'a\\nb\\nc\\n'"), c)
            .await
            .expect("start");
        // 轮询至 3 行齐（进程退出后日志仍可读）。
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let (t, cursor) = loop {
            let out = tool
                .execute(
                    serde_json::json!({"op": "logs", "name": "hubsrv", "head": true, "lines": 100}),
                    c,
                )
                .await
                .unwrap();
            let t = out.to_llm_text();
            if t.contains("[3][stdout] c") {
                let cursor: u64 = t
                    .split("cursor=")
                    .nth(1)
                    .and_then(|s| s.split('（').next())
                    .and_then(|s| s.parse().ok())
                    .expect("cursor 数字");
                break (t, cursor);
            }
            assert!(tokio::time::Instant::now() < deadline, "{t}");
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        };
        assert!(
            t.contains("[1][stdout] a") && t.contains("[2][stdout] b"),
            "{t}"
        );

        // 游标续读：cursor=2 → 只剩 c。
        let out = tool
            .execute(
                serde_json::json!({"op": "logs", "name": "hubsrv", "cursor": 2, "lines": 100}),
                c,
            )
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(
            t.contains("[3][stdout] c") && !t.contains("[1][stdout] a"),
            "{t}"
        );

        // grep 过滤。
        let out = tool
            .execute(
                serde_json::json!({"op": "logs", "name": "hubsrv", "grep": "^b", "head": true}),
                c,
            )
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(
            t.contains("[2][stdout] b") && !t.contains("[1][stdout] a"),
            "{t}"
        );
        let _ = cursor;
    }

    // ── 异步后台作业面（jobs/cancel/wait ids + watch 抑制投递）───────────────

    type SlowJobRun = Box<
        dyn FnOnce(
                agent_core::jobs::JobRunContext,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<String, String>> + Send>,
            > + Send,
    >;

    fn slow_job() -> (
        SlowJobRun,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
    ) {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let s = Arc::clone(&started);
        let r = Arc::clone(&release);
        let f = move |cx: agent_core::jobs::JobRunContext| {
            let s = Arc::clone(&s);
            let r = Arc::clone(&r);
            Box::pin(async move {
                s.notify_one();
                tokio::select! {
                    _ = r.notified() => {}
                    _ = cx.cancel.cancelled() => {}
                }
                Ok("bg-out".to_string())
            })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Result<String, String>> + Send>,
                >
        };
        (Box::new(f) as SlowJobRun, started, release)
    }

    #[tokio::test]
    async fn jobs_op_lists_and_cancel_routes_bg_jobs() {
        let hub = agent_core::hub::Hub::new().shared();
        let jobs_mgr = agent_core::jobs::AsyncJobManager::build(4, 60_000, None);
        let tool = HubTool::register(Arc::clone(&hub), "main").with_jobs(Arc::clone(&jobs_mgr));
        let (run, started, release) = slow_job();
        let id = jobs_mgr
            .register(
                agent_core::jobs::AsyncJobType::Bash,
                "sleep 100",
                run,
                agent_core::jobs::RegisterOptions {
                    owner_id: Some("main".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        started.notified().await;
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);
        // jobs 列出后台作业行。
        let out = tool
            .execute(serde_json::json!({"op": "jobs"}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(out.contains(&id), "jobs should list bg row: {out}");
        assert!(out.contains("sleep 100"));
        // cancel 路由到管理器（无需监督句柄）。
        let out = tool
            .execute(serde_json::json!({"op": "cancel", "ids": [id]}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(out.contains("已下达取消"), "{out}");
        release.notify_one();
        assert_eq!(
            jobs_mgr.get_job(&id).unwrap().status,
            agent_core::jobs::JobStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn wait_ids_returns_result_once_and_suppresses_delivery() {
        use std::sync::Mutex as StdMutex;
        let hub = agent_core::hub::Hub::new().shared();
        let log: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let log2 = Arc::clone(&log);
        let sink: agent_core::jobs::DeliverySink = Arc::new(move |job_id, text, _j| {
            let log = Arc::clone(&log2);
            Box::pin(async move {
                log.lock().unwrap().push((job_id, text));
                Ok(())
            })
        });
        let jobs_mgr = agent_core::jobs::AsyncJobManager::build(4, 60_000, Some(sink));
        let tool = HubTool::register(Arc::clone(&hub), "main").with_jobs(Arc::clone(&jobs_mgr));
        let id = jobs_mgr
            .register(
                agent_core::jobs::AsyncJobType::Bash,
                "echo hi",
                |_cx| Box::pin(async { Ok("hi-out".to_string()) }),
                agent_core::jobs::RegisterOptions {
                    owner_id: Some("main".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);
        // wait ids：作业完结即返回结果（期限 2s 足够单线程运行时轮转）。
        let out = tool
            .execute(
                serde_json::json!({"op": "wait", "ids": [id], "timeout_ms": 2000}),
                c,
            )
            .await
            .unwrap()
            .to_llm_text();
        assert!(out.contains("hi-out"), "wait returns result: {out}");
        // watch + consume：自动投递被抑制。
        assert!(
            log.lock().unwrap().is_empty(),
            "delivery suppressed by watch"
        );
        // 再次 wait：无新事件（已消费）。
        let out2 = tool
            .execute(
                serde_json::json!({"op": "wait", "ids": [id], "timeout_ms": 200}),
                c,
            )
            .await
            .unwrap()
            .to_llm_text();
        assert!(out2.contains("已完结且结果已被消费"), "{out2}");
    }
}
