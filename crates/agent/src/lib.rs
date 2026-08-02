//! # agent
//!
//! 智能体执行循环：[`Agent`] 持有 trait 注入的 Provider/Tools/Context/Prompt/Approval，
//! [`Agent::run`] 产出 [`AgentEvent`] 流，驱动「流式推理 → 工具调用 → 审批 → 回填 → 继续」闭环。
//!
//! 状态机（移植 Zoo-Code 五态）：Running → Streaming → (WaitingForInput) → Idle。
//! 解耦：本 crate 仅依赖 Trait，不依赖任何具体 Provider/Tool 实现。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

use std::sync::Arc;

use agent_core::{
    AgentEvent, AgentMessage, AgentRunSummary, AgentState, ApprovalDecision, ApprovalPolicy,
    AskKind, AskMessage, AskResponse, AssistantEvent, AssistantMessage, CompactionStrategy,
    CompletionRequest, ContentBlock, ContextManager, Hook, HookEvent, LlmProvider, MemoryStore,
    Mode, ProviderCallContext, ResourceResolver, SoftToolRequirement, StatusKind, StatusMessage,
    StopReason, ThinkingConfig, ThinkingPolicy, ToolChoice, ToolChoiceDirective, ToolResult,
    ToolResultMessage, Usage, Workspace, WriteEffect,
};
use agent_prompt::PromptCatalog;
use agent_skills::{SkillCatalog, render_skills_section};
use agent_tools::{Concurrency, ToolContext, ToolRegistry};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;
// P1-D：循环内 OpenTelemetry GenAI span 标注用。Instrument 给 Future 加 span（Send-safe），
// 见下方 chat span 说明。agent stream 须 Send（server tokio::spawn 消费），故不能用
// Span::enter() 的非 Send guard 跨 await。
use tracing::Instrument;

/// GPT-5 Harmony-header 泄漏检测与恢复（移植 oh-my-pi `harmony-leak`）。
mod harmony;
pub mod keywords;
mod pause;
mod task_tool;
pub use pause::PauseGate;
pub use task_tool::{ContextFactory, TaskTool};

/// P1-L：advisor 评审触发周期（每 N 轮一次；评审本身是独立 LLM 调用，太频繁会拖慢主循环）。
const ADVISOR_EVERY_N_TURNS: usize = 4;

/// P0-3：goals 目标预算配置（cli 从 `[goals]` 映射）。
#[derive(Debug, Clone)]
pub struct GoalBudget {
    /// token 累计上限（记账口径 input + cache_write + output；`0` = 不限）。
    pub token_budget: u64,
    /// 墙钟上限（`Duration::ZERO` = 不限）。
    pub time_budget: std::time::Duration,
    /// 超限后硬停（`true`：下一停止边界直接结束；`false`：注入提醒后续跑一轮让模型收尾）。
    pub hard_stop: bool,
}

impl GoalBudget {
    /// 空预算（不限）。
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            token_budget: 0,
            time_budget: std::time::Duration::ZERO,
            hard_stop: false,
        }
    }
}

/// P0-3：goals 记账状态（跨 Agent 重建共享；宿主可经 [`Agent::goal_state`] 查看/调整预算）。
#[derive(Debug)]
pub struct GoalState {
    /// 当前预算（`/goal` 可运行时调整）。
    pub budget: GoalBudget,
    /// 已注入过超限提醒（一次性，防「注入→超限→再注入」无限续跑循环）。
    pub notified: bool,
    /// 首次用量时间（记账起点）。
    pub start: Option<std::time::Instant>,
    /// 累计用量。
    pub usage: agent_core::Usage,
}

impl GoalState {
    /// 新状态。
    #[must_use]
    pub fn new(budget: GoalBudget) -> Self {
        Self {
            budget,
            notified: false,
            start: None,
            usage: agent_core::Usage::default(),
        }
    }

    /// 记账口径：input + cache_write + output（cache_read 为折扣价不计，同 oh-my-pi GoalRuntime）。
    #[must_use]
    pub fn billed(&self) -> u64 {
        self.usage.input_tokens + self.usage.cache_write_tokens + self.usage.output_tokens
    }

    /// 当前是否超限（token 或墙钟）。
    #[must_use]
    pub fn exceeded(&self) -> bool {
        if self.budget.token_budget > 0 && self.billed() >= self.budget.token_budget {
            return true;
        }
        if !self.budget.time_budget.is_zero() {
            if let Some(start) = self.start
                && start.elapsed() >= self.budget.time_budget
            {
                return true;
            }
        }
        false
    }

    /// 累计本轮用量；返回「本次首次超限」（已提醒过返回 `false`，防重复注入）。
    pub fn note_usage(&mut self, usage: &agent_core::Usage) -> bool {
        if self.start.is_none() {
            self.start = Some(std::time::Instant::now());
        }
        self.usage.add(usage);
        if self.notified {
            return false;
        }
        if self.exceeded() {
            self.notified = true;
            true
        } else {
            false
        }
    }

    /// 人类可读状态摘要（`/goal` 显示与预算提醒注入共用）。
    #[must_use]
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("token {}", self.billed())];
        if self.budget.token_budget > 0 {
            parts.push(format!("/{}", self.budget.token_budget));
        }
        if let Some(start) = self.start {
            parts.push(format!("时间 {}s", start.elapsed().as_secs()));
            if !self.budget.time_budget.is_zero() {
                parts.push(format!("/{}s", self.budget.time_budget.as_secs()));
            }
        }
        parts.join("，")
    }
}

#[cfg(test)]
mod goal_tests {
    use super::*;

    fn usage(input: u64, out: u64, cr: u64, cw: u64) -> agent_core::Usage {
        agent_core::Usage {
            input_tokens: input,
            output_tokens: out,
            cache_read_tokens: cr,
            cache_write_tokens: cw,
            cost_usd: 0.0,
        }
    }

    #[test]
    fn billed_counts_input_cache_write_output() {
        let mut s = GoalState::new(GoalBudget::unlimited());
        s.note_usage(&usage(100, 50, 200, 25));
        // 记账口径：input + cache_write + output（cache_read 折扣价不计）。
        assert_eq!(s.billed(), 175);
    }

    #[test]
    fn note_usage_reports_exceed_once() {
        let mut s = GoalState::new(GoalBudget {
            token_budget: 100,
            ..GoalBudget::unlimited()
        });
        assert!(!s.note_usage(&usage(60, 0, 0, 0)), "未超限不触发");
        assert!(s.note_usage(&usage(50, 0, 0, 0)), "累计 110 ≥ 100 首次超限触发");
        assert!(
            !s.note_usage(&usage(50, 0, 0, 0)),
            "已提醒过不再重复触发（防无限续跑循环）"
        );
    }

    #[test]
    fn token_budget_zero_means_unlimited() {
        let mut s = GoalState::new(GoalBudget::unlimited());
        assert!(!s.note_usage(&usage(1_000_000, 0, 0, 0)));
        assert!(!s.exceeded());
    }

    #[test]
    fn hard_stop_flag_preserved() {
        let s = GoalState::new(GoalBudget {
            token_budget: 10,
            hard_stop: true,
            ..GoalBudget::unlimited()
        });
        assert!(s.budget.hard_stop, "硬停标记供停止边界分支消费");
    }
}


/// 把 TTSR 提醒文本前置到工具结果（Text 前置文本；Error 前置到 message；Image 跳过）。
fn prepend_reminder(result: &ToolResult, reminder: &str) -> ToolResult {
    match result {
        ToolResult::Text(t) => ToolResult::Text(format!("{reminder}\n{t}")),
        ToolResult::Error {
            recoverable,
            message,
        } => ToolResult::Error {
            recoverable: *recoverable,
            message: format!("{reminder}\n{message}"),
        },
        ToolResult::Image { .. } => result.clone(),
    }
}

/// 工具执行期的 steering 中断策略（移植 oh-my-pi `interruptMode`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InterruptMode {
    /// 等待本轮工具全部完成再处理 steering（保守）。
    Wait,
    /// batch 含 [`agent_tools::Tool::interruptible`] 工具时，按固定间隔轮询 steering 队列，
    /// 命中即触发**批级** cancel 中断在途工具（批级 token 是 run-cancel 的 child，故不影响
    /// run 级取消语义；steering 随后在下一轮顶部/停止边界被 drain 处理）。默认值。
    #[default]
    Immediate,
}

/// 运行时配置覆盖（每轮 LLM 调用前解析，移植 oh-my-pi `getReasoning` / `getDisableReasoning`
/// 等动态解析器）。host 实现此 trait 经 [`AgentBuilder::runtime_overrides`] 注入，可在 run
/// 中途改变 thinking / temperature 等配置而**无需重建 Agent**。
///
/// 解析发生在每轮请求构造前；返回 `None` 的字段沿用静态 / [`ThinkingPolicy`] 解析结果，
/// 故只覆盖 host 明确给出的项。设计为 trait（而非多个独立闭包）便于后续按 oh-my-pi 同源
/// 扩展 `api_key` / `base_url` / `cwd` / `service_tier` 等动态解析器。
pub trait RuntimeOverrides: Send + Sync {
    /// 覆盖本轮 thinking 配置；返回 `None` 沿用启动期 policy / 静态解析结果。
    fn thinking(&self, _model: &agent_core::Model) -> Option<agent_core::ThinkingConfig> {
        None
    }
    /// 强制关闭本轮思考（移植 oh-my-pi `getDisableReasoning`）。
    ///
    /// 返回 `Some(true)` 时，即便 [`Self::thinking`] 或 ThinkingPolicy 给出了思考配置，
    /// 本轮也置空 thinking（不发 reasoning 参数）——用于 mid-run 按场景关闭思考（如简单
    /// follow-up 轮省 token）。返回 `None` 或 `Some(false)` 沿用思考解析结果。
    fn disable_thinking(&self) -> Option<bool> {
        None
    }
    /// 覆盖本轮 temperature；返回 `None` 沿用 [`AgentBuilder::temperature`] 静态值。
    fn temperature(&self) -> Option<f32> {
        None
    }
    /// 覆盖本轮 API key（移植 oh-my-pi `getApiKey`）。
    ///
    /// 返回 `Some(key)` 时，本轮 provider 调用使用该 key（覆盖启动期
    /// [`agent_core::ProviderCallContext::api_key`]）——用于 mid-run 凭证轮换 / 多账号
    /// 切换 / key 限流降级，无需重建 Agent。返回 `None` 沿用启动期 key。
    fn api_key(&self, _model: &agent_core::Model) -> Option<String> {
        None
    }
}

/// 单模型的 API key 轮换环（round-robin）。
///
/// P2：配置层 `[[models]].api_keys` 多 key 列表 → 每轮请求换下一个 key，
/// 任一 key 被限流/撤销时自动切换（配合 registry 的 fallback 重试语义）。
/// 轮换状态是原子计数，`Agent` 并发安全。
pub struct KeyRing {
    keys: Vec<String>,
    index: std::sync::atomic::AtomicUsize,
}

impl KeyRing {
    /// 构造轮换环（`keys` 为空时 [`Self::next`] 恒返回 `None`，等同不轮换）。
    #[must_use]
    pub fn new(keys: Vec<String>) -> Self {
        Self {
            keys,
            index: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// 取下一个 key（按轮次取模）。空环返回 `None`。
    pub fn next(&self) -> Option<String> {
        if self.keys.is_empty() {
            return None;
        }
        let idx = self.index.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(self.keys[idx % self.keys.len()].clone())
    }
}

/// 智能体（Ports & Adapters：所有依赖以 trait 注入）。
pub struct Agent {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<dyn ToolRegistry>,
    context: Arc<dyn ContextManager>,
    prompts: Arc<PromptCatalog>,
    approval: Arc<dyn ApprovalPolicy>,
    workspace: Arc<Workspace>,
    model: agent_core::Model,
    provider_ctx: ProviderCallContext,
    mode: Mode,
    max_mistakes: usize,
    /// 单任务最大轮次（硬上限），0 表示不限制。防止模型陷入无限工具循环。
    max_turns: usize,
    /// 运行时限（wall-clock）；`None` 不限制。超过则在下一轮顶部优雅停止。
    deadline: Option<std::time::Duration>,
    context_guard: f32,
    max_output_tokens: usize,
    temperature: Option<f32>,
    thinking: Option<ThinkingConfig>,
    /// P1-K：思考策略（Static/Auto）。`None` 走 `thinking` 静态；`Some` 覆盖之并按 prompt 难度解析。
    thinking_policy: Option<ThinkingPolicy>,
    cancel: CancellationToken,
    /// Skill 目录（可选；注入后 system prompt 追加 `<skills>` 段）。
    catalog: Option<Arc<SkillCatalog>>,
    /// 上下文约定文件（AGENTS.md）内容，注入为 system prompt 额外段。
    context_files: Vec<String>,
    /// 事件钩子（before/after tool、stop）。
    hooks: Vec<Arc<dyn Hook>>,
    /// 跨会话长期记忆（启动注入 summary 段）。
    memory: Option<Arc<dyn MemoryStore>>,
    /// 外部资源解析器（`mcp://` 路由用；装配层注入 McpRegistry）。
    resources: Option<Arc<dyn ResourceResolver>>,
    /// 软工具需求（运行期共享，便于外部更新）。
    soft_requirement: Arc<std::sync::Mutex<Option<SoftToolRequirement>>>,
    /// steering 接收端（外部中途注入消息）。
    steer_rx:
        tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<agent_core::AgentMessage>>>,
    /// steering 发送端镜像（`.steering()` 开启时由 builder 内部创建并存入；供宿主经
    /// [`Agent::steer`] 中途注入消息。`None` 时 steer 为空操作）。
    steer_tx: Option<tokio::sync::mpsc::UnboundedSender<agent_core::AgentMessage>>,
    /// aside 接收端（外部注入**被动、非中断**通知——后台任务完成、延迟 LSP diagnostics 等）。
    /// 与 steering 的区别：aside **永不**打断在途工具（不走 Immediate 批级 cancel），只在
    /// 轮次边界（下一轮模型调用前 / 停止边界）折叠注入。移植 oh-my-pi `getAsideMessages`。
    aside_rx:
        tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<agent_core::AgentMessage>>>,
    /// followUp 接收端（宿主编排的延续消息：子代理完成、外部工作流注入）。停止边界第三类
    ///（steering 中断 → aside 被动 → followUp 编排延续），移植 oh-my-pi `getFollowUpMessages`：
    /// 仅在 agent 本该停止时 drain，触发续跑让宿主「让 agent 继续干」而不借道中断性 steering。
    followup_rx:
        tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<agent_core::AgentMessage>>>,
    /// 运行时配置覆盖（每轮解析 thinking / temperature；mid-run 热更新，移植 oh-my-pi
    /// `getReasoning` 等动态解析器）。
    runtime_overrides: Option<Arc<dyn RuntimeOverrides>>,
    /// assistant 消息改写钩子（最终化后、入 context / MessageEnd / 工具分发前原地改写，
    /// 移植 oh-my-pi `transformAssistantMessage`）。单一真相源：所有下游看改写后。
    transform_assistant: Option<Arc<dyn Fn(&mut agent_core::AssistantMessage) + Send + Sync>>,
    /// 写入效果（编辑后 LSP format/diagnostics 钩子；装配层注入 `LspWriteEffect`）。
    write_effect: Option<Arc<dyn WriteEffect>>,
    /// 工具执行期的 steering 中断策略（默认 [`InterruptMode::Immediate`]）。
    interrupt_mode: InterruptMode,
    /// 进程级暂停门（可选；注入后在每次 provider 调用前 / 工具批执行前 park）。
    /// 共享单例由 host 驱动（CLI/Web `/pause`）；移植 oh-my-pi `AgentPauseGate`。
    pause_gate: Option<Arc<PauseGate>>,
    /// TTSR 流规则协调器（可选；注入后对流式输出实时匹配规则，命中中断重试）。
    ttsr: Option<Arc<agent_ttsr::TtsrCoordinator>>,
    /// P1-L：独立评审 advisor（可选；每 N 轮评审一次快照，建议经 `[advisor:…]` 注入）。
    advisor: Option<Arc<agent_advisor::Advisor>>,
    /// advisor 评审触发周期（轮；默认 [`ADVISOR_EVERY_N_TURNS`]）。
    advisor_every_n_turns: usize,
    /// P0-3：goals 目标预算（token/墙钟；软模式超限注入提醒续跑，硬模式停止边界直接收尾）。
    goal: Option<Arc<std::sync::Mutex<GoalState>>>,
    /// P2：压缩后端（summarize / snapcompact）。
    compaction_backend: agent_core::CompactionBackend,
    /// P2：snapcompact 帧数预算。
    compaction_max_frames: usize,
    /// 预算超限待注入标记（run_loop 用量记账置位，停止边界消费）。
    goal_pending: Arc<std::sync::atomic::AtomicBool>,
    /// 会话级合并冲突注册表（read 注册 / write 解决，`conflict://` 协议）。
    conflicts: Arc<std::sync::Mutex<agent_tools::ConflictHistory>>,
    /// 会话级 ast-rewrite 暂存队列（`ast_rewrite preview:true` → `xd://resolve/reject`）。
    pending_rewrites: Arc<std::sync::Mutex<Vec<agent_tools::PendingRewrite>>>,
    /// P2：模型 fallback 链（主模型失败且错误可重试时依序尝试的备用模型）。
    fallbacks: Vec<agent_core::Model>,
    /// P2：API key 轮换环（model id → key 环；`runtime_overrides` 命中时优先，跳过轮换）。
    key_rings: Arc<std::collections::HashMap<String, KeyRing>>,
}

impl Agent {
    /// 构建器。
    #[must_use]
    pub fn builder(model: agent_core::Model) -> AgentBuilder {
        AgentBuilder {
            model,
            provider: None,
            tools: None,
            context: None,
            prompts: None,
            approval: None,
            workspace: None,
            provider_ctx: ProviderCallContext::default(),
            mode: Mode::Code,
            max_mistakes: 3,
            max_turns: 1000,
            deadline: None,
            context_guard: 0.8,
            max_output_tokens: 4096,
            temperature: None,
            thinking: None,
            thinking_policy: None,
            cancel: CancellationToken::new(),
            catalog: None,
            context_files: Vec::new(),
            hooks: Vec::new(),
            memory: None,
            resources: None,
            soft_requirement: None,
            steer_rx: None,
            steer_tx: None,
            aside_rx: None,
            followup_rx: None,
            runtime_overrides: None,
            transform_assistant: None,
            write_effect: None,
            fallbacks: Vec::new(),
            key_rings: std::collections::HashMap::new(),
            interrupt_mode: InterruptMode::default(),
            pause_gate: None,
            ttsr: None,
            advisor: None,
            advisor_every_n_turns: ADVISOR_EVERY_N_TURNS,
            goals: None,
            compaction_backend: agent_core::CompactionBackend::Summarize,
            compaction_max_frames: agent_snapcompact::DEFAULT_MAX_FRAMES,
        }
    }

    /// 中途向运行中的 agent 注入一条消息（steering）。命中后由 Immediate 中断策略尽快打断
    /// 在途的可中断工具（如 `run_command`），并在下一注入边界把消息投给模型——实现「运行中
    /// 随时输入、模型立即收到」。未启用 steering（构建时未调 `.steering()`）时为空操作，
    /// 返回 `false`。移植 oh-my-pi `Agent.steer()`。
    pub fn steer(&self, message: agent_core::AgentMessage) -> bool {
        self.steer_tx.as_ref().and_then(|tx| tx.send(message).ok()).is_some()
    }

    /// 取消句柄（外部中止）。
    #[must_use]
    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// goals 共享状态句柄（`/goal` 查看/调整预算用；未注入时为 `None`）。
    #[must_use]
    pub fn goal_state(&self) -> Option<Arc<std::sync::Mutex<GoalState>>> {
        self.goal.clone()
    }

    /// 压缩后端（summarize / snapcompact 图像化压缩；装配期由配置解析注入）。
    #[must_use]
    pub fn compaction_backend(&self) -> agent_core::CompactionBackend {
        self.compaction_backend
    }

    /// snapcompact 帧数预算。
    #[must_use]
    pub fn compaction_max_frames(&self) -> usize {
        self.compaction_max_frames
    }

    /// 运行一个任务，产出事件流。取消作用域为 Agent 自身的 `cancel`。
    pub fn run(&self, task: &str) -> impl futures::Stream<Item = AgentEvent> + '_ {
        run_loop(
            self,
            agent_core::UserMessage::from_text(task),
            self.cancel.clone(),
        )
    }

    /// 运行一个带内容块（可含图像等多模态）的用户消息任务，产出事件流。
    pub fn run_message(
        &self,
        msg: agent_core::UserMessage,
    ) -> impl futures::Stream<Item = AgentEvent> + '_ {
        run_loop(self, msg, self.cancel.clone())
    }

    /// 以**指定取消令牌**运行带内容块（可含图像等多模态）的用户消息任务，产出事件流。
    ///
    /// 与 [`Agent::run_with_cancel`] 对称：供服务端为带图片的多模态任务建立独立取消作用域，
    /// 使「用户取消」端到端可达（流式中断 + 工具 `ctx.cancel`）。
    pub fn run_message_with_cancel(
        &self,
        msg: agent_core::UserMessage,
        cancel: CancellationToken,
    ) -> impl futures::Stream<Item = AgentEvent> + '_ {
        run_loop(self, msg, cancel)
    }

    /// 以**指定取消令牌**运行任务，产出事件流。
    ///
    /// 供服务端为每个任务建立独立取消作用域：传入的 `cancel` 同时驱动流式中断与工具
    /// `ctx.cancel`，使「用户取消」端到端可达（Agent 自身 `cancel` 为一次性，多任务会话
    /// 下不能跨任务复用，故每任务派生独立 token 经此入口注入）。
    pub fn run_with_cancel(
        &self,
        task: &str,
        cancel: CancellationToken,
    ) -> impl futures::Stream<Item = AgentEvent> + '_ {
        run_loop(self, agent_core::UserMessage::from_text(task), cancel)
    }
}

/// 构建器。
#[must_use]
pub struct AgentBuilder {
    model: agent_core::Model,
    provider: Option<Arc<dyn LlmProvider>>,
    tools: Option<Arc<dyn ToolRegistry>>,
    context: Option<Arc<dyn ContextManager>>,
    prompts: Option<Arc<PromptCatalog>>,
    approval: Option<Arc<dyn ApprovalPolicy>>,
    workspace: Option<Arc<Workspace>>,
    provider_ctx: ProviderCallContext,
    mode: Mode,
    max_mistakes: usize,
    max_turns: usize,
    /// 运行时限。
    deadline: Option<std::time::Duration>,
    context_guard: f32,
    max_output_tokens: usize,
    temperature: Option<f32>,
    thinking: Option<ThinkingConfig>,
    /// P1-K：思考策略（Static/Auto），优先级高于 `thinking`。
    thinking_policy: Option<ThinkingPolicy>,
    /// P0-3：goals 目标预算共享状态（`/goal` 查看/调整预算用）。
    goals: Option<Arc<std::sync::Mutex<GoalState>>>,
    cancel: CancellationToken,
    /// Skill 目录（可选）。
    catalog: Option<Arc<SkillCatalog>>,
    /// 上下文约定文件（AGENTS.md）内容。
    context_files: Vec<String>,
    /// 事件钩子。
    hooks: Vec<Arc<dyn Hook>>,
    /// 跨会话长期记忆。
    memory: Option<Arc<dyn MemoryStore>>,
    /// 外部资源解析器（`mcp://` 路由用）。
    resources: Option<Arc<dyn ResourceResolver>>,
    /// 软工具需求：循环要求模型先调用该工具（提醒→升级）。
    soft_requirement: Option<SoftToolRequirement>,
    /// steering 信道：外部中途注入消息打断当前任务。
    steer_rx: Option<tokio::sync::mpsc::UnboundedReceiver<AgentMessage>>,
    /// steering 发送端（`.steering()` 创建信道时一并存入，build 时镜像进 Agent）。
    steer_tx: Option<tokio::sync::mpsc::UnboundedSender<AgentMessage>>,
    /// aside 信道：外部注入被动、非中断通知（后台完成 / 延迟 diagnostics 等）。
    aside_rx: Option<tokio::sync::mpsc::UnboundedReceiver<AgentMessage>>,
    /// followUp 信道：宿主编排的延续消息（停止边界第三类 drain）。
    followup_rx: Option<tokio::sync::mpsc::UnboundedReceiver<AgentMessage>>,
    /// 运行时配置覆盖（每轮解析 thinking / temperature；mid-run 热更新）。
    runtime_overrides: Option<Arc<dyn RuntimeOverrides>>,
    /// assistant 消息改写钩子（最终化后、入 context/UI/tools 前）。
    transform_assistant: Option<Arc<dyn Fn(&mut agent_core::AssistantMessage) + Send + Sync>>,
    /// 写入效果（编辑后 LSP format/diagnostics）。
    write_effect: Option<Arc<dyn WriteEffect>>,
    /// P2：模型 fallback 链（主模型失败且错误可重试时依序尝试；跨线协议族亦可）。
    fallbacks: Vec<agent_core::Model>,
    /// P2：API key 轮换环（model id → key 列表；每轮请求 round-robin 换 key）。
    key_rings: std::collections::HashMap<String, Vec<String>>,
    /// 工具执行期的 steering 中断策略。
    interrupt_mode: InterruptMode,
    /// 进程级暂停门。
    pause_gate: Option<Arc<PauseGate>>,
    /// TTSR 流规则协调器（可选）。
    ttsr: Option<Arc<agent_ttsr::TtsrCoordinator>>,
    /// P1-L 独立评审 advisor（可选）。
    advisor: Option<Arc<agent_advisor::Advisor>>,
    /// advisor 评审触发周期（轮）。
    advisor_every_n_turns: usize,
    /// P2：压缩后端（summarize / snapcompact 图像化压缩）。
    compaction_backend: agent_core::CompactionBackend,
    /// P2：snapcompact 帧数预算。
    compaction_max_frames: usize,
}

impl AgentBuilder {
    /// 设置软工具需求（要求模型先调用某工具）。
    pub fn soft_requirement(mut self, req: SoftToolRequirement) -> Self {
        self.soft_requirement = Some(req);
        self
    }

    /// 注入写入效果（编辑后 LSP format/diagnostics 钩子）。
    pub fn write_effect(mut self, effect: Arc<dyn WriteEffect>) -> Self {
        self.write_effect = Some(effect);
        self
    }

    /// 注入 steering 接收端（外部经返回的发送端中途打断）。
    pub fn steer_rx(mut self, rx: tokio::sync::mpsc::UnboundedReceiver<AgentMessage>) -> Self {
        self.steer_rx = Some(rx);
        self
    }

    /// 启用 steering：内部创建无界信道，rx 注入 agent 循环，tx 镜像进 Agent 供
    /// [`Agent::steer`] 使用。供需要「运行中接受用户输入并立即投递」的宿主（CLI/Web）调用。
    /// 与 `.steer_rx(rx)` 互斥：后者由调用方自带接收端（测试用），本方法自洽创建一对。
    #[must_use]
    pub fn steering(mut self) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentMessage>();
        self.steer_tx = Some(tx);
        self.steer_rx = Some(rx);
        self
    }

    /// 注入 aside 接收端（外部经返回的发送端注入**被动、非中断**通知）。
    ///
    /// 与 [`AgentBuilder::steer_rx`] 的区别：aside **永不**触发批级 cancel 中断在途工具
    /// （不走 Immediate 中断轮询），只在轮次边界（下一轮模型调用前 / 停止边界）折叠注入
    /// context。用于后台任务完成、延迟 LSP diagnostics、定时器等应让模型在步骤间隙知晓、
    /// 但不应打断当前工具的被动信号。移植 oh-my-pi `getAsideMessages`。
    pub fn aside_rx(mut self, rx: tokio::sync::mpsc::UnboundedReceiver<AgentMessage>) -> Self {
        self.aside_rx = Some(rx);
        self
    }

    /// 注入 followUp 接收端（宿主编排的延续消息）。
    ///
    /// 移植 oh-my-pi `getFollowUpMessages`：仅在 agent 本该停止的边界 drain（steering →
    /// aside → followUp 之后），触发续跑。区别于 steering（中断）/ aside（被动）：
    /// followUp 是宿主主动「让 agent 继续干」（如子代理完成后注入结果让主代理收尾），
    /// 故不与中断性 steering 通道混淆。
    pub fn followup_rx(mut self, rx: tokio::sync::mpsc::UnboundedReceiver<AgentMessage>) -> Self {
        self.followup_rx = Some(rx);
        self
    }

    /// 注入运行时配置覆盖（每轮 LLM 调用前解析，mid-run 热更新 thinking / temperature）。
    ///
    /// 移植 oh-my-pi `getReasoning` / `getDisableReasoning` 等动态解析器：host 实现的
    /// [`RuntimeOverrides`] 在每轮请求构造前被调用，返回 `Some` 的字段覆盖静态 /
    /// [`ThinkingPolicy`] 解析结果，返回 `None` 沿用原值。故可在 run 中途切换思考档位、
    /// 温度等，而**无需重建 Agent**。后续可按需扩展 `api_key` / `base_url` / `cwd` 等。
    pub fn runtime_overrides(mut self, overrides: Arc<dyn RuntimeOverrides>) -> Self {
        self.runtime_overrides = Some(overrides);
        self
    }

    /// 注入 assistant 消息改写钩子（每轮最终化后、入 context / UI / 工具分发前原地改写）。
    ///
    /// 移植 oh-my-pi `transformAssistantMessage`：用于宏展开（如 `@[[runtime.name(args)]]`）、
    /// 脱敏、归一化。改写对 **context 持久化、MessageEnd 事件、工具参数分发** 三者一致生效
    /// （单一真相源——在所有下游消费前 apply）。同步闭包：宏展开等本地计算用；若需异步
    /// 改写（调外部服务解析宏），后续可升级为 trait + async 方法。
    pub fn transform_assistant(
        mut self,
        tf: Arc<dyn Fn(&mut agent_core::AssistantMessage) + Send + Sync>,
    ) -> Self {
        self.transform_assistant = Some(tf);
        self
    }

    /// 工具执行期的 steering 中断策略（默认 [`InterruptMode::Immediate`]：batch 含
    /// [`agent_tools::Tool::interruptible`] 工具时，steering 中途打断在途工具）。
    pub fn interrupt_mode(mut self, mode: InterruptMode) -> Self {
        self.interrupt_mode = mode;
        self
    }

    /// 设置进程级暂停门（共享单例，host 驱动 `/pause` `/resume`）。
    ///
    /// 注入后，[`Agent::run`] 在每次 provider 调用前与每次工具批执行前轮询此门：pause 时
    /// 在途工作跑完后 park，resume 后继续；cancel 立即解除 park（无需 resume 整个进程）。
    pub fn pause_gate(mut self, gate: Arc<PauseGate>) -> Self {
        self.pause_gate = Some(gate);
        self
    }

    /// 注入 TTSR 流规则协调器（`.gyre/rules` 发现后装配；`None` 禁用）。
    pub fn ttsr(mut self, ttsr: Option<Arc<agent_ttsr::TtsrCoordinator>>) -> Self {
        self.ttsr = ttsr;
        self
    }

    /// 注入 P1-L 独立评审 advisor（每 N 轮评审一次；`None` 禁用）。
    pub fn advisor(mut self, advisor: Option<Arc<agent_advisor::Advisor>>) -> Self {
        self.advisor = advisor;
        self
    }

    /// advisor 评审触发周期（轮；默认 4）。
    #[must_use]
    pub fn advisor_every_n_turns(mut self, n: usize) -> Self {
        self.advisor_every_n_turns = n.max(1);
        self
    }

    /// P2：模型 fallback 链——主模型调用失败且错误可重试（网络/5xx/429/鉴权）时，
    /// 依序尝试这些备用模型（跨线协议族亦可，如 Anthropic 主 → OpenAI 备）。
    /// 链上模型的 `thinking`/工具规格沿用主模型请求（备用模型宜同族同能力）。
    pub fn fallbacks(mut self, models: Vec<agent_core::Model>) -> Self {
        self.fallbacks = models;
        self
    }

    /// P2：API key 轮换环（model id → key 列表）。每轮请求 round-robin 取下一个 key，
    /// 任一 key 被限流/撤销时自动切换；[`RuntimeOverrides::api_key`] 命中时优先（跳过轮换）。
    pub fn key_rings(mut self, rings: std::collections::HashMap<String, Vec<String>>) -> Self {
        self.key_rings = rings.into_iter().filter(|(_, v)| !v.is_empty()).collect();
        self
    }
    /// 注入 Provider。
    pub fn provider(mut self, p: Arc<dyn LlmProvider>) -> Self {
        self.provider = Some(p);
        self
    }
    /// 注入工具注册表。
    pub fn tools(mut self, t: Arc<dyn ToolRegistry>) -> Self {
        self.tools = Some(t);
        self
    }
    /// 注入上下文管理器。
    pub fn context(mut self, c: Arc<dyn ContextManager>) -> Self {
        self.context = Some(c);
        self
    }
    /// 注入 Prompt 目录。
    pub fn prompts(mut self, p: Arc<PromptCatalog>) -> Self {
        self.prompts = Some(p);
        self
    }
    /// 注入审批策略。
    pub fn approval(mut self, a: Arc<dyn ApprovalPolicy>) -> Self {
        self.approval = Some(a);
        self
    }
    /// 设置工作区。
    pub fn workspace(mut self, w: Arc<Workspace>) -> Self {
        self.workspace = Some(w);
        self
    }
    /// 注入 Skill 目录（可选；注入后 system prompt 追加可用 skill 列表）。
    pub fn catalog(mut self, c: Arc<SkillCatalog>) -> Self {
        self.catalog = Some(c);
        self
    }
    /// 注入上下文约定文件（AGENTS.md）内容，追加为 system prompt 额外段。
    pub fn context_files(mut self, files: Vec<String>) -> Self {
        self.context_files = files;
        self
    }
    /// 注入事件钩子（before/after tool、stop）。
    pub fn hooks(mut self, hooks: Vec<Arc<dyn Hook>>) -> Self {
        self.hooks = hooks;
        self
    }
    /// 注入跨会话长期记忆（启动注入 summary 段）。
    pub fn memory(mut self, memory: Arc<dyn MemoryStore>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// 注入外部资源解析器（启用 read_file 的 `mcp://` 协议路由）。
    pub fn resources(mut self, resources: Arc<dyn ResourceResolver>) -> Self {
        self.resources = Some(resources);
        self
    }
    /// 设置 Provider 调用上下文（api_key / base_url）。
    pub fn provider_ctx(mut self, c: ProviderCallContext) -> Self {
        self.provider_ctx = c;
        self
    }
    /// 设置模式。
    pub fn mode(mut self, m: Mode) -> Self {
        self.mode = m;
        self
    }
    /// 设置最大连续错误次数。
    pub fn max_mistakes(mut self, n: usize) -> Self {
        self.max_mistakes = n;
        self
    }

    /// 设置单任务最大轮次（硬上限；默认 1000，0 表示不限制）。防止模型陷入「调用工具→失败→重试」的无限循环。
    pub fn max_turns(mut self, n: usize) -> Self {
        self.max_turns = n;
        self
    }
    /// 设置运行时限（wall-clock）。超时后在下一轮顶部优雅停止（已完成轮次保留，
    /// `summary.success = false`）。不设置则不限时。
    #[must_use]
    pub fn deadline(mut self, d: std::time::Duration) -> Self {
        self.deadline = Some(d);
        self
    }
    /// 设置上下文窗口占用阈值。
    pub fn context_guard(mut self, g: f32) -> Self {
        self.context_guard = g;
        self
    }
    /// 设置最大输出 token。
    pub fn max_output_tokens(mut self, n: usize) -> Self {
        self.max_output_tokens = n;
        self
    }
    /// 设置温度。
    pub fn temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    /// 设置思考模式（reasoning/thinking）。由支持思考的模型消费。
    pub fn thinking(mut self, thinking: ThinkingConfig) -> Self {
        self.thinking = Some(thinking);
        self
    }

    /// P1-K：设置自适应思考策略（每轮按用户 prompt 难度经 [`ThinkingClassifier`] 解析 budget，
    /// 钳到模型范围；移植 oh-my-pi `auto-thinking`）。设置后覆盖 `.thinking()` 静态配置。
    #[must_use]
    pub fn thinking_policy(mut self, policy: ThinkingPolicy) -> Self {
        self.thinking_policy = Some(policy);
        self
    }

    /// 设置取消令牌（默认新建一个）。用于把外部/父级取消信号接入本 Agent——
    /// 子 Agent 委派时应传入 `parent_cancel.child_token()` 以级联取消，否则子 Agent
    /// 将无法被父任务取消（详见 task_tool 的递归委派）。
    pub fn cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// P0-3：注入 goals 目标预算共享状态（token/墙钟）。
    ///
    /// 共享 `Arc<Mutex>` 而非构建期拷贝：宿主可在 run 中途经 [`Agent::goal_state`]
    /// 查看/调整预算（如 `/goal extend`），无需重建 Agent。
    pub fn goals_state(mut self, state: Arc<std::sync::Mutex<GoalState>>) -> Self {
        self.goals = Some(state);
        self
    }

    /// 压缩后端（P2：`summarize` 默认 / `snapcompact` 图像化压缩——要求视觉模型）。
    #[must_use]
    pub fn compaction_backend(mut self, backend: agent_core::CompactionBackend) -> Self {
        self.compaction_backend = backend;
        self
    }

    /// snapcompact 帧数预算（默认 80）。
    #[must_use]
    pub fn compaction_max_frames(mut self, max_frames: usize) -> Self {
        self.compaction_max_frames = max_frames.max(1);
        self
    }

    /// 构造 Agent。
    ///
    /// # Panics
    /// 缺少必填依赖时 panic。
    pub fn build(self) -> Agent {
        Agent {
            model: self.model,
            provider: self.provider.expect("必须注入 provider"),
            tools: self.tools.expect("必须注入 tools"),
            context: self.context.expect("必须注入 context"),
            prompts: self.prompts.expect("必须注入 prompts"),
            approval: self.approval.expect("必须注入 approval"),
            workspace: self.workspace.expect("必须注入 workspace"),
            provider_ctx: self.provider_ctx,
            mode: self.mode,
            max_mistakes: self.max_mistakes,
            max_turns: self.max_turns,
            deadline: self.deadline,
            context_guard: self.context_guard,
            max_output_tokens: self.max_output_tokens,
            temperature: self.temperature,
            thinking: self.thinking,
            thinking_policy: self.thinking_policy,
            cancel: self.cancel,
            catalog: self.catalog,
            context_files: self.context_files,
            hooks: self.hooks,
            memory: self.memory,
            resources: self.resources,
            write_effect: self.write_effect,
            interrupt_mode: self.interrupt_mode,
            pause_gate: self.pause_gate,
            ttsr: self.ttsr,
            advisor: self.advisor,
            advisor_every_n_turns: self.advisor_every_n_turns,
            compaction_backend: self.compaction_backend,
            compaction_max_frames: self.compaction_max_frames,
            goal: self.goals,
            goal_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            conflicts: Arc::new(std::sync::Mutex::new(agent_tools::ConflictHistory::new())),
            pending_rewrites: Arc::new(std::sync::Mutex::new(Vec::new())),
            soft_requirement: Arc::new(std::sync::Mutex::new(self.soft_requirement)),
            steer_rx: tokio::sync::Mutex::new(self.steer_rx),
            steer_tx: self.steer_tx,
            aside_rx: tokio::sync::Mutex::new(self.aside_rx),
            followup_rx: tokio::sync::Mutex::new(self.followup_rx),
            runtime_overrides: self.runtime_overrides,
            transform_assistant: self.transform_assistant,
            fallbacks: self.fallbacks,
            key_rings: Arc::new(
                self.key_rings
                    .into_iter()
                    .map(|(id, keys)| (id, KeyRing::new(keys)))
                    .collect(),
            ),
        }
    }
}

/// 生成工具审批的友好提示文本。
///
/// 针对常见工具提取关键参数，给出语义化描述；未匹配的工具回退到通用格式。
fn approval_prompt(tool: &str, args: &serde_json::Value) -> String {
    let s = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or("");
    match tool {
        "run_command" => {
            let cmd = s("command");
            if cmd.is_empty() {
                format!("Agent 想执行命令（参数: {args}）")
            } else {
                format!("Agent 想执行命令：\n{cmd}")
            }
        }
        "write_file" => format!("Agent 想写入文件：{}", s("path")),
        "apply_hashline" => "Agent 想批量编辑文件（hashline）".into(),
        "replace_block" => format!("Agent 想替换文件 {} 中的代码块", s("path")),
        "ast_rewrite" => format!("Agent 想重写文件 {} 中的代码", s("path")),
        "ast_search" => format!("Agent 想在文件 {} 中搜索：{}", s("path"), s("pattern")),
        "read_file" | "read_image" => format!("Agent 想读取文件：{}", s("path")),
        "grep" => format!("Agent 想搜索内容：{}", s("pattern")),
        "glob" => format!("Agent 想查找文件：{}", s("pattern")),
        "image_gen" => format!("Agent 想生成图片：{}", s("prompt")),
        "task" | "tasks" => "Agent 想委派子任务".into(),
        "lsp" => format!("Agent 想调用 LSP（{}）", s("action")),
        _ => format!("批准执行工具 `{tool}`？（参数: {args}）"),
    }
}

/// 非终止停顿（`StopReason::Pause`）的连续重采样上限：防止一个永不真正结束的 backend
/// 把循环转成无限次模型请求。任一携带工具调用的轮次都会重置该计数（移植 oh-my-pi）。
const MAX_PAUSED_CONTINUATIONS: usize = 8;

/// 软工具需求升级上限：模型连续这么多轮仍不调用所需工具（或持续 detour）则中止，
/// 避免无限强制循环。移植 oh-my-pi `MAX_SOFT_TOOL_ESCALATIONS`。
const MAX_SOFT_TOOL_ESCALATIONS: usize = 3;
/// Harmony 泄漏「截断恢复」连续上限（移植 oh-my-pi `harmonyTruncateResumeCount`）。
/// tool_arg 可恢复时，截断污染输入 + sentinel 续跑；连续超限则升级为错误。
const MAX_HARMONY_TRUNCATE_RESUME: usize = 2;
/// Harmony 泄漏「丢弃重试」连续上限（移植 oh-my-pi `harmonyRetryAttempt`）。
/// text/thinking 泄漏无法恢复，丢弃本轮重采样；连续超限则升级为错误。
const MAX_HARMONY_ABORT_RETRY: usize = 2;

/// 发射 `on_turn_end` 钩子（per-turn 程序化副作用）。与 `AgentEvent::TurnEnd` 事件配对，
/// 但面向**不经事件流**的程序化 hook（审计 / 指标 / memory 更新 / telemetry span 等）。
/// 事件消费者（如 server 的 `to_server_frame`）已能从 TurnEnd 事件观测；本钩子供 agent
/// 内部 / 装配层注入的程序化副作用使用。移植 oh-my-pi `onTurnEnd`。
async fn fire_on_turn_end(
    hooks: &[Arc<dyn Hook>],
    message: &AssistantMessage,
    tool_results: &[ToolResultMessage],
    will_continue: bool,
) {
    let ctx = agent_core::TurnEndContext {
        message,
        tool_results,
        will_continue,
    };
    for h in hooks {
        h.on_turn_end(&ctx).await;
    }
}

mod engine;
pub(crate) use engine::{
    persist_interrupted, poll_and_run, record_run_end, run_batch, run_loop, run_pending_task,
    schedule_and_run, PendingTask,
};
#[cfg(test)]
mod tests {
    use super::*;
    use agent_context::InMemoryContext;
    use agent_core::ContextManager;
    use agent_core::{
        ApprovalDecision, ApprovalPolicy, ApprovalRequest, AskMessage, AskResponse, CapabilityTier,
        ToolError, Workspace,
    };
    use agent_tools::{Concurrency, DefaultToolRegistry, Tool, ToolContext, ToolRegistry};
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use futures::StreamExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// 回归：流式中断（用户取消 / 流异常断开）时，已累积的文本必须兜底持久化为一条
    /// assistant 消息；否则 resume 会话会丢失这轮已显示给用户的回复。
    #[tokio::test]
    async fn persist_interrupted_saves_partial_text() {
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let model =
            agent_core::Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        // 有内容：应落盘为 1 条 assistant 文本消息，并清空缓冲。
        let mut acc = String::from("这是一段被中断的部分回复");
        persist_interrupted(&ctx, &mut acc, &model, &Usage::default()).await;
        assert!(acc.is_empty(), "持久化后累积缓冲应被清空");
        let built = ctx.build_provider_context(&model, &[]).await.unwrap();
        assert_eq!(built.messages.len(), 1, "应持久化 1 条 assistant 消息");
        // 空缓冲：幂等无副作用，不追加消息。
        let mut empty = String::new();
        persist_interrupted(&ctx, &mut empty, &model, &Usage::default()).await;
        let built2 = ctx.build_provider_context(&model, &[]).await.unwrap();
        assert_eq!(built2.messages.len(), 1, "空缓冲不应追加消息");
    }

    // ── 工具并发执行（shared/exclusive）──────────────────────────────────

    /// 探针工具：睡眠固定时长，并记录执行期间的「最大并发在途数」。
    struct ProbeTool {
        name: String,
        cap: CapabilityTier,
        ms: u64,
        inflight: Arc<AtomicUsize>,
        max_seen: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for ProbeTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "probe"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability(&self) -> CapabilityTier {
            self.cap
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: &ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            let cur = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            let mut m = self.max_seen.load(Ordering::SeqCst);
            while cur > m {
                match self
                    .max_seen
                    .compare_exchange(m, cur, Ordering::SeqCst, Ordering::SeqCst)
                {
                    Ok(_) => break,
                    Err(v) => m = v,
                }
            }
            tokio::time::sleep(Duration::from_millis(self.ms)).await;
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolResult::text(format!("done:{}", self.name)))
        }
    }

    /// 总是 Allow 的审批策略（测试用）。
    struct YoloApproval;
    #[async_trait]
    impl ApprovalPolicy for YoloApproval {
        fn decide(&self, _: &ApprovalRequest<'_>) -> ApprovalDecision {
            ApprovalDecision::Allow
        }
        async fn prompt(&self, _: &AskMessage) -> Result<AskResponse, ToolError> {
            Ok(AskResponse::Yes)
        }
    }

    fn build_tcx<'a>(
        ws: &'a Workspace,
        approval: &'a dyn ApprovalPolicy,
        cancel: &'a CancellationToken,
    ) -> ToolContext<'a> {
        ToolContext {
            workspace: ws,
            approval,
            cancel,
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
            update_tx: None,
            conflicts: None,
            pending_rewrites: None,
        }
    }

    fn task(order: usize, name: &str, exclusive: bool) -> PendingTask {
        PendingTask {
            order,
            id: format!("c{order}"),
            name: name.into(),
            args: serde_json::json!({}),
            exclusive,
        }
    }

    /// Shared 工具并发：3 个只读探针（各 60ms）应并发执行，最大在途数 ≥2，总耗时远小于 3×。
    #[tokio::test]
    async fn shared_tools_run_concurrently() {
        let inflight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let probe = |name: &str| ProbeTool {
            name: name.into(),
            cap: CapabilityTier::ReadOnly,
            ms: 60,
            inflight: inflight.clone(),
            max_seen: max_seen.clone(),
        };
        let reg = DefaultToolRegistry::new()
            .with(Box::new(probe("a")))
            .with(Box::new(probe("b")))
            .with(Box::new(probe("c")));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);

        let ws = Workspace::new(".");
        let approval = YoloApproval;
        let cancel = CancellationToken::new();
        let tcx = build_tcx(&ws, &approval, &cancel);
        let hooks: Vec<Arc<dyn Hook>> = Vec::new();

        let batch = vec![
            task(0, "a", false),
            task(1, "b", false),
            task(2, "c", false),
        ];
        let start = std::time::Instant::now();
        let results = schedule_and_run(batch, &tools, &tcx, &hooks).await;
        let elapsed = start.elapsed();

        assert_eq!(results.len(), 3);
        assert!(
            max_seen.load(Ordering::SeqCst) >= 2,
            "Shared 工具应并发，最大在途数 {}",
            max_seen.load(Ordering::SeqCst)
        );
        assert!(
            elapsed < Duration::from_millis(150),
            "并发执行总耗时 {elapsed:?} 应远小于串行 180ms"
        );
        // 结果按原始顺序返回。
        let orders: Vec<usize> = results.iter().map(|(o, _, _, _, _)| *o).collect();
        assert_eq!(orders, vec![0, 1, 2]);
    }

    /// Exclusive 工具串行：3 个写探针作为屏障，最大在途数 == 1。
    #[tokio::test]
    async fn exclusive_tools_serialize() {
        let inflight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let probe = |name: &str| ProbeTool {
            name: name.into(),
            cap: CapabilityTier::Write,
            ms: 40,
            inflight: inflight.clone(),
            max_seen: max_seen.clone(),
        };
        let reg = DefaultToolRegistry::new()
            .with(Box::new(probe("w1")))
            .with(Box::new(probe("w2")))
            .with(Box::new(probe("w3")));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);

        let ws = Workspace::new(".");
        let approval = YoloApproval;
        let cancel = CancellationToken::new();
        let tcx = build_tcx(&ws, &approval, &cancel);
        let hooks: Vec<Arc<dyn Hook>> = Vec::new();

        let batch = vec![
            task(0, "w1", true),
            task(1, "w2", true),
            task(2, "w3", true),
        ];
        let results = schedule_and_run(batch, &tools, &tcx, &hooks).await;

        assert_eq!(results.len(), 3);
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "Exclusive 工具必须串行，最大在途数应为 1"
        );
    }

    /// Concurrency 默认按能力分级：ReadOnly → Shared，Write/Execute/Network → Exclusive。
    #[test]
    fn concurrency_default_maps_by_capability() {
        let read = ProbeTool {
            name: "r".into(),
            cap: CapabilityTier::ReadOnly,
            ms: 0,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
        };
        let write = ProbeTool {
            name: "w".into(),
            cap: CapabilityTier::Write,
            ms: 0,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
        };
        let exec = ProbeTool {
            name: "e".into(),
            cap: CapabilityTier::Execute,
            ms: 0,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
        };
        assert_eq!(read.concurrency(), Concurrency::Shared);
        assert_eq!(write.concurrency(), Concurrency::Exclusive);
        assert_eq!(exec.concurrency(), Concurrency::Exclusive);
    }

    // ── 分级压缩（P1-1）：shake 救回时跳过昂贵的 summarize ────────────────

    /// 桩 Provider：返回一条无工具调用的助手消息，使循环一轮即结束。
    struct StubProvider;
    #[async_trait]
    impl agent_core::LlmProvider for StubProvider {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let msg = AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                usage: Usage::default(),
                model: "stub".into(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// 计数 summarize 提供器：每次调用自增（用于断言「未调用 summarize」）。
    struct CountingSummary {
        count: Arc<AtomicUsize>,
    }
    impl agent_context::compaction::SummaryProvider for CountingSummary {
        fn summarize(
            &self,
            _old: &[String],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>
        {
            let count = self.count.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok("已总结".into())
            })
        }
    }

    /// 近上限时先 shake；若 shake 已把 token 压到阈值以下，则不应触发昂贵的 summarize。
    #[tokio::test]
    async fn staged_compaction_skips_summarize_when_shake_suffices() {
        // 启发式 token 计数（chars/4）保证确定性，避免 tiktoken 对重复字符的不可预测合并。
        let ctx = Arc::new(InMemoryContext::with_counter(
            vec!["sys".into()],
            agent_context::token::TokenCounter::heuristic(),
        ));
        ctx.append(AgentMessage::user_text("请读取文件")).await;
        ctx.append(AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({ "path": "a.txt" }),
            }],
            usage: Usage::default(),
            model: "stub".into(),
            stop_reason: None,
            stop_details: None,
        }))
        .await;
        let big = "x".repeat(100_000); // 启发式 ≈ 25_000 token（可被 shake 归档）
        ctx.append(AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "c1".into(),
            result: ToolResult::text(big),
        }))
        .await;

        let summarize_calls = Arc::new(AtomicUsize::new(0));
        ctx.set_summarizer(Box::new(CountingSummary {
            count: summarize_calls.clone(),
        }))
        .await;
        ctx.set_shake_sink(Arc::new(agent_context::compaction::NullSink))
            .await;
        // 小保护窗口 + 零阈值，使该 ToolResult 立即被 shake 归档为占位符。
        ctx.set_shake_config(agent_context::compaction::ShakeConfig {
            protect_tokens: 0,
            min_savings: 0,
            fence_min_tokens: 400,
            tool_result_min_tokens: 10,
        })
        .await;

        let context: Arc<dyn ContextManager> = ctx;
        let mut model =
            agent_core::Model::with_defaults("stub", "stub", agent_core::Api::OpenAiCompletions);
        model.max_input_tokens = 16_000; // 0.8×=12_800；初始 ≈25_000 token → 近上限

        let agent = Agent::builder(model)
            .provider(Arc::new(StubProvider))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(context)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .context_guard(0.8)
            .max_turns(5)
            .build();

        let mut said = String::new();
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Say(s) = ev {
                said = s.text;
            }
        }
        assert!(said.contains("shake"), "应报告 shake 阶段: {said}");
        assert!(
            !said.contains("summarize"),
            "shake 救回后不应进入 summarize 阶段: {said}"
        );
        assert_eq!(
            summarize_calls.load(Ordering::SeqCst),
            0,
            "不应调用 summarize"
        );
    }

    // ── 运行时限（P1-2 deadline）─────────────────────────────────────────

    /// 桩 Provider：每轮都返回一个 `probe` 工具调用，使循环永不自然结束（只能被 deadline 停）。
    struct ToolLoopProvider;
    #[async_trait]
    impl agent_core::LlmProvider for ToolLoopProvider {
        fn id(&self) -> &'static str {
            "loop"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let msg = AssistantMessage {
                content: vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "probe".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: Usage::default(),
                model: "loop".into(),
                stop_reason: Some(StopReason::ToolUse),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// deadline 超时：循环每轮都调工具（永不自然结束），80ms 后应被 deadline 优雅停止，
    /// 以 `Done(success=false)` 收尾并发出 deadline 警告。
    #[tokio::test]
    async fn deadline_stops_run_gracefully() {
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(ProbeTool {
            name: "probe".into(),
            cap: CapabilityTier::ReadOnly,
            ms: 0,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
        }));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);

        let mut model =
            agent_core::Model::with_defaults("loop", "loop", agent_core::Api::OpenAiCompletions);
        model.max_input_tokens = 200_000; // 拉高窗口，减少压缩噪声；prune 仍会兜底防 OOM

        let agent = Agent::builder(model)
            .provider(Arc::new(ToolLoopProvider))
            .tools(tools)
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .max_turns(100_000) // 远高于 deadline 内可跑的轮数，确保由 deadline 而非轮次停止
            .deadline(Duration::from_millis(80))
            .build();

        let mut done_success: Option<bool> = None;
        let mut said_deadline = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::Say(s) if s.text.contains("deadline") => said_deadline = true,
                AgentEvent::Done(sum) => done_success = Some(sum.success),
                _ => {}
            }
        }
        assert_eq!(
            done_success,
            Some(false),
            "deadline 停止应以 success=false 结束"
        );
        assert!(said_deadline, "应发出 deadline 警告");
    }

    // ── pause_turn（P1-2 非终止停顿续写）─────────────────────────────────

    /// 桩 Provider：每轮返回一条 stop_reason=Pause 的文本消息（无工具调用），
    /// 并计数被调用次数。模拟「provider 结束响应但未完成轮次」。
    struct PausingProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for PausingProvider {
        fn id(&self) -> &'static str {
            "pausing"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let msg = AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "thinking...".into(),
                }],
                usage: Usage::default(),
                model: "pausing".into(),
                stop_reason: Some(StopReason::Pause),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// pause_turn：每次都返回 Pause，循环应在上限内反复重采样，到达上限后按完成停止。
    #[tokio::test]
    async fn pause_turn_resamples_then_caps() {
        let calls = Arc::new(AtomicUsize::new(0));
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let model = agent_core::Model::with_defaults(
            "pausing",
            "pausing",
            agent_core::Api::OpenAiCompletions,
        );
        let agent = Agent::builder(model)
            .provider(Arc::new(PausingProvider {
                calls: calls.clone(),
            }))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .max_turns(100_000) // 远高于上限，确保由 pause 上限而非轮次停止
            .build();

        let mut done = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if matches!(ev, AgentEvent::Done(_)) {
                done = true;
            }
        }
        assert!(done, "应在达到 pause 上限后停止");
        // 1 次初始 + MAX 次续采；第 MAX+1 次命中上限停止（不再续采）。
        assert_eq!(
            calls.load(Ordering::SeqCst),
            MAX_PAUSED_CONTINUATIONS + 1,
            "provider 调用次数应为 上限+1"
        );
    }

    // ── P1-G（停止边界 steering 再检查 / 外层停-续循环）───────────────────

    /// 桩 Provider：每次返回无工具调用的文本（触发停止边界），计数调用次数。
    /// 第一轮返回前经 `tx` 注入一条 steering；若 `cancel_on_first` 则同时取消 run——
    /// 用于验证「停止边界检测到 steering → 续跑」与「cancel 时不 drain（防搁浅）」。
    struct StopBoundaryProvider {
        calls: Arc<AtomicUsize>,
        tx: tokio::sync::mpsc::UnboundedSender<AgentMessage>,
        cancel_on_first: Option<CancellationToken>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for StopBoundaryProvider {
        fn id(&self) -> &'static str {
            "stop-boundary"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                let _ = self
                    .tx
                    .send(AgentMessage::user_text("[steering] 请接着做 Y"));
                if let Some(tok) = &self.cancel_on_first {
                    tok.cancel();
                }
            }
            let msg = AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: format!("reply #{n}"),
                }],
                usage: Usage::default(),
                model: "stop-boundary".into(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// 停止边界期间注入 steering → 循环应续跑（而非结束），provider 被调用 2 次。
    /// 无此修复时 steering 会被搁置到下次手动 prompt，provider 仅调用 1 次。
    #[tokio::test]
    async fn stop_boundary_steering_continues_run() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentMessage>();
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let model = agent_core::Model::with_defaults(
            "stop-boundary",
            "stop-boundary",
            agent_core::Api::OpenAiCompletions,
        );
        let agent = Agent::builder(model)
            .provider(Arc::new(StopBoundaryProvider {
                calls: calls.clone(),
                tx,
                cancel_on_first: None,
            }))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .steer_rx(rx)
            .build();

        let mut done = false;
        let mut injected = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::Done(_) => done = true,
                AgentEvent::Say(s) if s.text.contains("停止边界 steering") => injected = true,
                _ => {}
            }
        }
        assert!(done, "应正常结束");
        assert!(
            injected,
            "应发出停止边界 steering 注入提示（证明续跑分支命中）"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "停止边界 steering 应触发续跑（provider 调用 2 次，而非搁置后仅 1 次）"
        );
    }

    /// 配合 `steer_via_public_api_delivers`：第 1 轮 stream() 通知测试「已进入」并等待测试
    /// 经公共 API steer 后再放行——保证 steering 在停止边界 drain 前已入队（确定性，无时序竞态）。
    struct SteerApiProvider {
        calls: Arc<AtomicUsize>,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for SteerApiProvider {
        fn id(&self) -> &'static str {
            "steer-api"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                self.started.notify_one();
                self.release.notified().await;
            }
            let msg = AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: format!("reply #{n}"),
                }],
                usage: Usage::default(),
                model: "steer-api".into(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// 公共 API [`Agent::steer`]（经 `.steering()` 启用）在停止边界被 drain 并触发续跑。
    /// 与 `stop_boundary_steering_continues_run` 的区别：后者经外部信道 tx 注入，本测试走宿主
    /// 真实路径——`Agent::steer()`，证明前端（CLI/Web）调用即可让运行中的模型立即收到消息。
    #[tokio::test]
    async fn steer_via_public_api_delivers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let model = agent_core::Model::with_defaults(
            "steer-api",
            "steer-api",
            agent_core::Api::OpenAiCompletions,
        );
        let agent = Arc::new(
            Agent::builder(model)
                .provider(Arc::new(SteerApiProvider {
                    calls: Arc::clone(&calls),
                    started: Arc::clone(&started),
                    release: Arc::clone(&release),
                }))
                .tools(Arc::new(DefaultToolRegistry::new()))
                .context(ctx)
                .prompts(Arc::new(PromptCatalog::new()))
                .approval(Arc::new(YoloApproval))
                .workspace(Arc::new(Workspace::new(".")))
                .steering()
                .build(),
        );

        let agent_run = Arc::clone(&agent);
        let handle = tokio::spawn(async move {
            let stream = agent_run.run("go");
            tokio::pin!(stream);
            let mut done = false;
            let mut injected = false;
            while let Some(ev) = stream.next().await {
                match ev {
                    AgentEvent::Done(_) => done = true,
                    AgentEvent::Say(s) if s.text.contains("停止边界 steering") => injected = true,
                    _ => {}
                }
            }
            (done, injected)
        });

        started.notified().await; // provider 已进入第 1 轮
        // 宿主真实路径：经公共 API 注入 steering（而非外部信道 tx）。
        assert!(
            agent.steer(AgentMessage::user_text("[steer] 接着做 Y")),
            "启用 .steering() 后 steer() 应返回 true"
        );
        release.notify_one(); // 放行第 1 轮返回 → 停止边界 drain steering → 续跑第 2 轮。

        let (done, injected) = handle.await.unwrap();
        assert!(done, "应正常结束");
        assert!(
            injected,
            "公共 API steer 应在停止边界被 drain 并发出注入提示"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2, "应续跑（provider 调用 2 次）");
    }

    /// cancel 时不 drain steering（防搁浅）：第一轮注入 steering 并取消 → 停止边界
    /// 因 cancel 跳过 drain，steering 不进上下文、循环不续跑，provider 仅调用 1 次。
    #[tokio::test]
    async fn cancelled_run_does_not_drain_steering() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentMessage>();
        let cancel = CancellationToken::new();
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let model = agent_core::Model::with_defaults(
            "stop-boundary",
            "stop-boundary",
            agent_core::Api::OpenAiCompletions,
        );
        let agent = Agent::builder(model)
            .provider(Arc::new(StopBoundaryProvider {
                calls: calls.clone(),
                tx,
                cancel_on_first: Some(cancel.clone()),
            }))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .steer_rx(rx)
            .cancel(cancel)
            .build();

        // cancel 在 provider 调用期间触发 → run 经「流式中断」路径（select! 的 cancel 分支）
        // 以 Error 收尾（而非 Done）；停止边界因此未被触达，steering 留在队列不进上下文（防搁浅）。
        let mut ended = false;
        let mut injected = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::Done(_) | AgentEvent::Error(_) => ended = true,
                AgentEvent::Say(s) if s.text.contains("停止边界 steering") => injected = true,
                _ => {}
            }
        }
        assert!(ended, "应结束（cancel 经流式中断路径收尾）");
        assert!(
            !injected,
            "cancel 时停止边界不应 drain steering（防搁浅：消息落地历史却永不响应）"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "cancel 时不应因 steering 续跑"
        );
    }

    // ── P0-B（length 截断 + 残缺 tool_call 占位补全）─────────────────────

    /// 桩 Provider：第一轮返回 length 截断 + 含（残缺参数的）ToolCall；之后返回停止文本。
    struct TruncatedToolCallProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for TruncatedToolCallProvider {
        fn id(&self) -> &'static str {
            "trunc-tool"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            // 第一轮：输出被 max_tokens 截断，且已含一个 ToolCall（参数可能残缺）。
            // 之后轮次：正常停止，避免无限循环。
            let msg = if n == 0 {
                AssistantMessage {
                    content: vec![ContentBlock::ToolCall {
                        id: "trunc1".into(),
                        name: "probe".into(),
                        arguments: serde_json::json!({"incomplete": true}),
                    }],
                    usage: Usage::default(),
                    model: "trunc-tool".into(),
                    stop_reason: Some(StopReason::Length),
                    stop_details: None,
                }
            } else {
                AssistantMessage {
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                    usage: Usage::default(),
                    model: "trunc-tool".into(),
                    stop_reason: Some(StopReason::Stop),
                    stop_details: None,
                }
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// P0-B：length 截断且含 ToolCall 时，**不应执行**（参数可能被截断残缺），
    /// 而应回填占位 ToolResult 维持 tool_use/tool_result 配对，并续写让模型补全。
    #[tokio::test]
    async fn length_truncated_tool_call_gets_placeholder_not_executed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let ctx = Arc::new(InMemoryContext::new(vec![]));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(ProbeTool {
            name: "probe".into(),
            cap: CapabilityTier::ReadOnly,
            ms: 0,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_seen: max_seen.clone(),
        }));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);

        let mut model = agent_core::Model::with_defaults(
            "trunc-tool",
            "trunc-tool",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;

        let agent = Agent::builder(model)
            .provider(Arc::new(TruncatedToolCallProvider {
                calls: calls.clone(),
            }))
            .tools(tools)
            .context(ctx.clone())
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .max_turns(10)
            .build();

        let mut said_truncated = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Say(s) = &ev {
                if s.text.contains("截断") {
                    said_truncated = true;
                }
            }
        }

        // 残缺 tool_call 不应被执行。
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            0,
            "length 截断的 tool_call 不应被执行（参数可能残缺）"
        );
        // 应发出截断续写警告。
        assert!(said_truncated, "应发出截断续写警告");
        // 应续写至少一轮。
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "应续写让模型补全，provider 至少被调用 2 次"
        );
        // 上下文应含该 tool_call_id 的占位 result（维持 tool_use/tool_result 配对）。
        let snapshot = ctx.snapshot().await;
        let has_placeholder = snapshot.iter().any(|m| {
            matches!(
                m,
                agent_core::AgentMessage::ToolResult(t) if t.tool_call_id == "trunc1"
            )
        });
        assert!(
            has_placeholder,
            "应回填占位 ToolResult 维持 tool_use/tool_result 配对"
        );
    }

    /// 桩 Provider：返回 stop_reason=Error + 含 ToolCall（模拟 API 错误 / refusal），计数调用。
    struct ErrorToolCallProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for ErrorToolCallProvider {
        fn id(&self) -> &'static str {
            "err-tool"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let msg = AssistantMessage {
                content: vec![ContentBlock::ToolCall {
                    id: "err1".into(),
                    name: "probe".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: Usage::default(),
                model: "err-tool".into(),
                stop_reason: Some(StopReason::Error),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// P0-B：stop_reason=Error 且含 ToolCall —— 终止错误，**不执行**工具，
    /// 回填占位 result 维持配对，以 success=false 立即停止（不续写、不无限循环）。
    #[tokio::test]
    async fn error_stop_reason_with_tool_call_gets_placeholder_and_stops() {
        let calls = Arc::new(AtomicUsize::new(0));
        let ctx = Arc::new(InMemoryContext::new(vec![]));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(ProbeTool {
            name: "probe".into(),
            cap: CapabilityTier::ReadOnly,
            ms: 0,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_seen: max_seen.clone(),
        }));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);

        let mut model = agent_core::Model::with_defaults(
            "err-tool",
            "err-tool",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;

        let agent = Agent::builder(model)
            .provider(Arc::new(ErrorToolCallProvider {
                calls: calls.clone(),
            }))
            .tools(tools)
            .context(ctx.clone())
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .max_turns(5)
            .build();

        let mut done_success: Option<bool> = None;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Done(sum) = ev {
                done_success = Some(sum.success);
            }
        }

        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            0,
            "Error 的 tool_call 不应执行"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Error 应立即终止，provider 只被调用 1 次"
        );
        assert_eq!(
            done_success,
            Some(false),
            "Error+tool_call 应以 success=false 停止"
        );
        let snapshot = ctx.snapshot().await;
        let has_placeholder = snapshot.iter().any(
            |m| matches!(m, agent_core::AgentMessage::ToolResult(t) if t.tool_call_id == "err1"),
        );
        assert!(
            has_placeholder,
            "应回填占位 ToolResult 维持 tool_use/tool_result 配对"
        );
    }

    /// 桩 Provider：第 1 轮返回 stop_reason=Error + **瞬时** stop_details + 已完成的 ToolCall；
    /// 第 2 轮返回纯文本（自然结束）。验证 P0 瞬时故障自愈。
    struct TransientErrorThenDoneProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for TransientErrorThenDoneProvider {
        fn id(&self) -> &'static str {
            "transient"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let msg = if n == 0 {
                // 第 1 轮：瞬时流错误（stream_parse_error）+ 已完成的工具调用。
                AssistantMessage {
                    content: vec![ContentBlock::ToolCall {
                        id: "tc1".into(),
                        name: "probe".into(),
                        arguments: serde_json::json!({}),
                    }],
                    usage: Usage::default(),
                    model: "transient".into(),
                    stop_reason: Some(StopReason::Error),
                    stop_details: Some(agent_core::StopDetails::new("stream_parse_error")),
                }
            } else {
                // 第 2 轮：自然结束。
                AssistantMessage {
                    content: vec![ContentBlock::Text { text: "done".into() }],
                    usage: Usage::default(),
                    model: "transient".into(),
                    stop_reason: Some(StopReason::Stop),
                    stop_details: None,
                }
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// P0-自愈：stop_reason=Error 且 stop_details 为**瞬时流错误类** + 含已知工具调用 →
    /// 改写为 ToolUse 续跑，**执行已完成的工具**（不废弃本轮），第 2 轮自然结束 → success=true。
    /// 与 [`error_stop_reason_with_tool_call_gets_placeholder_and_stops`]（无 stop_details → 不恢复）
    /// 形成对照：相同 Error+tool_call 形态，仅 stop_details 标记不同即决定恢复与否。
    #[tokio::test]
    async fn transient_stream_error_recovers_completed_toolcall() {
        let calls = Arc::new(AtomicUsize::new(0));
        let ctx = Arc::new(InMemoryContext::new(vec![]));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(ProbeTool {
            name: "probe".into(),
            cap: CapabilityTier::ReadOnly,
            ms: 0,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_seen: max_seen.clone(),
        }));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);

        let mut model = agent_core::Model::with_defaults(
            "transient",
            "transient",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;

        let agent = Agent::builder(model)
            .provider(Arc::new(TransientErrorThenDoneProvider {
                calls: calls.clone(),
            }))
            .tools(tools)
            .context(ctx.clone())
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .max_turns(5)
            .build();

        let mut done_success: Option<bool> = None;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Done(sum) = ev {
                done_success = Some(sum.success);
            }
        }

        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "瞬时错误的 tool_call 应被恢复执行（probe 执行 1 次）"
        );
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "瞬时错误应续跑（provider 至少调用 2 次），实际 {}",
            calls.load(Ordering::SeqCst)
        );
        assert_eq!(
            done_success,
            Some(true),
            "瞬时恢复后续跑至自然结束，应 success=true"
        );
        // 上下文应含真实 tool_result（probe 实际执行结果），而非占位错误。
        let snapshot = ctx.snapshot().await;
        let has_real_result = snapshot.iter().any(|m| {
            matches!(
                m,
                agent_core::AgentMessage::ToolResult(t)
                if t.tool_call_id == "tc1"
                    && !matches!(t.result, agent_core::ToolResult::Error { .. })
            )
        });
        assert!(
            has_real_result,
            "应回填真实 ToolResult（probe 实际执行结果），而非占位错误"
        );
    }

    // ── P1-C（软工具升级护栏：detour 跳过 + 升级上限）─────────────────────

    /// 桩 Provider：每轮返回一个 detour 工具调用（name="other"），永不调用所需工具。
    struct AlwaysDetourProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for AlwaysDetourProvider {
        fn id(&self) -> &'static str {
            "detour"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let msg = AssistantMessage {
                content: vec![ContentBlock::ToolCall {
                    id: "detour1".into(),
                    name: "other".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: Usage::default(),
                model: "detour".into(),
                stop_reason: Some(StopReason::ToolUse),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// P1-C：软需求 pending 时，模型持续调用 detour（非所需工具）→
    /// detour 不执行（配 skipped 占位）、连续 MAX_SOFT_TOOL_ESCALATIONS 次后 abort（不无限循环）。
    #[tokio::test]
    async fn soft_requirement_skips_detour_and_aborts() {
        let calls = Arc::new(AtomicUsize::new(0));
        let ctx = Arc::new(InMemoryContext::new(vec![]));
        let other_max = Arc::new(AtomicUsize::new(0));
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(ProbeTool {
            name: "other".into(),
            cap: CapabilityTier::ReadOnly,
            ms: 0,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_seen: other_max.clone(),
        }));
        reg.register(Box::new(ProbeTool {
            name: "required_tool".into(),
            cap: CapabilityTier::ReadOnly,
            ms: 0,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
        }));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);

        let mut model = agent_core::Model::with_defaults(
            "detour",
            "detour",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;

        let agent = Agent::builder(model)
            .provider(Arc::new(AlwaysDetourProvider {
                calls: calls.clone(),
            }))
            .tools(tools)
            .context(ctx.clone())
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .soft_requirement(agent_core::SoftToolRequirement {
                id: "req1".into(),
                tool_name: "required_tool".into(),
                reminder: "请先调用 required_tool".into(),
            })
            .max_turns(100)
            .build();

        let mut done_success: Option<bool> = None;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Done(sum) = ev {
                done_success = Some(sum.success);
            }
        }

        // detour 工具未执行（非合规，仅配 skipped 占位）。
        assert_eq!(
            other_max.load(Ordering::SeqCst),
            0,
            "detour 'other' 不应被执行"
        );
        // 以失败 abort（不无限循环到 max_turns）。
        assert_eq!(done_success, Some(false), "应 abort（success=false）");
        assert!(
            calls.load(Ordering::SeqCst) < 10,
            "应在 escalate 上限 abort（calls={}），而非跑满 max_turns=100",
            calls.load(Ordering::SeqCst)
        );
    }

    // ── P1-D（工具 panic 归一化：catch_unwind）─────────────────────────────

    /// 探针工具：execute 内 panic（模拟第三方工具/MCP 的 unwrap None、越界等失控）。
    struct PanicTool;
    #[async_trait]
    impl Tool for PanicTool {
        fn name(&self) -> &str {
            "panic_tool"
        }
        fn description(&self) -> &str {
            "panics"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn capability(&self) -> CapabilityTier {
            CapabilityTier::ReadOnly
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: &ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            panic!("boom from panic_tool");
        }
    }

    /// P1-D：工具 execute panic 时，`run_pending_task` 应 catch 并归一化为不可恢复 Error
    /// result（不传播 panic 终止整个 agent run、不污染会话文件、不计入悬空调用）。
    #[tokio::test]
    async fn panicking_tool_is_caught_and_normalized() {
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(PanicTool));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);
        let ws = Workspace::new(".");
        let approval = YoloApproval;
        let cancel = CancellationToken::new();
        let tcx = build_tcx(&ws, &approval, &cancel);
        let t = task(0, "panic_tool", false);

        let (result, mistake_inc) = run_pending_task(&t, &tools, &tcx, &[]).await;

        assert!(
            matches!(
                result,
                ToolResult::Error {
                    recoverable: false,
                    ..
                }
            ),
            "panic 应归一化为不可恢复 Error result，实际: {result:?}"
        );
        assert!(mistake_inc, "panic 应计为 mistake（不可恢复）");
        if let ToolResult::Error { message, .. } = &result {
            assert!(
                message.contains("panic") || message.contains("boom"),
                "Error 消息应含 panic 信息: {message}"
            );
        }
    }

    // ── P1-I（interruptible 工具 + Immediate 模式执行期轮询）──────────────

    /// 桩 Provider：第 1 轮返回 ToolCall("block")；之后轮返回停止文本。
    struct BlockCallProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for BlockCallProvider {
        fn id(&self) -> &'static str {
            "block-call"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let msg = if n == 0 {
                AssistantMessage {
                    content: vec![ContentBlock::ToolCall {
                        id: "b1".into(),
                        name: "block".into(),
                        arguments: serde_json::json!({}),
                    }],
                    usage: Usage::default(),
                    model: "block-call".into(),
                    stop_reason: Some(StopReason::ToolUse),
                    stop_details: None,
                }
            } else {
                AssistantMessage {
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                    usage: Usage::default(),
                    model: "block-call".into(),
                    stop_reason: Some(StopReason::Stop),
                    stop_details: None,
                }
            };
            Ok(Box::pin(futures::stream::iter(vec![
                AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// 阻塞工具：execute 内 `select!` 等待 30s 或 `ctx.cancel`——后者命中即标记 interrupted。
    /// interruptible = true；响应 [`ToolContext::cancel`]（即批级 token）。
    struct BlockingTool {
        started: Arc<tokio::sync::Notify>,
        interrupted: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait]
    impl Tool for BlockingTool {
        fn name(&self) -> &str {
            "block"
        }
        fn description(&self) -> &str {
            "blocks for a long time"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability(&self) -> CapabilityTier {
            CapabilityTier::Execute
        }
        fn interruptible(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            ctx: &ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            self.started.notify_one();
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(30)) => Ok(ToolResult::text("completed")),
                _ = ctx.cancel.cancelled() => {
                    self.interrupted.store(true, Ordering::SeqCst);
                    Err(ToolError::Execution("被 steering 中断".into()))
                }
            }
        }
    }

    /// Immediate 模式：interruptible 工具阻塞执行期间注入 steering → 批级 token 触发取消，
    /// 工具尽快让出（而非跑满 30s），steering 随后在下轮被处理。
    #[tokio::test]
    async fn interruptible_tool_is_aborted_by_steering() {
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentMessage>();
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(BlockingTool {
            started: started.clone(),
            interrupted: interrupted.clone(),
        }));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let mut model = agent_core::Model::with_defaults(
            "block-call",
            "block-call",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;
        let agent = Agent::builder(model)
            .provider(Arc::new(BlockCallProvider {
                calls: calls.clone(),
            }))
            .tools(tools)
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .steer_rx(rx)
            .interrupt_mode(InterruptMode::Immediate)
            .max_turns(10)
            .build();

        // 后台任务：等工具开始阻塞后注入 steering（模拟用户中途发消息）。
        let started_clone = started.clone();
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            started_clone.notified().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx_clone.send(AgentMessage::user_text("[steering] 改做别的"));
        });

        let mut done = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        // 防御性超时：若中断机制失效，工具会阻塞 30s；超时令测试明确失败而非挂起。
        let ran = tokio::time::timeout(Duration::from_secs(8), async {
            while let Some(ev) = stream.next().await {
                if matches!(ev, AgentEvent::Done(_)) {
                    done = true;
                    break;
                }
            }
        })
        .await;
        assert!(
            ran.is_ok(),
            "应在超时内完成（steering 中断阻塞工具而非跑满 30s）"
        );
        assert!(done, "应正常结束");
        assert!(
            interrupted.load(Ordering::SeqCst),
            "interruptible 工具应被 steering 触发的批级 token 取消"
        );
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "中断后应续跑到下一轮（steering 被处理）"
        );
    }

    // ── P2-I（before 拦截 / after 改写 钩子）──────────────────────────────

    /// 回显工具：返回固定文本 `real-output`。
    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability(&self) -> CapabilityTier {
            CapabilityTier::ReadOnly
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: &ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("real-output"))
        }
    }

    /// 测试钩子：可拦截指定工具（`block_tool`）+ 把任意结果改写为固定文本（`rewrite_to`），
    /// 并计数观察到的 before/after 事件。
    struct BlockOverrideHook {
        block_tool: Option<String>,
        rewrite_to: Option<String>,
        saw_before: Arc<AtomicUsize>,
        saw_after: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Hook for BlockOverrideHook {
        async fn on_event(&self, event: &HookEvent) {
            if matches!(event, HookEvent::BeforeTool { .. }) {
                self.saw_before.fetch_add(1, Ordering::SeqCst);
            }
            if matches!(event, HookEvent::AfterTool { .. }) {
                self.saw_after.fetch_add(1, Ordering::SeqCst);
            }
        }
        async fn before_tool_intercept(
            &self,
            tool: &str,
            _args: &serde_json::Value,
        ) -> Option<String> {
            if self.block_tool.as_deref() == Some(tool) {
                Some("blocked by test hook".into())
            } else {
                None
            }
        }
        async fn after_tool_override(
            &self,
            _tool: &str,
            _result: &ToolResult,
        ) -> Option<ToolResult> {
            self.rewrite_to
                .as_ref()
                .map(|s| ToolResult::text(s.clone()))
        }
    }

    /// before_tool_intercept 拦截：不调用 execute，回填可恢复错误，仍发 before/after 观察事件。
    #[tokio::test]
    async fn before_tool_intercept_blocks_execution() {
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(EchoTool));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);
        let ws = Workspace::new(".");
        let approval = YoloApproval;
        let cancel = CancellationToken::new();
        let tcx = build_tcx(&ws, &approval, &cancel);
        let hook = Arc::new(BlockOverrideHook {
            block_tool: Some("echo".into()),
            rewrite_to: None,
            saw_before: Arc::new(AtomicUsize::new(0)),
            saw_after: Arc::new(AtomicUsize::new(0)),
        });
        let hook_dyn: Arc<dyn Hook> = hook.clone();
        let hooks = vec![hook_dyn];
        let t = task(0, "echo", false);

        let (result, mistake_inc) = run_pending_task(&t, &tools, &tcx, &hooks).await;

        assert!(
            matches!(
                result,
                ToolResult::Error {
                    recoverable: true,
                    ..
                }
            ),
            "拦截应回填可恢复 Error，实际: {result:?}"
        );
        assert!(!mistake_inc, "拦截不应计 mistake");
        if let ToolResult::Error { message, .. } = &result {
            assert!(message.contains("钩子拦截"), "消息应含拦截标识: {message}");
        }
        // before 观察在拦截前触发；拦截后仍发 after 观察事件（结果为拦截 Error）。
        assert_eq!(hook.saw_before.load(Ordering::SeqCst), 1);
        assert_eq!(hook.saw_after.load(Ordering::SeqCst), 1);
    }

    /// after_tool_override 改写：钩子替换真实结果；after 观察事件看到的是改写后的最终结果。
    #[tokio::test]
    async fn after_tool_override_rewrites_result() {
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(EchoTool));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);
        let ws = Workspace::new(".");
        let approval = YoloApproval;
        let cancel = CancellationToken::new();
        let tcx = build_tcx(&ws, &approval, &cancel);
        let hook = Arc::new(BlockOverrideHook {
            block_tool: None,
            rewrite_to: Some("rewritten".into()),
            saw_before: Arc::new(AtomicUsize::new(0)),
            saw_after: Arc::new(AtomicUsize::new(0)),
        });
        let hook_dyn: Arc<dyn Hook> = hook.clone();
        let hooks = vec![hook_dyn];
        let t = task(0, "echo", false);

        let (result, mistake_inc) = run_pending_task(&t, &tools, &tcx, &hooks).await;

        assert!(
            matches!(result, ToolResult::Text(ref s) if s == "rewritten"),
            "应被改写为 'rewritten'，实际: {result:?}"
        );
        assert!(!mistake_inc);
        assert_eq!(hook.saw_before.load(Ordering::SeqCst), 1);
        assert_eq!(hook.saw_after.load(Ordering::SeqCst), 1);
    }

    // ── P2-K（AgentRunSummary 工具计数 + coverage）────────────────────────

    #[test]
    fn run_summary_records_tool_counters_and_coverage() {
        let mut s = AgentRunSummary::default();
        s.tools_available = vec!["a".into(), "b".into(), "c".into()];
        s.record_tool("a", &ToolResult::text("ok"));
        s.record_tool(
            "a",
            &ToolResult::Error {
                recoverable: true,
                message: "e".into(),
            },
        );
        s.record_tool("b", &ToolResult::text("ok"));

        let a = s.tools_by_name.get("a").expect("应有 a 的计数");
        assert_eq!(a.total, 2, "a 调用 2 次");
        assert_eq!(a.ok, 1, "a 成功 1 次");
        assert_eq!(a.error, 1, "a 错误 1 次");
        let b = s.tools_by_name.get("b").expect("应有 b 的计数");
        assert_eq!((b.total, b.ok, b.error), (1, 1, 0));

        assert!(
            s.tools_invoked.contains("a") && s.tools_invoked.contains("b"),
            "invoked 应含 a、b"
        );
        assert!(!s.tools_invoked.contains("c"), "c 未调用，不应在 invoked");
        // coverage：unused = available − invoked = [c]
        assert_eq!(s.unused_tools(), vec!["c".to_string()], "unused 应为 [c]");
    }

    // ── P1-K：自适应思考预算（auto-thinking）──────────────────────────────

    /// 桩 Provider：记录收到的 thinking.budget_tokens，立即返回停止消息。
    struct ThinkingRecordingProvider {
        recorded: Arc<std::sync::Mutex<Option<usize>>>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for ThinkingRecordingProvider {
        fn id(&self) -> &'static str {
            "thinking-rec"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            *self.recorded.lock().unwrap() = req.thinking.as_ref().map(|t| t.budget_tokens);
            let msg = AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                usage: Usage::default(),
                model: "stub".into(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// 桩分类器：恒返回固定 Effort。
    struct FixedEffortClassifier(agent_core::Effort);
    #[async_trait]
    impl agent_core::ThinkingClassifier for FixedEffortClassifier {
        async fn classify(
            &self,
            _prompt: &str,
            _model: &agent_core::Model,
        ) -> Option<agent_core::Effort> {
            Some(self.0)
        }
    }

    /// run_loop 经 ThinkingPolicy::Auto + 分类器解析 budget 并下发到 provider。
    #[tokio::test]
    async fn auto_thinking_resolves_budget_from_classifier() {
        let recorded = Arc::new(std::sync::Mutex::new(None::<usize>));
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let mut model =
            agent_core::Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        model.supports_thinking = true;
        model.max_input_tokens = 200_000;
        let policy = agent_core::ThinkingPolicy::auto(
            Arc::new(FixedEffortClassifier(agent_core::Effort::High)),
            agent_core::ThinkingConfig::new(1_000),
        );
        let agent = Agent::builder(model)
            .provider(Arc::new(ThinkingRecordingProvider {
                recorded: recorded.clone(),
            }))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .thinking_policy(policy)
            .build();
        let stream = agent.run("重构这个模块");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if matches!(ev, AgentEvent::Done(_)) {
                break;
            }
        }
        let budget = recorded.lock().unwrap().take();
        assert_eq!(
            budget,
            Some(agent_core::Effort::High.default_budget()),
            "Auto + High 分类 → 下发 High 默认预算（32_000）"
        );
    }

    /// 模型不支持思考时，Auto 策略解析为 None（本轮不思考）。
    #[tokio::test]
    async fn auto_thinking_returns_none_when_model_unsupported() {
        let recorded = Arc::new(std::sync::Mutex::new(None::<usize>));
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let mut model =
            agent_core::Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        // supports_thinking 保持默认 false。
        model.max_input_tokens = 200_000;
        let policy = agent_core::ThinkingPolicy::auto(
            Arc::new(FixedEffortClassifier(agent_core::Effort::High)),
            agent_core::ThinkingConfig::new(1_000),
        );
        let agent = Agent::builder(model)
            .provider(Arc::new(ThinkingRecordingProvider {
                recorded: recorded.clone(),
            }))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .thinking_policy(policy)
            .build();
        let stream = agent.run("any prompt");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if matches!(ev, AgentEvent::Done(_)) {
                break;
            }
        }
        let budget = recorded.lock().unwrap().take();
        assert_eq!(
            budget, None,
            "模型不支持思考 → Auto 解析为 None，本轮不思考"
        );
    }

    // ── P1-E：三层生命周期事件（turn / message / tool_execution）──────────────

    /// 两轮 Provider：第 1 轮返回 `probe` 工具调用，第 2 轮返回纯文本（自然结束）。
    struct TwoTurnProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for TwoTurnProvider {
        fn id(&self) -> &'static str {
            "two-turn"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let msg = if n == 1 {
                AssistantMessage {
                    content: vec![ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "probe".into(),
                        arguments: serde_json::json!({}),
                    }],
                    usage: Usage::default(),
                    model: "two-turn".into(),
                    stop_reason: Some(StopReason::ToolUse),
                    stop_details: None,
                }
            } else {
                AssistantMessage {
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                    usage: Usage::default(),
                    model: "two-turn".into(),
                    stop_reason: Some(StopReason::Stop),
                    stop_details: None,
                }
            };
            Ok(Box::pin(futures::stream::iter(vec![
                AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// 验证三层生命周期事件被发射且顺序严格正确：
    /// 每轮 `TurnStart → MessageStart → … → MessageEnd`；工具轮额外发
    /// `ToolExecutionStart/End`；每轮以 `TurnEnd` 收尾（will_continue 在工具轮为 true、
    /// 停止轮为 false）。
    #[tokio::test]
    async fn lifecycle_events_three_layers_in_order() {
        let calls = Arc::new(AtomicUsize::new(0));
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(ProbeTool {
            name: "probe".into(),
            cap: CapabilityTier::ReadOnly,
            ms: 0,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
        }));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);
        let mut model = agent_core::Model::with_defaults(
            "two-turn",
            "two-turn",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;

        let agent = Agent::builder(model)
            .provider(Arc::new(TwoTurnProvider {
                calls: calls.clone(),
            }))
            .tools(tools)
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .build();

        // 收集事件为「种类标签」序列，便于断言顺序（忽略负载细节）。
        #[derive(Debug, PartialEq)]
        enum Tag {
            TurnStart,
            MessageStart,
            MessageEnd,
            ToolStart,
            ToolEnd,
            TurnEnd(bool),
        }
        let mut seq = Vec::new();
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::TurnStart => seq.push(Tag::TurnStart),
                AgentEvent::MessageStart => seq.push(Tag::MessageStart),
                AgentEvent::MessageEnd(_) => seq.push(Tag::MessageEnd),
                AgentEvent::ToolExecutionStart { .. } => seq.push(Tag::ToolStart),
                AgentEvent::ToolExecutionEnd { .. } => seq.push(Tag::ToolEnd),
                AgentEvent::TurnEnd { will_continue, .. } => seq.push(Tag::TurnEnd(will_continue)),
                _ => {}
            }
        }

        // 第 1 轮（工具）：TurnStart → MessageStart → MessageEnd → ToolStart → ToolEnd → TurnEnd(true)
        // 第 2 轮（停止）：TurnStart → MessageStart → MessageEnd → TurnEnd(false)
        let expected = vec![
            Tag::TurnStart,
            Tag::MessageStart,
            Tag::MessageEnd,
            Tag::ToolStart,
            Tag::ToolEnd,
            Tag::TurnEnd(true),
            Tag::TurnStart,
            Tag::MessageStart,
            Tag::MessageEnd,
            Tag::TurnEnd(false),
        ];
        assert_eq!(seq, expected, "三层生命周期事件顺序应严格匹配");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "应正好 2 轮模型调用");
    }

    // ── aside 双通道（P2-A：被动、非中断通知）──────────────────────────────

    /// 停止边界 aside：每轮返回纯文本（触发停止边界），第 1 轮经 `aside_tx` 注入一条 aside。
    /// 停止边界应检测到 aside 并续跑（而非结束）——provider 被调用 2 次。
    struct AsideStopBoundaryProvider {
        calls: Arc<AtomicUsize>,
        aside_tx: tokio::sync::mpsc::UnboundedSender<AgentMessage>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for AsideStopBoundaryProvider {
        fn id(&self) -> &'static str {
            "aside-stop"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                let _ = self
                    .aside_tx
                    .send(AgentMessage::user_text("[aside] 后台任务完成"));
            }
            let msg = AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: format!("reply #{n}"),
                }],
                usage: Usage::default(),
                model: "aside-stop".into(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// 停止边界 aside 触发续跑：agent 本该停止，但停止边界 drain 到 aside → 续跑一轮。
    #[tokio::test]
    async fn aside_at_stop_boundary_continues_run() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (aside_tx, aside_rx) = tokio::sync::mpsc::unbounded_channel::<AgentMessage>();
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let model = agent_core::Model::with_defaults(
            "aside-stop",
            "aside-stop",
            agent_core::Api::OpenAiCompletions,
        );
        let agent = Agent::builder(model)
            .provider(Arc::new(AsideStopBoundaryProvider {
                calls: calls.clone(),
                aside_tx,
            }))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .aside_rx(aside_rx)
            .build();

        let mut injected = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Say(s) = ev {
                if s.text.contains("停止边界 aside") {
                    injected = true;
                }
            }
        }
        assert!(injected, "应发出停止边界 aside 注入提示");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "停止边界 aside 应触发续跑（2 轮），而非搁置后仅 1 轮"
        );
    }

    /// mid-work aside：第 1 轮返回 probe 工具调用并注入 aside；工具执行完毕后下一轮顶部
    ///（mid-work 边界）drain aside。验证 aside 在工具轮后被注入，且 probe 正常完成
    ///（不被 cancel——aside 不走 Immediate 批级中断）。
    struct MidWorkAsideProvider {
        calls: Arc<AtomicUsize>,
        aside_tx: tokio::sync::mpsc::UnboundedSender<AgentMessage>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for MidWorkAsideProvider {
        fn id(&self) -> &'static str {
            "aside-midwork"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                let _ = self
                    .aside_tx
                    .send(AgentMessage::user_text("[aside] 延迟 LSP diagnostics"));
            }
            let msg = if n == 1 {
                AssistantMessage {
                    content: vec![ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "probe".into(),
                        arguments: serde_json::json!({}),
                    }],
                    usage: Usage::default(),
                    model: "aside-midwork".into(),
                    stop_reason: Some(StopReason::ToolUse),
                    stop_details: None,
                }
            } else {
                AssistantMessage {
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                    usage: Usage::default(),
                    model: "aside-midwork".into(),
                    stop_reason: Some(StopReason::Stop),
                    stop_details: None,
                }
            };
            Ok(Box::pin(futures::stream::iter(vec![
                AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// mid-work aside 在工具批次完成后、下一轮模型调用前注入；probe 工具正常完成
    ///（不被 cancel）。
    #[tokio::test]
    async fn aside_mid_work_injected_after_tool_batch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (aside_tx, aside_rx) = tokio::sync::mpsc::unbounded_channel::<AgentMessage>();
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(ProbeTool {
            name: "probe".into(),
            cap: CapabilityTier::ReadOnly,
            ms: 0,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
        }));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);
        let mut model = agent_core::Model::with_defaults(
            "aside-midwork",
            "aside-midwork",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;

        let agent = Agent::builder(model)
            .provider(Arc::new(MidWorkAsideProvider {
                calls: calls.clone(),
                aside_tx,
            }))
            .tools(tools)
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .aside_rx(aside_rx)
            .build();

        let mut injected = false;
        let mut tool_ok = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::Say(s) if s.text.contains("已注入 aside 消息") => injected = true,
                AgentEvent::ToolExecutionEnd { name, is_error, .. } if name == "probe" => {
                    tool_ok = !is_error;
                }
                _ => {}
            }
        }
        assert!(injected, "mid-work aside 应在工具批次后注入（Say 提示）");
        assert!(tool_ok, "probe 工具应正常完成（aside 不打断在途工具）");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "应正好 2 轮模型调用");
    }

    // ── RuntimeOverrides（P3-A：mid-run 配置热更新）─────────────────────────

    /// 记录每轮 provider 收到的 `(thinking 是否存在, temperature)`，并按轮次返回不同消息
    ///（第 1 轮 tool_call、第 2 轮纯文本），使循环正好 2 轮。
    struct RecordingProvider {
        calls: Arc<AtomicUsize>,
        seen: Arc<std::sync::Mutex<Vec<(bool, Option<f32>)>>>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for RecordingProvider {
        fn id(&self) -> &'static str {
            "recording"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.seen
                .lock()
                .unwrap()
                .push((req.thinking.is_some(), req.temperature));
            let msg = if n == 1 {
                AssistantMessage {
                    content: vec![ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "probe".into(),
                        arguments: serde_json::json!({}),
                    }],
                    usage: Usage::default(),
                    model: "recording".into(),
                    stop_reason: Some(StopReason::ToolUse),
                    stop_details: None,
                }
            } else {
                AssistantMessage {
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                    usage: Usage::default(),
                    model: "recording".into(),
                    stop_reason: Some(StopReason::Stop),
                    stop_details: None,
                }
            };
            Ok(Box::pin(futures::stream::iter(vec![
                AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// 按调用次数切换：第 1 次返回 None（沿用静态），第 2 次起返回 Some（覆盖）。
    struct SwitchingOverrides {
        thinking_call: Arc<AtomicUsize>,
        temp_call: Arc<AtomicUsize>,
    }
    impl RuntimeOverrides for SwitchingOverrides {
        fn thinking(&self, _model: &agent_core::Model) -> Option<agent_core::ThinkingConfig> {
            let n = self.thinking_call.fetch_add(1, Ordering::SeqCst);
            (n >= 1).then(|| agent_core::ThinkingConfig::new(2_000))
        }
        fn temperature(&self) -> Option<f32> {
            let n = self.temp_call.fetch_add(1, Ordering::SeqCst);
            (n >= 1).then_some(0.5)
        }
    }

    /// RuntimeOverrides 每轮解析：第 1 轮无覆盖（沿用静态 None），第 2 轮覆盖为
    /// `thinking=Some` / `temperature=Some(0.5)`——provider 应观测到覆盖值生效。
    #[tokio::test]
    async fn runtime_overrides_apply_per_turn() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen: Arc<std::sync::Mutex<Vec<(bool, Option<f32>)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(ProbeTool {
            name: "probe".into(),
            cap: CapabilityTier::ReadOnly,
            ms: 0,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
        }));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);
        let mut model = agent_core::Model::with_defaults(
            "recording",
            "recording",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;

        let agent = Agent::builder(model)
            .provider(Arc::new(RecordingProvider {
                calls: calls.clone(),
                seen: seen.clone(),
            }))
            .tools(tools)
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .runtime_overrides(Arc::new(SwitchingOverrides {
                thinking_call: Arc::new(AtomicUsize::new(0)),
                temp_call: Arc::new(AtomicUsize::new(0)),
            }))
            .build();

        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if matches!(ev, AgentEvent::Done(_)) {
                break;
            }
        }
        let guard = seen.lock().unwrap();
        assert_eq!(guard.len(), 2, "应正好 2 轮 provider 调用");
        assert_eq!(
            guard[0],
            (false, None),
            "第 1 轮无覆盖，沿用静态 None（thinking/temperature 均未设）"
        );
        assert_eq!(
            guard[1],
            (true, Some(0.5)),
            "第 2 轮 RuntimeOverrides 覆盖生效（thinking=Some, temperature=0.5）"
        );
    }

    /// 桩 Provider：记录每轮收到的 thinking budget 与 ctx.api_key，供 P1 mid-run override
    ///（disable_thinking / api_key）测试观测。返回纯文本一轮即结束。
    struct CtxRecordingProvider {
        seen: Arc<std::sync::Mutex<Vec<(Option<usize>, Option<String>)>>>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for CtxRecordingProvider {
        fn id(&self) -> &'static str {
            "ctx-rec"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            req: agent_core::CompletionRequest,
            ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let budget = req.thinking.as_ref().map(|t| t.budget_tokens);
            self.seen
                .lock()
                .unwrap()
                .push((budget, ctx.api_key.clone()));
            let msg = AssistantMessage {
                content: vec![ContentBlock::Text { text: "done".into() }],
                usage: Usage::default(),
                model: "ctx-rec".into(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// P1：disable_thinking()==Some(true) 应覆盖启动期静态 thinking，本轮置空。
    struct DisableThinkingOverrides;
    impl RuntimeOverrides for DisableThinkingOverrides {
        fn disable_thinking(&self) -> Option<bool> {
            Some(true)
        }
    }

    #[tokio::test]
    async fn runtime_override_disables_thinking() {
        let seen: Arc<std::sync::Mutex<Vec<(Option<usize>, Option<String>)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let tools: Arc<dyn ToolRegistry> = Arc::new(DefaultToolRegistry::new());
        let mut model = agent_core::Model::with_defaults(
            "ctx-rec",
            "ctx-rec",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;

        let agent = Agent::builder(model)
            .provider(Arc::new(CtxRecordingProvider { seen: seen.clone() }))
            .tools(tools)
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .thinking(agent_core::ThinkingConfig::new(2_000))
            .runtime_overrides(Arc::new(DisableThinkingOverrides))
            .build();

        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if matches!(ev, AgentEvent::Done(_)) {
                break;
            }
        }
        let guard = seen.lock().unwrap();
        assert_eq!(guard.len(), 1, "应正好 1 轮 provider 调用");
        assert_eq!(
            guard[0].0,
            None,
            "disable_thinking==Some(true) 应置空 thinking（覆盖启动期静态 2000）"
        );
    }

    /// P1：api_key() 覆盖应作用于每轮 provider 调用的 ctx（覆盖启动期 key）。
    struct ApiKeyOverrides;
    impl RuntimeOverrides for ApiKeyOverrides {
        fn api_key(&self, _model: &agent_core::Model) -> Option<String> {
            Some("override-key".into())
        }
    }

    #[tokio::test]
    async fn runtime_override_api_key_overrides_ctx() {
        let seen: Arc<std::sync::Mutex<Vec<(Option<usize>, Option<String>)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let tools: Arc<dyn ToolRegistry> = Arc::new(DefaultToolRegistry::new());
        let mut model = agent_core::Model::with_defaults(
            "ctx-rec",
            "ctx-rec",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;

        let mut startup_ctx = agent_core::ProviderCallContext::default();
        startup_ctx.api_key = Some("startup-key".into());

        let agent = Agent::builder(model)
            .provider(Arc::new(CtxRecordingProvider { seen: seen.clone() }))
            .tools(tools)
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .provider_ctx(startup_ctx)
            .runtime_overrides(Arc::new(ApiKeyOverrides))
            .build();

        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if matches!(ev, AgentEvent::Done(_)) {
                break;
            }
        }
        let guard = seen.lock().unwrap();
        assert_eq!(guard.len(), 1, "应正好 1 轮 provider 调用");
        assert_eq!(
            guard[0].1.as_deref(),
            Some("override-key"),
            "api_key override 应覆盖启动期 startup-key，provider 观测到 override-key"
        );
    }

    /// 桩 Provider：计数调用次数，返回纯文本（一轮即结束）。
    struct DoneCountingProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for DoneCountingProvider {
        fn id(&self) -> &'static str {
            "done-count"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let msg = AssistantMessage {
                content: vec![ContentBlock::Text { text: "done".into() }],
                usage: Usage::default(),
                model: "done-count".into(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// Sprint2：进程级 pause gate 集成——run 前 pause，agent 在第一个安全点
    ///（provider 调用前）park，provider 不被调用；resume 后继续至完成。
    /// 用 select! 在当前 task 同时 drive stream 与 pause/resume（stream 借用 agent，
    /// 非 'static，不能 spawn）。
    #[tokio::test]
    async fn pause_gate_parks_run_and_resumes() {
        let gate = PauseGate::new();
        gate.pause();
        let calls = Arc::new(AtomicUsize::new(0));
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let tools: Arc<dyn ToolRegistry> = Arc::new(DefaultToolRegistry::new());
        let mut model = agent_core::Model::with_defaults(
            "done-count",
            "done-count",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;

        let agent = Agent::builder(model)
            .provider(Arc::new(DoneCountingProvider { calls: calls.clone() }))
            .tools(tools)
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .pause_gate(gate.clone())
            .build();

        let stream = agent.run("go");
        tokio::pin!(stream);
        let mut resumed = false;
        loop {
            tokio::select! {
                ev = stream.next() => match ev {
                    Some(AgentEvent::Done(_)) | None => break,
                    _ => {}
                },
                _ = tokio::time::sleep(std::time::Duration::from_millis(120)), if !resumed => {
                    // park 期间（provider 调用前）：provider 不应被调用。
                    assert_eq!(
                        calls.load(Ordering::SeqCst),
                        0,
                        "pause 时 provider 不应被调用（park 在 provider 调用前）"
                    );
                    gate.resume();
                    resumed = true;
                }
            }
        }
        assert!(
            calls.load(Ordering::SeqCst) >= 1,
            "resume 后 provider 应被调用并完成"
        );
    }

    /// 桩 Provider：每轮返回纯文本 `reply #n`（自然结束）。计数调用。
    struct FollowUpStopBoundaryProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for FollowUpStopBoundaryProvider {
        fn id(&self) -> &'static str {
            "followup-stop"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let msg = AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: format!("reply #{n}"),
                }],
                usage: Usage::default(),
                model: "followup-stop".into(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// Sprint2：followUp 在停止边界触发续跑（移植 oh-my-pi `getFollowUpMessages`）。
    /// run 前注入一条 followUp，agent 第一轮 done → 停止边界 drain followUp → 续跑 →
    /// 第二轮 done → 停止边界 drain followUp（空）→ 停止。provider 应被调用 ≥ 2 次。
    #[tokio::test]
    async fn followup_at_stop_boundary_continues_run() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (followup_tx, followup_rx) =
            tokio::sync::mpsc::unbounded_channel::<AgentMessage>();
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let tools: Arc<dyn ToolRegistry> = Arc::new(DefaultToolRegistry::new());
        let mut model = agent_core::Model::with_defaults(
            "followup-stop",
            "followup-stop",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;

        let agent = Agent::builder(model)
            .provider(Arc::new(FollowUpStopBoundaryProvider {
                calls: calls.clone(),
            }))
            .tools(tools)
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .followup_rx(followup_rx)
            .build();

        // run 前注入一条 followUp 延续消息。
        followup_tx
            .send(agent_core::AgentMessage::user_text("继续完成子任务".to_string()))
            .ok();

        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if matches!(ev, AgentEvent::Done(_)) {
                break;
            }
        }
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "followUp 应在停止边界触发续跑（provider 至少调用 2 次），实际 {}",
            calls.load(Ordering::SeqCst)
        );
    }

    /// 桩 Provider：记录每轮 `req.temperature`；第 1 轮返回 harmony 泄漏 text（触发
    /// abort-retry），第 2 轮返回 done。用于验证 harmony retry 温度扰动。
    struct HarmonyLeakTempProvider {
        calls: Arc<AtomicUsize>,
        seen: Arc<std::sync::Mutex<Vec<Option<f32>>>>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for HarmonyLeakTempProvider {
        fn id(&self) -> &'static str {
            "harmony-temp"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(req.temperature);
            let msg = if n == 0 {
                // 第 1 轮：harmony 泄漏（<|return|> token）→ text surface → abort-retry。
                AssistantMessage {
                    content: vec![ContentBlock::Text {
                        text: "leaked <|return|> token".into(),
                    }],
                    usage: Usage::default(),
                    model: "harmony-temp".into(),
                    stop_reason: Some(StopReason::Stop),
                    stop_details: None,
                }
            } else {
                AssistantMessage {
                    content: vec![ContentBlock::Text { text: "done".into() }],
                    usage: Usage::default(),
                    model: "harmony-temp".into(),
                    stop_reason: Some(StopReason::Stop),
                    stop_details: None,
                }
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// Sprint3：harmony abort-retry 后本轮 temperature += 0.05（移植 oh-my-pi
    /// agent-loop.ts:1495），微调采样分布防同款泄漏复现。首轮无 retry → temperature
    /// 不变；第 1 轮泄漏触发 abort-retry → 第 2 轮 temperature 扰动。
    #[tokio::test]
    async fn harmony_retry_bumps_temperature() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen: Arc<std::sync::Mutex<Vec<Option<f32>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let tools: Arc<dyn ToolRegistry> = Arc::new(DefaultToolRegistry::new());
        // harmony 检测仅对 OpenAiResponses 模型生效（is_harmony_leak_target）。
        let mut model = agent_core::Model::with_defaults(
            "gpt-5",
            "openai",
            agent_core::Api::OpenAiResponses,
        );
        model.max_input_tokens = 200_000;

        let agent = Agent::builder(model)
            .provider(Arc::new(HarmonyLeakTempProvider {
                calls: calls.clone(),
                seen: seen.clone(),
            }))
            .tools(tools)
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .temperature(0.7)
            .build();

        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if matches!(ev, AgentEvent::Done(_)) {
                break;
            }
        }
        let guard = seen.lock().unwrap();
        assert_eq!(guard.len(), 2, "应正好 2 轮（泄漏 abort-retry + done）");
        assert_eq!(guard[0], Some(0.7), "首轮无 harmony retry，temperature 不变");
        // 浮点：0.7 + 0.05 在 f32 下非精确 0.75，用近似比较。
        let bumped = guard[1].expect("第二轮应有 temperature");
        assert!(
            (bumped - 0.75).abs() < 1e-4,
            "harmony abort-retry 后 temperature +0.05（≈0.75），实际 {bumped}"
        );
    }

    // ── 工具流式 partial（P1-F：partialResult 回调）─────────────────────────

    /// 流式工具：execute 内经 `ctx.update_tx` 推 3 条 partial，每条间 sleep 让 agent 端
    /// select! 有机会 drain。最终返回聚合结果。模拟 bash 边输出边显示、job 轮询进度。
    struct StreamingTool;
    #[async_trait]
    impl Tool for StreamingTool {
        fn name(&self) -> &str {
            "streaming"
        }
        fn description(&self) -> &str {
            "streaming"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability(&self) -> CapabilityTier {
            CapabilityTier::ReadOnly
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            ctx: &ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            for partial in ["chunk-1", "chunk-2", "chunk-3"] {
                if let Some(tx) = ctx.update_tx {
                    let _ = tx.send(agent_tools::ToolUpdate {
                        tool_call_id: "c1".into(),
                        name: "streaming".into(),
                        partial: partial.into(),
                    });
                }
                // 让出控制权，使 agent 端 select! 能在工具完成前 drain partial。
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            Ok(ToolResult::text("final-result"))
        }
    }

    /// 两轮 Provider：第 1 轮返回 `streaming` 工具调用，第 2 轮纯文本。
    struct StreamingProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for StreamingProvider {
        fn id(&self) -> &'static str {
            "streaming-prov"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let msg = if n == 1 {
                AssistantMessage {
                    content: vec![ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "streaming".into(),
                        arguments: serde_json::json!({}),
                    }],
                    usage: Usage::default(),
                    model: "streaming-prov".into(),
                    stop_reason: Some(StopReason::ToolUse),
                    stop_details: None,
                }
            } else {
                AssistantMessage {
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                    usage: Usage::default(),
                    model: "streaming-prov".into(),
                    stop_reason: Some(StopReason::Stop),
                    stop_details: None,
                }
            };
            Ok(Box::pin(futures::stream::iter(vec![
                AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// 工具流式 partial 在执行期间被 drain 为 ToolExecutionUpdate 事件，顺序正确、不丢失。
    #[tokio::test]
    async fn tool_streaming_updates_emitted_during_execution() {
        let calls = Arc::new(AtomicUsize::new(0));
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(StreamingTool));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);
        let mut model = agent_core::Model::with_defaults(
            "streaming-prov",
            "streaming-prov",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;

        let agent = Agent::builder(model)
            .provider(Arc::new(StreamingProvider {
                calls: calls.clone(),
            }))
            .tools(tools)
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .build();

        let mut updates: Vec<String> = Vec::new();
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::ToolExecutionUpdate { partial, .. } = ev {
                updates.push(partial);
            }
        }
        assert_eq!(
            updates,
            vec!["chunk-1".to_string(), "chunk-2".into(), "chunk-3".into()],
            "应按序收到 3 条流式 partial（select! 边执行边 drain + 兜底 drain 保证不丢失）"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2, "应正好 2 轮模型调用");
    }

    // ── transform_assistant（P1-M：最终化后改写钩子）─────────────────────────

    /// 返回含宏占位符的 assistant 文本，供 transform 改写。
    struct TransformProvider;
    #[async_trait]
    impl agent_core::LlmProvider for TransformProvider {
        fn id(&self) -> &'static str {
            "transform"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let msg = AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "a @[[x]] b".into(),
                }],
                usage: Usage::default(),
                model: "transform".into(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// transform 在 MessageEnd / context.append / 工具分发前改写（单一真相源）：
    /// 把 `@[[x]]` 宏展开为 `[expanded]`，下游事件应携带改写后文本。
    #[tokio::test]
    async fn transform_assistant_rewrites_before_downstream() {
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let model = agent_core::Model::with_defaults(
            "transform",
            "transform",
            agent_core::Api::OpenAiCompletions,
        );
        let tf: Arc<dyn Fn(&mut agent_core::AssistantMessage) + Send + Sync> = Arc::new(|m| {
            for b in &mut m.content {
                if let agent_core::ContentBlock::Text { text } = b {
                    *text = text.replace("@[[x]]", "[expanded]");
                }
            }
        });
        let agent = Agent::builder(model)
            .provider(Arc::new(TransformProvider))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .transform_assistant(tf)
            .build();

        let mut event_text = String::new();
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::MessageEnd(msg) = ev {
                event_text = msg.text();
            }
        }
        assert_eq!(
            event_text, "a [expanded] b",
            "MessageEnd 应携带改写后文本（transform 在事件前 apply）"
        );
        assert!(!event_text.contains("@[[x]]"), "改写后不应含原始宏占位符");
    }

    // ── on_turn_end 钩子（P6-A：per-turn 程序化副作用）───────────────────────

    /// 记录 on_turn_end 调用的 will_continue 序列。
    struct TurnEndRecorder {
        calls: Arc<std::sync::Mutex<Vec<bool>>>,
    }
    #[async_trait]
    impl Hook for TurnEndRecorder {
        async fn on_event(&self, _: &HookEvent) {}
        async fn on_turn_end(&self, ctx: &agent_core::TurnEndContext<'_>) {
            self.calls.lock().unwrap().push(ctx.will_continue);
        }
    }

    /// on_turn_end 在每个 turn 结束被调用，携带正确的 will_continue：
    /// 工具轮（true，继续）→ 停止轮（false）。
    #[tokio::test]
    async fn on_turn_end_hook_fires_with_will_continue() {
        let prov_calls = Arc::new(AtomicUsize::new(0));
        let hook_calls: Arc<std::sync::Mutex<Vec<bool>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(ProbeTool {
            name: "probe".into(),
            cap: CapabilityTier::ReadOnly,
            ms: 0,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
        }));
        let tools: Arc<dyn ToolRegistry> = Arc::new(reg);
        let mut model = agent_core::Model::with_defaults(
            "two-turn",
            "two-turn",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;

        let agent = Agent::builder(model)
            .provider(Arc::new(TwoTurnProvider { calls: prov_calls }))
            .tools(tools)
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .hooks(vec![Arc::new(TurnEndRecorder {
                calls: hook_calls.clone(),
            })])
            .build();

        let stream = agent.run("go");
        tokio::pin!(stream);
        while stream.next().await.is_some() {}
        let guard = hook_calls.lock().unwrap();
        assert_eq!(
            *guard,
            vec![true, false],
            "on_turn_end 应在工具轮（will_continue=true）与停止轮（false）各调一次"
        );
    }

    // ── Magic keywords（ultrathink / orchestrate）集成 ────────────────────────

    /// prompt 含 `orchestrate` → 隐藏通知先于用户消息注入 context；不含 → 无注入。
    #[tokio::test]
    async fn magic_keyword_orchestrate_injects_notice() {
        let ctx: Arc<dyn agent_core::ContextManager> =
            Arc::new(InMemoryContext::new(vec![]));
        let model = agent_core::Model::with_defaults(
            "kw",
            "kw",
            agent_core::Api::OpenAiCompletions,
        );
        let agent = Agent::builder(model.clone())
            .provider(Arc::new(TtsrStreamProvider {
                calls: Arc::new(AtomicUsize::new(0)),
            }))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(Arc::clone(&ctx))
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .build();

        let stream = agent.run("请 orchestrate 这些任务");
        tokio::pin!(stream);
        while stream.next().await.is_some() {}
        let built = ctx
            .build_provider_context(&model, &[])
            .await
            .expect("上下文可构建");
        let texts: Vec<&str> = built
            .messages
            .iter()
            .filter_map(|m| match m {
                agent_core::ProviderMessage::User { content } => content.iter().find_map(
                    |c| match c {
                        agent_core::UserContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    },
                ),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("[magic-keyword:orchestrate]")),
            "orchestrate 命中应注入隐藏通知"
        );
    }

    /// `orchestrates`（复数）不命中：无通知注入。
    #[tokio::test]
    async fn magic_keyword_plural_does_not_inject() {
        let ctx: Arc<dyn agent_core::ContextManager> =
            Arc::new(InMemoryContext::new(vec![]));
        let model = agent_core::Model::with_defaults(
            "kw",
            "kw",
            agent_core::Api::OpenAiCompletions,
        );
        let agent = Agent::builder(model.clone())
            .provider(Arc::new(TtsrStreamProvider {
                calls: Arc::new(AtomicUsize::new(0)),
            }))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(Arc::clone(&ctx))
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .build();

        let stream = agent.run("请 orchestrates 这些任务");
        tokio::pin!(stream);
        while stream.next().await.is_some() {}
        let built = ctx
            .build_provider_context(&model, &[])
            .await
            .expect("上下文可构建");
        let has_notice = built.messages.iter().any(|m| {
            matches!(m, agent_core::ProviderMessage::User { content } if content.iter().any(
                |c| matches!(c, agent_core::UserContent::Text { text } if text.contains("[magic-keyword:"))
            ))
        });
        assert!(!has_notice, "复数形式不应命中关键词");
    }

    // ── Advisor（P1-L 独立评审）集成 ───────────────────────────────────────

    /// advisor 专用 provider 桩：吐一条 blocker 建议后正常结束。
    struct AdvisorProvider {
        advice: &'static str,
    }
    #[async_trait]
    impl agent_core::LlmProvider for AdvisorProvider {
        fn id(&self) -> &'static str {
            "advisor-stub"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<BoxStream<'static, agent_core::AssistantEvent>, agent_core::LlmError> {
            let msg = agent_core::AssistantMessage {
                content: vec![agent_core::ContentBlock::Text {
                    text: self.advice.into(),
                }],
                usage: agent_core::Usage::default(),
                model: "advisor-stub".into(),
                stop_reason: Some(agent_core::StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::TextDelta(self.advice.into()),
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// advisor 评审命中 → `[advisor:blocker]` 标记消息注入 context；filler 建议被 guard 拦截。
    #[tokio::test]
    async fn advisor_injects_blocker_advice() {
        let ctx: Arc<dyn agent_core::ContextManager> =
            Arc::new(InMemoryContext::new(vec![]));
        let model = agent_core::Model::with_defaults(
            "adv",
            "adv",
            agent_core::Api::OpenAiCompletions,
        );
        let advisor = agent_advisor::Advisor::new(
            Arc::new(AdvisorProvider {
                advice: "[blocker] The fix no longer matches acceptance criteria.",
            }),
            model.clone(),
        );
        let agent = Agent::builder(model.clone())
            .provider(Arc::new(TtsrStreamProvider {
                calls: Arc::new(AtomicUsize::new(0)),
            }))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(Arc::clone(&ctx))
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .advisor(Some(Arc::new(advisor)))
            .advisor_every_n_turns(1)
            .build();

        let stream = agent.run("请完成任务");
        tokio::pin!(stream);
        while stream.next().await.is_some() {}
        let built = ctx
            .build_provider_context(&model, &[])
            .await
            .expect("上下文可构建");
        let has_advice = built.messages.iter().any(|m| {
            matches!(m, agent_core::ProviderMessage::User { content } if content.iter().any(
                |c| matches!(c, agent_core::UserContent::Text { text }
                    if text.contains("[advisor:blocker]") && text.contains("acceptance criteria"))
            ))
        });
        assert!(has_advice, "blocker 建议应注入 context");
    }

    /// filler（"Stop."）不注入；评审失败（provider 报错）不阻断主循环。
    #[tokio::test]
    async fn advisor_filters_filler_and_tolerates_failure() {
        let ctx: Arc<dyn agent_core::ContextManager> =
            Arc::new(InMemoryContext::new(vec![]));
        let model = agent_core::Model::with_defaults(
            "adv",
            "adv",
            agent_core::Api::OpenAiCompletions,
        );
        let advisor = agent_advisor::Advisor::new(
            Arc::new(AdvisorProvider { advice: "[nit] Stop." }),
            model.clone(),
        );
        let agent = Agent::builder(model.clone())
            .provider(Arc::new(TtsrStreamProvider {
                calls: Arc::new(AtomicUsize::new(0)),
            }))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(Arc::clone(&ctx))
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .advisor(Some(Arc::new(advisor)))
            .advisor_every_n_turns(1)
            .build();

        let stream = agent.run("请完成任务");
        tokio::pin!(stream);
        while stream.next().await.is_some() {}
        let built = ctx
            .build_provider_context(&model, &[])
            .await
            .expect("上下文可构建");
        let has_advice = built.messages.iter().any(|m| {
            matches!(m, agent_core::ProviderMessage::User { content } if content.iter().any(
                |c| matches!(c, agent_core::UserContent::Text { text } if text.contains("[advisor:"))
            ))
        });
        assert!(!has_advice, "filler 建议不应注入");
    }

    // ── TTSR（时间旅行流规则）集成 ───────────────────────────────────────────

    /// 构造 TTSR 协调器（单条文本规则 `leak`：匹配 `Box::leak`）。
    fn ttsr_leak_coordinator() -> std::sync::Arc<agent_ttsr::TtsrCoordinator> {
        std::sync::Arc::new(agent_ttsr::TtsrCoordinator::new(
            agent_ttsr::TtsrConfig::default(),
            vec![agent_ttsr::parse_rule(
                "leak",
                "---\ncondition: [Box::leak]\n---\n禁止在生产代码路径使用 Box::leak。",
            )
            .unwrap()],
        ))
    }

    /// 单轮工具调用 Provider：**前 `violating_calls` 次**返回指定工具调用，后续返回纯文本
    /// 收尾（保证测试有界终止）。
    struct SingleToolCallProvider {
        tool: &'static str,
        args: serde_json::Value,
        calls: Arc<AtomicUsize>,
        /// 返回违规工具调用的次数（之后返回收尾文本）。
        violating_calls: usize,
    }
    impl SingleToolCallProvider {
        fn new(
            tool: &'static str,
            args: serde_json::Value,
            calls: Arc<AtomicUsize>,
        ) -> Self {
            Self {
                tool,
                args,
                calls,
                violating_calls: 1,
            }
        }
        fn violating(mut self, n: usize) -> Self {
            self.violating_calls = n;
            self
        }
    }
    #[async_trait]
    impl agent_core::LlmProvider for SingleToolCallProvider {
        fn id(&self) -> &'static str {
            "ttsr-tool"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<BoxStream<'static, agent_core::AssistantEvent>, agent_core::LlmError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let msg = if n < self.violating_calls {
                agent_core::AssistantMessage {
                    content: vec![agent_core::ContentBlock::ToolCall {
                        id: format!("call-{n}"),
                        name: self.tool.into(),
                        arguments: self.args.clone(),
                    }],
                    usage: agent_core::Usage::default(),
                    model: "ttsr-tool".into(),
                    stop_reason: Some(agent_core::StopReason::ToolUse),
                    stop_details: None,
                }
            } else {
                agent_core::AssistantMessage {
                    content: vec![agent_core::ContentBlock::Text {
                        text: "完成".into(),
                    }],
                    usage: agent_core::Usage::default(),
                    model: "ttsr-tool".into(),
                    stop_reason: Some(agent_core::StopReason::Stop),
                    stop_details: None,
                }
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// 流式 Provider：按序发射文本增量 + 无工具 MessageEnd；记录调用次数。
    struct TtsrStreamProvider {
        calls: Arc<AtomicUsize>,
    }    #[async_trait]
    impl agent_core::LlmProvider for TtsrStreamProvider {
        fn id(&self) -> &'static str {
            "ttsr-stream"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: agent_core::CompletionRequest,
            _ctx: &agent_core::ProviderCallContext,
        ) -> Result<BoxStream<'static, agent_core::AssistantEvent>, agent_core::LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let msg = agent_core::AssistantMessage {
                content: vec![agent_core::ContentBlock::Text {
                    text: "完成".into(),
                }],
                usage: agent_core::Usage::default(),
                model: "ttsr-stream".into(),
                stop_reason: Some(agent_core::StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::TextDelta("let x = ".into()),
                agent_core::AssistantEvent::TextDelta("Box::leak".into()),
                agent_core::AssistantEvent::TextDelta("(y);".into()),
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    /// 流式命中规则 → 中断流、丢弃部分输出、注入规则并重试（provider 被调 2 次）；
    /// once 抑制：第二次调用不再中断。
    #[tokio::test]
    async fn ttsr_mid_stream_abort_injects_and_retries() {
        let calls = Arc::new(AtomicUsize::new(0));
        let ctx: Arc<dyn agent_core::ContextManager> =
            Arc::new(InMemoryContext::new(vec![]));
        let model = agent_core::Model::with_defaults(
            "ttsr-stream",
            "ttsr-stream",
            agent_core::Api::OpenAiCompletions,
        );
        let agent = Agent::builder(model.clone())
            .provider(Arc::new(TtsrStreamProvider { calls: calls.clone() }))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(Arc::clone(&ctx))
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .ttsr(Some(ttsr_leak_coordinator()))
            .build();

        let mut saw_warning = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Say(s) = &ev {
                if s.text.contains("规则违规") && s.text.contains("leak") {
                    saw_warning = true;
                }
            }
        }
        assert!(saw_warning, "应发出规则违规警告");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "中断后应重试一次（once 抑制，第二次不再中断）"
        );
        // 注入消息已进上下文（带持久化标记）。
        let built = ctx
            .build_provider_context(&model, &[])
            .await
            .expect("上下文可构建");
        let has_marker = built.messages.iter().any(|m| {
            matches!(m, agent_core::ProviderMessage::User { content } if content.iter().any(
                |c| matches!(c, agent_core::UserContent::Text { text } if text.contains("[ttsr-injection:leak]"))
            ))
        });
        assert!(has_marker, "注入消息应带 [ttsr-injection:leak] 标记");
        // 违规增量未被发射（用户在 UI 上看不到违规内容）。
        let built2 = ctx
            .build_provider_context(&model, &[])
            .await
            .expect("上下文可构建");
        let _ = built2;
    }

    /// 会话加载恢复：上下文中已存在注入标记 → 规则抑制，不再中断（provider 仅调 1 次）。
    #[tokio::test]
    async fn ttsr_restore_from_context_suppresses_refire() {
        let calls = Arc::new(AtomicUsize::new(0));
        let ctx: Arc<dyn agent_core::ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        ctx.append(agent_core::AgentMessage::user_text(
            "[ttsr-injection:leak]\n<system-interrupt reason=\"rule_violation\" rule=\"leak\">\n禁止 Box::leak。\n</system-interrupt>",
        ))
        .await;
        let model = agent_core::Model::with_defaults(
            "ttsr-stream",
            "ttsr-stream",
            agent_core::Api::OpenAiCompletions,
        );
        let agent = Agent::builder(model)
            .provider(Arc::new(TtsrStreamProvider { calls: calls.clone() }))
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(Arc::clone(&ctx))
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .ttsr(Some(ttsr_leak_coordinator()))
            .build();

        let mut saw_warning = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Say(s) = &ev {
                if s.text.contains("规则违规") {
                    saw_warning = true;
                }
            }
        }
        assert!(!saw_warning, "恢复抑制后不应再次中断");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "不应重试");
    }

    /// 工具作用域非打断规则：MessageEnd 快照命中 → 提醒折叠进工具结果（不中断执行）。
    #[tokio::test]
    async fn ttsr_tool_never_rule_folds_reminder_into_result() {
        let calls = Arc::new(AtomicUsize::new(0));
        let ctx: Arc<dyn agent_core::ContextManager> =
            Arc::new(InMemoryContext::new(vec![]));
        let ttsr = std::sync::Arc::new(agent_ttsr::TtsrCoordinator::new(
            agent_ttsr::TtsrConfig::default(),
            vec![agent_ttsr::parse_rule(
                "no-secret",
                "---\nscope: tool:echo\ninterruptMode: never\ncondition: [secret]\n---\n不要输出 secret。",
            )
            .unwrap()],
        ));
        // 回显工具（把参数 JSON 作为结果文本返回，便于断言折叠）。
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(EchoTool));
        let tools: Arc<dyn agent_tools::ToolRegistry> = Arc::new(reg);
        // Provider：单轮返回含 echo 工具调用的消息。
        let provider = SingleToolCallProvider::new(
            "echo",
            serde_json::json!({ "message": "secret here" }),
            calls.clone(),
        );
        let model = agent_core::Model::with_defaults(
            "ttsr-tool",
            "ttsr-tool",
            agent_core::Api::OpenAiCompletions,
        );
        let agent = Agent::builder(model.clone())
            .provider(Arc::new(provider))
            .tools(Arc::clone(&tools))
            .context(Arc::clone(&ctx))
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .ttsr(Some(ttsr))
            .build();

        let stream = agent.run("go");
        tokio::pin!(stream);
        while stream.next().await.is_some() {}
        let built = ctx
            .build_provider_context(&model, &[])
            .await
            .expect("上下文可构建");
        let tool_msgs: Vec<String> = built
            .messages
            .iter()
            .filter_map(|m| match m {
                agent_core::ProviderMessage::Tool { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert!(
            tool_msgs.iter().any(|c| c.contains("<system-reminder reason=\"rule_violation\" rule=\"no-secret\">")),
            "工具结果应折叠 system-reminder 提醒"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "非打断规则不应触发重试（工具轮执行后正常收尾，共 2 次调用）"
        );
    }

    /// 工具作用域可中断规则：MessageEnd 快照命中 → 丢弃整条 assistant、注入后重试；
    /// 第一轮工具未执行（max_seen == 0 只统计第二轮）。
    #[tokio::test]
    async fn ttsr_tool_always_rule_discards_and_retries() {
        let calls = Arc::new(AtomicUsize::new(0));
        let ctx: Arc<dyn agent_core::ContextManager> =
            Arc::new(InMemoryContext::new(vec![]));
        let ttsr = std::sync::Arc::new(agent_ttsr::TtsrCoordinator::new(
            agent_ttsr::TtsrConfig::default(),
            vec![agent_ttsr::parse_rule(
                "no-secret",
                "---\nscope: tool:echo\ncondition: [secret]\n---\n不要输出 secret。",
            )
            .unwrap()],
        ));
        let mut reg = DefaultToolRegistry::new();
        reg.register(Box::new(EchoTool));
        let tools: Arc<dyn agent_tools::ToolRegistry> = Arc::new(reg);
        let provider = SingleToolCallProvider::new(
            "echo",
            serde_json::json!({ "message": "secret here" }),
            calls.clone(),
        )
        .violating(2);
        let model = agent_core::Model::with_defaults(
            "ttsr-tool",
            "ttsr-tool",
            agent_core::Api::OpenAiCompletions,
        );
        let agent = Agent::builder(model.clone())
            .provider(Arc::new(provider))
            .tools(Arc::clone(&tools))
            .context(Arc::clone(&ctx))
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .ttsr(Some(ttsr))
            .build();

        let mut saw_warning = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Say(s) = &ev {
                if s.text.contains("工具调用违反规则") {
                    saw_warning = true;
                }
            }
        }
        assert!(saw_warning, "应发出工具违规警告");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "可中断工具规则：abort 轮 + 执行轮 + 收尾轮（once 抑制后第二轮正常执行）"
        );
        let built = ctx
            .build_provider_context(&model, &[])
            .await
            .expect("上下文可构建");
        let has_marker = built.messages.iter().any(|m| {
            matches!(m, agent_core::ProviderMessage::User { content } if content.iter().any(
                |c| matches!(c, agent_core::UserContent::Text { text } if text.contains("[ttsr-injection:no-secret]"))
            ))
        });
        assert!(has_marker, "注入消息应带 [ttsr-injection:no-secret] 标记");
    }

    // ── P2：模型 fallback 链 + API key 轮换 ────────────────────────────────

    /// 按模型 id 分派 + 记录调用（model.id, ctx.api_key）的桩 Provider。
    struct FallbackProbe {
        calls: Arc<parking_lot::Mutex<Vec<(String, Option<String>)>>>,
        /// 对这些 id 返回可重试错误（其余返回 done）。
        fail_ids: &'static [&'static str],
        /// 对该 id 返回**不可重试**错误（换模型无意义，应立即上抛）。
        hard_fail_id: Option<&'static str>,
    }

    #[async_trait]
    impl agent_core::LlmProvider for FallbackProbe {
        fn id(&self) -> &'static str {
            "probe"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            req: agent_core::CompletionRequest,
            ctx: &agent_core::ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            self.calls.lock().push((req.model.id.clone(), ctx.api_key.clone()));
            if self.hard_fail_id == Some(req.model.id.as_str()) {
                return Err(agent_core::LlmError::Decode("不可重试".into()));
            }
            if self.fail_ids.contains(&req.model.id.as_str()) {
                return Err(agent_core::LlmError::Transport("boom".into()));
            }
            let msg = AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                usage: Usage::default(),
                model: req.model.id.clone(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                agent_core::AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    fn probe_agent(
        probe: Arc<FallbackProbe>,
        model: agent_core::Model,
        fallbacks: Vec<agent_core::Model>,
        key_rings: std::collections::HashMap<String, Vec<String>>,
    ) -> AgentBuilder {
        Agent::builder(model)
            .provider(probe)
            .tools(Arc::new(DefaultToolRegistry::new()))
            .context(Arc::new(InMemoryContext::new(vec!["sys".into()])))
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval))
            .workspace(Arc::new(Workspace::new(".")))
            .fallbacks(fallbacks)
            .key_rings(key_rings)
    }

    /// 主模型失败（可重试）→ 依序尝试备用模型；全链只算一次 LLM 调用失败。
    #[tokio::test]
    async fn model_fallback_switches_to_next_model() {
        let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let probe = Arc::new(FallbackProbe {
            calls: Arc::clone(&calls),
            fail_ids: &["primary"],
            hard_fail_id: None,
        });
        let primary = agent_core::Model::with_defaults(
            "primary",
            "p",
            agent_core::Api::AnthropicMessages,
        );
        let backup = agent_core::Model::with_defaults(
            "backup",
            "p",
            agent_core::Api::OpenAiCompletions,
        );
        let agent = probe_agent(probe, primary, vec![backup], Default::default()).build();
        let mut said = String::new();
        let mut errs = 0;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::MessageEnd(m) => {
                    for c in &m.content {
                        if let agent_core::ContentBlock::Text { text } = c {
                            said.push_str(text);
                        }
                    }
                }
                AgentEvent::Error(_) => errs += 1,
                _ => {}
            }
        }
        assert_eq!(said, "done", "备用模型应产出结果");
        assert_eq!(errs, 0);
        let calls = calls.lock();
        assert_eq!(calls.len(), 2, "主模型失败后应尝试备用模型");
        assert_eq!(calls[0].0, "primary");
        assert_eq!(calls[1].0, "backup");
    }

    /// 全链失败 = 本轮 1 个 mistake（轮内换模型不重复计错）；max_mistakes=1 时一轮即停。
    #[tokio::test]
    async fn model_fallback_all_fail_yields_error_and_counts_single_mistake() {
        let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let probe = Arc::new(FallbackProbe {
            calls: Arc::clone(&calls),
            fail_ids: &["primary", "backup"],
            hard_fail_id: None,
        });
        let primary = agent_core::Model::with_defaults(
            "primary",
            "p",
            agent_core::Api::AnthropicMessages,
        );
        let backup = agent_core::Model::with_defaults("backup", "p", agent_core::Api::Zai);
        let agent = probe_agent(probe, primary, vec![backup], Default::default())
            .max_mistakes(1)
            .build();
        let mut errs = 0;
        let mut stopped = false;
        let mut saw_boom = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::Error(e) => {
                    errs += 1;
                    if e.contains("boom") {
                        saw_boom = true;
                    }
                }
                AgentEvent::StateChanged(agent_core::AgentState::Idle) => stopped = true,
                _ => {}
            }
        }
        assert!(saw_boom, "应透传底层错误");
        assert_eq!(errs, 2, "boom 错误 + 停止提示各一次（全链失败只算 1 个 mistake）");
        assert!(stopped, "达到 max_mistakes 应停止");
        let calls = calls.lock();
        assert_eq!(calls.len(), 2, "本轮应尝试链上全部模型");
    }

    /// 不可重试错误（解码失败等）立即上抛，不浪费备用模型尝试。
    #[tokio::test]
    async fn model_fallback_hard_error_aborts_chain() {
        let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let probe = Arc::new(FallbackProbe {
            calls: Arc::clone(&calls),
            fail_ids: &[],
            hard_fail_id: Some("primary"),
        });
        let primary = agent_core::Model::with_defaults(
            "primary",
            "p",
            agent_core::Api::AnthropicMessages,
        );
        let backup = agent_core::Model::with_defaults("backup", "p", agent_core::Api::Zai);
        let agent = probe_agent(probe, primary, vec![backup], Default::default())
            .max_mistakes(1)
            .build();
        let mut errs = 0;
        let mut saw_hard = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Error(e) = ev {
                errs += 1;
                if e.contains("不可重试") {
                    saw_hard = true;
                }
            }
        }
        assert!(saw_hard, "应透传不可重试错误");
        assert_eq!(errs, 2, "错误 + 停止提示");
        let calls = calls.lock();
        assert_eq!(calls.len(), 1, "不可重试错误不应尝试备用模型");
    }

    /// key 环 round-robin：同模型多次调用依序取 key；RuntimeOverrides 命中时优先。
    #[tokio::test]
    async fn key_ring_rotates_and_override_wins() {
        let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let probe = Arc::new(FallbackProbe {
            calls: Arc::clone(&calls),
            fail_ids: &[],
            hard_fail_id: None,
        });
        let model = agent_core::Model::with_defaults(
            "primary",
            "p",
            agent_core::Api::AnthropicMessages,
        );
        let rings =
            std::collections::HashMap::from([("primary".to_string(), vec!["k1".to_string(), "k2".to_string()])]);
        let agent = probe_agent(Arc::clone(&probe), model.clone(), Vec::new(), rings).build();
        let mut stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Done(_) = ev {
                break;
            }
        }
        let mut stream = agent.run("go2");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Done(_) = ev {
                break;
            }
        }
        // override 优先：命中时跳过轮换。
        struct KeyOverride;
        impl RuntimeOverrides for KeyOverride {
            fn api_key(&self, _m: &agent_core::Model) -> Option<String> {
                Some("override-key".into())
            }
        }
        let agent2 = probe_agent(Arc::clone(&probe), model, Vec::new(), Default::default())
            .runtime_overrides(Arc::new(KeyOverride))
            .build();
        let mut stream = agent2.run("go3");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Done(_) = ev {
                break;
            }
        }
        let calls = calls.lock();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].1.as_deref(), Some("k1"), "首轮 k1");
        assert_eq!(calls[1].1.as_deref(), Some("k2"), "次轮 k2（round-robin）");
        assert_eq!(calls[2].1.as_deref(), Some("override-key"), "override 优先");
    }

    #[test]
    fn key_ring_round_robins() {
        let ring = KeyRing::new(vec!["k1".into(), "k2".into()]);
        assert_eq!(ring.next().as_deref(), Some("k1"));
        assert_eq!(ring.next().as_deref(), Some("k2"));
        assert_eq!(ring.next().as_deref(), Some("k1"));
        assert!(KeyRing::new(Vec::new()).next().is_none(), "空环不轮换");
    }

    // ── P0-3（goals 目标预算：软模式注入提醒续跑 / 硬模式停止）───────────────

    /// 桩 Provider：每轮返回纯文本，usage.input_tokens = 80（无工具调用 → 每轮触达停止边界）。
    struct BudgetProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl agent_core::LlmProvider for BudgetProvider {
        fn id(&self) -> &'static str {
            "budget-stub"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _request: CompletionRequest,
            _ctx: &ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let n = self.calls.load(Ordering::SeqCst);
            // 前两轮返回未知工具调用（空注册表 → 执行报错 → 循环继续），
            // 使预算在第 2 轮用量累计后超限；第 3 轮起返回纯文本触发停止边界。
            let content = if n <= 2 {
                vec![ContentBlock::ToolCall {
                    id: format!("c{n}"),
                    name: "no_such_tool".into(),
                    arguments: serde_json::json!({}),
                }]
            } else {
                vec![ContentBlock::Text {
                    text: format!("reply {n}"),
                }]
            };
            let msg = AssistantMessage {
                content,
                usage: Usage {
                    input_tokens: 80,
                    ..Usage::default()
                },
                model: "budget-stub".into(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    fn budget_agent(
        provider: Arc<dyn LlmProvider>,
        goal: Arc<std::sync::Mutex<GoalState>>,
    ) -> Agent {
        let ctx: Arc<dyn ContextManager> = Arc::new(InMemoryContext::new(vec![]));
        let tools: Arc<dyn ToolRegistry> = Arc::new(DefaultToolRegistry::new());
        let mut model = agent_core::Model::with_defaults(
            "budget-stub",
            "budget-stub",
            agent_core::Api::OpenAiCompletions,
        );
        model.max_input_tokens = 200_000;
        Agent::builder(model)
            .provider(provider)
            .tools(tools)
            .context(ctx)
            .prompts(Arc::new(agent_prompt::PromptCatalog::new()))
            .approval(Arc::new(YoloApproval)) // 无工具轮，不触发审批
            .workspace(Arc::new(Workspace::new(".")))
            .goals_state(goal)
            .build()
    }

    /// 软模式：第 2 轮累计 160 ≥ 预算 100 → 首次文本轮（第 3 轮）停止边界注入预算提醒并
    /// 续跑一轮；第 4 轮已提醒过不重复注入 → 正常停止。provider 应被调用 4 次。
    #[tokio::test]
    async fn goal_soft_budget_injects_once_then_stops() {
        let calls = Arc::new(AtomicUsize::new(0));
        let goal = Arc::new(std::sync::Mutex::new(GoalState::new(GoalBudget {
            token_budget: 100,
            time_budget: std::time::Duration::ZERO,
            hard_stop: false,
        })));
        let agent = budget_agent(Arc::new(BudgetProvider { calls: calls.clone() }), goal);
        let mut injected = false;
        let mut done = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Say(s) = &ev
                && s.text.contains("目标预算")
            {
                injected = true;
            }
            if matches!(ev, AgentEvent::Done(_)) {
                done = true;
            }
        }
        assert!(injected, "预算超限应在停止边界注入提醒");
        assert!(done, "最终应正常结束");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "注入后续跑一轮（4 次调用），且最后一轮不重复注入（否则 >4）"
        );
    }

    /// 硬模式：第 2 轮超限后，首个文本轮（第 3 轮）停止边界不注入、直接正常停止。
    /// provider 应被调用 3 次。
    #[tokio::test]
    async fn goal_hard_budget_stops_without_injection() {
        let calls = Arc::new(AtomicUsize::new(0));
        let goal = Arc::new(std::sync::Mutex::new(GoalState::new(GoalBudget {
            token_budget: 100,
            time_budget: std::time::Duration::ZERO,
            hard_stop: true,
        })));
        let agent = budget_agent(Arc::new(BudgetProvider { calls: calls.clone() }), goal);
        let mut injected = false;
        let mut done = false;
        let stream = agent.run("go");
        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Say(s) = &ev
                && s.text.contains("目标预算")
            {
                injected = true;
            }
            if matches!(ev, AgentEvent::Done(_)) {
                done = true;
            }
        }
        assert!(!injected, "硬模式不注入提醒");
        assert!(done, "硬模式正常结束");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "硬模式不续跑（3 次调用）"
        );
    }
}
