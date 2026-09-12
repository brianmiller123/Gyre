//! task 工具：委派子任务给独立子 Agent（独立上下文，不继承父对话）。
//!
//! 支持：
//! - 单任务（`task: <string>`）：同步委派。
//! - 多任务并行（`tasks: [<string>, ...]`）：按 `max_concurrent` 并发护栏并行执行，聚合结果。
//!
//! 子 Agent 复用父 Provider/Tools/Workspace，审批自动放行（MVP，避免嵌套交互死锁）。
//! 子 Agent 默认继承父 temperature/thinking（由装配层注入；消除「父开思考、子不思考」割裂）。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_core::{
    AgentEvent, AgentState, ApprovalDecision, ApprovalPolicy, ApprovalRequest, AskMessage,
    AskResponse, CapabilityTier, ContextManager, LlmProvider, Mode, Model, ProviderCallContext,
    StatusKind, ThinkingConfig, ToolError, ToolResult, Workspace,
};
use agent_prompt::PromptCatalog;
use agent_tools::{Tool, ToolContext, ToolRegistry};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::Agent;

/// 子 Agent 文本累积上限（防超长输出 OOM）。
const SUB_TEXT_MAX: usize = 64 * 1024;
/// task 工具在途（并发 + 递归）委派数硬上限：拦截「子 Agent 再委派 task」的指数级爆炸。
const MAX_INFLIGHT_TASKS: usize = 16;
/// `output_schema` 校验失败后的重试上限（移植 omp schema permissive 重试语义）。
const MAX_SCHEMA_RETRIES: usize = 2;
/// 子 Agent 上下文工厂：每次委派创建一个全新上下文（不继承父对话）。
pub type ContextFactory = Arc<dyn Fn() -> Arc<dyn ContextManager> + Send + Sync>;

/// task 工具：把子任务委派给一个独立子 Agent 执行，返回其最终文本输出。
///
/// 子 Agent 拥有独立上下文，复用父的 Provider/Tools/Workspace；
/// 审批自动放行（MVP，避免嵌套交互死锁）。适合复杂任务的分片/并行。
///
/// 字段均 `Arc`/`Copy`，故实现 `Clone` 以便在并行多任务时按值 move 进 `tokio::spawn`。
#[derive(Clone)]
pub struct TaskTool {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<dyn ToolRegistry>,
    prompts: Arc<PromptCatalog>,
    workspace: Arc<Workspace>,
    model: Model,
    provider_ctx: ProviderCallContext,
    mode: Mode,
    max_mistakes: usize,
    context_guard: f32,
    max_output_tokens: usize,
    context_factory: ContextFactory,
    /// 子 Agent 继承的 temperature（`None` 用模型默认）。
    temperature: Option<f32>,
    /// 子 Agent 继承的 thinking 配置（`None` 不思考）。
    thinking: Option<ThinkingConfig>,
    /// 多任务并行时的并发护栏（≥1；装配层取自 `[subagent].max_concurrent`）。
    max_concurrent: usize,
    /// 父级审批策略（可选；注入后子 Agent 尊重父级 `decide` 的 Deny，仅交互式 prompt 自动放行）。
    approval: Option<Arc<dyn ApprovalPolicy>>,
    /// 子 Agent 监控总线（可选；注入后子 Agent 生命周期可被 Web/CLI 实时观测）。
    supervisor: Option<Arc<agent_supervisor::Supervisor>>,
    /// 在途委派计数（TaskTool 经注册表在父/子 Agent 间共享同一 Arc，故可追踪整棵递归树，
    /// 用作 [`MAX_INFLIGHT_TASKS`] 护栏，防止指数级递归委派耗尽资源）。
    depth: Arc<AtomicUsize>,
    /// 命名子代理定义（H18；空则只有隐式 `task` 行为）。`Arc<[...]>` 切片共享，
    /// 避免 `Clone` 时深拷贝定义列表。
    agents: Arc<[crate::agent_def::AgentDefinition]>,
    /// 模型别名 → 运行时模型（定义 `model:` 覆盖用；装配层从配置 profile 构建）。
    model_overrides: Arc<std::collections::HashMap<String, Model>>,
    /// 进程内消息总线（H43；注入后子代理以 task id 入册、可被寻址，收件箱消息在
    /// 轮次边界作为 steering 注入）。
    hub: Option<Arc<agent_core::hub::Hub>>,
    /// 子代理入册 id 计数器（`task-1`、`task-2`…）。
    task_seq: Arc<AtomicUsize>,
}

impl TaskTool {
    /// 构造。
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<dyn ToolRegistry>,
        prompts: Arc<PromptCatalog>,
        workspace: Arc<Workspace>,
        model: Model,
        provider_ctx: ProviderCallContext,
        mode: Mode,
        max_mistakes: usize,
        context_guard: f32,
        max_output_tokens: usize,
        context_factory: ContextFactory,
        temperature: Option<f32>,
        thinking: Option<ThinkingConfig>,
        max_concurrent: usize,
    ) -> Self {
        Self {
            provider,
            tools,
            prompts,
            workspace,
            model,
            provider_ctx,
            mode,
            max_mistakes,
            context_guard,
            max_output_tokens,
            context_factory,
            temperature,
            thinking,
            max_concurrent: max_concurrent.max(1),
            approval: None,
            supervisor: None,
            depth: Arc::new(AtomicUsize::new(0)),
            agents: Arc::from(Vec::new()),
            model_overrides: Arc::new(std::collections::HashMap::new()),
            hub: None,
            task_seq: Arc::new(AtomicUsize::new(1)),
        }
    }

    /// 注入消息总线（H43）：子代理以 `task-<n>` 入册，可被 `hub send` 寻址；
    /// 其收件箱消息在子代理的轮次边界注入（steering），实现双向寻址。
    #[must_use]
    pub fn with_hub(mut self, hub: Arc<agent_core::hub::Hub>) -> Self {
        self.hub = Some(hub);
        self
    }

    /// 注入命名子代理定义（H18）。
    #[must_use]
    pub fn with_agents(mut self, agents: Vec<crate::agent_def::AgentDefinition>) -> Self {
        self.agents = Arc::from(agents);
        self
    }

    /// 注入模型别名覆盖表（定义 `model:` 用；键为别名，值为运行时模型）。
    #[must_use]
    pub fn with_model_overrides(
        mut self,
        overrides: std::collections::HashMap<String, Model>,
    ) -> Self {
        self.model_overrides = Arc::new(overrides);
        self
    }

    /// 按名解析定义；`None` / 未知名字回退到内置 `task` 语义（全工具、继承模型）。
    fn resolve_agent(
        &self,
        name: Option<&str>,
    ) -> Result<Option<crate::agent_def::AgentDefinition>, String> {
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            return Ok(None);
        };
        match crate::agent_def::find_agent(&self.agents, name) {
            Some(def) => Ok(Some(def.clone())),
            None => Err(format!(
                "未知子代理 {name:?}；可用: {}",
                self.agents
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// 注入监控总线（构建器式；不改 `new()` 签名以保持向后兼容）。
    #[must_use]
    pub fn with_supervisor(mut self, supervisor: Arc<agent_supervisor::Supervisor>) -> Self {
        self.supervisor = Some(supervisor);
        self
    }

    /// 注入父级审批策略：子 Agent 将尊重父级 `decide`（父级 `Deny` 的危险操作同样被拒），
    /// 仅交互式 `prompt` 自动放行以避免嵌套交互死锁。未注入则维持全自动放行（向后兼容）。
    #[must_use]
    pub fn with_approval(mut self, approval: Arc<dyn ApprovalPolicy>) -> Self {
        self.approval = Some(approval);
        self
    }

    /// 构建一个子 Agent（独立上下文），注入继承的 temperature/thinking 与父级取消令牌。
    ///
    /// `cancel` 应为父级 cancel 的 `child_token()`——父任务取消时级联取消子 Agent，
    /// 否则子 Agent 在父任务取消后仍会继续烧 token / 跑工具。
    fn build_sub_agent(
        &self,
        cancel: CancellationToken,
        def: Option<&crate::agent_def::AgentDefinition>,
    ) -> Agent {
        let sub_context = (self.context_factory)();
        // 子 Agent 审批：注入了父级策略则尊重其 Deny（仅 prompt 自动放行），否则全自动放行。
        let approval: Arc<dyn ApprovalPolicy> = match &self.approval {
            Some(p) => Arc::new(DelegatedApproval::new(Arc::clone(p))),
            None => Arc::new(AlwaysAllow),
        };
        // H18：定义覆盖 —— 模型别名、工具白名单（能力面裁剪）、角色提示段、思考档位。
        let model = def
            .and_then(|d| d.model.as_deref())
            .and_then(|alias| self.model_overrides.get(alias).cloned())
            .unwrap_or_else(|| self.model.clone());
        let tools: Arc<dyn ToolRegistry> = match def.and_then(|d| d.tools.as_deref()) {
            Some(allowed) => Arc::new(agent_tools::FilteredRegistry::new(
                Arc::clone(&self.tools),
                allowed.iter().cloned().collect(),
            )),
            None => Arc::clone(&self.tools),
        };
        let mut builder = Agent::builder(model)
            .provider(Arc::clone(&self.provider))
            .tools(tools)
            // H43：需要注入 hub 消息时必须启用 steering 通道。
            .steering()
            .context(sub_context)
            .prompts(Arc::clone(&self.prompts))
            .approval(approval)
            .workspace(Arc::clone(&self.workspace))
            .provider_ctx(self.provider_ctx.clone())
            .mode(self.mode)
            .max_mistakes(self.max_mistakes)
            .context_guard(self.context_guard)
            .max_output_tokens(self.max_output_tokens)
            .cancel(cancel);
        if let Some(t) = self.temperature {
            builder = builder.temperature(t);
        }
        // 思考档位：定义的 `thinking:` 覆盖父级继承值。
        let thinking = def
            .and_then(|d| d.thinking.as_deref())
            .and_then(thinking_from_level)
            .or_else(|| self.thinking.clone());
        if let Some(tc) = thinking {
            builder = builder.thinking(tc);
        }
        // 角色定义正文：Gyre 的基础提示承载工具政策/委派契约，故**追加**而非替换。
        if let Some(d) = def.filter(|d| !d.system_prompt.is_empty()) {
            builder = builder.context_files(vec![format!(
                "\n\n<agent_definition name=\"{}\" read_only=\"{}\">\n{}\n</agent_definition>\n",
                d.name, d.read_only, d.system_prompt
            )]);
        }
        builder.build()
    }

    /// 运行单个子 Agent 任务（无 schema 约束，行为与历史版本一致）。
    /// 把子代理注册进消息总线并启动「收件箱 → steering」转发任务（H43）。
    ///
    /// 未注入 hub → 返回 `None`（行为与历史一致）。转发任务随守卫 drop 而中止，
    /// 注册项同时注销（避免名册残留已结束的子代理）。
    fn register_in_hub(&self, label: &str, sub_agent: Arc<Agent>) -> Option<HubRegistration> {
        let hub = self.hub.as_ref()?;
        let id = format!("task-{}", self.task_seq.fetch_add(1, Ordering::Relaxed));
        // H41：容量感知注册（满员时不创建收件箱；调用方已在派生前预拒，这里兜底）。
        let mut handle = match hub.try_register_handle(
            id.clone(),
            Some(label.to_string()),
            agent_core::hub::AgentStatus::Running,
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "子代理未入册（名册已满）");
                return None;
            }
        };
        let steer_agent = sub_agent;
        let hub_for_ack = Arc::clone(hub);
        let task = tokio::spawn(async move {
            while let Some(msg) = handle.recv().await {
                // 回执：消息注入即确认（发送方的 `wait_ack` 随之返回）。
                if let Some(ack_id) = msg.ack_id {
                    hub_for_ack.ack(ack_id);
                }
                // 注入为 steering（下一轮 step 边界生效）。
                if !steer_agent.steer(agent_core::AgentMessage::user_text(format!(
                    "[hub 来自 {}] {}",
                    msg.from, msg.body
                ))) {
                    tracing::debug!(from = %msg.from, "子代理 steering 通道不可用，hub 消息丢弃");
                }
            }
        });
        Some(HubRegistration {
            hub: Arc::clone(hub),
            id,
            forwarder: task,
        })
    }

    async fn run_one(
        &self,
        task: String,
        cancel: CancellationToken,
        def: Option<crate::agent_def::AgentDefinition>,
    ) -> SubOutcome {
        self.run_with_schema(task, cancel, None, def).await
    }

    /// 运行单个子 Agent 任务，可选 `output_schema` typed 输出约束。
    ///
    /// typed 路径（移植 omp task `schema` 语义）：任务包装 `<output_contract>` 要求
    /// 只输出一个 JSON 值；输出经 [`extract_json`] 提取 + [`validate_json_schema`]
    /// 校验，失败时在同一子 Agent 上下文注入纠错反馈重试（上限 [`MAX_SCHEMA_RETRIES`]），
    /// 仍失败则返回可恢复错误（附最终校验错误）。重试复用同一子 Agent——纠错对话
    /// 留在其上下文中，模型能看到自己上一次的输出。
    ///
    /// 若注入了 [`agent_supervisor::Supervisor`]，则把子 Agent 事件流观测为
    /// 子 Agent 生命周期（阶段 / 轮次 / 工具 / 用量 / 日志）；否则行为与改动前完全一致。
    async fn run_with_schema(
        &self,
        task: String,
        cancel: CancellationToken,
        schema: Option<&serde_json::Value>,
        def: Option<crate::agent_def::AgentDefinition>,
    ) -> SubOutcome {
        let sub_agent = Arc::new(self.build_sub_agent(cancel, def.as_ref()));
        // 观测标签带上定义名（Web/CLI 仪表盘据此区分 scout / reviewer / …）。
        let label = match def.as_ref() {
            Some(d) => format!("{}: {}", d.name, label_for(&task)),
            None => label_for(&task),
        };
        let sid: Option<String> = match self.supervisor.as_ref() {
            Some(s) => Some(s.spawn(None, label.clone(), task.clone()).await),
            None => None,
        };
        // H43：子代理入册（task id = `task-<n>`），并把收件箱消息在轮次边界注入
        // （steering）。返回守卫：函数结束（含 panic unwind）即注销 + 中止转发任务。
        let _hub_guard = self.register_in_hub(&label, Arc::clone(&sub_agent));

        let first_prompt = match schema {
            Some(s) => typed_task_prompt(&task, s),
            None => task.clone(),
        };
        let (mut text, mut errored) = self.run_turn(&sub_agent, &first_prompt, &sid).await;

        let mut json = None;
        if let Some(s) = schema {
            let mut last_err = String::new();
            let mut attempts = 0;
            loop {
                if errored.is_none() {
                    match crate::extract_json(&text) {
                        Some(v) => match crate::validate_json_schema(&v, s) {
                            Ok(()) => {
                                json = Some(v);
                                break;
                            }
                            Err(e) => last_err = e,
                        },
                        None => last_err = "输出中未找到 JSON 值".to_string(),
                    }
                } else {
                    last_err = errored.clone().unwrap_or_default();
                }
                if attempts >= MAX_SCHEMA_RETRIES {
                    break;
                }
                attempts += 1;
                let (t2, e2) = self
                    .run_turn(&sub_agent, &retry_feedback(&last_err, &text), &sid)
                    .await;
                text = t2;
                errored = e2;
            }
            if json.is_none() {
                return SubOutcome {
                    task,
                    text,
                    error: Some(format!(
                        "typed 输出未通过 schema 校验（重试 {MAX_SCHEMA_RETRIES} 次后仍失败）：{last_err}"
                    )),
                    json: None,
                };
            }
        }

        let success = errored.is_none() && (json.is_some() || schema.is_none());
        if let (Some(s), Some(id)) = (self.supervisor.as_ref(), &sid) {
            s.finish(id, success, errored.clone()).await;
        }

        SubOutcome {
            task,
            text,
            error: errored,
            json,
        }
    }

    /// 消费一个子 Agent 轮次的事件流，返回（文本累积, 致命错误）。
    ///
    /// typed 重试轮复用同一子 Agent 实例（上下文保留纠错对话）；supervisor 观测
    /// 按轮次进行（streaming 相位在每轮内首次 `TextDelta` 时置位）。
    async fn run_turn(
        &self,
        sub_agent: &Agent,
        prompt: &str,
        sid: &Option<String>,
    ) -> (String, Option<String>) {
        let events = sub_agent.run(prompt);
        tokio::pin!(events);
        let mut text = String::new();
        let mut errored: Option<String> = None;
        let mut streaming = false;
        while let Some(ev) = events.next().await {
            match ev {
                AgentEvent::TextDelta(d) => {
                    if text.len() < SUB_TEXT_MAX {
                        text.push_str(&d);
                        if text.len() > SUB_TEXT_MAX {
                            // 安全截断：回退到最近的 UTF-8 字符边界，避免 truncate 落在
                            // 多字节字符中间导致 panic。
                            let mut end = SUB_TEXT_MAX;
                            while end > 0 && !text.is_char_boundary(end) {
                                end -= 1;
                            }
                            text.truncate(end);
                            text.push_str("\n...(子 Agent 输出过长，已截断)");
                        }
                    }
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), sid) {
                        if !streaming {
                            streaming = true;
                            s.set_phase(id, agent_supervisor::SubAgentPhase::Streaming)
                                .await;
                        }
                    }
                }
                AgentEvent::ToolExec { name, output } => {
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), sid) {
                        s.record_tool_call(id, &name).await;
                        s.log(
                            id,
                            agent_supervisor::LogLevel::Info,
                            format!("[{name}] {}", truncate(&output, 240)),
                        )
                        .await;
                    }
                }
                AgentEvent::Usage(u) => {
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), sid) {
                        s.record_usage(id, &u).await;
                    }
                }
                AgentEvent::StateChanged(st) => {
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), sid) {
                        if let Some(phase) = match st {
                            AgentState::Running => Some(agent_supervisor::SubAgentPhase::Running),
                            AgentState::Streaming => {
                                Some(agent_supervisor::SubAgentPhase::Streaming)
                            }
                            AgentState::WaitingForInput => {
                                Some(agent_supervisor::SubAgentPhase::WaitingTool)
                            }
                            _ => None,
                        } {
                            s.set_phase(id, phase).await;
                        }
                    }
                }
                AgentEvent::Say(msg) => {
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), sid) {
                        let lvl = match msg.kind {
                            StatusKind::Error => agent_supervisor::LogLevel::Error,
                            StatusKind::Warning => agent_supervisor::LogLevel::Warn,
                            StatusKind::Thinking => agent_supervisor::LogLevel::Debug,
                            _ => agent_supervisor::LogLevel::Info,
                        };
                        s.log(id, lvl, msg.text).await;
                    }
                }
                AgentEvent::Done(summary) => {
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), sid) {
                        for _ in 0..summary.turns {
                            s.record_turn(id).await;
                        }
                    }
                }
                AgentEvent::Error(e) => {
                    errored = Some(e.clone());
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), sid) {
                        s.log(id, agent_supervisor::LogLevel::Error, &e).await;
                    }
                }
                AgentEvent::ThinkingDelta(_)
                | AgentEvent::Ask(_)
                | AgentEvent::Assistant(_)
                | AgentEvent::TurnStart
                | AgentEvent::TurnEnd { .. }
                | AgentEvent::MessageStart
                | AgentEvent::MessageEnd(_)
                | AgentEvent::ToolExecutionStart { .. }
                | AgentEvent::ToolExecutionUpdate { .. }
                | AgentEvent::ToolExecutionEnd { .. }
                // 结构化会话事件：展示文本已由配对的 Say 记账到 supervisor 日志。
                | AgentEvent::Session(_) => {}
            }
        }
        (text, errored)
    }
}

/// 单个子任务运行结果。
struct SubOutcome {
    task: String,
    text: String,
    error: Option<String>,
    /// typed 输出（`output_schema` 校验通过后的 JSON 值；普通任务为 None）。
    json: Option<serde_json::Value>,
}

/// 子 Agent 审批：自动放行（未注入父级策略时的回退；避免嵌套交互死锁）。
struct AlwaysAllow;
#[async_trait]
impl ApprovalPolicy for AlwaysAllow {
    fn decide(&self, _r: &ApprovalRequest<'_>) -> ApprovalDecision {
        ApprovalDecision::Allow
    }
    async fn prompt(&self, _a: &AskMessage) -> Result<AskResponse, ToolError> {
        Ok(AskResponse::Yes)
    }
}

/// 委派审批：`decide` 委托父级策略（尊重规则引擎的 Allow/Deny，杜绝子 Agent 无脑放行
/// 父级已 Deny 的危险操作——修复审批旁路），`prompt`（交互式询问）自动放行以避免
/// 嵌套交互死锁（子 Agent 运行时无独立 UI 循环）。
struct DelegatedApproval {
    parent: Arc<dyn ApprovalPolicy>,
}

impl DelegatedApproval {
    fn new(parent: Arc<dyn ApprovalPolicy>) -> Self {
        Self { parent }
    }
}

#[async_trait]
impl ApprovalPolicy for DelegatedApproval {
    fn decide(&self, r: &ApprovalRequest<'_>) -> ApprovalDecision {
        self.parent.decide(r)
    }
    async fn prompt(&self, _a: &AskMessage) -> Result<AskResponse, ToolError> {
        Ok(AskResponse::Yes)
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &'static str {
        "task"
    }
    fn description(&self) -> &'static str {
        "委派子任务给独立子 Agent（独立上下文，不继承当前对话），返回其最终结果。支持单任务（task）或并行多任务（tasks，按并发护栏并行）。单任务可给 output_schema（JSON Schema）要求结构化 JSON 输出——校验失败自动带纠错反馈重试，成功返回紧凑 JSON。适合并行处理或分片复杂任务。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "单个子任务描述（须自包含，含必要上下文；与 tasks 二选一）" },
                "tasks": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "多个可并行的子任务描述（与 task 二选一；按并发护栏并行执行后聚合结果）"
                },
                "agent": {
                    "type": "string",
                    "description": "命名子代理（缺省 task = 通用全工具）。可用名与各自能力见 system prompt 的 <subagents> 段；只读代理无写/执行工具。"
                },
                "output_schema": {
                    "type": "object",
                    "description": "typed 输出契约（仅与 task 单任务联用）：JSON Schema（type/properties/required/items/enum/界限等子集）。给出时子 Agent 须只输出一个符合 schema 的 JSON 值，校验通过后父级收到紧凑 JSON"
                }
            }
        })
    }
    fn capability(&self) -> CapabilityTier {
        // 委派本身不直接写盘；子 Agent 内部操作走其自己的审批（此处自动放行）。
        CapabilityTier::ReadOnly
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        // 解析任务列表：优先 tasks（数组），回退 task（单）。
        let tasks: Vec<String> =
            if let Some(arr) = input.get("tasks").and_then(serde_json::Value::as_array) {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .filter(|s| !s.trim().is_empty())
                    .collect()
            } else {
                match input.get("task").and_then(serde_json::Value::as_str) {
                    Some(t) if !t.trim().is_empty() => vec![t.to_string()],
                    _ => return Err(ToolError::InvalidArgs("缺少 `task` 或 `tasks`".into())),
                }
            };
        if tasks.is_empty() {
            return Err(ToolError::InvalidArgs("`tasks` 为空".into()));
        }
        // H18：命名子代理（缺省 = 隐式 task：全工具、继承父级模型）。
        let def = match self.resolve_agent(input.get("agent").and_then(serde_json::Value::as_str)) {
            Ok(d) => d,
            Err(msg) => return Err(ToolError::InvalidArgs(msg)),
        };

        // typed 输出契约：仅支持单任务（多任务聚合无法对齐单一 schema）。
        let output_schema = input.get("output_schema").cloned();
        if let Some(s) = &output_schema {
            if !s.is_object() {
                return Err(ToolError::InvalidArgs(
                    "`output_schema` 必须是 JSON Schema 对象".into(),
                ));
            }
            if tasks.len() > 1 {
                return Err(ToolError::InvalidArgs(
                    "`output_schema` 仅支持单任务（task），不与 tasks 数组联用".into(),
                ));
            }
        }

        // 在途委派护栏：经共享 `depth` 计数器追踪整棵递归树，拦截指数级递归委派。
        // 计数器在函数返回时（含所有早退路径）由 guard 自动递减。
        let prev_inflight = self.depth.fetch_add(1, Ordering::SeqCst);
        let _inflight_guard = InflightGuard {
            counter: Arc::clone(&self.depth),
        };
        if prev_inflight >= MAX_INFLIGHT_TASKS {
            return Err(ToolError::Execution(format!(
                "task 委派在途数超限（{MAX_INFLIGHT_TASKS}），疑似递归委派失控"
            )));
        }

        // H41：名册容量预拒——满员时在**派生之前**报错（而不是注册表无限增长后才失败）。
        if let Some(hub) = &self.hub
            && hub.at_capacity()
        {
            return Err(ToolError::Execution(format!(
                "子代理名册已满（上限 {}）：请等现有子代理结束或调大 [subagent] max_registry",
                hub.capacity().saturating_sub(1).max(1)
            )));
        }

        // 单任务：直接同步执行。子 Agent 取父级 cancel 的 child，级联取消。
        if tasks.len() == 1 {
            let out = self
                .run_with_schema(
                    tasks.into_iter().next().expect("non-empty"),
                    _ctx.cancel.child_token(),
                    output_schema.as_ref(),
                    def,
                )
                .await;
            return finish_single(out);
        }

        // 多任务：并发护栏（信号量）并行执行，许可在 spawn 前 acquire 以限制 in-flight 子 Agent。
        let sem = Arc::new(tokio::sync::Semaphore::new(self.max_concurrent));
        let mut join = Vec::with_capacity(tasks.len());
        // 克隆父级 cancel 句柄，便于在每个 spawn 任务内派生独立 child token。
        let parent_cancel = _ctx.cancel.clone();
        for task in tasks {
            let permit = sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| ToolError::Execution(format!("并发信号量已关闭: {e}")))?;
            // 在 move 前 derive 独立 child token（child_token 取 &self，不 move 父句柄）。
            let sub_cancel = parent_cancel.child_token();
            let this = self.clone();
            let def = def.clone();
            join.push(tokio::spawn(async move {
                let _permit = permit; // 持有至任务结束，归还许可
                this.run_one(task, sub_cancel, def).await
            }));
        }

        let mut outs: Vec<SubOutcome> = Vec::with_capacity(join.len());
        for handle in join {
            match handle.await {
                Ok(o) => outs.push(o),
                Err(je) => outs.push(SubOutcome {
                    task: String::new(),
                    text: String::new(),
                    error: Some(format!("子任务 panic: {je}")),
                    json: None,
                }),
            }
        }

        // 聚合：逐任务分节输出，标注失败。
        let mut any_text = false;
        let mut buf = String::new();
        for (i, o) in outs.iter().enumerate() {
            buf.push_str(&format!("## 子任务 {}\n{}\n\n", i + 1, o.task));
            if o.text.trim().is_empty() {
                if let Some(e) = &o.error {
                    buf.push_str(&format!("（失败: {e}）\n\n"));
                } else {
                    buf.push_str("（无输出）\n\n");
                }
            } else {
                buf.push_str(&o.text);
                buf.push_str("\n\n");
                any_text = true;
            }
        }
        if !any_text {
            let first_err = outs
                .iter()
                .find_map(|o| o.error.clone())
                .unwrap_or_else(|| "所有子任务均无输出".to_string());
            return Err(ToolError::Execution(format!(
                "子 Agent 均失败: {first_err}"
            )));
        }
        Ok(ToolResult::text(buf.trim_end().to_string()))
    }
}

/// 定义 `thinking:` 档位 → 思考配置（token 预算；与 `ThinkingConfig` 的档位阈值一致）。
fn thinking_from_level(level: &str) -> Option<ThinkingConfig> {
    match level.trim().to_ascii_lowercase().as_str() {
        "high" => Some(ThinkingConfig::new(32_000)),
        "medium" | "mid" => Some(ThinkingConfig::new(12_000)),
        "low" | "minimal" => Some(ThinkingConfig::new(2_000)),
        _ => None,
    }
}

/// 子代理在总线上的注册守卫（H43）：drop 时注销并中止转发任务。
struct HubRegistration {
    hub: Arc<agent_core::hub::Hub>,
    id: String,
    forwarder: tokio::task::JoinHandle<()>,
}

impl Drop for HubRegistration {
    fn drop(&mut self) {
        self.hub.unregister(&self.id);
        self.forwarder.abort();
    }
}

/// 在途委派计数 RAII 守卫：确保任何返回路径（含早退/panic unwind）都递减计数器。
struct InflightGuard {
    counter: Arc<AtomicUsize>,
}
impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 单任务结果归一化（与原行为一致）。
fn finish_single(out: SubOutcome) -> Result<ToolResult, ToolError> {
    // typed 输出优先：返回紧凑 JSON（父 Agent 直接消费结构化数据）。
    if let Some(v) = out.json {
        let compact = serde_json::to_string(&v)
            .map_err(|e| ToolError::Execution(format!("typed 输出序列化失败: {e}")))?;
        return Ok(ToolResult::text(compact));
    }
    if out.text.trim().is_empty() {
        if let Some(e) = out.error {
            return Err(ToolError::Execution(format!("子 Agent 失败: {e}")));
        }
        return Ok(ToolResult::text("（子 Agent 无文本输出）"));
    }
    Ok(ToolResult::text(out.text))
}

/// `output_schema` 的任务包装：明确要求「只输出一个符合 schema 的 JSON 值」。
fn typed_task_prompt(task: &str, schema: &serde_json::Value) -> String {
    let schema_str = serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string());
    format!(
        "{task}\n\n<output_contract>\n最终输出必须**只是一个 JSON 值**，符合以下 JSON Schema \
(支持关键字：type/properties/required/items/enum/const/长度与数值界限/allOf/anyOf/oneOf)：\n\
{schema_str}\n不要输出 markdown 围栏、注释或任何 JSON 之外的文本。\n</output_contract>"
    )
}

/// schema 校验失败的纠错反馈（同上下文重试轮注入）。
fn retry_feedback(errors: &str, previous: &str) -> String {
    format!(
        "你上一次输出未通过 JSON Schema 校验：{errors}\n上一次输出（截断）：{}\n\
请重新给出**只含一个 JSON 值**的输出，严格符合上述 schema。",
        truncate(previous, 2000)
    )
}

/// 监控卡片标签：任务首部截断到 40 字符（换行折叠为空格）。
fn label_for(task: &str) -> String {
    let t = task.trim().replace('\n', " ");
    ellipsis(&t, 40)
}

/// 截断到 `max` 字符（超长加 …）。
fn truncate(s: &str, max: usize) -> String {
    ellipsis(s.trim(), max)
}

fn ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut o: String = s.chars().take(max.saturating_sub(1)).collect();
        o.push('…');
        o
    }
}

#[cfg(test)]
mod execution_tests {
    use super::*;
    use agent_core::{
        ApprovalDecision, ApprovalPolicy, ApprovalRequest, AskMessage, AskResponse, AssistantEvent,
        AssistantMessage, ContentBlock, LlmProvider, StopReason, ToolSpec,
    };
    use agent_tools::{Tool, ToolContext, ToolRegistry};

    /// 可编排 Provider：记录每次请求的工具面与 hub 名册，按脚本返回文本。
    struct ScriptProvider {
        calls: Arc<AtomicUsize>,
        seen_tools: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        seen_hub: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        hub: Option<Arc<agent_core::hub::Hub>>,
        replies: Vec<String>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ScriptProvider {
        fn id(&self) -> &'static str {
            "task-script"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            request: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen_tools
                .lock()
                .unwrap()
                .push(request.tools.iter().map(|t| t.name.clone()).collect());
            if let Some(hub) = &self.hub {
                self.seen_hub.lock().unwrap().push(hub.peers());
            }
            let text = self
                .replies
                .get(n)
                .cloned()
                .unwrap_or_else(|| format!("reply-{n}"));
            // 子 Agent 的文本经 `AgentEvent::TextDelta` 收集（`run_turn`），
            // 故桩 Provider 必须发流式增量 + MessageEnd（只发 MessageEnd 会被视为无输出）。
            Ok(Box::pin(futures::stream::iter(vec![
                AssistantEvent::TextDelta(text.clone()),
                AssistantEvent::MessageEnd(AssistantMessage {
                    content: vec![ContentBlock::Text { text }],
                    usage: agent_core::Usage::default(),
                    model: "task-script".into(),
                    stop_reason: Some(StopReason::Stop),
                    stop_details: None,
                }),
            ])))
        }
    }

    /// 记录型注册表：暴露 read_file / write_file / run_command 三个规格。
    struct ThreeToolRegistry;

    struct DummyTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "dummy"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn capability(&self) -> agent_core::CapabilityTier {
            agent_core::CapabilityTier::ReadOnly
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<agent_core::ToolResult, agent_core::ToolError> {
            Ok(agent_core::ToolResult::text("dummy"))
        }
    }

    impl ToolRegistry for ThreeToolRegistry {
        fn specs(&self) -> Vec<ToolSpec> {
            ["read_file", "write_file", "run_command"]
                .iter()
                .map(|n| ToolSpec {
                    name: (*n).to_string(),
                    description: "d".into(),
                    schema: serde_json::json!({"type": "object"}),
                })
                .collect()
        }
        fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
            match name {
                "read_file" | "write_file" | "run_command" => Some(Arc::new(DummyTool(Box::leak(
                    name.to_string().into_boxed_str(),
                )))),
                _ => None,
            }
        }
    }

    struct NoopApproval;
    #[async_trait::async_trait]
    impl ApprovalPolicy for NoopApproval {
        fn decide(&self, _r: &ApprovalRequest<'_>) -> ApprovalDecision {
            ApprovalDecision::Allow
        }
        async fn prompt(&self, _a: &AskMessage) -> Result<AskResponse, agent_core::ToolError> {
            Ok(AskResponse::Yes)
        }
    }

    fn ctx<'a>(ws: &'a agent_core::Workspace, cancel: &'a CancellationToken) -> ToolContext<'a> {
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

    fn tool(
        provider: Arc<dyn LlmProvider>,
        registry: Arc<dyn ToolRegistry>,
        defs: Vec<crate::agent_def::AgentDefinition>,
        hub: Option<Arc<agent_core::hub::Hub>>,
    ) -> TaskTool {
        let mut t = TaskTool::new(
            provider,
            registry,
            Arc::new(agent_prompt::PromptCatalog::new()),
            Arc::new(agent_core::Workspace::new(".")),
            agent_core::Model::with_defaults("m", "p", agent_core::Api::OpenAiCompletions),
            agent_core::ProviderCallContext::default(),
            agent_core::Mode::Code,
            3,
            0.8,
            4096,
            Arc::new(|| {
                Arc::new(agent_context::InMemoryContext::new(vec![]))
                    as Arc<dyn agent_core::ContextManager>
            }),
            None,
            None,
            4,
        )
        .with_agents(defs);
        if let Some(h) = hub {
            t = t.with_hub(h);
        }
        t
    }

    fn read_only_def(name: &str) -> crate::agent_def::AgentDefinition {
        crate::agent_def::AgentDefinition {
            name: name.into(),
            description: "d".into(),
            system_prompt: "只读角色".into(),
            tools: Some(vec!["read_file".into()]),
            model: None,
            thinking: None,
            read_only: true,
            source: crate::agent_def::AgentSource::Project,
            file_path: None,
        }
    }

    /// 单任务：子 Agent 文本原样回传（`task` 契约的最小可用面）。
    #[tokio::test]
    async fn single_task_returns_sub_agent_text() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(ScriptProvider {
            calls: calls.clone(),
            seen_tools: Arc::new(std::sync::Mutex::new(Vec::new())),
            seen_hub: Arc::new(std::sync::Mutex::new(Vec::new())),
            hub: None,
            replies: vec!["子代理回答".into()],
        });
        let t = tool(provider, Arc::new(ThreeToolRegistry), Vec::new(), None);
        let ws = agent_core::Workspace::new(".");
        let cancel = CancellationToken::new();
        let out = t
            .execute(serde_json::json!({"task": "查一下 X"}), &ctx(&ws, &cancel))
            .await
            .unwrap();
        assert_eq!(out.to_llm_text(), "子代理回答");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// 多任务：并行分节聚合，每个子任务的文本都出现且带序号标题。
    #[tokio::test]
    async fn parallel_tasks_aggregate_each_result() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(ScriptProvider {
            calls: calls.clone(),
            seen_tools: Arc::new(std::sync::Mutex::new(Vec::new())),
            seen_hub: Arc::new(std::sync::Mutex::new(Vec::new())),
            hub: None,
            replies: vec!["第一份".into(), "第二份".into()],
        });
        let t = tool(provider, Arc::new(ThreeToolRegistry), Vec::new(), None);
        let ws = agent_core::Workspace::new(".");
        let cancel = CancellationToken::new();
        let out = t
            .execute(
                serde_json::json!({"tasks": ["任务甲", "任务乙"]}),
                &ctx(&ws, &cancel),
            )
            .await
            .unwrap();
        let text = out.to_llm_text();
        assert!(
            text.contains("## 子任务 1") && text.contains("## 子任务 2"),
            "{text}"
        );
        assert!(text.contains("任务甲") && text.contains("任务乙"), "{text}");
        assert!(text.contains("第一份") && text.contains("第二份"), "{text}");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "两个子任务各一次请求");
    }

    /// 未知命名代理：在调用 Provider 之前即拒绝（避免白跑一次子 Agent）。
    #[tokio::test]
    async fn unknown_agent_rejected_without_provider_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(ScriptProvider {
            calls: calls.clone(),
            seen_tools: Arc::new(std::sync::Mutex::new(Vec::new())),
            seen_hub: Arc::new(std::sync::Mutex::new(Vec::new())),
            hub: None,
            replies: vec!["x".into()],
        });
        let t = tool(
            provider,
            Arc::new(ThreeToolRegistry),
            vec![read_only_def("scout")],
            None,
        );
        let ws = agent_core::Workspace::new(".");
        let cancel = CancellationToken::new();
        let err = t
            .execute(
                serde_json::json!({"task": "x", "agent": "ghost"}),
                &ctx(&ws, &cancel),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "拒绝应发生在 Provider 调用前"
        );
    }

    /// H18 只读白名单：子 Agent 的模型侧**看不到** write/run 工具（能力面裁剪，非审批拦截）。
    #[tokio::test]
    async fn read_only_agent_filters_write_tools_from_request() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(ScriptProvider {
            calls: calls.clone(),
            seen_tools: Arc::clone(&seen),
            seen_hub: Arc::new(std::sync::Mutex::new(Vec::new())),
            hub: None,
            replies: vec!["ok".into()],
        });
        let t = tool(
            provider,
            Arc::new(ThreeToolRegistry),
            vec![read_only_def("scout")],
            None,
        );
        let ws = agent_core::Workspace::new(".");
        let cancel = CancellationToken::new();
        t.execute(
            serde_json::json!({"task": "看看代码", "agent": "scout"}),
            &ctx(&ws, &cancel),
        )
        .await
        .unwrap();
        let tools = seen.lock().unwrap();
        let names = tools.first().expect("应有一次请求");
        assert!(names.contains(&"read_file".to_string()), "{names:?}");
        assert!(!names.contains(&"write_file".to_string()), "{names:?}");
        assert!(!names.contains(&"run_command".to_string()), "{names:?}");
    }

    /// `output_schema`：首次输出不是合法 JSON → 带纠错反馈重试一次并返回紧凑 JSON。
    #[tokio::test]
    async fn output_schema_retries_until_valid_json() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(ScriptProvider {
            calls: calls.clone(),
            seen_tools: Arc::new(std::sync::Mutex::new(Vec::new())),
            seen_hub: Arc::new(std::sync::Mutex::new(Vec::new())),
            hub: None,
            replies: vec!["我觉得答案大概是 42".into(), "{\"answer\":42}".into()],
        });
        let t = tool(provider, Arc::new(ThreeToolRegistry), Vec::new(), None);
        let ws = agent_core::Workspace::new(".");
        let cancel = CancellationToken::new();
        let out = t
            .execute(
                serde_json::json!({
                    "task": "回答一个问题",
                    "output_schema": {
                        "type": "object",
                        "properties": {"answer": {"type": "integer"}},
                        "required": ["answer"]
                    }
                }),
                &ctx(&ws, &cancel),
            )
            .await
            .unwrap();
        assert_eq!(out.to_llm_text(), "{\"answer\":42}");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "一次重试后通过");
    }

    /// H43：运行期子代理以 `task-<n>` 在册（Provider 调用时刻可见），结束后注销。
    #[tokio::test]
    async fn sub_agent_is_registered_on_hub_only_while_running() {
        let calls = Arc::new(AtomicUsize::new(0));
        let hub = agent_core::hub::Hub::new().shared();
        let seen_hub = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(ScriptProvider {
            calls: calls.clone(),
            seen_tools: Arc::new(std::sync::Mutex::new(Vec::new())),
            seen_hub: Arc::clone(&seen_hub),
            hub: Some(Arc::clone(&hub)),
            replies: vec!["done".into()],
        });
        let t = tool(
            provider,
            Arc::new(ThreeToolRegistry),
            Vec::new(),
            Some(Arc::clone(&hub)),
        );
        let ws = agent_core::Workspace::new(".");
        let cancel = CancellationToken::new();
        t.execute(serde_json::json!({"task": "跑一下"}), &ctx(&ws, &cancel))
            .await
            .unwrap();
        let snapshots = seen_hub.lock().unwrap();
        let during = snapshots.first().expect("Provider 调用时应有名册快照");
        assert!(
            during.iter().any(|p| p == "task-1"),
            "运行期应入册 task-1：{during:?}"
        );
        drop(snapshots);
        assert!(
            hub.peers().is_empty(),
            "委派结束后应注销：{:?}",
            hub.peers()
        );
    }
}
#[cfg(test)]
mod agent_selection_tests {
    use super::*;
    use crate::agent_def::{AgentDefinition, AgentSource};

    fn tool_with_agents(agents: Vec<AgentDefinition>) -> TaskTool {
        use agent_core::LlmProvider;
        use agent_tools::ToolRegistry;
        struct NoopProvider;
        #[async_trait::async_trait]
        impl LlmProvider for NoopProvider {
            fn id(&self) -> &'static str {
                "noop"
            }
            fn supports(&self) -> &[agent_core::Api] {
                &[]
            }
            async fn stream(
                &self,
                _request: agent_core::CompletionRequest,
                _ctx: &agent_core::ProviderCallContext,
            ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
                Err(agent_core::LlmError::Unsupported("noop".into()))
            }
        }
        struct EmptyRegistry;
        impl ToolRegistry for EmptyRegistry {
            fn specs(&self) -> Vec<agent_core::ToolSpec> {
                Vec::new()
            }
            fn get(&self, _name: &str) -> Option<std::sync::Arc<dyn agent_tools::Tool>> {
                None
            }
        }
        TaskTool::new(
            Arc::new(NoopProvider),
            Arc::new(EmptyRegistry),
            Arc::new(agent_prompt::PromptCatalog::new()),
            Arc::new(agent_core::Workspace::new(std::path::PathBuf::from("."))),
            agent_core::Model::with_defaults("m", "p", agent_core::Api::OpenAiCompletions),
            agent_core::ProviderCallContext::default(),
            agent_core::Mode::Code,
            3,
            0.8,
            4096,
            Arc::new(|| {
                Arc::new(agent_context::InMemoryContext::new(vec![]))
                    as Arc<dyn agent_core::ContextManager>
            }),
            None,
            None,
            2,
        )
        .with_agents(agents)
    }

    fn def(name: &str) -> AgentDefinition {
        AgentDefinition {
            name: name.to_string(),
            description: "d".into(),
            system_prompt: "p".into(),
            tools: Some(vec!["read_file".into()]),
            model: None,
            thinking: None,
            read_only: true,
            source: AgentSource::Project,
            file_path: None,
        }
    }

    #[test]
    fn resolves_named_agent_and_rejects_unknown() {
        let tool = tool_with_agents(vec![def("scout"), def("reviewer")]);
        // 缺省 → 无定义（隐式 task：全工具、继承模型）。
        assert!(tool.resolve_agent(None).unwrap().is_none());
        assert!(tool.resolve_agent(Some("  ")).unwrap().is_none());
        let scout = tool.resolve_agent(Some("scout")).unwrap().unwrap();
        assert_eq!(scout.name, "scout");
        assert!(scout.read_only);
        // 未知名字 → 明确错误并列出可用名（不静默回退到全工具！）。
        let err = tool.resolve_agent(Some("nope")).unwrap_err();
        assert!(err.contains("nope"), "{err}");
        assert!(err.contains("scout") && err.contains("reviewer"), "{err}");
    }

    /// H18：schema 暴露 `agent` 参数（否则模型无法选择命名子代理）。
    #[test]
    fn schema_exposes_agent_parameter() {
        let tool = tool_with_agents(vec![def("scout")]);
        let schema = tool.schema();
        assert!(schema["properties"]["agent"]["type"] == "string");
        assert!(tool.description().contains("task"));
    }

    #[test]
    fn thinking_level_maps_to_budget() {
        assert_eq!(
            thinking_from_level("high").map(|c| c.budget_tokens),
            Some(32_000)
        );
        assert_eq!(
            thinking_from_level("MEDIUM").map(|c| c.budget_tokens),
            Some(12_000)
        );
        assert_eq!(
            thinking_from_level(" low ").map(|c| c.budget_tokens),
            Some(2_000)
        );
        assert!(thinking_from_level("bogus").is_none());
    }
}
