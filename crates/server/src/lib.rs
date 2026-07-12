//! # agent-server
//!
//! HTTP/WebSocket 服务：[`SessionManager`] + 带宽优化线协议 + [`WebApprovalPolicy`] +
//! 静态前端托管。体现「Agent 循环对传输透明」——循环产出 [`AgentEvent`](agent_core::AgentEvent)，
//! 经驱动任务转为 [`ServerFrame`] 广播给 WebSocket 订阅者；审批经 [`ClientFrame::Respond`] 回执。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

use std::collections::HashMap;
use std::sync::Arc;

use agent::{Agent, AgentBuilder};
use agent_supervisor::{SubAgentStatus, Supervisor};
use agent_config::{discover_commands, Config, ModelProfile, RulesEngine};
use agent_core::{
    AgentEvent, AgentState, ApprovalDecision, ApprovalPolicy, ApprovalRequest, AskMessage,
    AskResponse, CompactionStrategy, ContextManager, LlmProvider, Mode, ProviderCallContext,
    SkillLevel, ToolError, Usage, UserContent, UserMessage, Workspace,
};
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Json, Response},
    routing::{delete, get},
};
use agent_tools::Tool;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

const APPROVAL_TIMEOUT_SECS: u64 = 300;
/// 单进程最大并发会话数（软上限，缓解无界创建导致的内存/任务泄漏）。
const MAX_SESSIONS: usize = 64;

// ──────────────────────────────────────────────────────────────────────────────
// 线协议（带宽优化：增量优先，不回传 partial 全量快照）
// ──────────────────────────────────────────────────────────────────────────────

/// 用户内容块（前端多模态输入；镜像 [`UserContent`] 但用前端友好的字段名）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentInput {
    /// 文本块。
    Text {
        /// 文本内容。
        text: String,
    },
    /// 图像块（base64）。
    Image {
        /// MIME 类型（image/png|jpeg|gif|webp）。
        mime: String,
        /// base64 数据（不含 `data:` 前缀）。
        data: String,
    },
}

impl ContentInput {
    /// 转为内部 [`UserContent`]。
    #[must_use]
    fn to_user_content(&self) -> UserContent {
        match self {
            Self::Text { text } => UserContent::Text { text: text.clone() },
            Self::Image { mime, data } => UserContent::Image {
                mime: mime.clone(),
                data: data.clone(),
            },
        }
    }
}

/// 客户端 → 服务端
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// 新建任务。
    NewTask {
        /// 任务文本。
        text: String,
        /// 模式覆盖（保留字段；实际模式在会话创建时确定）。
        mode: Option<Mode>,
        /// 可选的多模态内容块（图片等）。存在时与 `text` 合并为一条 [`UserMessage`]。
        #[serde(default)]
        content: Option<Vec<ContentInput>>,
    },
    /// 审批/回答回执。
    Respond {
        /// 对应 AskMessage.id。
        ask_id: String,
        /// 响应。
        response: AskResponse,
    },
    /// 取消当前任务。
    Cancel,
    /// 手动压缩上下文（shake + summarize + prune，与 CLI `/compact` 一致）。
    Compact,
}

/// 服务端 → 客户端
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// 状态机变更。
    StateChanged {
        /// 新状态。
        state: AgentState,
    },
    /// 文本增量。
    TextDelta { delta: String },
    /// 思考增量（reasoning / thinking）。
    ThinkingDelta { delta: String },
    /// 信息性输出。
    Say { text: String },
    /// 需审批/回答（对应 ClientFrame::Respond）。
    Ask { ask: AskMessage },
    /// 工具执行进度。
    ToolExec { name: String, output: String },
    /// 用量。
    Usage(Usage),
    /// 完成。
    Done {
        turns: u64,
        tool_calls: u64,
        success: bool,
    },
    /// 错误。
    Error { message: String },
    /// 子 Agent 监控快照（聚合后下发，前端整体替换）。
    SubAgents {
        /// 全部子 Agent 状态（含日志尾部）。
        agents: Vec<SubAgentStatus>,
    },
    /// 上下文窗口 token 占比（current / limit），供前端展示压缩进度。
    ContextUsage {
        /// 当前上下文估算 token 数。
        current: usize,
        /// 模型上限。
        limit: usize,
    },
}

fn to_server_frame(ev: AgentEvent) -> ServerFrame {
    match ev {
        AgentEvent::StateChanged(s) => ServerFrame::StateChanged { state: s },
        AgentEvent::TextDelta(d) => ServerFrame::TextDelta { delta: d },
        AgentEvent::ThinkingDelta(d) => ServerFrame::ThinkingDelta { delta: d },
        AgentEvent::Say(s) => ServerFrame::Say { text: s.text },
        AgentEvent::Ask(a) => ServerFrame::Ask { ask: a },
        AgentEvent::ToolExec { name, output } => ServerFrame::ToolExec { name, output },
        AgentEvent::Usage(u) => ServerFrame::Usage(u),
        AgentEvent::Done(s) => ServerFrame::Done {
            turns: s.turns,
            tool_calls: s.tool_calls,
            success: s.success,
        },
        AgentEvent::Error(m) => ServerFrame::Error { message: m },
        AgentEvent::Assistant(_) => ServerFrame::TextDelta {
            delta: String::new(),
        },
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Web 审批策略：decide 走规则引擎，prompt 经 WS 推送并等待 Respond 回执
// ──────────────────────────────────────────────────────────────────────────────

type PendingMap = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<AskResponse>>>>;

/// Web 版 ApprovalPolicy：把 Ask 转为 [`ServerFrame::Ask`] 推给浏览器，挂起等待 [`ClientFrame::Respond`]。
pub struct WebApprovalPolicy {
    rules: RulesEngine,
    tx: broadcast::Sender<ServerFrame>,
    pending: PendingMap,
}

impl WebApprovalPolicy {
    #[must_use]
    pub fn new(
        rules: RulesEngine,
        tx: broadcast::Sender<ServerFrame>,
        pending: PendingMap,
    ) -> Self {
        Self { rules, tx, pending }
    }
}

#[async_trait::async_trait]
impl ApprovalPolicy for WebApprovalPolicy {
    fn decide(&self, request: &ApprovalRequest<'_>) -> ApprovalDecision {
        self.rules.decide(request)
    }

    async fn prompt(&self, ask: &AskMessage) -> Result<AskResponse, ToolError> {
        let id = ask.id.clone();
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.pending.lock().await.insert(id.clone(), resp_tx);
        let _ = self.tx.send(ServerFrame::Ask { ask: ask.clone() });

        match tokio::time::timeout(
            std::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS),
            resp_rx,
        )
        .await
        {
            Ok(Ok(response)) => Ok(response),
            _ => {
                self.pending.lock().await.remove(&id);
                Err(ToolError::Execution("审批超时或被丢弃".into()))
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 会话与驱动
// ──────────────────────────────────────────────────────────────────────────────

/// 一个会话：Agent + 双向信道 + 待审批表 + 子 Agent 监控总线。
pub struct Session {
    /// 客户端帧入口。
    pub inbound: mpsc::UnboundedSender<ClientFrame>,
    /// 服务端帧广播。
    pub broadcast: broadcast::Sender<ServerFrame>,
    /// 待审批回执表（驱动任务用以解析 Respond）。
    pub pending: PendingMap,
    /// 子 Agent 监控总线（TaskTool 与转发器共享同一份状态）。
    pub supervisor: Supervisor,
    /// 驱动任务句柄（[`Session::shutdown`] 时 abort，防长时运行会话任务泄漏）。
    pub driver_handle: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// 子 Agent 监控转发任务句柄（[`Session::shutdown`] 时 abort）。
    pub forwarder_handle: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Skill 目录（只读快照，`/api/sessions/{id}/skills` 用）。
    pub skills: Arc<agent_skills::SkillCatalog>,
    /// MCP 工具注册表（只读快照，`/api/sessions/{id}/mcp` 用）。
    pub mcp: Arc<agent_mcp::McpRegistry>,
    /// 是否有任务正在运行（驱动任务维护真值）。WS 重连时据此回推一帧
    /// [`AgentState::Running`]，避免重连前端误判为空闲而发起新任务。
    pub running: Arc<std::sync::atomic::AtomicBool>,
}

impl Session {
    /// 中止驱动与监控转发任务，释放长时运行会话的后台资源（幂等）。
    pub async fn shutdown(&self) {
        if let Some(h) = self.driver_handle.lock().await.take() {
            h.abort();
        }
        if let Some(h) = self.forwarder_handle.lock().await.take() {
            h.abort();
        }
    }
}

/// 会话管理器（多并发会话）。
#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    config: Arc<Config>,
    http: reqwest::Client,
    cwd: Arc<std::path::PathBuf>,
    /// 协同中继（端到端加密）：按不透明 room_id 广播密封字节，永不接触明文/密钥。
    relay: agent_collab::Relay,
    /// 协同中继维护循环是否已启动（惰性，首次创建会话时拉起）。
    maintenance_started: Arc<std::sync::atomic::AtomicBool>,
}

impl SessionManager {
    /// 构造。
    #[must_use]
    pub fn new(config: Arc<Config>, http: reqwest::Client, cwd: Arc<std::path::PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            config,
            http,
            cwd,
            relay: agent_collab::Relay::new(),
            maintenance_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// 协同中继句柄（供路由直接 join/publish）。
    #[must_use]
    pub fn relay(&self) -> &agent_collab::Relay {
        &self.relay
    }

    /// 工作区根目录（agent 打开的目录）。
    #[must_use]
    pub fn cwd(&self) -> &std::path::Path {
        self.cwd.as_path()
    }

    /// 创建/恢复/fork 会话。
    ///
    /// - `model`：模型别名（对应 `[[models]] alias`）；`None` 用默认模型。
    /// - `resume`：`Some(id)` 则恢复历史会话（复用 `<cwd>/.agent/sessions/<id>.jsonl`）。
    /// - `fork`：`Some(id)` 则把源会话复制为新 id 后继续（resume 与 fork 互斥，resume 优先）。
    /// - `mode_override`：`Some(code|architect|ask|debug)` 覆盖配置默认模式（修复此前 Web
    ///   模式切换不生效——模式仅在会话创建时确定，故切换模式即以新会话 resume 旧 id）。
    ///
    /// 会话与 CLI 共享同一目录，互可见。
    pub async fn create_session(
        &self,
        model: Option<&str>,
        resume: Option<&str>,
        fork: Option<&str>,
        mode_override: Option<&str>,
    ) -> Result<String, String> {
        // 解析会话 id：resume（复用）> fork（复制为新 id）> 新建。
        let store = agent_context::SessionStore::for_cwd(&self.cwd);
        let id = if let Some(r) = resume.filter(|r| is_safe_session_id(r)) {
            r.to_string()
        } else if let Some(src) = fork.filter(|f| is_safe_session_id(f)) {
            store.fork(src).map_err(|e| e.to_string())?
        } else {
            agent_context::SessionStore::new_id()
        };

        // 重用已活跃会话：纯 resume（无 mode 覆盖）且目标 id 已是内存中活跃会话时，
        // 直接返回该会话 id——跳过「重建→覆盖」，否则会杀掉该会话正在运行的任务
        // （前端切换会话后原循环停止的核心修复）。mode 覆盖（switchMode 需应用新
        // system prompt）仍走重建路径。
        if resume.is_some() && mode_override.is_none()
            && self.inner.lock().await.contains_key(&id)
        {
            return Ok(id);
        }

        // 软上限检查前移：在装配 Agent / spawn 任务之前拒绝，避免超限时白白启动后台任务。
        {
            let inner = self.inner.lock().await;
            if inner.len() >= MAX_SESSIONS {
                return Err(format!("活跃会话数已达上限 {MAX_SESSIONS}"));
            }
        }

        // 惰性启动协同中继维护循环：周期清理无订阅者的房间，防止长时运行内存泄漏。
        if !self
            .maintenance_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let relay = self.relay.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                interval.tick().await; // 丢弃立即触发的首拍
                loop {
                    interval.tick().await;
                    let _ = relay.cleanup_empty().await;
                }
            });
        }

        let (broadcast_tx, _) = broadcast::channel::<ServerFrame>(1024);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<ClientFrame>();

        let supervisor = Supervisor::new();
        // Skill 目录 + MCP 注册表：每个会话加载一次，供 build_agent 与只读端点（/skills /mcp）共享。
        let skill_opts = self.config.skills.to_load_options();
        let skill_catalog: Arc<agent_skills::SkillCatalog> = if skill_opts.enabled {
            match agent_skills::SkillRegistry::native(self.cwd.as_path().to_path_buf())
                .load(&skill_opts)
                .await
            {
                Ok(cat) => Arc::new(cat),
                Err(e) => {
                    tracing::warn!(error = %e, "skill 加载失败，已降级为空");
                    Arc::new(agent_skills::SkillCatalog::default())
                }
            }
        } else {
            Arc::new(agent_skills::SkillCatalog::default())
        };
        let mcp: Arc<agent_mcp::McpRegistry> =
            Arc::new(agent_mcp::McpRegistry::load(&self.config.mcp).await);
        let (agent, context) = build_agent(
            &self.config,
            self.http.clone(),
            &self.cwd,
            &pending,
            &broadcast_tx,
            &supervisor,
            &id,
            model,
            mode_override,
            Arc::clone(&mcp),
            Arc::clone(&skill_catalog),
        )
        .await?;

        // 运行标志与会话共享同一份 Arc：驱动任务维护其真值，WS 重连时据此回推状态。
        let running = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let deps = DriverDeps {
            agent: Arc::new(agent),
            tx: broadcast_tx.clone(),
            pending: Arc::clone(&pending),
            running: Arc::clone(&running),
            current_cancel: Arc::new(tokio::sync::Mutex::new(None)),
            context: Arc::clone(&context),
        };
        let driver_handle = tokio::spawn(driver(deps, inbound_rx));
        // 子 Agent 监控：聚合事件为 ServerFrame::SubAgents（≈8fps）下发 WS 订阅者。
        let forwarder_handle = tokio::spawn(supervisor_forwarder(
            supervisor.clone(),
            broadcast_tx.clone(),
        ));

        let session = Arc::new(Session {
            inbound: inbound_tx,
            broadcast: broadcast_tx,
            pending,
            supervisor,
            driver_handle: Arc::new(tokio::sync::Mutex::new(Some(driver_handle))),
            forwarder_handle: Arc::new(tokio::sync::Mutex::new(Some(forwarder_handle))),
            skills: skill_catalog,
            mcp,
            running,
        });
        let mut inner = self.inner.lock().await;
        inner.insert(id.clone(), session);
        Ok(id)
    }

    /// 取会话。
    pub async fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.inner.lock().await.get(id).cloned()
    }

    /// 关闭并移除会话：中止驱动与监控转发任务、释放后台资源（幂等）。
    pub async fn close_session(&self, id: &str) -> bool {
        if let Some(session) = self.inner.lock().await.remove(id) {
            session.shutdown().await;
            true
        } else {
            false
        }
    }

    /// 模型 profile 列表（前端选择）。
    pub fn models(&self) -> Vec<ModelProfileView> {
        let mut out = vec![profile_view("default", &self.config.default_model)];
        for m in &self.config.models {
            out.push(profile_view(m.alias.as_deref().unwrap_or(&m.id), m));
        }
        out
    }

    /// 期望的鉴权 token（从 config.server.auth_token，已 ${ENV} 展开）。
    /// 返回 None 表示无需鉴权。
    pub fn expected_token(&self) -> Option<String> {
        self.config
            .server
            .auth_token
            .as_ref()
            .map(|t| agent_config::expand_env(t))
    }

    /// 当前活跃会话数（可观测端点用）。
    pub async fn active_session_count(&self) -> usize {
        self.inner.lock().await.len()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelProfileView {
    alias: String,
    id: String,
    api: String,
}

fn profile_view(alias: &str, p: &ModelProfile) -> ModelProfileView {
    ModelProfileView {
        alias: alias.to_string(),
        id: p.id.clone(),
        api: p.api.as_str().to_string(),
    }
}

#[derive(Clone)]
struct DriverDeps {
    agent: Arc<Agent>,
    tx: broadcast::Sender<ServerFrame>,
    pending: PendingMap,
    running: Arc<std::sync::atomic::AtomicBool>,
    /// 当前运行任务的取消句柄（None 表示无任务运行）。
    current_cancel: Arc<tokio::sync::Mutex<Option<CancellationToken>>>,
    /// 上下文管理器（手动压缩 `/compact` + 任务后 ContextUsage 下发）。
    context: Arc<dyn ContextManager>,
}

/// RAII 守卫：确保 spawn 任务即使 panic（或被 abort）也能复位 `running`，防止会话永久楔死。
///
/// 仅复位 `running`（`AtomicBool`，Drop 无需 await）；`current_cancel` 的清理仍由正常
/// 完成路径的尾部代码负责——即便 panic 后残留旧 token，下一次 `NewTask`/`Compact` 会覆盖它。
struct CancelGuard {
    running: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.running.store(false, Ordering::SeqCst);
    }
}

/// 会话驱动：消费 ClientFrame，驱动 Agent 事件流，解析审批回执。
async fn driver(deps: DriverDeps, mut inbound: mpsc::UnboundedReceiver<ClientFrame>) {
    use std::sync::atomic::Ordering;
    // 初始上下文占比：客户端连接后立即获知基线（前端 Inspector 进度条）。
    {
        let u = deps.context.token_usage();
        if let Err(e) = deps.tx.send(ServerFrame::ContextUsage {
            current: u.current,
            limit: u.limit,
        }) {
            tracing::warn!(?e, "广播初始 ContextUsage 失败");
        }
    }
    while let Some(frame) = inbound.recv().await {
        match frame {
            ClientFrame::NewTask { text, content, .. } => {
                if deps.running.swap(true, Ordering::SeqCst) {
                    if let Err(e) = deps.tx.send(ServerFrame::Error {
                        message: "已有任务运行中".into(),
                    }) {
                        tracing::warn!(?e, "广播错误帧失败");
                    }
                    continue;
                }
                // 为本任务创建独立取消句柄
                let task_cancel = CancellationToken::new();
                {
                    let mut guard = deps.current_cancel.lock().await;
                    *guard = Some(task_cancel.clone());
                }
                let d = deps.clone();
                let run_cancel = task_cancel.clone();
                let select_cancel = task_cancel.clone();
                tokio::spawn(async move {
                    // RAII 守卫：即使流处理 panic（或任务被 abort），Drop 时也会复位 running，
                    // 防止会话因 running 永真而永久楔死。
                    let _guard = CancelGuard {
                        running: d.running.clone(),
                    };
                    // 用独立取消作用域运行：该 token 同时驱动 Agent 流式中断与工具 ctx.cancel，
                    // 使「用户取消」端到端可达（修复此前工具层 cancel 分支在 server 模式永不命中）。
                    // 多模态：content 非空时把 text（若有）+ 内容块合并为一条 UserMessage。
                    // 多模态/文本两条路径返回不同的 `impl Stream` 具体类型，
                    // 装箱为统一的 trait 对象后由 select! 消费。
                    let mut stream: std::pin::Pin<
                        Box<dyn futures::Stream<Item = AgentEvent> + Send + '_>,
                    > = match content {
                        Some(blocks) if !blocks.is_empty() => {
                            let mut contents: Vec<UserContent> = Vec::new();
                            if !text.is_empty() {
                                contents.push(UserContent::Text { text });
                            }
                            for b in blocks {
                                contents.push(b.to_user_content());
                            }
                            Box::pin(d.agent.run_message_with_cancel(
                                UserMessage { content: contents },
                                run_cancel,
                            ))
                        }
                        _ => Box::pin(d.agent.run_with_cancel(&text, run_cancel)),
                    };
                    loop {
                        tokio::select! {
                            biased;
                            _ = select_cancel.cancelled() => {
                                if let Err(e) = d.tx.send(ServerFrame::Say {
                                    text: "任务已取消".into(),
                                }) {
                                    tracing::warn!(?e, "广播取消消息失败");
                                }
                                break;
                            }
                            ev = stream.next() => {
                                match ev {
                                    Some(ev) => {
                                        if let Err(e) = d.tx.send(to_server_frame(ev)) {
                                            tracing::warn!(?e, "广播 Agent 事件帧失败");
                                        }
                                    }
                                    None => break,
                                }
                            }
                        }
                    }
                    d.running.store(false, Ordering::SeqCst);
                    // 补偿状态：若 Agent 流异常终止未发送 Idle/Done，此处兜底确保前端脱离 running 态。
                    if let Err(e) = d.tx.send(ServerFrame::StateChanged { state: AgentState::Idle }) {
                        tracing::warn!(?e, "广播补偿 Idle 状态失败");
                    }
                    let mut guard = d.current_cancel.lock().await;
                    *guard = None;
                    // 任务结束后下发最新上下文占比（前端据此刷新压缩进度）。
                    let u = d.context.token_usage();
                    if let Err(e) = d.tx.send(ServerFrame::ContextUsage {
                        current: u.current,
                        limit: u.limit,
                    }) {
                        tracing::warn!(?e, "广播任务后 ContextUsage 失败");
                    }
                });
            }
            ClientFrame::Respond { ask_id, response } => {
                if let Some(resolver) = deps.pending.lock().await.remove(&ask_id) {
                    let _ = resolver.send(response);
                }
            }
            ClientFrame::Cancel => {
                // 真正中止：触发当前任务的取消句柄
                let guard = deps.current_cancel.lock().await;
                if let Some(token) = guard.as_ref() {
                    token.cancel();
                    if let Err(e) = deps.tx.send(ServerFrame::Say {
                        text: "取消信号已发送".into(),
                    }) {
                        tracing::warn!(?e, "广播取消确认失败");
                    }
                } else {
                    if let Err(e) = deps.tx.send(ServerFrame::Say {
                        text: "无运行中任务".into(),
                    }) {
                        tracing::warn!(?e, "广播无任务提示失败");
                    }
                }
            }
            ClientFrame::Compact => {
                // 手动压缩（与 CLI `/compact` 一致）：shake + summarize + prune。
                // 需独占：不与运行任务并发改写上下文。
                if deps.running.swap(true, Ordering::SeqCst) {
                    if let Err(e) = deps.tx.send(ServerFrame::Error {
                        message: "任务运行中，无法压缩".into(),
                    }) {
                        tracing::warn!(?e, "广播压缩错误失败");
                    }
                    continue;
                }
                let d = deps.clone();
                tokio::spawn(async move {
                    // RAII 守卫：同 NewTask，确保 panic/abort 时 running 被复位。
                    let _guard = CancelGuard {
                        running: d.running.clone(),
                    };
                    if let Err(e) = d.tx.send(ServerFrame::Say {
                        text: "正在压缩上下文…".into(),
                    }) {
                        tracing::warn!(?e, "广播压缩开始消息失败");
                    }
                    let _ = d.context.compact(CompactionStrategy::Shake).await;
                    let _ = d
                        .context
                        .compact(CompactionStrategy::Summarize { max_tokens: 0 })
                        .await;
                    let _ = d
                        .context
                        .compact(CompactionStrategy::Prune { keep_recent: 8 })
                        .await;
                    let u = d.context.token_usage();
                    if let Err(e) = d.tx.send(ServerFrame::Say {
                        text: format!("压缩完成，当前 {} / {} tokens", u.current, u.limit),
                    }) {
                        tracing::warn!(?e, "广播压缩完成消息失败");
                    }
                    if let Err(e) = d.tx.send(ServerFrame::ContextUsage {
                        current: u.current,
                        limit: u.limit,
                    }) {
                        tracing::warn!(?e, "广播压缩后 ContextUsage 失败");
                    }
                    d.running.store(false, Ordering::SeqCst);
                });
            }
        }
    }
}

/// 从配置装配 Agent（与 CLI 一致，审批替换为 WebApprovalPolicy）。
///
/// 返回装配好的 Agent 与其上下文管理器（后者供驱动任务做 `/compact` 与 ContextUsage 下发）。
#[allow(clippy::too_many_arguments)]
async fn build_agent(
    config: &Config,
    http: reqwest::Client,
    cwd: &std::path::Path,
    pending: &PendingMap,
    broadcast_tx: &broadcast::Sender<ServerFrame>,
    supervisor: &Supervisor,
    // 会话 id（决定持久化路径 `<cwd>/.agent/sessions/<id>.jsonl`；resume 时复用历史）。
    session_id: &str,
    alias: Option<&str>,
    // 模式覆盖（None 用配置默认；修复 Web 模式切换此前不生效）。
    mode_override: Option<&str>,
    // MCP 注册表（create_session 加载并共享；只读端点 `/mcp` 复用）。
    mcp: Arc<agent_mcp::McpRegistry>,
    // Skill 目录（create_session 加载并共享；只读端点 `/skills` 复用）。
    skill_catalog: Arc<agent_skills::SkillCatalog>,
) -> Result<(Agent, Arc<dyn agent_core::ContextManager>), String> {
    let profile = config.resolve_model(alias).map_err(|e| e.to_string())?;
    use secrecy::ExposeSecret;
    let api_key: String = profile.resolve_api_key().expose_secret().to_string();
    let model = agent_core::Model {
        id: profile.id.clone(),
        provider: "openai-compatible".into(),
        api: profile.api,
        max_input_tokens: profile.effective_max_input_tokens(),
        max_output_tokens: profile.max_output_tokens.unwrap_or(4096),
        supports_tools: true,
        supports_streaming: true,
        supports_thinking: config.agent.enable_thinking,
        extra_body: profile.extra_body.clone(),
    };

    let mut registry = agent_llm::ProviderRegistry::new();
    for p in agent_llm::collect_providers(http) {
        registry.register(p);
    }
    let provider: Arc<dyn LlmProvider> = Arc::new(registry);
    let provider_ctx = ProviderCallContext {
        api_key: Some(api_key),
        base_url: Some(profile.base_url.clone()),
        max_in_flight: None,
    };
    let mode = mode_override
        .map(|m| match m.trim().to_ascii_lowercase().as_str() {
            "architect" => Mode::Architect,
            "ask" => Mode::Ask,
            "debug" => Mode::Debug,
            _ => Mode::Code,
        })
        .unwrap_or(config.agent.mode);
    let prompts = Arc::new(agent_prompt::PromptCatalog::new());
    // 持久化上下文（与 CLI 共享 <cwd>/.agent/sessions/<id>.jsonl）：resume 时自动加载历史。
    let session_path = agent_context::SessionStore::for_cwd(cwd).path_for(session_id);
    let ctx = agent_context::PersistentContext::open(prompts.system(mode), &session_path)
        .await
        .map_err(|e| e.to_string())?;
    ctx.set_summarizer(Box::new(
        agent_context::compaction::LlmSummaryProvider::new(
            Arc::clone(&provider),
            model.clone(),
            provider_ctx.clone(),
        ),
    ))
    .await;
    let context: Arc<dyn agent_core::ContextManager> = Arc::new(ctx);
    let workspace = Arc::new(Workspace::new(cwd));
    // 审批策略（提前构造：TaskTool 子 Agent 需注入以尊重规则引擎的 Deny，修复审批旁路）。
    // 用运行时 mode（含 mode_override）覆盖 config 默认 mode，使 mode→审批联动生效
    // （code/debug 写类放行；ask/architect 写类询问），否则切换模式时审批门槛不跟随。
    let mut agent_cfg = config.agent.clone();
    agent_cfg.mode = mode;
    let rules = RulesEngine::new(Arc::new(agent_cfg));
    let approval: Arc<dyn ApprovalPolicy> = Arc::new(WebApprovalPolicy::new(
        rules,
        broadcast_tx.clone(),
        Arc::clone(pending),
    ));
    // 子 Agent 工具集（builtin + MCP，不含 task 以防递归）
    let mut sub_reg = agent_tools::builtin_tools();
    if config.github.enabled {
        sub_reg = sub_reg.with(Box::new(agent_tools::GithubTool::new(config.github.allow_write)));
    }
    for t in mcp.tools() {
        sub_reg = sub_reg.with(Box::new(t.clone()));
    }
    let sub_tools: Arc<dyn agent_tools::ToolRegistry> = Arc::new(sub_reg);
    // task 工具（委派子 Agent）—— 受 [subagent] 控制：
    //   enabled 开关 / max_concurrent 并发护栏 / inherit_parent 继承父 temperature·thinking /
    //   独立 max_output_tokens（回退父 profile）
    let sub_max_output = config
        .subagent
        .effective_max_output(profile.max_output_tokens.unwrap_or(4096));
    let sub_temperature = if config.subagent.inherit_parent {
        profile.temperature
    } else {
        None
    };
    let sub_thinking = if config.subagent.inherit_parent && config.agent.enable_thinking {
        Some(agent_core::ThinkingConfig::new(
            config.agent.reasoning_budget.unwrap_or(16_000),
        ))
    } else {
        None
    };
    // 父 Agent 工具集 = builtin + MCP + task（task 受开关控制）
    let (mut tool_registry, lsp_pool) = agent_tools::builtin_tools_with_pool();
    if config.github.enabled {
        tool_registry =
            tool_registry.with(Box::new(agent_tools::GithubTool::new(config.github.allow_write)));
    }
    for t in mcp.tools() {
        tool_registry = tool_registry.with(Box::new(t.clone()));
    }
    if config.subagent.enabled {
        let task_tool = agent::TaskTool::new(
            Arc::clone(&provider),
            Arc::clone(&sub_tools),
            Arc::clone(&prompts),
            Arc::clone(&workspace),
            model.clone(),
            provider_ctx.clone(),
            mode,
            config.agent.max_mistakes,
            config.agent.context_window_guard,
            sub_max_output,
            Arc::new(|| {
                Arc::new(agent_context::InMemoryContext::new(vec![]))
                    as Arc<dyn agent_core::ContextManager>
            }),
            sub_temperature,
            sub_thinking,
            config.subagent.max_concurrent,
        )
        .with_supervisor(supervisor.clone())
        .with_approval(Arc::clone(&approval));
        tool_registry = tool_registry.with(Box::new(task_tool));
    }
    let tools: Arc<dyn agent_tools::ToolRegistry> = Arc::new(tool_registry);

    let mut context_files = agent_config::discover_context_files(cwd);
    if config.github.enabled {
        context_files.push(agent_tools::PROMPT_SECTION.to_string());
    }
    // 长期记忆（可选；按 cwd 项目作用域）
    let memory: Option<Arc<dyn agent_core::MemoryStore>> = if config.memory.enabled {
        Some(Arc::new(agent_memory::LocalMemoryStore::new(cwd)))
    } else {
        None
    };

    // 模型的输出 token 预算（来自 profile.max_output_tokens，回退 4096）须下发给
    // Agent 作为每轮请求的 max_tokens；否则 assemble 未设置会回落到硬编码 4096，
    // 长回复被中途截断（finish_reason=length）→ 误报「任务完成」。
    let max_output_tokens = model.max_output_tokens;
    let agent = assemble(
        Agent::builder(model),
        provider,
        tools,
        lsp_pool,
        Arc::clone(&context),
        prompts,
        approval,
        workspace,
        provider_ctx,
        mode,
        max_output_tokens,
        config,
        skill_catalog,
        context_files,
        memory,
    );
    Ok((agent, context))
}

#[allow(clippy::too_many_arguments)]
fn assemble(
    builder: AgentBuilder,
    provider: Arc<dyn LlmProvider>,
    tools: Arc<dyn agent_tools::ToolRegistry>,
    lsp_pool: agent_tools::LspPool,
    context: Arc<dyn agent_core::ContextManager>,
    prompts: Arc<agent_prompt::PromptCatalog>,
    approval: Arc<dyn ApprovalPolicy>,
    workspace: Arc<Workspace>,
    provider_ctx: ProviderCallContext,
    mode: Mode,
    max_output_tokens: usize,
    config: &Config,
    catalog: Arc<agent_skills::SkillCatalog>,
    context_files: Vec<String>,
    memory: Option<Arc<dyn agent_core::MemoryStore>>,
) -> Agent {
    let workspace_root = workspace.root().to_path_buf();
    // fuzzy 配置 → 全局覆盖（首次装配 set，OnceLock 幂等；与 CLI 一致）。
    {
        let mut opts = agent_tools::FuzzyOpts::from_env();
        match config.agent.tools.edit.fuzzy.as_str() {
            "on" | "1" | "true" => opts.enabled = true,
            "off" | "0" | "false" => opts.enabled = false,
            _ => {}
        }
        opts.threshold = config.agent.tools.edit.fuzzy_threshold;
        agent_tools::set_fuzzy_opts(opts);
    }
    let builder = builder
        .provider(provider)
        .tools(tools)
        .context(context)
        .prompts(prompts)
        .approval(approval)
        .workspace(workspace)
        .provider_ctx(provider_ctx)
        .mode(mode)
        .max_output_tokens(max_output_tokens)
        .max_mistakes(config.agent.max_mistakes)
        .max_turns(config.agent.max_turns)
        .context_guard(config.agent.context_window_guard)
        .catalog(catalog)
        .context_files(context_files);
    // 注入思考模式（若 config 启用）—— 与 CLI 一致
    let builder = if config.agent.enable_thinking {
        builder.thinking(agent_core::ThinkingConfig::new(
            config.agent.reasoning_budget.unwrap_or(16_000),
        ))
    } else {
        builder
    };
    // 编辑后 LSP writethrough：lsp 启用且 edit 开启时注入（与 CLI 一致）。
    let builder = if config.tools.effective("lsp", false)
        && (config.agent.tools.edit.format_on_write
            || config.agent.tools.edit.diagnostics_on_write)
    {
        builder.write_effect(std::sync::Arc::new(
            agent_tools::LspWriteEffect::new(
                workspace_root,
                std::sync::Arc::clone(&lsp_pool),
                config.agent.tools.edit.format_on_write,
                config.agent.tools.edit.diagnostics_on_write,
                config.agent.tools.edit.diagnostics_deduplicate,
            ),
        ) as std::sync::Arc<dyn agent_core::WriteEffect>)
    } else {
        builder
    };
    if let Some(m) = memory {
        builder.memory(m).build()
    } else {
        builder.build()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// axum 路由
// ──────────────────────────────────────────────────────────────────────────────

/// 内嵌式前端资源：编译期把 `web/` 构建产物打包进二进制
/// （排除 `web/c5-ui/` 源码目录，仅含 `index.html` + `assets/`）。
///
/// - **release**：产物完全内嵌，单二进制即可 `--serve`。
/// - **debug**：`rust-embed` 默认从磁盘实时读取，便于前端改动即时生效。
#[derive(rust_embed::RustEmbed)]
#[folder = "../../web"]
#[exclude = "c5-ui/**"]
struct WebAsset;

/// 静态资源 + SPA 兜底：命中内嵌文件则返回，否则回退到 `index.html`
/// （前端为单页应用，任意前端路由都由客户端路由接管）。
async fn serve_embedded(uri: Uri) -> Response {
    let req_path = uri.path().trim_start_matches('/');
    if !req_path.is_empty() {
        if let Some(file) = WebAsset::get(req_path) {
            return asset_response(req_path, &file);
        }
    }
    match WebAsset::get("index.html") {
        Some(file) => asset_response("index.html", &file),
        None => (
            StatusCode::NOT_FOUND,
            "前端构建产物未嵌入：请在仓库根执行 `npm --prefix web/c5-ui run build` 后重新编译",
        )
            .into_response(),
    }
}

/// 将内嵌文件转为 HTTP 响应（按扩展名推断 `Content-Type` + 缓存策略）。
///
/// 缓存策略（标准 SPA 模式，修复前端升级后浏览器仍加载旧 bundle 的问题）：
/// - `.html`（SPA 入口，引用内容哈希命名的 bundle）：`no-cache`，浏览器必须回源
///   校验，确保拿到指向最新 bundle 的 `index.html`。
/// - 其它资源（Vite 内容哈希文件名，内容变则文件名变）：
///   `public, max-age=31536000, immutable`，可永久缓存。
fn asset_response(path: &str, file: &rust_embed::EmbeddedFile) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // 文件名以哈希命名的静态资源（js/css/字体/图片）可永久缓存；
    // HTML 入口必须 no-cache，否则浏览器会用陈旧的 index.html（指向旧 bundle）。
    let cache_control = if path.ends_with(".html") {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    Response::builder()
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CACHE_CONTROL, cache_control)
        .body(Body::from(file.data.to_vec()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// 构建 axum Router（静态前端内嵌于二进制，运行时无需 `web/` 目录）。
#[must_use]
pub fn app(state: SessionManager) -> Router {
    Router::new()
        .route("/api/sessions", get(create_session))
        .route("/api/sessions/list", get(list_sessions))
        .route("/api/sessions/{id}", delete(delete_session).post(rename_session))
        .route("/api/sessions/{id}/agents", get(list_agents))
        .route("/api/sessions/{id}/skills", get(list_skills))
        .route("/api/sessions/{id}/skill/{name}", get(skill_body))
        .route("/api/sessions/{id}/mcp", get(list_mcp))
        .route("/api/sessions/{id}/history", get(session_history))
        .route("/api/commands", get(list_commands))
        .route("/api/models", get(list_models))
        .route("/api/stats", get(stats))
        .route("/api/workspace", get(workspace_info))
        .route("/api/fs", get(list_dir))
        .route("/api/file", get(read_file))
        .route("/ws/{id}", get(ws_handler))
        .route("/api/collab/room", get(new_collab_room))
        .route("/collab/{room_id}", get(collab_ws_handler))
        .with_state(state)
        .fallback(serve_embedded)
}

/// 运行时统计（本地可观测端点）。
async fn stats(
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    Json(serde_json::json!({
        "active_sessions": state.active_session_count().await,
        "models_available": state.models().len(),
    }))
    .into_response()
}

async fn create_session(
    State(state): State<SessionManager>,
    axum::extract::Query(params): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &params.token) {
        return resp;
    }
    match state
        .create_session(
            params.model.as_deref(),
            params.resume.as_deref(),
            params.fork.as_deref(),
            params.mode.as_deref(),
        )
        .await
    {
        Ok(id) => Json(serde_json::json!({ "session_id": id, "ws_url": format!("/ws/{id}") }))
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn list_models(
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    Json(state.models()).into_response()
}

/// 子 Agent 监控快照（REST 补全：迟到的客户端或非 WS 消费方）。
async fn list_agents(
    State(state): State<SessionManager>,
    Path(id): Path<String>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    match state.get(&id).await {
        Some(session) => {
            let agents = session.supervisor.snapshot().await;
            Json(serde_json::json!({ "agents": agents })).into_response()
        }
        None => (StatusCode::NOT_FOUND, "会话不存在").into_response(),
    }
}

/// `/api/sessions/{id}/skills` → 已加载 Skill 列表（只读；镜像 CLI `/skills`）。
async fn list_skills(
    State(state): State<SessionManager>,
    Path(id): Path<String>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    match state.get(&id).await {
        Some(session) => {
            let skills: Vec<serde_json::Value> = session
                .skills
                .skills
                .iter()
                .map(|s| {
                    let level = match s.source.level {
                        SkillLevel::User => "user",
                        SkillLevel::Project => "project",
                    };
                    serde_json::json!({
                        "name": s.name,
                        "description": s.description,
                        "level": level,
                        "hide": s.hide,
                    })
                })
                .collect();
            Json(serde_json::json!({ "skills": skills })).into_response()
        }
        None => (StatusCode::NOT_FOUND, "会话不存在").into_response(),
    }
}

/// `/api/sessions/{id}/skill/{name}` → 指定 Skill 的正文（注入对话用；镜像 CLI `/skill:<名>`）。
async fn skill_body(
    State(state): State<SessionManager>,
    Path((id, name)): Path<(String, String)>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    match state.get(&id).await {
        Some(session) => match session.skills.find(&name) {
            Some(skill) => match tokio::fs::read_to_string(&skill.file_path).await {
                Ok(body) => Json(serde_json::json!({ "name": skill.name, "body": body }))
                    .into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("读取 skill 失败: {e}"),
                )
                    .into_response(),
            },
            None => (StatusCode::NOT_FOUND, "未知 skill").into_response(),
        },
        None => (StatusCode::NOT_FOUND, "会话不存在").into_response(),
    }
}
async fn list_mcp(
    State(state): State<SessionManager>,
    Path(id): Path<String>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    match state.get(&id).await {
        Some(session) => {
            let tools: Vec<serde_json::Value> = session
                .mcp
                .tools()
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name(),
                        "description": t.description(),
                    })
                })
                .collect();
            Json(serde_json::json!({ "tools": tools })).into_response()
        }
        None => (StatusCode::NOT_FOUND, "会话不存在").into_response(),
    }
}

/// `/api/commands` → 自定义 slash 命令（`.agent/commands/*.md` + 用户级，与 CLI 同源）。
async fn list_commands(
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    let commands: Vec<serde_json::Value> = discover_commands(state.cwd())
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "description": c.description,
                "body": c.body,
            })
        })
        .collect();
    Json(serde_json::json!({ "commands": commands })).into_response()
}

/// 聚合并转发子 Agent 监控事件为 `ServerFrame::SubAgents` 全量快照。
///
/// 采用「dirty 标记 + 125ms 节拍」：事件仅置位，节拍到期才下发最新快照，
/// 突发不补帧、带宽友好（≈8fps），且天然幂等（前端整体替换）。
async fn supervisor_forwarder(sup: Supervisor, tx: broadcast::Sender<ServerFrame>) {
    let mut rx = sup.subscribe();
    let mut dirty = false;
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(125));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // 丢弃立即触发的首拍，避免空快照
    loop {
        tokio::select! {
            biased;
            ev = rx.recv() => match ev {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => dirty = true,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = interval.tick() => {
                if dirty {
                    dirty = false;
                    let agents = sup.snapshot().await;
                    if let Err(e) = tx.send(ServerFrame::SubAgents { agents }) {
                        tracing::warn!(?e, "广播 SubAgents 快照失败");
                    }
                }
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 文件浏览（只读）：工作区根信息 / 列目录 / 读文件。路径越界一律 403。
// ──────────────────────────────────────────────────────────────────────────────

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB，超限只返回截断 + 标记

/// `GET /api/workspace` → 工作区根信息。
async fn workspace_info(
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    let cwd = state.cwd();
    Json(serde_json::json!({
        "root": cwd.display().to_string(),
        "name": cwd.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| ".".into()),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct FsParams {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

/// `GET /api/fs?path=` → 目录直接子项（name / kind / size）。
async fn list_dir(
    State(state): State<SessionManager>,
    axum::extract::Query(p): axum::extract::Query<FsParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &p.token) {
        return resp;
    }
    let rel = p.path.as_deref().unwrap_or(".");
    let Some(full) = safe_join(state.cwd(), rel) else {
        return (StatusCode::FORBIDDEN, "路径越界").into_response();
    };
    let entries = match tokio::task::spawn_blocking(move || collect_entries(&full)).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return (StatusCode::BAD_REQUEST, e).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    Json(serde_json::json!({ "path": rel, "entries": entries })).into_response()
}

/// `GET /api/file?path=` → 文件内容（UTF-8 文本）。超 2MiB 截断。
async fn read_file(
    State(state): State<SessionManager>,
    axum::extract::Query(p): axum::extract::Query<FsParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &p.token) {
        return resp;
    }
    let Some(path_str) = p.path.as_deref() else {
        return (StatusCode::BAD_REQUEST, "缺少 path").into_response();
    };
    let Some(full) = safe_join(state.cwd(), path_str) else {
        return (StatusCode::FORBIDDEN, "路径越界").into_response();
    };
    let meta = match tokio::fs::metadata(&full).await {
        Ok(m) => m,
        Err(_) => return (StatusCode::NOT_FOUND, "文件不存在").into_response(),
    };
    if !meta.is_file() {
        return (StatusCode::BAD_REQUEST, "不是文件").into_response();
    }
    let size = meta.len();
    let truncated = size > MAX_FILE_BYTES;
    // 超限时仅读取前 MAX_FILE_BYTES，避免大文件整段入内存（OOM）。
    let bytes = if truncated {
        match read_bounded_head(&full, MAX_FILE_BYTES as usize).await {
            Ok(b) => b,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    } else {
        match tokio::fs::read(&full).await {
            Ok(b) => b,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    };
    // 二进制探测：含 NUL 视为非文本
    if bytes.iter().any(|&b| b == 0) {
        return Json(serde_json::json!({
            "path": path_str,
            "binary": true,
            "size": size,
            "truncated": truncated,
            "content": null,
        }))
        .into_response();
    }
    let content = String::from_utf8_lossy(&bytes).into_owned();
    Json(serde_json::json!({
        "path": path_str,
        "binary": false,
        "size": size,
        "truncated": truncated,
        "content": content,
    }))
    .into_response()
}

/// 读取文件前 `max` 字节（防大文件 OOM，循环填满缓冲避免短读）。
async fn read_bounded_head(path: &std::path::Path, max: usize) -> Result<Vec<u8>, std::io::Error> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; max];
    let mut filled = 0usize;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]).await {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// 将相对路径拼到工作区根，canonicalize 后确认仍位于根内（防 `..` 越界）。
fn safe_join(root: &std::path::Path, rel: &str) -> Option<std::path::PathBuf> {
    let joined = if std::path::Path::new(rel).is_absolute() {
        std::path::PathBuf::from(rel)
    } else {
        root.join(rel)
    };
    let canon = joined.canonicalize().ok()?;
    let root_canon = root.canonicalize().ok()?;
    if canon.starts_with(&root_canon) {
        Some(canon)
    } else {
        None
    }
}

/// 收集目录直接子项（目录在前，文件按名排序），跳过常见噪声目录。
/// 目录列表条目数上限（防止巨型目录耗尽阻塞线程池）。
const MAX_DIR_ENTRIES: usize = 2000;

fn collect_entries(dir: &std::path::Path) -> Result<Vec<serde_json::Value>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut count = 0usize;
    for entry in rd.flatten() {
        if count >= MAX_DIR_ENTRIES {
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" || name == "target" || name == "node_modules" {
            continue;
        }
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let item = serde_json::json!({
            "name": name,
            "kind": if ft.is_dir() { "dir" } else { "file" },
            "size": size,
        });
        if ft.is_dir() {
            dirs.push(item);
        } else {
            files.push(item);
        }
        count += 1;
    }
    dirs.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    dirs.extend(files);
    Ok(dirs)
}

/// 鉴权 + 模型选择查询参数（`?token=xxx&model=alias`）。
#[derive(Debug, Deserialize)]
struct SessionParams {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// 恢复指定历史会话（会话 id）。
    #[serde(default)]
    resume: Option<String>,
    /// 复制源会话为新 id 后继续（resume 与 fork 互斥，resume 优先）。
    #[serde(default)]
    fork: Option<String>,
    /// 模式覆盖（code|architect|ask|debug）。
    #[serde(default)]
    mode: Option<String>,
}

/// 会话 id 安全校验：仅允许 `[A-Za-z0-9_-]`，防路径穿越（`/` `\` `..` 等）。
fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 读取会话 JSONL 的首条用户文本（列表预览用）。
fn read_first_user(path: &std::path::Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file).lines().flatten() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<agent_core::AgentMessage>(trimmed) else {
            continue;
        };
        if let agent_core::AgentMessage::User(u) = msg {
            let text: String = u
                .content
                .iter()
                .filter_map(|c| match c {
                    agent_core::UserContent::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            let t = text.trim();
            if !t.is_empty() {
                return Some(preview_text(t, 100));
            }
        }
    }
    None
}

/// 读取会话历史（user/thinking/assistant 文本，前端恢复对话用）。
fn read_history(path: &std::path::Path) -> Vec<serde_json::Value> {
    use std::io::BufRead;
    let mut out = Vec::new();
    let Some(file) = std::fs::File::open(path).ok() else {
        return out;
    };
    for line in std::io::BufReader::new(file).lines().flatten() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<agent_core::AgentMessage>(trimmed) else {
            continue;
        };
        match msg {
            agent_core::AgentMessage::User(u) => {
                let text: String = u
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        agent_core::UserContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let t = text.trim();
                if !t.is_empty() {
                    out.push(serde_json::json!({ "kind": "user", "text": t }));
                }
            }
            agent_core::AgentMessage::Assistant(a) => {
                // 思考内容（reasoning）：通常先于正文出现，前端以可折叠「思考」块展示，
                // 与实时流的 ThinkingDelta 行为一致。
                let thinking: String = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        agent_core::ContentBlock::Thinking { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let tk = thinking.trim();
                if !tk.is_empty() {
                    out.push(serde_json::json!({ "kind": "thinking", "text": tk }));
                }
                let text: String = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        agent_core::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let t = text.trim();
                if !t.is_empty() {
                    out.push(serde_json::json!({ "kind": "assistant", "text": t }));
                }
            }
            _ => {}
        }
    }
    out
}

/// 截断预览文本（换行折叠为空格，超长加 …）。
fn preview_text(s: &str, max: usize) -> String {
    let s = s.trim().replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let mut o: String = s.chars().take(max.saturating_sub(1)).collect();
        o.push('…');
        o
    }
}

/// 历史会话列表（含首条用户输入预览，前端切换用；与 CLI `/sessions` 同源）。
async fn list_sessions(
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    let store = agent_context::SessionStore::for_cwd(state.cwd());
    let sessions: Vec<serde_json::Value> = store
        .list()
        .into_iter()
        .map(|s| {
            let path = store.path_for(&s.id);
            let preview = read_first_user(&path).unwrap_or_else(|| "(空会话)".into());
            let title = store.title_for(&s.id);
            let mtime_ms = s
                .mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            serde_json::json!({
                "id": s.id,
                "preview": preview,
                "title": title,
                "mtime_ms": mtime_ms,
                "bytes": s.bytes,
            })
        })
        .collect();
    Json(serde_json::json!({ "sessions": sessions })).into_response()
}

/// 单个会话的历史消息（user/thinking/assistant 文本，恢复对话展示用）。
async fn session_history(
    State(state): State<SessionManager>,
    Path(id): Path<String>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    if !is_safe_session_id(&id) {
        return (StatusCode::BAD_REQUEST, "非法会话 id").into_response();
    }
    let path = agent_context::SessionStore::for_cwd(state.cwd()).path_for(&id);
    let items = read_history(&path);
    Json(serde_json::json!({ "items": items })).into_response()
}

/// 重命名请求体（自定义会话标题）。
#[derive(Debug, Deserialize)]
struct RenameBody {
    #[serde(default)]
    title: String,
}

/// `DELETE /api/sessions/{id}` → 关闭活跃会话（释放后台任务）并删除落盘文件 + 标题。
///
/// 删除当前会话时，前端会另起新会话；此处只负责清理服务端资源（幂等）。
async fn delete_session(
    State(state): State<SessionManager>,
    Path(id): Path<String>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    if !is_safe_session_id(&id) {
        return (StatusCode::BAD_REQUEST, "非法会话 id").into_response();
    }
    // 关闭可能在内存中的活跃会话（中止驱动/监控任务，释放后台资源）。
    state.close_session(&id).await;
    let store = agent_context::SessionStore::for_cwd(state.cwd());
    match store.delete(&id) {
        Ok(true) => Json(serde_json::json!({ "ok": true, "id": id })).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "会话不存在").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("删除失败: {e}"),
        )
            .into_response(),
    }
}

/// `POST /api/sessions/{id}` → 设置/更新自定义标题（重命名）。
async fn rename_session(
    State(state): State<SessionManager>,
    Path(id): Path<String>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
    axum::Json(body): axum::Json<RenameBody>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    if !is_safe_session_id(&id) {
        return (StatusCode::BAD_REQUEST, "非法会话 id").into_response();
    }
    let store = agent_context::SessionStore::for_cwd(state.cwd());
    match store.set_title(&id, &body.title) {
        Ok(()) => Json(serde_json::json!({
            "ok": true,
            "id": id,
            "title": body.title.trim(),
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("重命名失败: {e}")).into_response(),
    }
}

/// 校验 token：配置了 auth_token 则必须匹配。
fn check_auth(state: &SessionManager, token: &Option<String>) -> Result<(), Response> {
    let Some(expected) = state.expected_token() else {
        return Ok(());
    };
    // 常量时间比较（避免按字节提前返回的时序侧信道）。长度差异可接受地泄露。
    let ok = token
        .as_deref()
        .map(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()))
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "无效或缺失 token").into_response())
    }
}

/// 近似常量时间的字节比较：不按首个差异提前返回。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn ws_handler(
    ws: axum::extract::ws::WebSocketUpgrade,
    Path(id): Path<String>,
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    let Some(session) = state.get(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ws.max_message_size(256 * 1024)
        .on_upgrade(move |socket| handle_socket(socket, session))
}

async fn handle_socket(socket: axum::extract::ws::WebSocket, session: Arc<Session>) {
    let (mut sink, mut input) = socket.split();
    let mut out_rx = session.broadcast.subscribe();

    // 重连补帧：若该会话正有任务运行，先给本连接回推一帧 Running 态，使重连前端
    // 立即反映真实进度（而非误判 no_task、允许发起新任务而被驱动任务拒绝）。
    if session
        .running
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        if let Ok(text) = serde_json::to_string(&ServerFrame::StateChanged { state: AgentState::Running }) {
            let _ = sink
                .send(axum::extract::ws::Message::Text(text.into()))
                .await;
        }
    }

    // 广播 → 客户端
    let send_task = tokio::spawn(async move {
        while let Ok(frame) = out_rx.recv().await {
            let Ok(text) = serde_json::to_string(&frame) else {
                continue;
            };
            if sink
                .send(axum::extract::ws::Message::Text(text.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // 客户端 → 入站
    while let Some(msg) = input.next().await {
        let Ok(msg) = msg else { break };
        if let axum::extract::ws::Message::Text(text) = msg {
            if let Ok(frame) = serde_json::from_str::<ClientFrame>(&text) {
                let _ = session.inbound.send(frame);
            }
        }
    }
    send_task.abort();
}

// ──────────────────────────────────────────────────────────────────────────────
// 协同中继（端到端加密）：按不透明 room_id 转发密封字节，永不接触明文/密钥
// ──────────────────────────────────────────────────────────────────────────────

/// `GET /api/collab/room` → 生成房间密钥，返回派生 room_id 与 base64url 密钥片段。
///
/// 密钥仅返回给调用方；中继只用派生的 room_id 路由，无法解密。
async fn new_collab_room(
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    let key = agent_collab::generate_room_key();
    let room_id = agent_collab::room_id(&key);
    Json(serde_json::json!({
        "room_id": room_id,
        "key": agent_collab::encode_room_key(&key),
        "ws_url": format!("/collab/{room_id}"),
    }))
    .into_response()
}

/// `GET /collab/{room_id}` → 升级为 WebSocket，桥接为协同中继。
async fn collab_ws_handler(
    ws: axum::extract::ws::WebSocketUpgrade,
    Path(room_id): Path<String>,
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    ws.max_message_size(256 * 1024)
        .on_upgrade(move |socket| collab_relay(socket, room_id, state.relay.clone()))
}

/// 双向桥接：中继广播 → 客户端；客户端密封字节 → 中继。
///
/// 密封字节作为二进制 WS 帧承载；中继对内容盲视（仅按 room_id 路由）。
/// 协同中继单条消息最大字节数。
const COLLAB_MSG_MAX_SIZE: usize = 64 * 1024;
/// 协同中继每秒消息数上限（速率限制窗口）。
const COLLAB_MSG_RATE_LIMIT: u32 = 30;

async fn collab_relay(
    socket: axum::extract::ws::WebSocket,
    room_id: String,
    relay: agent_collab::Relay,
) {
    let (mut sink, mut input) = socket.split();
    let mut rx = relay.join(&room_id).await;

    // 中继广播 → 客户端
    let send_task = tokio::spawn(async move {
        while let Ok(bytes) = rx.recv().await {
            if sink
                .send(axum::extract::ws::Message::Binary(bytes.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // 客户端 → 中继广播（带速率限制 + 大小上限，防洪泛 DoS）
    let mut rate_window = std::time::Instant::now();
    let mut rate_count = 0u32;
    while let Some(Ok(msg)) = input.next().await {
        match msg {
            axum::extract::ws::Message::Binary(b) => {
                if b.len() > COLLAB_MSG_MAX_SIZE {
                    continue;
                }
                if rate_window.elapsed() >= std::time::Duration::from_secs(1) {
                    rate_window = std::time::Instant::now();
                    rate_count = 0;
                }
                rate_count += 1;
                if rate_count > COLLAB_MSG_RATE_LIMIT {
                    continue;
                }
                relay.publish(&room_id, b.to_vec()).await;
            }
            axum::extract::ws::Message::Close(_) => break,
            _ => {}
        }
    }
    send_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_frame_maps_text_delta() {
        let f = to_server_frame(AgentEvent::TextDelta("hi".into()));
        match f {
            ServerFrame::TextDelta { delta } => assert_eq!(delta, "hi"),
            _ => panic!("应为 TextDelta"),
        }
    }

    /// `StateChanged` 为 struct variant，序列化后 `state` 字段直接承载 AgentState 字符串值，
    /// 而非被 serde 展平为 `{"running": null}` 之类（newtype 包装 fieldless enum 的陷阱）。
    #[test]
    fn serialize_state_changed_emits_state_field() {
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&ServerFrame::StateChanged { state: AgentState::Running }).unwrap())
                .unwrap();
        assert_eq!(v["type"], "state_changed");
        assert_eq!(v["state"], "running");
        assert_eq!(v.get("running"), None, "不应残留 newtype 展平的 null 字段");
    }

    /// `read_history` 应从 assistant 消息中分别提取 thinking 与 text，
    /// 且 thinking 排在正文之前（与实时流 ThinkingDelta → TextDelta 顺序一致）。
    #[test]
    fn read_history_includes_thinking() {
        use agent_core::{AssistantMessage, ContentBlock, Usage};
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "agent_history_test_{}_{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let msg = agent_core::AgentMessage::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Thinking { text: "先思考一下".into(), signature: None },
                ContentBlock::Text { text: "最终回答".into() },
            ],
            usage: Usage::default(),
            model: "test".into(),
            stop_reason: None,
        });
        let line = serde_json::to_string(&msg).unwrap();
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "{line}").unwrap();
        }
        let items = read_history(&path);
        let _ = std::fs::remove_file(&path);
        let kinds: Vec<&str> = items.iter().map(|v| v["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["thinking", "assistant"]);
        assert_eq!(items[0]["text"].as_str().unwrap(), "先思考一下");
        assert_eq!(items[1]["text"].as_str().unwrap(), "最终回答");
    }
}
