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
    supervisor: Option<agent_supervisor::Supervisor>,
    /// 在途委派计数（TaskTool 经注册表在父/子 Agent 间共享同一 Arc，故可追踪整棵递归树，
    /// 用作 [`MAX_INFLIGHT_TASKS`] 护栏，防止指数级递归委派耗尽资源）。
    depth: Arc<AtomicUsize>,
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
        }
    }

    /// 注入监控总线（构建器式；不改 `new()` 签名以保持向后兼容）。
    #[must_use]
    pub fn with_supervisor(mut self, supervisor: agent_supervisor::Supervisor) -> Self {
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
    fn build_sub_agent(&self, cancel: CancellationToken) -> Agent {
        let sub_context = (self.context_factory)();
        // 子 Agent 审批：注入了父级策略则尊重其 Deny（仅 prompt 自动放行），否则全自动放行。
        let approval: Arc<dyn ApprovalPolicy> = match &self.approval {
            Some(p) => Arc::new(DelegatedApproval::new(Arc::clone(p))),
            None => Arc::new(AlwaysAllow),
        };
        let mut builder = Agent::builder(self.model.clone())
            .provider(Arc::clone(&self.provider))
            .tools(Arc::clone(&self.tools))
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
        if let Some(tc) = self.thinking.clone() {
            builder = builder.thinking(tc);
        }
        builder.build()
    }

    /// 运行单个子 Agent 任务，返回其（任务, 文本, 错误）。
    ///
    /// 若注入了 [`agent_supervisor::Supervisor`]，则把子 Agent 事件流观测为
    /// 子 Agent 生命周期（阶段 / 轮次 / 工具 / 用量 / 日志）；否则行为与改动前完全一致。
    async fn run_one(&self, task: String, cancel: CancellationToken) -> SubOutcome {
        let sub_agent = self.build_sub_agent(cancel);
        let sid: Option<String> = match self.supervisor.as_ref() {
            Some(s) => Some(s.spawn(None, label_for(&task), task.clone()).await),
            None => None,
        };

        let events = sub_agent.run(&task);
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
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), &sid) {
                        if !streaming {
                            streaming = true;
                            s.set_phase(id, agent_supervisor::SubAgentPhase::Streaming)
                                .await;
                        }
                    }
                }
                AgentEvent::ToolExec { name, output } => {
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), &sid) {
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
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), &sid) {
                        s.record_usage(id, &u).await;
                    }
                }
                AgentEvent::StateChanged(st) => {
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), &sid) {
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
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), &sid) {
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
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), &sid) {
                        for _ in 0..summary.turns {
                            s.record_turn(id).await;
                        }
                    }
                }
                AgentEvent::Error(e) => {
                    errored = Some(e.clone());
                    if let (Some(s), Some(id)) = (self.supervisor.as_ref(), &sid) {
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
                | AgentEvent::ToolExecutionEnd { .. } => {}
            }
        }

        let success = errored.is_none();
        if let (Some(s), Some(id)) = (self.supervisor.as_ref(), &sid) {
            s.finish(id, success, errored.clone()).await;
        }

        SubOutcome {
            task,
            text,
            error: errored,
        }
    }
}

/// 单个子任务运行结果。
struct SubOutcome {
    task: String,
    text: String,
    error: Option<String>,
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
    fn name(&self) -> &str {
        "task"
    }
    fn description(&self) -> &str {
        "委派子任务给独立子 Agent（独立上下文，不继承当前对话），返回其最终结果。支持单任务（task）或并行多任务（tasks，按并发护栏并行）。适合并行处理或分片复杂任务。"
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

        // 单任务：直接同步执行。子 Agent 取父级 cancel 的 child，级联取消。
        if tasks.len() == 1 {
            let out = self
                .run_one(
                    tasks.into_iter().next().expect("non-empty"),
                    _ctx.cancel.child_token(),
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
            join.push(tokio::spawn(async move {
                let _permit = permit; // 持有至任务结束，归还许可
                this.run_one(task, sub_cancel).await
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
    if out.text.trim().is_empty() {
        if let Some(e) = out.error {
            return Err(ToolError::Execution(format!("子 Agent 失败: {e}")));
        }
        return Ok(ToolResult::text("（子 Agent 无文本输出）"));
    }
    Ok(ToolResult::text(out.text))
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
