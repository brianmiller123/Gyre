//! Swarm 单代理执行：把每个 swarm 代理作为独立子 Agent 运行。
//!
//! 移植自 [`oh-my-pi swarm-extension/executor.ts`](../../../third/oh-my-pi/packages/swarm-extension/src/swarm/executor.ts)
//! （原版走 `runSubprocess` 子进程；本实现复用 [`agent::Agent`] 的子代理装配，避免子进程开销）。
//!
//! 解耦：执行策略以 [`SwarmAgentRunner`] 端口注入 pipeline；默认实现 [`AgentSwarmRunner`]
//! 持有共享的 Provider/Tools/Prompts，每次按代理定义构建全新子 Agent（独立上下文）。
//! 角色与额外上下文经 `Agent::context_files` 注入为 system prompt 段；取消经 `cancel_handle` 传播。

use std::sync::Arc;

use agent_core::{
    AgentEvent, ApprovalDecision, ApprovalPolicy, ApprovalRequest, AskMessage, AskResponse,
    ContextManager, LlmProvider, Mode, Model, ProviderCallContext, ThinkingConfig, ToolError,
    Workspace,
};
use agent_prompt::PromptCatalog;
use agent_tools::ToolRegistry;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::schema::SwarmAgent;

/// 单代理执行结果（对标 oh-my-pi `SingleResult`）。
#[derive(Debug, Clone, Default)]
pub struct SwarmAgentResult {
    /// 退出码（0 成功）。
    pub exit_code: i32,
    /// 文本输出。
    pub output: String,
    /// 错误信息（可选）。
    pub error: Option<String>,
}

/// 子代理上下文工厂：每次执行创建独立上下文（不跨代理/轮次串话）。
pub type ContextFactory = Arc<dyn Fn() -> Arc<dyn ContextManager> + Send + Sync>;

/// 进度回调 `Fn(agent_name, message)`。
pub type ProgressFn = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// 单代理执行端口：由 pipeline 调用，每个 wave 内对每个代理各调一次。
#[async_trait::async_trait]
pub trait SwarmAgentRunner: Send + Sync {
    /// 执行单个 swarm 代理。
    async fn run(
        &self,
        agent: &SwarmAgent,
        task: &str,
        model_override: Option<&Model>,
        cancel: &CancellationToken,
        on_progress: Option<&ProgressFn>,
    ) -> SwarmAgentResult;
}

/// 默认执行器：复用 [`agent::Agent`] 子代理装配。
pub struct AgentSwarmRunner {
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
    /// 子代理继承的 temperature（`None` 用模型默认）。
    temperature: Option<f32>,
    /// 子代理继承的 thinking 配置（`None` 不思考）。
    thinking: Option<ThinkingConfig>,
    /// 父级审批策略（可选；注入后子代理尊重父级 Deny，仅交互式 prompt 自动放行）。
    approval: Option<Arc<dyn ApprovalPolicy>>,
}

impl AgentSwarmRunner {
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
            approval: None,
        }
    }

    /// 注入父级审批策略：子代理将尊重父级 `decide`（父级 `Deny` 的危险操作同样被拒），
    /// 仅交互式 `prompt` 自动放行以避免嵌套交互死锁。未注入则维持全自动放行（向后兼容）。
    #[must_use]
    pub fn with_approval(mut self, approval: Arc<dyn ApprovalPolicy>) -> Self {
        self.approval = Some(approval);
        self
    }
}

/// 子代理审批：自动放行（与 TaskTool 一致，避免嵌套交互死锁）。
struct AlwaysAllow;
#[async_trait::async_trait]
impl ApprovalPolicy for AlwaysAllow {
    fn decide(&self, _r: &ApprovalRequest<'_>) -> ApprovalDecision {
        ApprovalDecision::Allow
    }
    async fn prompt(&self, _a: &AskMessage) -> Result<AskResponse, ToolError> {
        Ok(AskResponse::Yes)
    }
}

/// 委派审批：`decide` 委托父级策略（尊重规则引擎的 Allow/Deny），`prompt` 自动放行
/// 以避免嵌套交互死锁（子代理运行时无独立 UI 循环）。
struct DelegatedApproval {
    parent: Arc<dyn ApprovalPolicy>,
}

impl DelegatedApproval {
    fn new(parent: Arc<dyn ApprovalPolicy>) -> Self {
        Self { parent }
    }
}

#[async_trait::async_trait]
impl ApprovalPolicy for DelegatedApproval {
    fn decide(&self, r: &ApprovalRequest<'_>) -> ApprovalDecision {
        self.parent.decide(r)
    }
    async fn prompt(&self, _a: &AskMessage) -> Result<AskResponse, ToolError> {
        Ok(AskResponse::Yes)
    }
}

/// 把 swarm 代理的角色 + 额外上下文拼成 system prompt 段。
#[must_use]
pub fn build_role_prompt(agent: &SwarmAgent) -> String {
    let mut parts = vec![format!("You are a {}.", agent.role)];
    if let Some(extra) = &agent.extra_context {
        if !extra.is_empty() {
            parts.push(extra.clone());
        }
    }
    parts.join("\n\n")
}

#[async_trait::async_trait]
impl SwarmAgentRunner for AgentSwarmRunner {
    async fn run(
        &self,
        agent: &SwarmAgent,
        task: &str,
        model_override: Option<&Model>,
        cancel: &CancellationToken,
        on_progress: Option<&ProgressFn>,
    ) -> SwarmAgentResult {
        let model = model_override
            .cloned()
            .unwrap_or_else(|| self.model.clone());
        let sub_context = (self.context_factory)();
        let role_prompt = build_role_prompt(agent);
        // 子代理审批：注入了父级策略则尊重其 Deny（仅 prompt 自动放行），否则全自动放行。
        let approval: Arc<dyn ApprovalPolicy> = match &self.approval {
            Some(p) => Arc::new(DelegatedApproval::new(Arc::clone(p))),
            None => Arc::new(AlwaysAllow),
        };
        let mut builder = agent::Agent::builder(model)
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
            .context_files(vec![role_prompt]);
        if let Some(t) = self.temperature {
            builder = builder.temperature(t);
        }
        if let Some(tc) = self.thinking.clone() {
            builder = builder.thinking(tc);
        }
        let sub = builder.build();

        if let Some(f) = on_progress {
            f(&agent.name, "running");
        }

        let cancel_handle = sub.cancel_handle();
        let events = sub.run(task);
        tokio::pin!(events);

        let mut output = String::new();
        let mut error: Option<String> = None;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    cancel_handle.cancel();
                    error = Some("swarm agent cancelled".to_string());
                    break;
                }
                ev = events.next() => match ev {
                    Some(AgentEvent::TextDelta(d)) => output.push_str(&d),
                    Some(AgentEvent::Error(e)) => error = Some(e),
                    Some(_) => {}
                    None => break,
                },
            }
        }

        let exit_code = if error.is_some() { 1 } else { 0 };
        SwarmAgentResult {
            exit_code,
            output,
            error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_prompt_includes_extra() {
        let agent = SwarmAgent {
            name: "a".into(),
            role: "Coder".into(),
            task: "t".into(),
            extra_context: Some("Use Rust.".into()),
            reports_to: vec![],
            waits_for: vec![],
            model: None,
        };
        let p = build_role_prompt(&agent);
        assert!(p.contains("You are a Coder."));
        assert!(p.contains("Use Rust."));
    }
}
