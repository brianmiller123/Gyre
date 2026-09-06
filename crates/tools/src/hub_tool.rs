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
    inbox: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<HubMessage>>,
    // peek 语义需要「通道 → 待取队列」单向搬运：mpsc 通道只能取不能窥，
    // pump 进待取队列后 peek 只读队列、消费路径才出队。
    pending: tokio::sync::Mutex<VecDeque<HubMessage>>,
    // 监督句柄（可选）：jobs 快照与 cancel 委托；未接入时 jobs 如实标 unknown、
    // cancel 明确报错（拒绝而非静默 no-op）。
    supervision: Option<Arc<dyn HubSupervision>>,
    // 进程托管（可选）：start/ps/logs/stop/restart/describe 与 name 分流的
    // send/wait 委托 ProcessManager；未接入时明确报错（拒绝而非静默 no-op）。
    processes: Option<Arc<ProcessManager>>,
}

impl HubTool {
    /// 构造并注册当前代理（收件箱随工具持有；监督句柄与进程托管未接入）。
    #[must_use]
    pub fn register(hub: Arc<agent_core::hub::Hub>, id: impl Into<String>) -> Self {
        let identity = HubIdentity::new(id);
        let inbox = hub.register(identity.id.clone());
        Self {
            hub,
            identity,
            inbox: tokio::sync::Mutex::new(inbox),
            pending: tokio::sync::Mutex::new(VecDeque::new()),
            supervision: None,
            processes: None,
        }
    }

    /// 从既有构造（测试用：已注册的收件箱）。
    #[must_use]
    pub fn from_parts(
        hub: Arc<agent_core::hub::Hub>,
        identity: HubIdentity,
        inbox: tokio::sync::mpsc::UnboundedReceiver<HubMessage>,
    ) -> Self {
        Self {
            hub,
            identity,
            inbox: tokio::sync::Mutex::new(inbox),
            pending: tokio::sync::Mutex::new(VecDeque::new()),
            supervision: None,
            processes: None,
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

    /// 把通道中当前可读的消息全部搬入待取队列（锁序恒为 inbox → pending）。
    async fn pump(&self) {
        let mut rx = self.inbox.lock().await;
        let mut pending = self.pending.lock().await;
        while let Ok(m) = rx.try_recv() {
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
        pending.remove(idx)
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
收件箱 / list 在册代理 / wait 阻塞等下一条消息 / inbox 列队中消息，peek 不消费）、\
子任务观测（jobs 快照 / cancel 取消，需装配层接入监督句柄）、长驻进程托管（start / \
ps / logs / stop / restart / describe；send / wait 入参含 name 时作用于进程，与 to / \
from 互斥）。进程托管为进程内最小实现，有意偏差：无 broker 子进程、无磁盘持久化、\
不做 detached/persist 生命周期、无 PTY（stdin/stdout 为管道）；进程退出不主动推送\
消息，用 wait（name + for=exit）或 logs（follow=true）轮询。消息与进程记录不跨会话\
保留；监督句柄/进程托管未接入时相应 op 明确报错而非静默忽略。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "op": { "type": "string",
                        "enum": ["send", "recv", "list", "wait", "inbox", "jobs", "cancel",
                                 "start", "ps", "logs", "stop", "restart", "describe"],
                        "default": "list",
                        "description": "send 投递 / recv 拉取收件箱（drain）/ list 在册代理 / wait 阻塞等下一条消息 / inbox 列队中消息 / jobs 子任务快照 / cancel 取消在途子任务；进程托管：start 拉起 / ps 快照 / logs 读日志 / stop 停止 / restart 重启 / describe 规格+状态" },
                "to": { "type": "string", "description": "send：目标代理 id（list 可查）" },
                "message": { "type": "string", "description": "send：消息体（纯文本）" },
                "from": { "type": "string", "description": "wait：只接受来自该代理 id 的消息（其余消息缓存不丢）" },
                "timeout_ms": { "type": "integer", "minimum": 0,
                                "description": "wait：超时毫秒（默认 120000；0=无限等待）" },
                "peek": { "type": "boolean", "description": "inbox：true 时仅列出队中消息，不消费" },
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
        let sup = self.supervision.as_ref().ok_or_else(|| {
            ToolError::Execution("cancel 需要监督句柄（装配层未接入），拒绝执行而非静默忽略".into())
        })?;
        // omp 语义（hub/jobs.ts `cancelAgentRegistration`）：先查注册表再动手——
        // 不在册的 id 按 not_found 上报，不透传句柄（对句柄的无谓调用即谎报状态）。
        let known: HashSet<String> = sup.jobs().await.into_iter().map(|j| j.id).collect();
        let mut lines = vec!["取消请求处理结果：".to_string()];
        for id in ids {
            if id == self.identity.id {
                lines.push(format!("- {id}：拒绝（不能取消自己）"));
                continue;
            }
            if !known.contains(&id) {
                lines.push(format!("- {id}：未找到（非本代理派生的子任务或不存在）"));
                continue;
            }
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
    async fn cancel_requires_supervision_handle() {
        let hub = agent_core::hub::Hub::new().shared();
        let tool = HubTool::register(Arc::clone(&hub), "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);
        // 句柄未接入：明确报错而非 no-op。
        let err = tool
            .execute(serde_json::json!({"op": "cancel", "ids": ["sub-1"]}), c)
            .await;
        assert!(err.is_err());
        // ids 缺失：参数错误。
        let (sup, _) = fake_sup(true);
        let tool = HubTool::register(Arc::clone(&hub), "main").with_supervision(sup);
        let err = tool.execute(serde_json::json!({"op": "cancel"}), c).await;
        assert!(err.is_err());
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
}
