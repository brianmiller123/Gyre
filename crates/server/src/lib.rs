//! # agent-server
//!
//! HTTP/WebSocket 服务：[`SessionManager`] + 带宽优化线协议 + [`WebApprovalPolicy`] +
//! 静态前端托管。体现「Agent 循环对传输透明」——循环产出 [`AgentEvent`](agent_core::AgentEvent)，
//! 经驱动任务转为 [`ServerFrame`] 广播给 WebSocket 订阅者；审批经 [`ClientFrame::Respond`] 回执。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

use std::collections::HashMap;
use std::sync::Arc;

use agent::{Agent, AgentBuilder, PauseGate};
use agent_config::{Config, ModelProfile, RulesEngine, discover_commands};
use agent_core::{
    AgentEvent, AgentState, ApprovalDecision, ApprovalMode, ApprovalPolicy, ApprovalRequest,
    AskMessage, AskResponse, AssistantMessage, CompactionStrategy, ContentBlock, ContextManager,
    LlmProvider, Mode, ProviderCallContext, SkillLevel, ToolError, ToolResult,
    ToolResultMessage, Usage, UserContent, UserMessage, Workspace,
};
use agent_supervisor::{SubAgentStatus, Supervisor};
use agent_tools::Tool;
use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

const APPROVAL_TIMEOUT_SECS: u64 = 300;
/// 单进程最大并发会话数（软上限，缓解无界创建导致的内存/任务泄漏）。
const MAX_SESSIONS: usize = 64;
/// WebSocket 单条入站消息上限（字节）。
///
/// 多模态图片以内联 base64 经 `new_task` 帧发送，256 KiB 会让绝大多数图片帧被
/// tungstenite 拒收（`Capacity::MessageTooBig`）→ 连接关闭、任务永不到达驱动、
/// 前端表现为「上传图片后无任何内容响应」。提升到 16 MiB 以容纳常见截图/照片。
const WS_MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
/// 单张上传图片解码后字节上限（与 CLI `read_image` 的 MAX_IMAGE_BYTES 对齐）。
const MAX_UPLOAD_IMAGE_BYTES: usize = 10 * 1024 * 1024;

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
    /// 图像引用（句柄）。先经 `POST /api/sessions/{id}/upload` 上传拿到 `upload_id`，
    /// 再以此句柄经 WS 发送，避免在控制信道传输大体积 base64（兼容旧的内联 `Image`）。
    ImageRef {
        /// `/upload` 返回的句柄。
        upload_id: String,
    },
}

impl ContentInput {
    /// 转为内部 [`UserContent`]。
    ///
    /// `ImageRef` 据 `uploads` 句柄表解析为真实 base64；句柄缺失（已过期/未知）返回
    /// `None`，由调用方决定跳过或告警。
    #[must_use]
    fn to_user_content(&self, uploads: &HashMap<String, (String, String)>) -> Option<UserContent> {
        match self {
            Self::Text { text } => Some(UserContent::Text { text: text.clone() }),
            Self::Image { mime, data } => Some(UserContent::Image {
                mime: mime.clone(),
                data: data.clone(),
            }),
            Self::ImageRef { upload_id } => {
                uploads
                    .get(upload_id)
                    .map(|(mime, data)| UserContent::Image {
                        mime: mime.clone(),
                        data: data.clone(),
                    })
            }
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
    /// 用量增量（单次 LLM 调用；前端累加 addUsage）。
    Usage(Usage),
    /// 用量快照（活跃分支累计全量；前端整体替换 SET）。
    ///
    /// 仅在 WS 连接建立时下发一次，作为基线恢复：切换会话 / 重连 / 切换模式时前端
    /// 累计态可能与持久化真相不同步，以此权威快照覆盖，避免丢失或重复累加。
    UsageSnapshot(Usage),
    /// 完成。
    Done {
        turns: u64,
        tool_calls: u64,
        success: bool,
    },
    /// 错误。
    Error { message: String },
    /// 用户在任务运行中提交的消息已作为 steering 投递给当前 agent（移植 oh-my-pi：忙时
    /// steer 而非拒绝）。Immediate 策略会尽快打断在途工具，并在下一边界把消息投给模型。
    /// 前端据此把该消息渲染为一次「插话」，区别于普通轮次。
    Steered { text: String },
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
    /// 应用层心跳（无业务负载）。由 WS 转发层在无其它帧时空闲下发，目的是：
    /// ① 保活反向代理 / LB 的读超时（默认常为 60s，慢速 LLM 长时间无数据会被切连）；
    /// ② 刷新前端「心跳看门狗」的活动时间戳——前端 `onmessage` 收到任意帧即更新
    ///    `lastActivityRef`，避免在慢速生成阶段误判后端静默终止。前端 `parseFrame`
    ///    对未知 `type` 返回 null，故本帧被静默忽略，不影响 transcript。
    Heartbeat,

    // ── 三层生命周期帧（镜像 AgentEvent 的 turn/message/tool_execution）──
    /// 轮次开始。
    TurnStart,
    /// 轮次结束：携带本轮最终 assistant 消息、工具结果、是否继续。
    TurnEnd {
        /// 本轮最终化的 assistant 消息。
        message: AssistantMessage,
        /// 本轮工具结果（按回填顺序）。
        tool_results: Vec<ToolResultMessage>,
        /// 是否继续下一轮。
        will_continue: bool,
    },
    /// assistant 消息开始。
    MessageStart,
    /// assistant 消息最终化。
    MessageEnd {
        /// 完整 assistant 消息快照。
        message: AssistantMessage,
    },
    /// 工具执行开始。
    ToolExecutionStart {
        /// 工具调用 id。
        tool_call_id: String,
        /// 工具名。
        name: String,
        /// 工具参数。
        args: serde_json::Value,
    },
    /// 工具执行流式 partial（预留）。
    ToolExecutionUpdate {
        /// 工具调用 id。
        tool_call_id: String,
        /// 工具名。
        name: String,
        /// 阶段性输出。
        partial: String,
    },
    /// 工具执行结束。
    ToolExecutionEnd {
        /// 工具调用 id。
        tool_call_id: String,
        /// 工具名。
        name: String,
        /// 工具结果。
        result: ToolResult,
        /// 是否为错误结果。
        is_error: bool,
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
        AgentEvent::TurnStart => ServerFrame::TurnStart,
        AgentEvent::TurnEnd {
            message,
            tool_results,
            will_continue,
        } => ServerFrame::TurnEnd {
            message,
            tool_results,
            will_continue,
        },
        AgentEvent::MessageStart => ServerFrame::MessageStart,
        AgentEvent::MessageEnd(message) => ServerFrame::MessageEnd { message },
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            name,
            args,
        } => ServerFrame::ToolExecutionStart {
            tool_call_id,
            name,
            args,
        },
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            name,
            partial,
        } => ServerFrame::ToolExecutionUpdate {
            tool_call_id,
            name,
            partial,
        },
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            name,
            result,
            is_error,
        } => ServerFrame::ToolExecutionEnd {
            tool_call_id,
            name,
            result,
            is_error,
        },
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

/// 会话级 LLM 句柄：供 `/enhance` 等只读端点直接调用模型，无需经驱动任务。
pub struct SessionLlm {
    /// 已装配的 Provider（与驱动 Agent 同源；路由到会话所选模型）。
    pub provider: Arc<dyn LlmProvider>,
    /// 单次调用的鉴权 / 网关上下文。
    pub ctx: ProviderCallContext,
    /// 会话当前模型（热切换模型后随 Agent 一起重建）。
    pub model: agent_core::Model,
}

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
    /// 会话上下文（共享同一份 `PersistentContext`，与驱动任务同源）。
    /// 供 `DELETE /api/sessions/{id}/messages/{line}` 在线删除单条消息使用。
    pub context: Arc<dyn agent_core::ContextManager>,
    /// 会话级 LLM 句柄（`/enhance` 等只读端点用）。
    pub llm: SessionLlm,
    /// 多模态图片上传句柄表（`POST /upload` 写、`new_task` 的 `ImageRef` 读，消费即清）。
    pub uploads: Arc<Mutex<HashMap<String, (String, String)>>>,
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
    /// host 侧房间机密（`room_id` → 房间密钥 + 规范 write token）。
    ///
    /// 本服务即 **host 进程**（对标 oh-my-pi 的 coding-agent 持密钥）：密钥由
    /// [`new_collab_room`] 本地生成、从不外发，仅用于 WS 桥向 guest 单播
    /// host 裁决帧（`Welcome` `read_only`）。中继 [`Relay`] 本身保持密钥盲视——
    /// 只存密封字节；host 只密封自己的帧、永不解封他人帧。
    collab_hosts: Arc<Mutex<HashMap<String, HostSecret>>>,
    /// 协同中继维护循环是否已启动（惰性，首次创建会话时拉起）。
    maintenance_started: Arc<std::sync::atomic::AtomicBool>,
    /// 进程级暂停门（共享单例）：注入每个会话的 Agent，由 `/api/pause` `/api/resume`
    /// 驱动。pause 时所有 agent loop 在下一安全点（provider 调用前 / 工具批执行前）
    /// park，在途 provider 流与已启动工具跑完后冻结，resume 后从原处继续（零丢失）。
    /// 移植 oh-my-pi `AgentPauseGate`。
    pause_gate: Arc<PauseGate>,
    /// SOCKS5 出站代理运行时控制器（共享单例；`/api/socks5` 路由驱动其开关，
    /// 实时生效并持久化）。`None` = 未配置（代理不可用，前端不显示开关）。
    socks5: Option<Arc<agent_proxy::Socks5Controller>>,
    /// 审批模式运行时控制器（共享单例；`/api/approval-mode` 路由驱动，实时生效并
    /// 持久化到 `.gyre/approval-mode.state`）。恒存在——默认档 always-ask 始终可用。
    approval: Arc<agent_config::ApprovalModeController>,
}

impl SessionManager {
    /// 构造。
    #[must_use]
    pub fn new(
        config: Arc<Config>,
        http: reqwest::Client,
        cwd: Arc<std::path::PathBuf>,
        socks5: Option<Arc<agent_proxy::Socks5Controller>>,
    ) -> Self {
        // 审批模式持久化路径：`<cwd>/.gyre/approval-mode.state`（与 SOCKS5 开关同目录）。
        let approval_path = cwd.join(".gyre").join("approval-mode.state");
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            config,
            http,
            cwd,
            relay: agent_collab::Relay::new(),
            collab_hosts: Arc::new(Mutex::new(HashMap::new())),
            maintenance_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_gate: PauseGate::new(),
            socks5,
            approval: Arc::new(agent_config::ApprovalModeController::new(Some(
                approval_path,
            ))),
        }
    }

    /// 审批模式控制器句柄（`/api/approval-mode` 路由驱动；会话创建时取其生效值）。
    #[must_use]
    pub fn approval(&self) -> &Arc<agent_config::ApprovalModeController> {
        &self.approval
    }

    /// SOCKS5 代理控制器句柄（`/api/socks5` 路由驱动其开关；`None` = 未配置）。
    #[must_use]
    pub fn socks5(&self) -> &Option<Arc<agent_proxy::Socks5Controller>> {
        &self.socks5
    }

    /// 协同中继句柄（供路由直接 join/publish）。
    #[must_use]
    pub fn relay(&self) -> &agent_collab::Relay {
        &self.relay
    }

    /// 取房间的 host 机密（存在 = 本服务创建的房间，可裁决读写）。
    #[must_use]
    pub(crate) async fn collab_host(&self, room_id: &str) -> Option<HostSecret> {
        self.collab_hosts.lock().await.get(room_id).cloned()
    }

    /// 工作区根目录（agent 打开的目录）。
    #[must_use]
    pub fn cwd(&self) -> &std::path::Path {
        self.cwd.as_path()
    }

    /// 进程级暂停门句柄（`/api/pause` `/api/resume` 路由驱动其 `pause()` / `resume()`）。
    #[must_use]
    pub fn pause_gate(&self) -> &Arc<PauseGate> {
        &self.pause_gate
    }

    /// 创建/恢复/fork 会话。
    ///
    /// - `model`：模型别名（对应 `[[models]] alias`）；`None` 用默认模型。
    /// - `resume`：`Some(id)` 则恢复历史会话（复用 `<cwd>/.agent/sessions/<id>.jsonl`）。
    /// - `fork`：`Some(id)` 则把源会话复制为新 id 后继续（resume 与 fork 互斥，resume 优先）。
    /// - `mode_override`：`Some(code|architect|ask|debug)` 覆盖配置默认模式（修复此前 Web
    ///   模式切换不生效——模式仅在会话创建时确定，故切换模式即以新会话 resume 旧 id）。
    /// - `cwd`：会话工作目录覆盖（ACP `session/new` 的 `NewSessionRequest.cwd` 携带的项目根）。
    ///   `None` 回退到服务端启动时锁定的 cwd（CLI / Web 场景）。修复 ACP 路径下客户端 cwd 被
    ///   丢弃、Agent 始终在进程 cwd（容器内常为 `/workspace`）而非当前项目目录工作的问题。
    ///
    /// 会话与 CLI 共享同一目录，互可见。
    pub async fn create_session(
        &self,
        model: Option<&str>,
        resume: Option<&str>,
        fork: Option<&str>,
        mode_override: Option<&str>,
        cwd: Option<&std::path::Path>,
    ) -> Result<String, String> {
        // 解析会话工作目录：客户端 cwd 优先，规范为绝对路径；相对路径相对服务端 cwd 解析；
        // 缺省回退到服务端启动时锁定的 cwd。
        let resolved_cwd: std::path::PathBuf = match cwd {
            Some(c) if c.is_absolute() => c.canonicalize().unwrap_or_else(|_| c.to_path_buf()),
            Some(c) => self.cwd.join(c),
            None => self.cwd.as_path().to_path_buf(),
        };
        let effective_cwd: &std::path::Path = &resolved_cwd;
        // 解析会话 id：resume（复用）> fork（复制为新 id）> 新建。
        let store = agent_context::SessionStore::for_cwd(effective_cwd);
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
        if resume.is_some() && mode_override.is_none() && self.inner.lock().await.contains_key(&id)
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
            match agent_skills::SkillRegistry::native(effective_cwd.to_path_buf())
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
        let (agent, context, llm) = build_agent(
            &self.config,
            self.http.clone(),
            effective_cwd,
            &pending,
            &broadcast_tx,
            &supervisor,
            &id,
            model,
            mode_override,
            Arc::clone(&mcp),
            Arc::clone(&skill_catalog),
            Arc::clone(&self.pause_gate),
            &self.approval,
        )
        .await?;

        // 运行标志与会话共享同一份 Arc：驱动任务维护其真值，WS 重连时据此回推状态。
        let running = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // 多模态图片上传句柄表：会话级共享（upload 路由写、驱动任务读）。
        let uploads = Arc::new(Mutex::new(HashMap::new()));
        let deps = DriverDeps {
            agent: Arc::new(agent),
            tx: broadcast_tx.clone(),
            pending: Arc::clone(&pending),
            running: Arc::clone(&running),
            current_cancel: Arc::new(tokio::sync::Mutex::new(None)),
            context: Arc::clone(&context),
            uploads: Arc::clone(&uploads),
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
            context,
            llm,
            uploads,
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
    /// 多模态图片上传句柄表（`POST /upload` 写、`new_task` 的 `ImageRef` 读，消费即清）。
    uploads: Arc<Mutex<HashMap<String, (String, String)>>>,
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
                    // 已有任务运行中：把消息作为 steering 投递（移植 oh-my-pi——忙时 steer 而非拒绝）。
                    // Immediate 策略会尽快打断在途的可中断工具，在下一注入边界把消息投给模型。
                    let frame_text = text.clone();
                    let _steered = match &content {
                        Some(blocks) if !blocks.is_empty() => {
                            let mut contents: Vec<UserContent> = Vec::new();
                            if !text.is_empty() {
                                contents.push(UserContent::Text { text });
                            }
                            let uploads_snapshot = deps.uploads.lock().await.clone();
                            for b in blocks {
                                if let Some(uc) = b.to_user_content(&uploads_snapshot) {
                                    contents.push(uc);
                                }
                            }
                            deps.uploads.lock().await.clear();
                            deps.agent.steer(agent_core::AgentMessage::user(contents))
                        }
                        _ => deps.agent.steer(agent_core::AgentMessage::user_text(text)),
                    };
                    if let Err(e) = deps.tx.send(ServerFrame::Steered { text: frame_text }) {
                        tracing::warn!(?e, "广播 steering 帧失败");
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
                            // 锁一次 uploads 快照：把 ImageRef 句柄解析为真实 base64；
                            // 缺失句柄跳过并告警。解析后清空，避免无界增长。
                            let uploads_snapshot = d.uploads.lock().await.clone();
                            for b in &blocks {
                                match b.to_user_content(&uploads_snapshot) {
                                    Some(uc) => contents.push(uc),
                                    None => tracing::warn!(
                                        "多模态内容块解析为空（upload_id 可能已过期/未知），已跳过"
                                    ),
                                }
                            }
                            d.uploads.lock().await.clear();
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
                    if let Err(e) = d.tx.send(ServerFrame::StateChanged {
                        state: AgentState::Idle,
                    }) {
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
    // 进程级暂停门（共享单例；注入 Agent，由 `/api/pause` `/api/resume` 驱动）。
    pause_gate: Arc<PauseGate>,
    // 审批模式控制器（共享单例；取其生效值作会话初始档 + 注入原子供运行时切换）。
    approval: &Arc<agent_config::ApprovalModeController>,
) -> Result<(Agent, Arc<dyn agent_core::ContextManager>, SessionLlm), String> {
    // P2：模型 fallback 链（主 profile + `fallbacks` 引用依序展开；跨线协议族亦可）。
    let chain = config.resolve_chain(alias).map_err(|e| e.to_string())?;
    let profile = chain[0];
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
    // fallback 链模型 + key 轮换环（model id → key 列表；空 = 单 key 不轮换）。
    let fallback_models: Vec<agent_core::Model> = chain
        .iter()
        .skip(1)
        .map(|p| {
            agent_core::Model {
                id: p.id.clone(),
                provider: "openai-compatible".into(),
                api: p.api,
                max_input_tokens: p.effective_max_input_tokens(),
                max_output_tokens: p.max_output_tokens.unwrap_or(4096),
                supports_tools: true,
                supports_streaming: true,
                supports_thinking: config.agent.enable_thinking,
                extra_body: p.extra_body.clone(),
            }
        })
        .collect();
    let key_rings: std::collections::HashMap<String, Vec<String>> = chain
        .iter()
        .map(|p| (p.id.clone(), p.key_ring()))
        .collect();

    let mut registry = agent_llm::ProviderRegistry::new();
    for p in agent_llm::collect_providers(http) {
        registry.register(p);
    }
    // 环境变量 opt-in in-band 工具调用（GYRE_INBAND_TOOLS=1）。
    let provider: Arc<dyn LlmProvider> = agent_llm::wrap_inband_if(
        Arc::new(registry),
        std::env::var("GYRE_INBAND_TOOLS").ok().as_deref(),
    );
    let provider_ctx = ProviderCallContext {
        api_key: Some(api_key),
        base_url: Some(profile.base_url.clone()),
        max_in_flight: None,
    };
    // 供只读端点（/enhance）直接调用模型，无需经驱动任务。
    let session_llm = SessionLlm {
        provider: Arc::clone(&provider),
        ctx: provider_ctx.clone(),
        model: model.clone(),
    };
    let mode = mode_override
        .map(|m| match m.trim().to_ascii_lowercase().as_str() {
            "architect" => Mode::Architect,
            "ask" => Mode::Ask,
            "debug" => Mode::Debug,
            "plan" => Mode::Plan,
            _ => Mode::Code,
        })
        .unwrap_or(config.agent.mode);
    let prompts = Arc::new(agent_prompt::PromptCatalog::new());
    // 持久化上下文（与 CLI 共享 <cwd>/.agent/sessions/<id>.jsonl）：resume 时自动加载历史。
    let session_path = agent_context::SessionStore::for_cwd(cwd).path_for(session_id);
    let ctx =
        agent_context::PersistentContext::open(prompts.system_with_platform(mode), &session_path)
            .await
            .map_err(|e| e.to_string())?;
    ctx.set_summarizer(Box::new(
        agent_context::compaction::LlmSummaryProvider::new(
            Arc::clone(&provider),
            model.clone(),
            provider_ctx.clone(),
            config.compaction.remote_endpoint.clone(),
        ),
    ))
    .await;
    // Shake 归档落盘到 <cwd>/.gyre/artifacts，使被压缩的大块可经 read_file artifact:// 回读。
    ctx.set_shake_sink(Arc::new(agent_context::compaction::DirSink::new(
        cwd.join(".gyre").join("artifacts"),
    )))
    .await;
    let context: Arc<dyn agent_core::ContextManager> = Arc::new(ctx);
    let workspace = Arc::new(Workspace::new(cwd));
    // 审批策略（提前构造：TaskTool 子 Agent 需注入以尊重规则引擎的 Deny，修复审批旁路）。
    // 用运行时 mode（含 mode_override）覆盖 config 默认 mode，使 mode→审批联动生效
    // （code/debug 写类放行；ask 全只读；architect 仅 plans/ 下 markdown 可写），否则切换
    // 模式时审批门槛不跟随。
    let mut agent_cfg = config.agent.clone();
    agent_cfg.mode = mode;
    // 审批模式初始档取控制器生效值（覆盖优先，否则配置默认）；共享原子注入引擎后，
    // Web 开关的运行时切换对已建会话实时生效（无需重建 Agent / 会话）。
    agent_cfg.approval_mode = approval.effective(agent_cfg.approval_mode);
    let rules = RulesEngine::new(Arc::new(agent_cfg))
        .with_workspace_root(Some(workspace.root().to_path_buf()))
        .with_approval_override(approval.shared());
    let approval: Arc<dyn ApprovalPolicy> = Arc::new(WebApprovalPolicy::new(
        rules,
        broadcast_tx.clone(),
        Arc::clone(pending),
    ));
    // run_command 命令拦截规则（cat/grep/find/echo-redirect → 专用工具）；按 [agent.commands.interceptor] 开关。
    let intercept = if config.agent.commands.interceptor.enabled {
        agent_tools::intercept::default_compiled()
    } else {
        Vec::new()
    };
    // 输出最小化器（[agent.commands.minimizer] enabled/max_lines）。
    let minimizer = compiled_minimizer(&config);
    // 子 Agent 工具集（builtin + MCP，不含 task 以防递归）
    let mut sub_reg = agent_tools::builtin_tools(intercept.clone(), minimizer);
    if config.github.enabled {
        sub_reg = sub_reg.with(Box::new(agent_tools::GithubTool::new(
            config.github.allow_write,
        )));
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
    let (mut tool_registry, lsp_pool) =
        agent_tools::builtin_tools_with_pool(intercept, compiled_minimizer(&config));
    if config.github.enabled {
        tool_registry = tool_registry.with(Box::new(agent_tools::GithubTool::new(
            config.github.allow_write,
        )));
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
    // 长期记忆（可选；按 cwd 项目作用域，backend 可切换；server 简化装配，无心智模型配置）。
    let memory: Option<Arc<dyn agent_core::MemoryStore>> = if config.memory.enabled {
        match config.memory.backend {
            agent_config::MemoryBackend::Local => {
                Some(Arc::new(agent_memory::LocalMemoryStore::new(cwd)))
            }
            agent_config::MemoryBackend::Structured => Some(Arc::new(
                agent_memory::StructuredMemoryStore::new(cwd)
                    .with_embedder(agent_memory::default_embedder()),
            )),
        }
    } else {
        None
    };

    // 模型的输出 token 预算（来自 profile.max_output_tokens，回退 4096）须下发给
    // Agent 作为每轮请求的 max_tokens；否则 assemble 未设置会回落到硬编码 4096，
    // 长回复被中途截断（finish_reason=length）→ 误报「任务完成」。
    let max_output_tokens = model.max_output_tokens;
    let agent = assemble(
        Agent::builder(model)
            .steering()
            .fallbacks(fallback_models)
            .key_rings(key_rings),
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
        pause_gate,
    );
    Ok((agent, context, session_llm))
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
    pause_gate: Arc<PauseGate>,
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
        .context_files(context_files)
        .pause_gate(pause_gate)
        .ttsr(ttsr_for(config, &workspace_root));
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
        && (config.agent.tools.edit.format_on_write || config.agent.tools.edit.diagnostics_on_write)
    {
        builder.write_effect(std::sync::Arc::new(agent_tools::LspWriteEffect::new(
            workspace_root,
            std::sync::Arc::clone(&lsp_pool),
            config.agent.tools.edit.format_on_write,
            config.agent.tools.edit.diagnostics_on_write,
            config.agent.tools.edit.diagnostics_deduplicate,
        )) as std::sync::Arc<dyn agent_core::WriteEffect>)
    } else {
        builder
    };
    if let Some(m) = memory {
        builder.memory(m).build()
    } else {
        builder.build()
    }
}

/// 装配 TTSR 流规则协调器（与 CLI 装配一致）：发现 `<root>/.gyre/rules/*.md`，
/// 从配置构造输出最小化器（`[agent.commands.minimizer] enabled/max_lines`）。
/// 未启用时返回 [`agent_tools::disabled`]（apply 恒 None，零开销）。
#[must_use]
fn compiled_minimizer(config: &agent_config::Config) -> agent_tools::Minimizer {
    let m = &config.agent.commands.minimizer;
    if m.enabled {
        agent_tools::Minimizer::new(agent_tools::default_filters(), m.max_lines)
    } else {
        agent_tools::disabled()
    }
}

/// 缺省有规则即启用；`[ttsr] enabled = false` 或 `disabled_rules` 可关闭/过滤。
fn ttsr_for(config: &agent_config::Config, workspace_root: &std::path::Path) -> Option<std::sync::Arc<agent_ttsr::TtsrCoordinator>> {
    if !config.ttsr.enabled.unwrap_or(true) {
        return None;
    }
    let rules = agent_ttsr::discover_rules(&workspace_root.join(".gyre/rules"));
    if rules.is_empty() {
        return None;
    }
    Some(std::sync::Arc::new(agent_ttsr::TtsrCoordinator::new(
        agent_ttsr::TtsrConfig {
            disabled_rules: config.ttsr.disabled_rules.clone(),
            ..Default::default()
        },
        rules,
    )))
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

/// `POST /api/pause`：进程级冻结所有 agent loop（移植 oh-my-pi `AgentPauseGate`）。
///
/// 在途 provider 流与已启动工具跑完后，每个 loop 在下一安全点（provider 调用前 /
/// 工具批执行前）park。queued steering / followUp 保持排队，[`resume_handler`] 后正常
/// 投递。幂等：已是暂停态时 `already_paused=true`。
async fn pause_handler(State(state): State<SessionManager>) -> Response {
    // pause() 返回 false 表示已是暂停态（无操作）。
    let already_paused = !state.pause_gate().pause();
    Json(serde_json::json!({
        "paused": true,
        "already_paused": already_paused,
    }))
    .into_response()
}

/// `POST /api/resume`：解除进程级冻结，所有 parked loop 从原处继续（零丢失）。
async fn resume_handler(State(state): State<SessionManager>) -> Response {
    let paused_for = state.pause_gate().resume();
    Json(serde_json::json!({
        "paused": false,
        "was_paused": paused_for.is_some(),
        "paused_secs": paused_for.map(|d| d.as_secs_f64()),
    }))
    .into_response()
}

/// `GET /api/socks5`：SOCKS5 出站代理状态（`configured` / `enabled` / `host` / `port` /
/// `username` / `redacted`）。响应**不含密码**——任何字段都不携带 `SecretString`。
async fn socks5_status(
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    match &state.socks5 {
        Some(c) => Json(serde_json::json!({
            "configured": true,
            "enabled": c.enabled(),
            "host": c.host(),
            "port": c.port(),
            "username": c.username(),
            "redacted": c.redacted(),
        }))
        .into_response(),
        None => Json(serde_json::json!({
            "configured": false,
            "enabled": false,
            "redacted": "socks5://<未配置>",
        }))
        .into_response(),
    }
}

/// `POST /api/socks5` 请求体：`{ "enabled": bool }`。
#[derive(serde::Deserialize)]
struct Socks5SetBody {
    enabled: bool,
}

/// `POST /api/socks5`：运行时切换代理开关（实时生效 + 落盘持久化，下次启动保留）。
/// 未配置（host/port 缺失）→ 400 说明缺失项。
async fn socks5_set(
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
    axum::Json(body): axum::Json<Socks5SetBody>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    let Some(c) = &state.socks5 else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "SOCKS5 代理未配置（缺少 host/port；请在 config.toml [socks5] 段或 \
             --socks host:port 补齐）",
        )
            .into_response();
    };
    c.set_enabled(body.enabled);
    Json(serde_json::json!({
        "configured": true,
        "enabled": c.enabled(),
        "redacted": c.redacted(),
    }))
    .into_response()
}

/// `GET /api/approval-mode`：审批模式状态。
///
/// - `mode`：运行时覆盖（`"always-ask" | "write" | "yolo"`；`null` = 未覆盖，用配置默认）。
/// - `effective`：当前实际生效档（会话创建与 `decide` 均取此值）。
/// - `config_default`：配置 `[agent].approval_mode` 默认档（供前端标注「恢复默认」）。
async fn approval_mode_status(
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    let default = state.config.agent.approval_mode;
    Json(serde_json::json!({
        "configured": true,
        "mode": state.approval().current().map(mode_json),
        "effective": mode_json(state.approval().effective(default)),
        "config_default": mode_json(default),
    }))
    .into_response()
}

/// `POST /api/approval-mode` 请求体：`{ "mode": "always-ask" | "write" | "yolo" | null }`。
/// `null` 恢复配置默认（清覆盖 + 删除 sidecar）。
#[derive(serde::Deserialize)]
struct ApprovalModeSetBody {
    mode: Option<String>,
}

/// `POST /api/approval-mode`：运行时切换审批模式（实时生效 + 落盘持久化，下次启动保留）。
/// 非法档位 → 400。
async fn approval_mode_set(
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
    axum::Json(body): axum::Json<ApprovalModeSetBody>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    let mode = match body.mode.as_deref() {
        None => None,
        Some("always-ask") => Some(ApprovalMode::AlwaysAsk),
        Some("write") => Some(ApprovalMode::Write),
        Some("yolo") => Some(ApprovalMode::Yolo),
        Some(other) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("无效审批模式: {other:?}（可用: always-ask / write / yolo / null）"),
            )
                .into_response();
        }
    };
    state.approval().set(mode);
    let default = state.config.agent.approval_mode;
    Json(serde_json::json!({
        "configured": true,
        "mode": state.approval().current().map(mode_json),
        "effective": mode_json(state.approval().effective(default)),
        "config_default": mode_json(default),
    }))
    .into_response()
}

/// ApprovalMode → JSON 字符串（`"always-ask"` 等，与 serde kebab-case 一致）。
fn mode_json(mode: ApprovalMode) -> serde_json::Value {
    serde_json::Value::String(match mode {
        ApprovalMode::AlwaysAsk => "always-ask",
        ApprovalMode::Write => "write",
        ApprovalMode::Yolo => "yolo",
    }
    .to_string())
}

/// 构建 axum Router（静态前端内嵌于二进制，运行时无需 `web/` 目录）。
#[must_use]
pub fn app(state: SessionManager) -> Router {
    Router::new()
        .route("/api/sessions", get(create_session))
        .route("/api/sessions/list", get(list_sessions))
        .route(
            "/api/sessions/{id}",
            delete(delete_session).post(rename_session),
        )
        .route("/api/sessions/{id}/agents", get(list_agents))
        .route("/api/sessions/{id}/skills", get(list_skills))
        .route("/api/sessions/{id}/skill/{name}", get(skill_body))
        .route("/api/sessions/{id}/mcp", get(list_mcp))
        .route("/api/sessions/{id}/enhance", post(enhance_prompt))
        .route("/api/sessions/{id}/upload", post(upload_image))
        .route("/api/sessions/{id}/history", get(session_history))
        .route("/api/sessions/{id}/branches", get(session_branches))
        .route("/api/sessions/{id}/branches/switch", post(switch_branch))
        .route("/api/sessions/{id}/messages/{line}", delete(delete_message))
        .route("/api/commands", get(list_commands))
        .route("/api/models", get(list_models))
        .route("/api/stats", get(stats))
        .route("/api/stats/trend", get(stats_trend))
        .route("/api/pause", post(pause_handler))
        .route("/api/resume", post(resume_handler))
        .route("/api/socks5", get(socks5_status).post(socks5_set))
        .route(
            "/api/approval-mode",
            get(approval_mode_status).post(approval_mode_set),
        )
        .route("/api/workspace", get(workspace_info))
        .route("/api/fs", get(list_dir))
        .route("/api/file", get(read_file))
        .route("/ws/{id}", get(ws_handler))
        .route("/api/collab/room", get(new_collab_room))
        .route("/collab/{room_id}", get(collab_ws_handler))
        // 上传 / 多模态：允许较大请求体（base64 图片），其余端点仍受此上限保护。
        .layer(DefaultBodyLimit::max(WS_MAX_MESSAGE_SIZE))
        .with_state(state)
        .fallback(serve_embedded)
}

/// 运行时统计（本地可观测端点）。
///
/// 载荷在原有 `active_sessions` / `models_available`（设置页连接测试与 Inspector
/// 仍在读）基础上扩展为完整聚合：`sessions`（扫描会话数 / 消息总数）、`usage`
/// （token 与成本）、`tools`（工具调用/错误）、`top_models`（模型用量）、`daily`
/// （按日 token/成本）。聚合数据来自 [`collect_stats`] 对会话 JSONL 的受限扫描
/// （见 [`STATS_MAX_FILES`] / [`STATS_MAX_FILE_BYTES`]）。
async fn stats(
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    let store = agent_context::SessionStore::for_cwd(state.cwd());
    let agg = collect_stats(&store, None);
    Json(serde_json::json!({
        "active_sessions": state.active_session_count().await,
        "models_available": state.models().len(),
        "sessions": agg.sessions,
        "usage": agg.usage,
        "tools": agg.tools,
        "top_models": agg.top_models,
        "daily": agg.daily,
    }))
    .into_response()
}

/// `/api/stats/trend` 查询参数（`?days=14`）。
#[derive(Debug, Deserialize)]
struct TrendParams {
    #[serde(default)]
    token: Option<String>,
    /// 统计窗口天数（1..=90，默认 14）。
    #[serde(default)]
    days: Option<u64>,
}

/// 按天聚合端点：`/api/stats/trend?days=14`。
///
/// 与 `/api/stats` 的 `daily` 同源（同一 [`collect_stats`] 扫描路径），`days` 控制
/// 窗口：输出最近 `days` 天（含今天）逐日补齐的数组，缺数据的日期补零，保证前端
/// 柱状图连续。
async fn stats_trend(
    State(state): State<SessionManager>,
    axum::extract::Query(p): axum::extract::Query<TrendParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &p.token) {
        return resp;
    }
    let days = p.days.unwrap_or(14).clamp(1, 90);
    let window = recent_dates(days);
    let store = agent_context::SessionStore::for_cwd(state.cwd());
    let agg = collect_stats(&store, Some(&window));
    Json(serde_json::json!({
        "days": days,
        "daily": agg.daily,
    }))
    .into_response()
}

// ──────────────────────────────────────────────────────────────────────────────
// 统计聚合（本地可观测：扫描会话 JSONL）
// ──────────────────────────────────────────────────────────────────────────────

/// 会话统计扫描上限：最多扫描**最近** 50 个会话文件（[`SessionStore::list`] 按
/// mtime 倒序），会话超多时保证端点恒定有界——本地可观测不做全量扫描。
const STATS_MAX_FILES: usize = 50;
/// 单会话文件字节上限：超过 4 MiB 的文件跳过（巨型会话不阻塞统计）。
const STATS_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// `tools` 最多返回条数（按调用次数倒序）。
const STATS_MAX_TOOLS: usize = 50;
/// `top_models` 最多返回条数（按 token 用量倒序）。
const STATS_MAX_MODELS: usize = 10;

/// 会话统计摘要。
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
struct SessionsSummary {
    /// 本次统计扫描的会话文件数（受 [`STATS_MAX_FILES`] 上限约束；磁盘上会话
    /// 更多时该值小于实际总数，前端以脚注说明）。
    total: u64,
    /// 扫描文件中解析出的消息总数。
    total_messages: u64,
}

/// 单工具聚合。
#[derive(Debug, Clone, PartialEq, Serialize)]
struct ToolStat {
    /// 工具名。
    name: String,
    /// 调用次数（assistant 消息中的 ToolCall 块计数）。
    calls: u64,
    /// 错误次数（对应 tool_call_id 的 ToolResult::Error 计数）。
    errors: u64,
}

/// 单模型聚合。
#[derive(Debug, Clone, PartialEq, Serialize)]
struct ModelStat {
    /// 模型 ID。
    model: String,
    /// LLM 轮次（assistant 消息条数，每条 = 一次模型请求）。
    turns: u64,
    /// 累计输入 token。
    input_tokens: u64,
    /// 累计输出 token。
    output_tokens: u64,
}

/// 单日聚合（`date` 为 `YYYY-MM-DD`，UTC）。
#[derive(Debug, Clone, PartialEq, Serialize)]
struct DailyStat {
    date: String,
    /// 输入 + 输出 token。
    tokens: u64,
    /// 预估成本（美元）。
    cost: f64,
}

/// 统计聚合结果（`/api/stats` 载荷主体）。
#[derive(Debug, Clone, PartialEq, Serialize)]
struct StatsAgg {
    sessions: SessionsSummary,
    usage: Usage,
    tools: Vec<ToolStat>,
    top_models: Vec<ModelStat>,
    daily: Vec<DailyStat>,
}

/// 聚合中间态（按会话文件流式累加）。
#[derive(Default)]
struct StatsAccumulator {
    sessions: u64,
    messages: u64,
    usage: Usage,
    tools: HashMap<String, ToolAgg>,
    models: HashMap<String, ModelAgg>,
    daily: HashMap<String, DayAgg>,
}

/// 单工具累加值。
#[derive(Default)]
struct ToolAgg {
    calls: u64,
    errors: u64,
}

/// 单模型累加值。
#[derive(Default)]
struct ModelAgg {
    turns: u64,
    input_tokens: u64,
    output_tokens: u64,
}

/// 单日累加值。
#[derive(Default)]
struct DayAgg {
    tokens: u64,
    cost: f64,
}

impl StatsAccumulator {
    /// 收敛为最终聚合结果。
    ///
    /// `window` 为 `Some` 时 `daily` 按窗口逐日补齐（趋势端点，缺数据补零）；
    /// 为 `None` 时输出全部有数据的日期（总览端点，升序）。
    fn finish(self, window: Option<&[String]>) -> StatsAgg {
        let mut tools: Vec<ToolStat> = self
            .tools
            .into_iter()
            .map(|(name, a)| ToolStat {
                name,
                calls: a.calls,
                errors: a.errors,
            })
            .collect();
        tools.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.name.cmp(&b.name)));
        tools.truncate(STATS_MAX_TOOLS);

        let mut top_models: Vec<ModelStat> = self
            .models
            .into_iter()
            .map(|(model, m)| ModelStat {
                model,
                turns: m.turns,
                input_tokens: m.input_tokens,
                output_tokens: m.output_tokens,
            })
            .collect();
        top_models.sort_by(|a, b| {
            (b.input_tokens + b.output_tokens)
                .cmp(&(a.input_tokens + a.output_tokens))
                .then_with(|| a.model.cmp(&b.model))
        });
        top_models.truncate(STATS_MAX_MODELS);

        let daily = match window {
            Some(dates) => dates
                .iter()
                .map(|date| {
                    let d = self.daily.get(date);
                    DailyStat {
                        date: date.clone(),
                        tokens: d.map_or(0, |x| x.tokens),
                        cost: d.map_or(0.0, |x| x.cost),
                    }
                })
                .collect(),
            None => {
                let mut v: Vec<DailyStat> = self
                    .daily
                    .into_iter()
                    .map(|(date, d)| DailyStat {
                        date,
                        tokens: d.tokens,
                        cost: d.cost,
                    })
                    .collect();
                v.sort_by(|a, b| a.date.cmp(&b.date));
                v
            }
        };

        StatsAgg {
            sessions: SessionsSummary {
                total: self.sessions,
                total_messages: self.messages,
            },
            usage: self.usage,
            tools,
            top_models,
            daily,
        }
    }
}

/// 扫描会话存储并聚合（受限：最近 [`STATS_MAX_FILES`] 个文件，单文件
/// ≤ [`STATS_MAX_FILE_BYTES`]，超限跳过）。
///
/// `window`：`Some(日期列表)` 时 `daily` 按窗口补齐（趋势端点）；`None` 时输出
/// 全部有数据的日期（总览端点）。
fn collect_stats(store: &agent_context::SessionStore, window: Option<&[String]>) -> StatsAgg {
    let mut acc = StatsAccumulator::default();
    for info in store.list().into_iter().take(STATS_MAX_FILES) {
        if info.bytes > STATS_MAX_FILE_BYTES {
            continue;
        }
        let path = store.path_for(&info.id);
        let lines = read_jsonl_lines(&path);
        if lines.is_empty() {
            // 空文件仍是磁盘上真实存在的会话：计入会话数，但不产生消息/用量。
            acc.sessions += 1;
            continue;
        }
        // 归属日期用会话文件的最后修改日期：JSONL 消息本身不携带时间戳，
        // 按文件级日期归天是本地可观测下的务实口径。
        let date = mtime_utc_date(info.mtime);
        aggregate_file_lines(&mut acc, &lines, &date);
    }
    acc.finish(window)
}

/// 读取会话 JSONL 的全部非空行（文件缺失 → 空）。
fn read_jsonl_lines(path: &std::path::Path) -> Vec<String> {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    std::io::BufReader::new(file)
        .lines()
        .flatten()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// 聚合一个会话文件的全部行（纯函数：给定行集合 + 归属日期 → 累加进 `acc`）。
///
/// 与 [`read_history`] 同源解析（[`parse_history_line`]：新会话树 / 旧线性两种格式
/// 都兼容）；无法解析的行跳过、不计入消息数。`tool_call_id → 工具名` 映射按文件
/// 独立维护——不同会话/分支可能复用同一 tool_call_id。
fn aggregate_file_lines(acc: &mut StatsAccumulator, lines: &[String], date: &str) {
    acc.sessions += 1;
    let mut call_names: HashMap<String, String> = HashMap::new();
    for line in lines {
        let Some(msg) = parse_history_line(line) else {
            continue;
        };
        acc.messages += 1;
        match &msg {
            agent_core::AgentMessage::Assistant(a) => {
                // token 用量与成本：每条 assistant 消息携带 provider 回填的 usage。
                acc.usage.add(&a.usage);
                let tokens = a.usage.total_tokens();
                let cost = a.usage.cost_usd;
                // 工具调用：直接取自 assistant 消息的 ToolCall 内容块。
                for (id, name, _args) in a.tool_calls() {
                    call_names.insert(id.to_string(), name.to_string());
                    acc.tools.entry(name.to_string()).or_default().calls += 1;
                }
                // 模型聚合：每条 assistant 消息 = 一次模型请求（一轮）。
                let m = acc.models.entry(a.model.clone()).or_default();
                m.turns += 1;
                m.input_tokens += a.usage.input_tokens;
                m.output_tokens += a.usage.output_tokens;
                // 按日聚合（归属会话文件的 mtime 日期）。
                let d = acc.daily.entry(date.to_string()).or_default();
                d.tokens += tokens;
                d.cost += cost;
            }
            agent_core::AgentMessage::ToolResult(tr) => {
                // 错误计数：按 tool_call_id 反查该文件内对应的工具名。
                if let Some(name) = call_names.get(&tr.tool_call_id) {
                    if matches!(tr.result, ToolResult::Error { .. }) {
                        acc.tools.entry(name.clone()).or_default().errors += 1;
                    }
                }
            }
            _ => {}
        }
    }
}

/// 把 [`std::time::SystemTime`] 转为 UTC 日期字符串 `YYYY-MM-DD`（统计归天用）。
fn mtime_utc_date(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    unix_to_utc_date(secs)
}

/// UNIX 纪元秒 → UTC 日期 `YYYY-MM-DD`（自实现，避免为本地可观测端点引入
/// chrono 依赖）。采用 Howard Hinnant 的 civil_from_days 算法。
fn unix_to_utc_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// 自 1970-01-01 起的天数 → (年, 月, 日)。
///
/// 算法来源：<https://howardhinnant.github.io/date_algorithms.html#civil_from_days>。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 最近 `days` 个 UTC 日期（含今天，升序）——趋势窗口。
fn recent_dates(days: u64) -> Vec<String> {
    let today = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        .div_euclid(86_400);
    (0..days)
        .rev()
        .map(|offset| unix_to_utc_date((today - offset as i64) * 86_400))
        .collect()
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
            // Web 场景：cwd 即服务端启动目录（已由 SessionManager 持有），无需覆盖。
            None,
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
                Ok(body) => {
                    Json(serde_json::json!({ "name": skill.name, "body": body })).into_response()
                }
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

// ── 提示增强与上下文感知建议（Web ↔ CLI 同源，逻辑在 agent_prompt）──────────────

/// `/enhance` 请求体。
#[derive(Debug, Deserialize)]
struct EnhanceBody {
    /// 待增强的草稿。
    draft: String,
}

/// `POST /api/sessions/{id}/enhance` → Roo-Code 风格的 LLM 草稿增强（镜像 CLI `/enhance`）。
///
/// body `{ "draft": "..." }`，返回 `{ "text": "..." }`（仅增强后的 prompt 文本）。
/// 草稿为空返回 400。
async fn enhance_prompt(
    State(state): State<SessionManager>,
    Path(id): Path<String>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
    axum::Json(body): axum::Json<EnhanceBody>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    if !is_safe_session_id(&id) {
        return (StatusCode::BAD_REQUEST, "非法会话 id").into_response();
    }
    let Some(session) = state.get(&id).await else {
        return (StatusCode::NOT_FOUND, "会话不存在").into_response();
    };
    let draft = body.draft.trim();
    if draft.is_empty() {
        return (StatusCode::BAD_REQUEST, "草稿为空").into_response();
    }
    match agent_prompt::enhance::enhance_collect(
        draft,
        session.llm.provider.as_ref(),
        &session.llm.ctx,
        &session.llm.model,
    )
    .await
    {
        Ok(text) => Json(serde_json::json!({ "text": text })).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("模型调用失败: {e}")).into_response(),
    }
}

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
    /// Collab write token（仅 `/collab/{room_id}` 使用；缺省 = 只读 view 链接）。
    #[serde(default)]
    wt: Option<String>,
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
    /// 会话绑定（仅 `/api/collab/room` 使用）：创建房间时绑定 agent 会话，
    /// guest 连接时由 host 把会话历史以快照块下发（transcript 分页传输）。
    #[serde(default)]
    sid: Option<String>,
}

/// 会话 id 安全校验：仅允许 `[A-Za-z0-9_-]`，防路径穿越（`/` `\` `..` 等）。
fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 解析一行会话 JSONL：优先按 [`agent_core::SessionNode`]（新会话树格式）解析取其
/// `message`；失败则回退为裸 [`agent_core::AgentMessage`]（旧线性日志格式）。
///
/// 使历史读取对新旧两种持久化格式都兼容。
fn parse_history_line(line: &str) -> Option<agent_core::AgentMessage> {
    if let Ok(node) = serde_json::from_str::<agent_core::SessionNode>(line) {
        return Some(node.message);
    }
    serde_json::from_str::<agent_core::AgentMessage>(line).ok()
}

/// 会话历史快照文本上限（字符）。
///
/// guest 连接时由 host 全量下发；超限截断保留**尾部（最新）**并加省略标记，
/// 避免超大历史压垮 WS 流（48 KiB/块 × 数百块仍是可控的，但无限会话不设防）。
const TRANSCRIPT_MAX_CHARS: usize = 4 * 1024 * 1024;

/// 把一条 [`agent_core::AgentMessage`] 渲染为 guest 可读的一行（多行）文本。
///
/// 内部机制消息（`SoftRequirement`）与空文本返回空串（调用方跳过）。
fn render_transcript_message(msg: &agent_core::AgentMessage) -> String {
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
            if t.is_empty() {
                String::new()
            } else {
                format!("[user] {t}")
            }
        }
        agent_core::AgentMessage::Assistant(a) => {
            let mut out = Vec::new();
            for block in &a.content {
                match block {
                    ContentBlock::Thinking { text, .. } if !text.trim().is_empty() => {
                        out.push(format!("[thinking] {}", text.trim()));
                    }
                    ContentBlock::Text { text } if !text.trim().is_empty() => {
                        out.push(format!("[assistant] {}", text.trim()));
                    }
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => {
                        let args = preview_text(&arguments.to_string(), 120);
                        out.push(format!("[tool] {name} {args}"));
                    }
                    _ => {}
                }
            }
            out.join("\n")
        }
        agent_core::AgentMessage::ToolResult(t) => {
            let text = t.result.to_llm_text();
            let text = text.trim();
            if text.is_empty() {
                String::new()
            } else {
                format!("[tool result] {}", preview_text(text, 2000))
            }
        }
        agent_core::AgentMessage::Status(s) => {
            let t = s.text.trim();
            if t.is_empty() {
                String::new()
            } else {
                format!("[status] {t}")
            }
        }
        agent_core::AgentMessage::Ask(a) => {
            let t = a.prompt.trim();
            if t.is_empty() {
                String::new()
            } else {
                format!("[ask] {t}")
            }
        }
        agent_core::AgentMessage::SoftRequirement(_) => String::new(),
    }
}

/// 把会话 JSONL 渲染为可读快照文本（guest transcript 分页传输的载荷）。
///
/// 与 [`read_history`] 同源（同一解析路径，新旧持久化格式兼容）；
/// 超限时保留尾部最新内容并加省略标记。文件缺失/无可用消息 → `None`。
fn render_session_transcript(path: &std::path::Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut parts: Vec<String> = Vec::new();
    for line in std::io::BufReader::new(file).lines().flatten() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(msg) = parse_history_line(trimmed) else {
            continue;
        };
        let rendered = render_transcript_message(&msg);
        if !rendered.is_empty() {
            parts.push(rendered);
        }
    }
    if parts.is_empty() {
        return None;
    }
    let total: usize = parts.iter().map(|p| p.len() + 1).sum();
    if total <= TRANSCRIPT_MAX_CHARS {
        return Some(parts.join("\n"));
    }
    // 超限：从尾部累积保留最新内容，头部省略。
    let mut kept: Vec<&str> = Vec::new();
    let mut len = 0usize;
    for p in parts.iter().rev() {
        if len + p.len() + 1 > TRANSCRIPT_MAX_CHARS {
            break;
        }
        len += p.len() + 1;
        kept.push(p.as_str());
    }
    kept.reverse();
    let dropped = parts.len() - kept.len();
    let mut out = format!("…（较早 {dropped} 条已省略，仅显示最近内容）\n");
    out.push_str(&kept.join("\n"));
    Some(out)
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
        let Some(msg) = parse_history_line(trimmed) else {
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
///
/// 每条历史项附带 `line` 字段：**该消息在内存日志中的索引**（每条成功解析的
/// `AgentMessage` 占一个索引，含 tool/status 等不展示的消息）。前端据此定位删除
/// 目标行，与 [`agent_core::ContextManager::delete_message_at`] 的索引同源。
fn read_history(path: &std::path::Path) -> Vec<serde_json::Value> {
    use std::io::BufRead;
    let mut out = Vec::new();
    let Some(file) = std::fs::File::open(path).ok() else {
        return out;
    };
    let mut msg_index = 0usize;
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(msg) = parse_history_line(trimmed) else {
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
                    out.push(serde_json::json!({ "kind": "user", "text": t, "line": msg_index }));
                }
            }
            agent_core::AgentMessage::Assistant(a) => {
                // 思考内容（reasoning）：通常先于正文出现，前端以可折叠「思考」块展示，
                // 与实时流的 ThinkingDelta 行为一致。思考与正文同属一条 assistant 消息，
                // 故共享同一 `line` 索引（删除任一会移除整条 assistant 轮次）。
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
                    out.push(
                        serde_json::json!({ "kind": "thinking", "text": tk, "line": msg_index }),
                    );
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
                    out.push(
                        serde_json::json!({ "kind": "assistant", "text": t, "line": msg_index }),
                    );
                }
            }
            _ => {}
        }
        msg_index += 1;
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

/// `GET /api/sessions/{id}/branches` → 会话分支树（节点 id / parent_id / 角色 / 预览 +
/// 活跃叶子 + 叶子列表）。前端渲染分支视图、做分支切换用。
///
/// 直接读 JSONL 文件（活跃与非活跃会话皆可用），兼容旧线性日志（迁移为单链树）。
async fn session_branches(
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
    let tree = read_branch_tree(&path);
    Json(tree).into_response()
}

/// `POST /api/sessions/{id}/branches/switch` → 切换活跃叶子（续写点）。
///
/// - 活跃会话：经内存上下文切换（同步 sidecar，立即影响下一轮）。
/// - 非活跃会话：仅写 sidecar（下次 open 生效）。
/// - 任务运行中：409（避免与并发 append 竞争）。
#[derive(Debug, Deserialize)]
struct SwitchBranchBody {
    #[serde(default)]
    token: Option<String>,
    /// 目标叶子节点 id。
    leaf_id: String,
    /// 是否在切换时注入被离开分支的 handoff 摘要（默认 false）。
    #[serde(default)]
    handoff: bool,
}

async fn switch_branch(
    State(state): State<SessionManager>,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<SwitchBranchBody>,
) -> Response {
    if let Err(resp) = check_auth(&state, &body.token) {
        return resp;
    }
    if !is_safe_session_id(&id) || body.leaf_id.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "非法会话 id 或 leaf_id").into_response();
    }
    let leaf_id = body.leaf_id.trim().to_string();
    match state.get(&id).await {
        Some(session) => {
            if session.running.load(std::sync::atomic::Ordering::SeqCst) {
                return (StatusCode::CONFLICT, "任务运行中，无法切换分支").into_response();
            }
            let ok = if body.handoff {
                match session.context.switch_branch_with_handoff(&leaf_id).await {
                    Ok(b) => b,
                    Err(e) => {
                        return (StatusCode::INTERNAL_SERVER_ERROR, format!("切换失败: {e}"))
                            .into_response();
                    }
                }
            } else {
                session.context.set_active_leaf(&leaf_id).await
            };
            if ok {
                Json(serde_json::json!({ "ok": true, "active_leaf": leaf_id })).into_response()
            } else {
                (StatusCode::NOT_FOUND, "目标叶子不存在").into_response()
            }
        }
        None => {
            // 非活跃会话：校验 leaf_id 存在并写 sidecar。
            let store = agent_context::SessionStore::for_cwd(state.cwd());
            let path = store.path_for(&id);
            if !path.exists() {
                return (StatusCode::NOT_FOUND, "会话不存在").into_response();
            }
            let tree = read_branch_tree(&path);
            let exists = tree["nodes"].as_array().map_or(false, |a| {
                a.iter().any(|n| n["id"].as_str() == Some(leaf_id.as_str()))
            });
            if !exists {
                return (StatusCode::NOT_FOUND, "目标叶子不存在").into_response();
            }
            let sidecar = path.with_extension("leaf");
            if let Err(e) = std::fs::write(&sidecar, leaf_id.as_bytes()) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("写入活跃叶子失败: {e}"),
                )
                    .into_response();
            }
            Json(serde_json::json!({ "ok": true, "active_leaf": leaf_id })).into_response()
        }
    }
}

/// 读取会话分支树（JSON 数组节点 + 活跃叶子 + 叶子列表）。
///
/// 兼容旧线性日志：裸 `AgentMessage` 行被迁移为单链树节点（`legacy-{i}`）。
fn read_branch_tree(path: &std::path::Path) -> serde_json::Value {
    use std::io::BufRead;
    let mut nodes: Vec<agent_core::SessionNode> = Vec::new();
    let mut legacy: Vec<agent_core::AgentMessage> = Vec::new();
    if let Ok(file) = std::fs::File::open(path) {
        for line in std::io::BufReader::new(file).lines().flatten() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if let Ok(n) = serde_json::from_str::<agent_core::SessionNode>(t) {
                if !legacy.is_empty() {
                    nodes.extend(agent_context::wrap_linear_as_nodes(&legacy));
                    legacy.clear();
                }
                nodes.push(n);
            } else if let Ok(m) = serde_json::from_str::<agent_core::AgentMessage>(t) {
                legacy.push(m);
            }
        }
    }
    if !legacy.is_empty() {
        nodes.extend(agent_context::wrap_linear_as_nodes(&legacy));
    }

    let leaves_owned = agent_context::leaves(&nodes);
    let leaves: Vec<&str> = leaves_owned.iter().map(String::as_str).collect();
    let sidecar = path.with_extension("leaf");
    let active_leaf = std::fs::read_to_string(&sidecar)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| nodes.last().map(|n| n.id.clone()));

    let nodes_json: Vec<serde_json::Value> = nodes
        .iter()
        .map(|n| {
            let (role, preview) = node_role_preview(&n.message);
            serde_json::json!({
                "id": n.id,
                "parent_id": n.parent_id,
                "role": role,
                "preview": preview,
            })
        })
        .collect();
    serde_json::json!({
        "nodes": nodes_json,
        "active_leaf": active_leaf,
        "leaves": leaves,
    })
}

/// 节点消息的角色 + 短预览（分支树展示用）。
fn node_role_preview(msg: &agent_core::AgentMessage) -> (&'static str, String) {
    match msg {
        agent_core::AgentMessage::User(u) => {
            let t: String = u
                .content
                .iter()
                .filter_map(|c| match c {
                    agent_core::UserContent::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            ("user", preview_text(&t, 120))
        }
        agent_core::AgentMessage::Assistant(a) => {
            let t: String = a
                .content
                .iter()
                .filter_map(|b| match b {
                    agent_core::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            ("assistant", preview_text(&t, 120))
        }
        agent_core::AgentMessage::ToolResult(t) => {
            ("tool", preview_text(&t.result.to_llm_text(), 120))
        }
        agent_core::AgentMessage::Status(s) => ("status", preview_text(&s.text, 120)),
        agent_core::AgentMessage::Ask(a) => ("ask", preview_text(&a.prompt, 120)),
        agent_core::AgentMessage::SoftRequirement(_) => ("soft_requirement", String::new()),
    }
}

/// `DELETE /api/sessions/{id}/messages/{line}` → 删除指定索引处的单条消息（含孤立 tool 结果）。
///
/// - **活跃会话**：在内存上下文就地删除并原子重写 JSONL，下一轮即生效。
/// - **非活跃会话**：直接离线改写 JSONL 文件（[`agent_context::delete_message_in_file`]）。
///
/// `line` 为消息在日志中的索引（与 `GET /history` 返回的 `line` 同源）。
/// 任务运行中返回 409，避免与并发 append 竞争；索引越界返回 404。
async fn delete_message(
    State(state): State<SessionManager>,
    Path((id, line)): Path<(String, usize)>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    if !is_safe_session_id(&id) {
        return (StatusCode::BAD_REQUEST, "非法会话 id").into_response();
    }
    let removed: usize = match state.get(&id).await {
        // 活跃会话：内存上下文 + JSONL 一致删除。
        Some(session) => {
            if session.running.load(std::sync::atomic::Ordering::SeqCst) {
                return (StatusCode::CONFLICT, "任务运行中，无法删除消息").into_response();
            }
            match session.context.delete_message_at(line).await {
                Ok(n) => n,
                Err(e) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, format!("删除失败: {e}"))
                        .into_response();
                }
            }
        }
        // 非活跃会话：离线改写 JSONL 文件。
        None => {
            let store = agent_context::SessionStore::for_cwd(state.cwd());
            let path = store.path_for(&id);
            if !path.exists() {
                return (StatusCode::NOT_FOUND, "会话不存在").into_response();
            }
            match agent_context::delete_message_in_file(&path, line).await {
                Ok(n) => n,
                Err(e) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, format!("删除失败: {e}"))
                        .into_response();
                }
            }
        }
    };
    if removed == 0 {
        return (StatusCode::NOT_FOUND, "消息行号越界").into_response();
    }
    Json(serde_json::json!({ "ok": true, "removed": removed })).into_response()
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
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("删除失败: {e}")).into_response(),
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

#[derive(Debug, Deserialize)]
struct UploadBody {
    /// MIME 类型（image/png|jpeg|gif|webp）。
    mime: String,
    /// base64 数据（不含 `data:` 前缀）。
    data: String,
}

/// `POST /api/sessions/{id}/upload` → 上传单张图片（base64），返回 `upload_id` 句柄。
///
/// 前端拿到句柄后以 `ContentInput::ImageRef` 经 WS 发送，避免在控制信道传输大体积
/// base64（解决 WS `max_message_size` 过小导致图片帧被拒收、前端无响应的问题）。
/// 句柄在驱动任务解析 `new_task` 时消费即清空，不长期驻留。
async fn upload_image(
    State(state): State<SessionManager>,
    Path(id): Path<String>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
    axum::Json(body): axum::Json<UploadBody>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    let Some(session) = state.get(&id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    const ALLOWED_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];
    if !ALLOWED_MIMES.contains(&body.mime.as_str()) {
        return (StatusCode::BAD_REQUEST, "不支持的图片 MIME 类型").into_response();
    }
    // 以 base64 文本长度近似解码后大小（≈ len·4/3），避免引入 base64 解码做精确校验；
    // 前端上传前已按解码字节硬限拦截，此处为防御性二次校验。
    if body.data.len() > MAX_UPLOAD_IMAGE_BYTES * 4 / 3 + 4 {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            "图片过大（解码后超过 10MiB 上限）",
        )
            .into_response();
    }
    let upload_id = uuid::Uuid::new_v4().to_string();
    session
        .uploads
        .lock()
        .await
        .insert(upload_id.clone(), (body.mime, body.data));
    Json(serde_json::json!({ "upload_id": upload_id })).into_response()
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
    ws.max_message_size(WS_MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_socket(socket, session))
}

async fn handle_socket(socket: axum::extract::ws::WebSocket, session: Arc<Session>) {
    let (mut sink, mut input) = socket.split();
    let mut out_rx = session.broadcast.subscribe();

    // 重连补帧：若该会话正有任务运行，先给本连接回推一帧 Running 态，使重连前端
    // 立即反映真实进度（而非误判 no_task、允许发起新任务而被驱动任务拒绝）。
    if session.running.load(std::sync::atomic::Ordering::SeqCst) {
        if let Ok(text) = serde_json::to_string(&ServerFrame::StateChanged {
            state: AgentState::Running,
        }) {
            let _ = sink
                .send(axum::extract::ws::Message::Text(text.into()))
                .await;
        }
    }

    // 用量快照回放：每次 WS 连接建立时下发活跃分支累计用量（SET 语义，前端整体替换）。
    // - 切换会话：前端 clear() 已清零，快照恢复为该会话的历史累计，不再丢失。
    // - 重连 / 切换模式：前端累计态可能滞后，以服务端持久化真相覆盖（权威对账）。
    // 与增量 `Usage`（addUsage 累加）区分，避免重连时对保留态重复累加。
    let acc = session.context.accumulated_usage();
    if let Ok(text) = serde_json::to_string(&ServerFrame::UsageSnapshot(acc)) {
        let _ = sink
            .send(axum::extract::ws::Message::Text(text.into()))
            .await;
    }

    // 广播 → 客户端
    let send_task = tokio::spawn(async move {
        // 应用层心跳：每 20 秒无业务帧时下发一帧 Heartbeat，保活反向代理读超时 +
        // 刷新前端心跳看门狗（慢速 LLM 长时间无 token 输出时尤为关键，避免误判静默
        // 终止 / 被中间代理切连而触发自动重连）。间隔远小于前端 60s 看门狗阈值，留足余量。
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(20));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        heartbeat.tick().await; // 跳过 interval 立即触发的首 tick，首帧心跳延迟到 20s 后
        loop {
            tokio::select! {
                biased;
                frame = out_rx.recv() => {
                    let Ok(frame) = frame else { break };
                    let Ok(text) = serde_json::to_string(&frame) else { continue };
                    if sink
                        .send(axum::extract::ws::Message::Text(text.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                _ = heartbeat.tick() => {
                    let Ok(text) = serde_json::to_string(&ServerFrame::Heartbeat) else { continue };
                    if sink
                        .send(axum::extract::ws::Message::Text(text.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
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

/// host 侧房间机密：房间密钥 + 规范 write token（[`new_collab_room`] 创建时登记）。
///
/// 本服务即 host 进程：密钥由服务本地生成、从不外发，仅用于向 guest 单播
/// host 裁决帧（Welcome read_only）。中继保持密钥盲视——host 只密封自己的帧，
/// 永不解封他人帧。
#[derive(Clone)]
struct HostSecret {
    key: agent_collab::RoomKey,
    write_token: String,
    /// 绑定的 agent 会话 id（`/api/collab/room?sid=` 传入；缺省 = 纯聊天房间，不下发快照）。
    session_id: Option<String>,
}

/// `GET /api/collab/room` → 生成房间密钥，返回派生 `room_id` 与 base64url 密钥片段。
///
/// 本服务作为 host 进程在内存保留密钥与规范 write token（[`SessionManager::collab_hosts`]），
/// 供 WS 桥向 guest 单播 read_only 裁决；密钥本地生成、从不外发，中继仍只路由密封字节。
async fn new_collab_room(
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    let key = agent_collab::generate_room_key();
    let room_id = agent_collab::room_id(&key);
    let write_token = agent_collab::generate_write_token();
    state.relay().set_write_token(&room_id, write_token.clone()).await;
    // 可选会话绑定：guest 连接时由 host 下发会话历史快照块。
    let session_id = auth.sid.filter(|s| is_safe_session_id(s));
    state
        .collab_hosts
        .lock()
        .await
        .insert(
            room_id.clone(),
            HostSecret {
                key,
                write_token: write_token.clone(),
                session_id,
            },
        );
    Json(serde_json::json!({
        "room_id": room_id,
        "key": agent_collab::encode_room_key(&key),
        // 可写分享链接须携带（URL query `wt`）；不含此令牌的链接 = 只读 view。
        "write_token": write_token,
        "ws_url": format!("/collab/{room_id}"),
    }))
    .into_response()
}

/// `GET /collab/{room_id}` → WebSocket 升级（协同中继桥）；普通浏览器 GET 返回 guest 页面。
///
/// guest 页面（单文件，无外部依赖）：从链接 `#` 片段取房间密钥，WebCrypto AES-GCM
/// 解封/密封帧，断线指数退避重连。密钥不发给服务器（片段不出现在 HTTP 请求中）。
async fn collab_ws_handler(
    ws: OptionalWsUpgrade,
    Path(room_id): Path<String>,
    State(state): State<SessionManager>,
    axum::extract::Query(auth): axum::extract::Query<SessionParams>,
) -> Response {
    if let Err(resp) = check_auth(&state, &auth.token) {
        return resp;
    }
    // 非 WS 请求（浏览器 GET，无 Upgrade 头）→ 渲染 guest 页面；否则升级为 WS 桥。
    let Some(ws) = ws.0 else {
        return (
            axum::http::StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/html; charset=utf-8",
            )],
            COLLAB_GUEST_PAGE,
        )
            .into_response();
    };
    let wt = auth.wt.clone();
    let host = state.collab_host(&room_id).await;
    let cwd = state.cwd().to_path_buf();
    ws.max_message_size(256 * 1024)
        .on_upgrade(move |socket| {
            // `wt` = write token（房间创建时随可写链接分发的 32 hex 字符）；
            // 缺省 = 只读 view 链接，publish 将被中继拒绝。
            collab_relay(socket, room_id, state.relay.clone(), wt, host, cwd)
        })
}

/// 可选 WebSocket 升级 extractor：非 WS 请求返回 `None`（浏览器 GET → guest 页面）。
///
/// axum 未导出 `WebSocketUpgrade` 的 rejection 类型（测试私有），无法用 `Result`
/// extractor 区分「非 WS」与「握手不完整」；此 wrapper 自行判定 Upgrade 头，
/// 缺失时短路为 `None`，完整握手仍委托 axum 原生 extractor（拒绝条件一致）。
struct OptionalWsUpgrade(Option<axum::extract::ws::WebSocketUpgrade>);

impl<S> axum::extract::FromRequestParts<S> for OptionalWsUpgrade
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let is_ws = axum::http::header::HeaderMap::get(
            &parts.headers,
            axum::http::header::UPGRADE,
        )
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
        if !is_ws {
            return Ok(Self(None));
        }
        // 握手不完整（缺 Sec-WebSocket-Key 等）→ 降级为 guest 页（无副作用，客户端自会报错）。
        let Ok(ws) = axum::extract::ws::WebSocketUpgrade::from_request_parts(parts, state).await
        else {
            return Ok(Self(None));
        };
        Ok(Self(Some(ws)))
    }
}

/// 协同 guest 页面（浏览器端到端加密视图）。
const COLLAB_GUEST_PAGE: &str = include_str!("collab_guest.html");

/// host 侧 `read_only` 裁决：按连接 `wt` 对照房间规范令牌，密封 Welcome 握手帧。
///
/// 裁决与中继发布校验同源（[`Relay::set_write_token`] 登记的即此规范令牌），
/// 故 UI 裁决与中继实际权限永远一致：错误/缺失 wt → `read_only: true`。
/// 桥不解封任何帧——只 seal 自己的裁决帧（中继保持密钥盲视）。
fn seal_host_welcome(
    host: &HostSecret,
    wt: Option<&str>,
    ts: u64,
    entry_count: usize,
) -> Result<Vec<u8>, agent_collab::CollabError> {
    let writable = wt.is_some_and(|t| t == host.write_token);
    agent_collab::seal(
        &host.key,
        &agent_collab::WireFrame::Welcome {
            client_id: "host".into(),
            proto: 1,
            read_only: !writable,
            entry_count,
            ts,
        },
    )
}

/// 双向桥接：中继广播 → 客户端；客户端密封字节 → 中继。
///
/// 密封字节作为二进制 WS 帧承载；中继对内容盲视（仅按 `room_id` 路由）。
/// 连接建立后先单播 host 裁决握手（本服务创建的房间），再补发历史密封帧，
/// 最后进入广播循环；握手帧与历史均为单播直发（不走中继），不干扰其他 guest。
/// 协同中继单条消息最大字节数。
const COLLAB_MSG_MAX_SIZE: usize = 64 * 1024;
/// 协同中继每秒消息数上限（速率限制窗口）。
const COLLAB_MSG_RATE_LIMIT: u32 = 30;

async fn collab_relay(
    socket: axum::extract::ws::WebSocket,
    room_id: String,
    relay: agent_collab::Relay,
    write_token: Option<String>,
    host: Option<HostSecret>,
    cwd: std::path::PathBuf,
) {
    let (mut sink, mut input) = socket.split();

    // host 握手：裁决读写 + 渲染绑定会话的 transcript（快照分块，entry_count 告知块数），
    // 全部单播直发（不走中继），不干扰其他 guest。
    if let Some(h) = &host {
        let ts = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(0);
        // 会话历史 → 快照分块（绑定 sid 且文件可读时；否则 0 块）。
        let transcript: Option<String> = h.session_id.as_deref().and_then(|sid| {
            let path = agent_context::SessionStore::for_cwd(&cwd).path_for(sid);
            render_session_transcript(&path)
        });
        let frames: Vec<agent_collab::WireFrame> = transcript
            .as_deref()
            .map(|t| agent_collab::chunk_snapshot(t, agent_collab::SNAPSHOT_CHUNK_MAX))
            .unwrap_or_default();
        let entry_count = frames.len();
        // Welcome 先发：read_only 裁决 + 快照块总数（guest 端据此显示加载进度）。
        let sealed = seal_host_welcome(h, write_token.as_deref(), ts, entry_count);
        if let Ok(sealed) = sealed {
            if sink
                .send(axum::extract::ws::Message::Binary(sealed.into()))
                .await
                .is_err()
            {
                return; // 握手帧发不出 → 连接已断，无需继续
            }
        }
        // 快照块序列（seq 0..entry_count，final_chunk 收尾）：分页传输会话历史。
        for frame in frames {
            if let Ok(sealed) = agent_collab::seal(&h.key, &frame) {
                if sink
                    .send(axum::extract::ws::Message::Binary(sealed.into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }

    // 原子取历史 + 订阅：新加入者先补发最近密封帧历史，再收实时广播。
    let (history, mut rx) = relay.join_with_replay(&room_id).await;
    for sealed in history {
        if sink
            .send(axum::extract::ws::Message::Binary(sealed.into()))
            .await
            .is_err()
        {
            return; // 客户端已断开，无需再开发送任务
        }
    }

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
                // 写权限校验：受管房间要求连接携带正确 wt（只读 view 连接被拒）。
                if let Err(e) = relay
                    .publish_with_token(&room_id, b.to_vec(), write_token.as_deref())
                    .await
                {
                    tracing::warn!(room = %room_id, error = %e, "collab publish 被拒（只读连接）");
                    continue;
                }
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
        let v: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&ServerFrame::StateChanged {
                state: AgentState::Running,
            })
            .unwrap(),
        )
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
                ContentBlock::Thinking {
                    text: "先思考一下".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "最终回答".into(),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
            stop_reason: None,
            stop_details: None,
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

    /// transcript 渲染：全部 role 前缀正确、SoftRequirement 跳过、tool_call 摘要截断。
    #[test]
    fn render_session_transcript_renders_all_roles() {
        use agent_core::{
            AgentMessage, AskKind, AskMessage, AssistantMessage, ContentBlock, SoftToolRequirement,
            StatusKind, StatusMessage, ToolResult, ToolResultMessage, Usage, UserContent,
            UserMessage,
        };
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "agent_transcript_{}_{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        {
            let mut f = std::fs::File::create(&path).unwrap();
            let lines = [
                serde_json::to_string(&AgentMessage::User(UserMessage {
                    content: vec![UserContent::Text { text: "你好".into() }],
                }))
                .unwrap(),
                serde_json::to_string(&AgentMessage::Assistant(AssistantMessage {
                    content: vec![
                        ContentBlock::Thinking {
                            text: "想想".into(),
                            signature: None,
                        },
                        ContentBlock::Text {
                            text: "回答".into(),
                        },
                        ContentBlock::ToolCall {
                            id: "t1".into(),
                            name: "read_file".into(),
                            arguments: serde_json::json!({"path": "src/lib.rs"}),
                        },
                    ],
                    usage: Usage::default(),
                    model: "test".into(),
                    stop_reason: None,
                    stop_details: None,
                }))
                .unwrap(),
                serde_json::to_string(&AgentMessage::ToolResult(ToolResultMessage {
                    tool_call_id: "t1".into(),
                    result: ToolResult::Text("文件内容".into()),
                }))
                .unwrap(),
                serde_json::to_string(&AgentMessage::Status(StatusMessage {
                    text: "进行中".into(),
                    kind: StatusKind::Info,
                }))
                .unwrap(),
                serde_json::to_string(&AgentMessage::Ask(AskMessage {
                    id: "a1".into(),
                    kind: AskKind::Followup,
                    prompt: "确认？".into(),
                }))
                .unwrap(),
                serde_json::to_string(&AgentMessage::SoftRequirement(SoftToolRequirement {
                    id: "s1".into(),
                    tool_name: "read_file".into(),
                    reminder: "内部机制".into(),
                }))
                .unwrap(),
            ];
            for line in lines {
                writeln!(f, "{line}").unwrap();
            }
        }
        let text = render_session_transcript(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(text.contains("[user] 你好"));
        assert!(text.contains("[thinking] 想想"));
        assert!(text.contains("[assistant] 回答"));
        assert!(text.contains("[tool] read_file {\"path\":\"src/lib.rs\"}"));
        assert!(text.contains("[tool result] 文件内容"));
        assert!(text.contains("[status] 进行中"));
        assert!(text.contains("[ask] 确认？"));
        assert!(!text.contains("内部机制"), "SoftRequirement 应跳过");
    }

    /// transcript 渲染：文件缺失 / 无可渲染消息 → None。
    #[test]
    fn render_session_transcript_missing_or_empty_returns_none() {
        let missing = std::env::temp_dir().join(format!(
            "agent_transcript_missing_{}_{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        assert_eq!(render_session_transcript(&missing), None, "缺失文件 → None");
        use agent_core::{AgentMessage, SoftToolRequirement};
        use std::io::Write;
        let empty = std::env::temp_dir().join(format!(
            "agent_transcript_empty_{}_{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        {
            let mut f = std::fs::File::create(&empty).unwrap();
            writeln!(
                f,
                "{}",
                serde_json::to_string(&AgentMessage::SoftRequirement(SoftToolRequirement {
                    id: "s1".into(),
                    tool_name: "x".into(),
                    reminder: "内部".into(),
                }))
                .unwrap()
            )
            .unwrap();
        }
        assert_eq!(render_session_transcript(&empty), None, "仅内部消息 → None");
        let _ = std::fs::remove_file(&empty);
    }

    /// host welcome 携带 entry_count（快照块总数，guest 端加载进度）。
    #[test]
    fn host_welcome_carries_entry_count() {
        let host = HostSecret {
            key: agent_collab::generate_room_key(),
            write_token: "tok".into(),
            session_id: None,
        };
        let sealed = seal_host_welcome(&host, None, 1, 7).unwrap();
        let frame = agent_collab::open(&host.key, &sealed).unwrap();
        match frame {
            agent_collab::WireFrame::Welcome { entry_count, .. } => {
                assert_eq!(entry_count, 7);
            }
            other => panic!("应为 Welcome，got {other:?}"),
        }
    }

    /// 会话树格式（SessionNode JSONL）的会话：read_history 与 read_branch_tree
    /// 都应正确解析，且分支结构（两叶子）可见。
    #[test]
    fn read_history_and_branches_parse_session_nodes() {
        use agent_core::{AgentMessage, SessionNode};
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "agent_tree_test_{}_{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        // a → b → b1（叶子1）；a → b → c1（叶子2）。
        let nodes = vec![
            SessionNode {
                id: "a".into(),
                parent_id: None,
                message: AgentMessage::user_text("a"),
            },
            SessionNode {
                id: "b".into(),
                parent_id: Some("a".into()),
                message: AgentMessage::user_text("b"),
            },
            SessionNode {
                id: "b1".into(),
                parent_id: Some("b".into()),
                message: AgentMessage::user_text("b1"),
            },
            SessionNode {
                id: "c1".into(),
                parent_id: Some("b".into()),
                message: AgentMessage::user_text("c1"),
            },
        ];
        {
            let mut f = std::fs::File::create(&path).unwrap();
            for n in &nodes {
                writeln!(f, "{}", serde_json::to_string(n).unwrap()).unwrap();
            }
        }
        // read_history：应解析全部 4 条（两条分支叶子都可见）。
        let items = read_history(&path);
        let texts: Vec<&str> = items.iter().map(|v| v["text"].as_str().unwrap()).collect();
        assert_eq!(texts, vec!["a", "b", "b1", "c1"]);

        // read_branch_tree：两叶子 b1、c1；active_leaf 默认末节点 c1。
        let tree = read_branch_tree(&path);
        let mut leaves: Vec<String> = tree["leaves"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        leaves.sort();
        assert_eq!(leaves, vec!["b1".to_string(), "c1".to_string()]);
        assert_eq!(tree["active_leaf"].as_str(), Some("c1"));
        assert_eq!(tree["nodes"].as_array().unwrap().len(), 4);

        let _ = std::fs::remove_file(&path);
    }

    /// Sprint 4-1：`/api/pause` `/api/resume` 处理器驱动 SessionManager 共享的进程级暂停门。
    /// 证明「HTTP 路由 → 共享单例 PauseGate」接线端到端正确；park/resume 机制本身由
    /// agent crate 的 `pause_gate_parks_run_and_resumes` 覆盖。
    #[tokio::test]
    async fn pause_and_resume_handlers_drive_shared_gate() {
        let cfg: Config = toml::from_str(include_str!("../../../config.example.toml"))
            .expect("示例配置应可解析为 Config");
        let mgr = SessionManager::new(
            Arc::new(cfg),
            reqwest::Client::new(),
            Arc::new(std::path::PathBuf::from(".")),
            None,
        );
        assert!(!mgr.pause_gate().paused(), "初始应为运行态");
        // pause 处理器驱动共享门 → 冻结。
        let _ = pause_handler(State(mgr.clone())).await;
        assert!(mgr.pause_gate().paused(), "pause 后应冻结");
        // resume 处理器解除 → 恢复运行。
        let _ = resume_handler(State(mgr.clone())).await;
        assert!(!mgr.pause_gate().paused(), "resume 后应恢复运行");
    }

    /// 浏览器 GET `/collab/{room}`（无 WS Upgrade 头）→ 返回 guest 页面（HTML）。
    #[tokio::test]
    async fn collab_guest_page_served_for_plain_browser_get() {
        use tower::ServiceExt;
        let cfg: Config = toml::from_str(include_str!("../../../config.example.toml"))
            .expect("示例配置应可解析为 Config");
        let mgr = SessionManager::new(
            Arc::new(cfg),
            reqwest::Client::new(),
            Arc::new(std::path::PathBuf::from(".")),
            None,
        );
        let router = app(mgr);
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/collab/0123456789abcdef0123456789abcdef")
                    .body(axum::body::Body::empty())
                    .expect("构造请求"),
            )
            .await
            .expect("路由可达");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let content_type = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_type.starts_with("text/html"),
            "应为 HTML: {content_type}"
        );
        let body = axum::body::to_bytes(resp.into_body(), 256 * 1024)
            .await
            .expect("读 body");
        let html = String::from_utf8_lossy(&body);
        // 关键元素：WebCrypto AES-GCM 解封 + 帧处理 + 断线重连。
        for marker in ["AES-GCM", "snapshot_chunk", "重连中", "crypto.subtle.importKey"] {
            assert!(html.contains(marker), "guest 页应含 {marker}");
        }
    }

    /// guest 页面与 Rust codec 的密封布局常量一致（12B IV + ciphertext+tag，32B 密钥）。
    #[test]
    fn guest_page_sealed_layout_matches_rust_codec() {
        assert!(COLLAB_GUEST_PAGE.contains("KEY_LEN = 32"), "密钥长度应一致");
        assert!(COLLAB_GUEST_PAGE.contains("IV_LEN = 12"), "IV 长度应一致");
        assert!(
            COLLAB_GUEST_PAGE.contains("[12B IV][ciphertext+tag]"),
            "布局注释应一致"
        );
    }

    /// host 裁决：正确 wt → 可写；缺失/错误 wt → 只读；帧可被房间密钥解封。
    #[test]
    fn host_welcome_adjudicates_read_only_from_connection_wt() {
        let host = HostSecret {
            key: agent_collab::generate_room_key(),
            write_token: "tok-canonical-0123456789abcdef".into(),
            session_id: None,
        };
        // 正确 wt → read_only: false（可写）。
        let sealed = seal_host_welcome(&host, Some("tok-canonical-0123456789abcdef"), 1, 0).unwrap();
        let frame = agent_collab::open(&host.key, &sealed).expect("裁决帧应可解封");
        match frame {
            agent_collab::WireFrame::Welcome {
                read_only, proto, ..
            } => {
                assert!(!read_only, "正确 wt 应判可写");
                assert_eq!(proto, 1);
            }
            other => panic!("应为 Welcome，got {other:?}"),
        }
        // 缺失 wt（view 链接）→ read_only: true。
        let sealed = seal_host_welcome(&host, None, 2, 0).unwrap();
        let frame = agent_collab::open(&host.key, &sealed).unwrap();
        assert!(
            matches!(
                frame,
                agent_collab::WireFrame::Welcome {
                    read_only: true,
                    ..
                }
            ),
            "view 链接应判只读"
        );
        // 错误 wt（过期/被篡改）→ read_only: true。
        let sealed = seal_host_welcome(&host, Some("tok-wrong"), 3, 0).unwrap();
        let frame = agent_collab::open(&host.key, &sealed).unwrap();
        assert!(
            matches!(
                frame,
                agent_collab::WireFrame::Welcome {
                    read_only: true,
                    ..
                }
            ),
            "错误 wt 应判只读"
        );
    }

    /// 统计聚合纯函数：给定 JSONL 行集合（新旧两种持久化格式混合 + 无法解析的行）
    /// → 正确的 sessions / usage / tools / top_models / daily 聚合。
    #[test]
    fn stats_aggregate_from_jsonl_lines() {
        use agent_core::{
            AgentMessage, AssistantMessage, ContentBlock, SessionNode, ToolResult,
            ToolResultMessage, Usage, UserMessage,
        };

        // 会话树格式（SessionNode 包裹）下的用户消息（新格式）。
        let user = SessionNode::root("n1".into(), AgentMessage::User(UserMessage::from_text("hi")));
        let mut lines = vec![serde_json::to_string(&user).unwrap()];

        // 旧线性格式（裸 AgentMessage）的 assistant：两次工具调用 + usage。
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 20,
            cache_write_tokens: 10,
            cost_usd: 0.003,
        };
        let asst = AgentMessage::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({ "path": "a.rs" }),
                },
                ContentBlock::ToolCall {
                    id: "t2".into(),
                    name: "run_command".into(),
                    arguments: serde_json::json!({ "cmd": "ls" }),
                },
                ContentBlock::Text {
                    text: "done".into(),
                },
            ],
            usage: usage.clone(),
            model: "claude-sonnet".into(),
            stop_reason: None,
            stop_details: None,
        });
        lines.push(serde_json::to_string(&asst).unwrap());

        // 工具结果：t1 错误、t2 成功——错误应归因到 read_file。
        lines.push(
            serde_json::to_string(&AgentMessage::ToolResult(ToolResultMessage {
                tool_call_id: "t1".into(),
                result: ToolResult::Error {
                    recoverable: true,
                    message: "boom".into(),
                },
            }))
            .unwrap(),
        );
        lines.push(
            serde_json::to_string(&AgentMessage::ToolResult(ToolResultMessage {
                tool_call_id: "t2".into(),
                result: ToolResult::text("ok"),
            }))
            .unwrap(),
        );

        // 另一模型的 assistant（无工具调用），验证模型聚合与 usage 累加。
        lines.push(
            serde_json::to_string(&AgentMessage::Assistant(AssistantMessage {
                content: vec![ContentBlock::ToolCall {
                    id: "t3".into(),
                    name: "write_file".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    cost_usd: 0.0001,
                },
                model: "deepseek-v3".into(),
                stop_reason: None,
                stop_details: None,
            }))
            .unwrap(),
        );
        // 无法解析的行：跳过且不计入消息数。
        lines.push("这不是 JSON".into());

        let mut acc = StatsAccumulator::default();
        aggregate_file_lines(&mut acc, &lines, "2026-08-01");
        // 再聚合一个空内容文件（同日期）：会话数 +1，消息/用量不变。
        aggregate_file_lines(&mut acc, &[serde_json::to_string(&user).unwrap()], "2026-08-01");

        let agg = acc.finish(None);
        assert_eq!(agg.sessions.total, 2);
        assert_eq!(agg.sessions.total_messages, 6, "坏行不应计入消息数");
        assert_eq!(agg.usage.input_tokens, 110);
        assert_eq!(agg.usage.output_tokens, 55);
        assert_eq!(agg.usage.cache_read_tokens, 20);
        assert_eq!(agg.usage.cache_write_tokens, 10);
        assert!((agg.usage.cost_usd - 0.0031).abs() < 1e-9);

        // tools：按调用次数降序；错误按 tool_call_id 归因到工具名。
        assert_eq!(agg.tools.len(), 3);
        let read_file = agg.tools.iter().find(|t| t.name == "read_file").unwrap();
        assert_eq!(read_file.calls, 1);
        assert_eq!(read_file.errors, 1, "Error 结果应计入错误");
        let run = agg.tools.iter().find(|t| t.name == "run_command").unwrap();
        assert_eq!(run.calls, 1);
        assert_eq!(run.errors, 0, "Text 结果不算错误");

        // top_models：按总 token 降序。
        assert_eq!(agg.top_models[0].model, "claude-sonnet");
        assert_eq!(agg.top_models[0].turns, 1);
        assert_eq!(agg.top_models[0].input_tokens, 100);
        assert_eq!(agg.top_models[0].output_tokens, 50);
        assert_eq!(agg.top_models[1].model, "deepseek-v3");
        assert_eq!(agg.top_models[1].turns, 1);

        // daily：两文件同日期合并。
        assert_eq!(agg.daily.len(), 1);
        assert_eq!(agg.daily[0].date, "2026-08-01");
        assert_eq!(agg.daily[0].tokens, 165);
        assert!((agg.daily[0].cost - 0.0031).abs() < 1e-9);
    }

    /// 趋势窗口：daily 按窗口逐日补齐（缺数据的日期补零、升序且与窗口等长）。
    #[test]
    fn stats_trend_window_fills_missing_days() {
        use agent_core::{AgentMessage, AssistantMessage, ContentBlock, Usage};
        let asst = AgentMessage::Assistant(AssistantMessage {
            content: vec![],
            usage: Usage {
                input_tokens: 7,
                output_tokens: 3,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.0,
            },
            model: "m".into(),
            stop_reason: None,
            stop_details: None,
        });
        let lines = vec![serde_json::to_string(&asst).unwrap()];

        let mut acc = StatsAccumulator::default();
        aggregate_file_lines(&mut acc, &lines, "2026-08-01");
        let window = vec![
            "2026-07-31".to_string(),
            "2026-08-01".to_string(),
            "2026-08-02".to_string(),
        ];
        let agg = acc.finish(Some(&window));
        assert_eq!(agg.daily.len(), 3);
        assert_eq!(agg.daily[0].date, "2026-07-31");
        assert_eq!(agg.daily[0].tokens, 0, "窗口内缺数据的天应补零");
        assert_eq!(agg.daily[1].tokens, 10);
        assert_eq!(agg.daily[2].tokens, 0);
    }

    /// 日期换算：civil_from_days 基准值 + 趋势窗口今天收尾。
    #[test]
    fn unix_date_conversion() {
        assert_eq!(unix_to_utc_date(0), "1970-01-01");
        assert_eq!(unix_to_utc_date(1_704_067_200), "2024-01-01");
        assert_eq!(unix_to_utc_date(1_700_000_000), "2023-11-14");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let window = recent_dates(3);
        assert_eq!(window.len(), 3);
        assert!(window[0] < window[1] && window[1] < window[2], "应升序");
        assert_eq!(window[2], unix_to_utc_date(now), "窗口应以今天收尾");
    }

    fn socks5_cfg(enabled: bool) -> agent_config::Socks5Config {
        agent_config::Socks5Config {
            enabled,
            host: "127.0.0.1".into(),
            port: Some(1080),
            username: Some("user".into()),
            password: secrecy::SecretString::from("s3cret"),
            connect_timeout_secs: 5,
        }
    }

    /// GET /api/socks5：已配置 → 载荷形状正确且绝不含密码；未配置 → configured=false。
    #[tokio::test]
    async fn socks5_status_route_reports_state_without_password() {
        use tower::ServiceExt;
        let cfg: Config = toml::from_str(include_str!("../../../config.example.toml"))
            .expect("示例配置应可解析为 Config");
        let ctrl = agent_proxy::Socks5Controller::new(&socks5_cfg(true), None, None)
            .expect("已配置应构造控制器");
        let mgr = SessionManager::new(
            Arc::new(cfg),
            reqwest::Client::new(),
            Arc::new(std::path::PathBuf::from(".")),
            Some(ctrl),
        );
        let router = app(mgr);

        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/socks5")
                    .body(axum::body::Body::empty())
                    .expect("构造请求"),
            )
            .await
            .expect("路由可达");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("读响应体");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(v["configured"], true);
        assert_eq!(v["enabled"], true);
        assert_eq!(v["host"], "127.0.0.1");
        assert_eq!(v["port"], 1080);
        assert_eq!(v["username"], "user");
        assert!(
            !body.windows(6).any(|w| w == b"s3cret"),
            "响应体不得含明文密码"
        );
        assert!(
            v["redacted"].as_str().is_some_and(|r| r.contains("user:***@")),
            "redacted 应脱敏: {v}"
        );

        // 未配置 → configured=false。
        let mgr2 = SessionManager::new(
            Arc::new(Config {
                socks5: agent_config::Socks5Config {
                    enabled: false,
                    ..socks5_cfg(false)
                },
                ..toml::from_str(include_str!("../../../config.example.toml"))
                    .expect("示例配置应可解析为 Config")
            }),
            reqwest::Client::new(),
            Arc::new(std::path::PathBuf::from(".")),
            None,
        );
        let router2 = app(mgr2);
        let resp = router2
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/socks5")
                    .body(axum::body::Body::empty())
                    .expect("构造请求"),
            )
            .await
            .expect("路由可达");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("读响应体");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(v["configured"], false);
        assert_eq!(v["enabled"], false);
    }

    /// POST /api/socks5：切换控制器开关；未配置 → 400。
    #[tokio::test]
    async fn socks5_set_route_toggles_controller() {
        use tower::ServiceExt;
        let cfg: Config = toml::from_str(include_str!("../../../config.example.toml"))
            .expect("示例配置应可解析为 Config");
        let ctrl = agent_proxy::Socks5Controller::new(&socks5_cfg(false), None, None)
            .expect("已配置应构造控制器");
        let mgr = SessionManager::new(
            Arc::new(cfg),
            reqwest::Client::new(),
            Arc::new(std::path::PathBuf::from(".")),
            Some(Arc::clone(&ctrl)),
        );
        let router = app(mgr);

        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/socks5")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"enabled":true}"#))
                    .expect("构造请求"),
            )
            .await
            .expect("路由可达");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert!(ctrl.enabled(), "控制器开关应被路由驱动为 true");

        // 未配置 → 400 说明缺失。
        let mgr2 = SessionManager::new(
            Arc::new(toml::from_str(include_str!("../../../config.example.toml")).unwrap()),
            reqwest::Client::new(),
            Arc::new(std::path::PathBuf::from(".")),
            None,
        );
        let router2 = app(mgr2);
        let resp = router2
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/socks5")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"enabled":true}"#))
                    .expect("构造请求"),
            )
            .await
            .expect("路由可达");
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    /// 测试用临时 cwd（避免在仓库根写 `.gyre/approval-mode.state`）。
    fn tmp_cwd(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gyre-server-approval-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// GET /api/approval-mode：未覆盖时 mode=null、effective=配置默认；形状正确。
    #[tokio::test]
    async fn approval_mode_status_route_reports_state() {
        use tower::ServiceExt;
        let cfg: Config = toml::from_str(include_str!("../../../config.example.toml"))
            .expect("示例配置应可解析为 Config");
        let mgr = SessionManager::new(
            Arc::new(cfg),
            reqwest::Client::new(),
            Arc::new(tmp_cwd("status")),
            None,
        );
        let router = app(mgr);
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/approval-mode")
                    .body(axum::body::Body::empty())
                    .expect("构造请求"),
            )
            .await
            .expect("路由可达");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("读响应体");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(v["configured"], true);
        assert_eq!(v["mode"], serde_json::Value::Null);
        assert_eq!(v["effective"], "always-ask");
        assert_eq!(v["config_default"], "always-ask");
    }

    /// POST /api/approval-mode：切换实时生效（控制器 + sidecar 落盘）；null 恢复默认；非法值 400。
    #[tokio::test]
    async fn approval_mode_set_route_toggles_controller() {
        use tower::ServiceExt;
        let cfg: Config = toml::from_str(include_str!("../../../config.example.toml"))
            .expect("示例配置应可解析为 Config");
        let cwd = tmp_cwd("set");
        let sidecar = cwd.join(".gyre").join("approval-mode.state");
        let mgr = SessionManager::new(
            Arc::new(cfg),
            reqwest::Client::new(),
            Arc::new(cwd),
            None,
        );
        let approval = mgr.approval().clone();
        let router = app(mgr);

        // 切到 yolo：控制器实时生效 + sidecar 落盘。
        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/approval-mode")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"mode":"yolo"}"#))
                    .expect("构造请求"),
            )
            .await
            .expect("路由可达");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(approval.current(), Some(ApprovalMode::Yolo));
        assert_eq!(
            std::fs::read_to_string(&sidecar).expect("sidecar 应已写入"),
            "yolo"
        );

        // 恢复默认：mode=null + sidecar 删除。
        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/approval-mode")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"mode":null}"#))
                    .expect("构造请求"),
            )
            .await
            .expect("路由可达");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(approval.current(), None);
        assert!(!sidecar.exists(), "恢复默认应删除 sidecar");

        // 非法档位 → 400。
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/approval-mode")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"mode":"reckless"}"#))
                    .expect("构造请求"),
            )
            .await
            .expect("路由可达");
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }
}
