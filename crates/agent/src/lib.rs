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
mod task_tool;
pub use task_tool::{ContextFactory, TaskTool};

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
    /// 覆盖本轮 temperature；返回 `None` 沿用 [`AgentBuilder::temperature`] 静态值。
    fn temperature(&self) -> Option<f32> {
        None
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
    /// aside 接收端（外部注入**被动、非中断**通知——后台任务完成、延迟 LSP diagnostics 等）。
    /// 与 steering 的区别：aside **永不**打断在途工具（不走 Immediate 批级 cancel），只在
    /// 轮次边界（下一轮模型调用前 / 停止边界）折叠注入。移植 oh-my-pi `getAsideMessages`。
    aside_rx:
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
            aside_rx: None,
            runtime_overrides: None,
            transform_assistant: None,
            write_effect: None,
            interrupt_mode: InterruptMode::default(),
        }
    }

    /// 取消句柄（外部中止）。
    #[must_use]
    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel.clone()
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
    /// aside 信道：外部注入被动、非中断通知（后台完成 / 延迟 diagnostics 等）。
    aside_rx: Option<tokio::sync::mpsc::UnboundedReceiver<AgentMessage>>,
    /// 运行时配置覆盖（每轮解析 thinking / temperature；mid-run 热更新）。
    runtime_overrides: Option<Arc<dyn RuntimeOverrides>>,
    /// assistant 消息改写钩子（最终化后、入 context/UI/tools 前）。
    transform_assistant: Option<Arc<dyn Fn(&mut agent_core::AssistantMessage) + Send + Sync>>,
    /// 写入效果（编辑后 LSP format/diagnostics）。
    write_effect: Option<Arc<dyn WriteEffect>>,
    /// 工具执行期的 steering 中断策略。
    interrupt_mode: InterruptMode,
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
            soft_requirement: Arc::new(std::sync::Mutex::new(self.soft_requirement)),
            steer_rx: tokio::sync::Mutex::new(self.steer_rx),
            aside_rx: tokio::sync::Mutex::new(self.aside_rx),
            runtime_overrides: self.runtime_overrides,
            transform_assistant: self.transform_assistant,
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

/// 执行循环本体（async_stream 生成器）。
fn run_loop(
    agent: &Agent,
    user_msg: agent_core::UserMessage,
    cancel: CancellationToken,
) -> impl futures::Stream<Item = AgentEvent> + '_ {
    let provider = Arc::clone(&agent.provider);
    let tools = Arc::clone(&agent.tools);
    let context = Arc::clone(&agent.context);
    let prompts = Arc::clone(&agent.prompts);
    let approval = Arc::clone(&agent.approval);
    let workspace = Arc::clone(&agent.workspace);
    let write_effect = agent.write_effect.clone();
    let model = agent.model.clone();
    let provider_ctx = agent.provider_ctx.clone();
    let mode = agent.mode;
    let max_mistakes = agent.max_mistakes;
    let max_turns = agent.max_turns;
    // 运行时限绝对时刻（在 run 起点把 Duration 折算成 Instant，循环内单调时钟比较）。
    let deadline_at = agent.deadline.map(|d| std::time::Instant::now() + d);
    let guard = agent.context_guard;
    let max_tokens = agent.max_output_tokens;
    let temperature = agent.temperature;
    let thinking = agent.thinking.clone();
    let thinking_policy = agent.thinking_policy.clone();
    // P1-K：提取本轮用户 prompt 文本（须在 user_msg move 进 context 前），供自适应思考分类。
    let prompt_text: String = user_msg
        .content
        .iter()
        .filter_map(|c| match c {
            agent_core::UserContent::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let catalog = agent.catalog.clone();
    let context_files = agent.context_files.clone();
    let hooks = agent.hooks.clone();
    let memory = agent.memory.clone();
    let resources = agent.resources.clone();
    let soft_requirement = Arc::clone(&agent.soft_requirement);
    let steer_rx = &agent.steer_rx;
    let aside_rx = &agent.aside_rx;
    // RuntimeOverrides：Arc clone（廉价），stream! 内每轮 `.as_ref()` 解析。
    let runtime_overrides = agent.runtime_overrides.clone();
    // transform_assistant：Arc clone（廉价），每轮最终化后调用。
    let transform_assistant = agent.transform_assistant.clone();

    async_stream::stream! {
        // P1-D：GenAI invoke_agent span（OTel 语义规范）——agent run 的逻辑根 span。
        // async_stream! 内 yield 不能放入嵌套 async block（无法 Instrument 覆盖含 yield 的整段），
        // 且 stream 须 Send（server tokio::spawn 消费），enter guard 非 Send 跨 await 会破坏 Send，
        // 故 invoke_agent 作为「属性载体 span」：chat/execute_tool 子 span 经 `parent: &invoke_span`
        // 显式挂载；主停止点 record 终态（success/turns/usage/duration）。详见 [`record_run_end`]。
        let invoke_span = tracing::info_span!(
            "gen_ai.invoke_agent",
            gen_ai.operation.name = "invoke_agent",
            gen_ai.request.model = %model.id,
            gen_ai.system = %model.provider,
            gyre.mode = ?mode,
            gyre.success = tracing::field::Empty,
            gyre.turns = tracing::field::Empty,
            gyre.duration = tracing::field::Empty,
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
        );
        let run_start = std::time::Instant::now();
        // 设置稳定前缀（system prompt + tool spec 指纹冻结）。
        // 若注入了 Skill 目录，则追加 `<skills>` 段（仅当 read_file 工具可用时）。
        let specs0 = tools.specs();
        let has_read = specs0.iter().any(|s| s.name == "read_file");
        let mut system = prompts.system_with_platform(mode);
        // 注入当前工作目录（workspace 根）：让模型知晓 cwd，避免盲猜而执行 cd /workspace。
        system.push(prompts.workspace_section(&workspace.root()));
        // 上下文约定文件（AGENTS.md）注入为额外 system 段
        for cf in &context_files {
            system.push(cf.clone());
        }
        // 跨会话长期记忆：注入 summary 段（若有）
        if let Some(mem) = &memory {
            if let Ok(Some(summary)) = mem.summary().await {
                system.push(format!(
                    "\n\n<memories>\n以下为来自过往会话的长期记忆摘要（启发式上下文，与当前仓库/用户指令冲突时以仓库与指令为准）:\n\n{summary}\n</memories>\n"
                ));
            }
        }
        if let Some(cat) = &catalog {
            let visible = cat.for_prompt(mode, has_read);
            if let Some(section) = render_skills_section(&visible) {
                system.push(section);
            }
        }
        context.set_system(system, &specs0).await;
        context.append(agent_core::AgentMessage::User(user_msg)).await;
        yield AgentEvent::StateChanged(AgentState::Running);

        // P1-K：自适应思考预算——按本轮 prompt 难度解析 ThinkingConfig（移植 oh-my-pi
        // auto-thinking）。Auto 策略经分类器决定 Effort → budget，钳到模型范围；分类失败 →
        // fallback；模型不支持思考 → None（本轮不思考）。Static/None → 沿用静态 thinking。
        // 每轮 prompt 难度恒定，解析一次/run 即可（分类器成本 ≤ 一次 tiny 模型调用）。
        let thinking: Option<ThinkingConfig> = if let Some(policy) = &thinking_policy {
            policy.resolve(&prompt_text, &model).await
        } else {
            thinking
        };

        let mut summary = AgentRunSummary::default();
        // P2-K：coverage——注册的可用工具名（排序），用于结束时计算「注册但从未调用」。
        summary.tools_available = specs0.iter().map(|s| s.name.clone()).collect();
        summary.tools_available.sort();
        let mut mistakes: usize = 0;
        // pause_turn 连续重采样计数（见 MAX_PAUSED_CONTINUATIONS）。
        let mut paused_continuations: usize = 0;
        // P0-C：Harmony 泄漏双计数器（truncate-resume / abort-retry，各自独立上限）。
        let mut harmony_truncate_resume: usize = 0;
        let mut harmony_retry: usize = 0;
        // 软需求状态：已注入的 id、是否需升级为强制
        let mut injected_soft_id: String = String::new();
        let mut escalate_soft = false;
        // P1-C：软需求升级计数——模型非合规（detour 或未调所需工具）连续 N 轮后中止。
        let mut soft_escalations: usize = 0;

        loop {
            // 取消检查
            if cancel.is_cancelled() {
                yield AgentEvent::Error("任务被取消".into());
                yield AgentEvent::StateChanged(AgentState::Idle);
                return;
            }

            // deadline 检查：超过运行时限则优雅停止（已完成轮次保留，success=false）。
            if deadline_at.is_some_and(|d| std::time::Instant::now() >= d) {
                yield AgentEvent::Say(StatusMessage {
                    text: "达到运行时限（deadline），停止".into(),
                    kind: StatusKind::Warning,
                });
                for h in &hooks {
                    h.on_event(&HookEvent::Stop { success: false }).await;
                }
                record_run_end(&invoke_span, &summary, run_start);
                yield AgentEvent::Done(summary);
                yield AgentEvent::StateChanged(AgentState::Idle);
                return;
            }

            // steering：非阻塞消费中途注入的消息
            {
                let mut guard_steer = steer_rx.lock().await;
                if let Some(rx) = guard_steer.as_mut() {
                    while let Ok(msg) = rx.try_recv() {
                        context.append(msg).await;
                        yield AgentEvent::Say(StatusMessage {
                            text: "已注入 steering 消息".into(),
                            kind: StatusKind::Info,
                        });
                    }
                }
            }

            // aside：被动、非中断通知——每轮模型调用前折叠注入（mid-work 边界）。
            // 与 steering 的关键区别：aside **不**触发批级 cancel、不走 Immediate 中断轮询，
            // 只在轮次边界消费，故不打断在途工具。典型来源：后台任务完成、延迟 LSP
            // diagnostics、定时器。移植 oh-my-pi `getAsideMessages`（mid-work 折叠）。
            {
                let mut guard_aside = aside_rx.lock().await;
                if let Some(rx) = guard_aside.as_mut() {
                    while let Ok(msg) = rx.try_recv() {
                        context.append(msg).await;
                        yield AgentEvent::Say(StatusMessage {
                            text: "已注入 aside 消息".into(),
                            kind: StatusKind::Info,
                        });
                    }
                }
            }

            // 软工具需求：新 id 首次出现时注入提醒（仅提醒，tool_choice 保持 auto 保护前缀缓存）
            let mut soft_tool_name_this_turn: Option<String> = None;
            let soft_tool_choice: Option<ToolChoiceDirective> = {
                // 先 clone 出快照，立即释放 std Mutex guard（guard 非 Send，不可跨 await）。
                // 即使 Mutex 被 poison（其他持锁线程 panic），仍恢复 inner 数据——
                // 否则软工具需求功能会无声失效且无任何日志（详见 workspace.rs 的同款处理）。
                let snapshot = {
                    let guard = soft_requirement
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    guard
                        .as_ref()
                        .map(|r| (r.id.clone(), r.tool_name.clone(), r.reminder.clone()))
                }; // guard 在此 drop，不跨 await
                if let Some((id, tool_name, reminder)) = snapshot {
                    soft_tool_name_this_turn = Some(tool_name.clone());
                    if id != injected_soft_id {
                        injected_soft_id = id;
                        context.append(AgentMessage::user_text(reminder)).await;
                    }
                    // 升级判定：若上一轮未调用该工具，则强制（由 escalate 标记触发）
                    if std::mem::take(&mut escalate_soft) {
                        Some(ToolChoiceDirective::Hard(ToolChoice::Function { name: tool_name }))
                    } else {
                        Some(ToolChoiceDirective::Soft(SoftToolRequirement {
                            id: injected_soft_id.clone(),
                            tool_name,
                            reminder: String::new(),
                        }))
                    }
                } else {
                    None
                }
            };

            let specs = tools.specs();
            let mut built = match context.build_provider_context(&model, &specs).await {
                Ok(c) => c,
                Err(e) => {
                    yield AgentEvent::Error(e.to_string());
                    yield AgentEvent::StateChanged(AgentState::Idle);
                    return;
                }
            };

            // 上下文压缩（接近上限）：分级触发——先 shake（机械去冗余 + 大块归档，便宜），
            // 重新评估；仍超限才 summarize（LLM handoff 摘要，贵且可能失败）；再超限才
            // prune（窗口兜底）。shake 能救回时即可省去昂贵的 summarize LLM 请求。
            // 注：每级后都重新 build，确保本轮请求使用压缩后快照（修复「压缩滞后一轮」）。
            if built.tokens.near_limit(guard) {
                let mut stages: Vec<&str> = vec!["shake"];
                let _ = context.compact(CompactionStrategy::Shake).await;
                built = match context.build_provider_context(&model, &specs).await {
                    Ok(c) => c,
                    Err(e) => {
                        yield AgentEvent::Error(e.to_string());
                        yield AgentEvent::StateChanged(AgentState::Idle);
                        return;
                    }
                };
                if built.tokens.near_limit(guard) {
                    stages.push("summarize");
                    let _ = context
                        .compact(CompactionStrategy::Summarize { max_tokens: 0 })
                        .await;
                    built = match context.build_provider_context(&model, &specs).await {
                        Ok(c) => c,
                        Err(e) => {
                            yield AgentEvent::Error(e.to_string());
                            yield AgentEvent::StateChanged(AgentState::Idle);
                            return;
                        }
                    };
                    if built.tokens.near_limit(guard) {
                        stages.push("prune");
                        let _ = context.compact(CompactionStrategy::Prune { keep_recent: 8 }).await;
                        built = match context.build_provider_context(&model, &specs).await {
                            Ok(c) => c,
                            Err(e) => {
                                yield AgentEvent::Error(e.to_string());
                                yield AgentEvent::StateChanged(AgentState::Idle);
                                return;
                            }
                        };
                    }
                }
                yield AgentEvent::Say(StatusMessage {
                    text: format!("上下文接近上限，已触发压缩（{}）", stages.join(" + ")),
                    kind: StatusKind::Warning,
                });
            }

            // P3-A：运行时覆盖（mid-run 热更新，移植 oh-my-pi getReasoning/getDisableReasoning）。
            // 每轮请求构造前解析 host 注入的 RuntimeOverrides：返回 Some 的字段覆盖静态 /
            // ThinkingPolicy 解析结果，None 沿用。故可在 run 中途切换思考档位 / 温度，无需重建
            // Agent。shadow 本轮 temperature / thinking，供下方 req 使用。
            let temperature = runtime_overrides
                .as_ref()
                .and_then(|ro| ro.temperature())
                .or(temperature);
            let thinking = runtime_overrides
                .as_ref()
                .and_then(|ro| ro.thinking(&model))
                .or_else(|| thinking.clone());

            let req = CompletionRequest {
                model: model.clone(),
                system: built.system.clone(),
                messages: built.messages,
                tools: specs.clone(),
                tool_choice: soft_tool_choice.clone(),
                max_tokens,
                temperature,
                thinking: thinking.clone(),
                // P0-A：前缀指纹透传为 cache_key，供 provider 观测/命中前缀缓存
                //（fingerprint 含 system + tool spec）。
                cache_key: Some(built.fingerprint.clone()),
                // 稳定前缀长度：provider 据此精确放置 cache_control breakpoint（移植
                // oh-my-pi longestStablePrefix）。压缩/分支切换/steering 后会缩短，
                // provider 端 breakpoint 随之前移，避免浪费缓存配额。
                stable_prefix_len: built.stable_prefix_len,
            };
            summary.turns += 1;
            // 轮次硬上限：防止模型陷入无限工具循环（即便每轮都「成功」）。0 表示不限制。
            if max_turns > 0 && summary.turns > max_turns as u64 {
                for h in &hooks {
                    h.on_event(&HookEvent::Stop { success: false }).await;
                }
                yield AgentEvent::Error(format!("达到最大轮次 {max_turns}，停止"));
                yield AgentEvent::StateChanged(AgentState::Idle);
                return;
            }

            // P1-E：轮次开始边界（turn 层生命周期）。在 max_turns 硬上限检查之后、provider
            // 调用之前——未真正进入轮次的提前 return（cancel/deadline/max_turns）不发
            // TurnStart，保证 TurnStart 与 TurnEnd 严格配对。
            yield AgentEvent::TurnStart;

            // P1-D：GenAI chat span（OTel 语义规范）。用 Instrument 覆盖 provider 网络调用：
            // agent stream 须 Send（server tokio::spawn 消费），enter guard 非 Send 跨 await 会
            // 破坏 Send；Instrument 给 Future 加 span 是 Send-safe。流式消费（含 yield）无法被
            // Instrument 覆盖（async_stream! 的 yield 不能入嵌套 block），故 chat span 精确覆盖
            // 握手 + 初始流建立；usage/finish_reason 在消息最终化后 record。
            let chat_span = tracing::info_span!(
                parent: &invoke_span,
                "gen_ai.chat",
                gen_ai.operation.name = "chat",
                gen_ai.request.model = %model.id,
                gen_ai.system = %model.provider,
                gyre.turn = summary.turns,
                gen_ai.usage.input_tokens = tracing::field::Empty,
                gen_ai.usage.output_tokens = tracing::field::Empty,
                gen_ai.response.finish_reason = tracing::field::Empty,
            );
            let mut event_stream = match provider
                .stream(req, &provider_ctx)
                .instrument(chat_span.clone())
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    mistakes += 1;
                    yield AgentEvent::Error(format!("LLM 调用失败: {e}"));
                    if mistakes >= max_mistakes {
                        for h in &hooks {
                            h.on_event(&HookEvent::Stop { success: false }).await;
                        }
                        yield AgentEvent::Error(format!("连续错误达到上限 {max_mistakes}，停止"));
                        yield AgentEvent::StateChanged(AgentState::Idle);
                        return;
                    }
                    continue;
                }
            };
            yield AgentEvent::StateChanged(AgentState::Streaming);
            // P1-E：消息开始边界（message 层生命周期）。流式首个增量前；消息体见后续
            // TextDelta/ThinkingDelta 增量与 MessageEnd。
            yield AgentEvent::MessageStart;

            // 累积流式事件，以 MessageEnd 为权威结束。流式阶段同样响应取消，
            // 避免上游挂起时取消信号无法中断（仅靠 loop 顶部检查不足以打断 await）。
            let mut authoritative: Option<AssistantMessage> = None;
            let mut usage = Usage::default();
            // 累积流式文本增量：流被取消/异常断开（未到达 MessageEnd）时，已生成并
            // 显示给用户的文本若不落盘会丢失对话历史。中断时兜底持久化（仅保留 Text——
            // Thinking 的 signature 在流式中不可靠，ToolCall 参数可能残缺，二者丢弃）。
            let mut acc_text = String::new();
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        // 中断兜底：持久化已生成的部分回复，避免丢失对话历史。
                        persist_interrupted(&context, &mut acc_text, &model, &usage).await;
                        yield AgentEvent::Error("任务被取消".into());
                        yield AgentEvent::StateChanged(AgentState::Idle);
                        return;
                    }
                    ev = event_stream.next() => match ev {
                        Some(AssistantEvent::TextDelta(d)) => {
                            acc_text.push_str(&d);
                            yield AgentEvent::TextDelta(d);
                        }
                        Some(AssistantEvent::ThinkingDelta(d)) => {
                            yield AgentEvent::ThinkingDelta(d);
                        }
                        Some(AssistantEvent::Usage(u)) => {
                            usage.add(&u);
                            yield AgentEvent::Usage(u);
                        }
                        Some(AssistantEvent::MessageEnd(msg)) => {
                            authoritative = Some(msg);
                            break;
                        }
                        Some(_) => {}
                        None => break,
                    },
                }
            }

            let Some(assistant) = authoritative else {
                // 未见 MessageEnd：流异常中断。兜底持久化已生成的部分文本，避免丢失。
                persist_interrupted(&context, &mut acc_text, &model, &usage).await;
                mistakes += 1;
                yield AgentEvent::Error("未收到完整助手消息".into());
                if mistakes >= max_mistakes {
                    yield AgentEvent::StateChanged(AgentState::Idle);
                    return;
                }
                continue;
            };
            mistakes = 0;
            summary.usage.add(&assistant.usage);
            // P1-D：record 本轮 usage + finish_reason 到 chat span（OTel GenAI 约定）。
            chat_span.record("gen_ai.usage.input_tokens", assistant.usage.input_tokens);
            chat_span.record("gen_ai.usage.output_tokens", assistant.usage.output_tokens);
            chat_span.record(
                "gen_ai.response.finish_reason",
                format!("{:?}", assistant.stop_reason),
            );
            // P1-M：transform assistant（最终化后、入 context/MessageEnd/工具分发前）。
            // 移植 oh-my-pi `transformAssistantMessage`：host 闭包原地改写 text / tool_call
            // args（宏展开、脱敏、归一化）。单一真相源——下游 context/UI/tools 都看改写后。
            let mut assistant = assistant;
            if let Some(tf) = &transform_assistant {
                tf(&mut assistant);
            }
            // P0-C：Harmony 协议泄漏缓解（GPT-5/Codex）。检测最终化 assistant 消息，命中则
            // 按表面分流：tool_arg 可恢复 → 截断续跑（truncate-resume 计数器，替换为清理后的
            // tool call）；text/thinking → 丢弃重试（abort-retry 计数器，不 append 泄漏内容、
            // 不 yield MessageEnd，直接 continue 重采样）。双计数器各自上限 MAX_HARMONY_*，
            // 超限升级为错误停止。移植 oh-my-pi harmony-leak 双计数器。
            if harmony::is_harmony_leak_target(&model) {
                if let Some(detection) = harmony::detect_in_message(&assistant) {
                    if let Some(recovered) = harmony::recover_tool_call(&assistant, &detection) {
                        if harmony_truncate_resume >= MAX_HARMONY_TRUNCATE_RESUME {
                            let ev = harmony::create_audit_event(
                                harmony::HarmonyAuditAction::Escalated,
                                &detection,
                                &model,
                                harmony_truncate_resume,
                                &recovered.removed,
                            );
                            harmony::log_audit("Harmony 截断恢复超限，停止", &ev);
                            yield AgentEvent::Error(format!(
                                "GPT-5 Harmony 泄漏截断恢复超限（{}）",
                                ev.signal
                            ));
                            yield AgentEvent::StateChanged(AgentState::Idle);
                            return;
                        }
                        harmony_truncate_resume += 1;
                        let ev = harmony::create_audit_event(
                            harmony::HarmonyAuditAction::TruncateResume,
                            &detection,
                            &model,
                            harmony_truncate_resume,
                            &recovered.removed,
                        );
                        harmony::log_audit("Harmony 泄漏，截断恢复续跑", &ev);
                        assistant = recovered.message;
                    } else {
                        if harmony_retry >= MAX_HARMONY_ABORT_RETRY {
                            let removed = harmony::extract_removed(&assistant, &detection);
                            let ev = harmony::create_audit_event(
                                harmony::HarmonyAuditAction::Escalated,
                                &detection,
                                &model,
                                harmony_retry,
                                &removed,
                            );
                            harmony::log_audit("Harmony 重试超限，停止", &ev);
                            yield AgentEvent::Error(format!(
                                "GPT-5 Harmony 泄漏重试超限（{}）",
                                ev.signal
                            ));
                            yield AgentEvent::StateChanged(AgentState::Idle);
                            return;
                        }
                        harmony_retry += 1;
                        let removed = harmony::extract_removed(&assistant, &detection);
                        let ev = harmony::create_audit_event(
                            harmony::HarmonyAuditAction::AbortRetry,
                            &detection,
                            &model,
                            harmony_retry,
                            &removed,
                        );
                        harmony::log_audit("Harmony 泄漏，丢弃本轮重试", &ev);
                        yield AgentEvent::Say(StatusMessage {
                            text: format!(
                                "检测到 GPT-5 Harmony 协议泄漏（{}），丢弃本轮回复重试",
                                ev.signal
                            ),
                            kind: StatusKind::Warning,
                        });
                        // 不 append 泄漏内容、不 yield MessageEnd，直接重采样（受 max_turns 保护）。
                        continue;
                    }
                }
            }
            // P1-E：消息结束边界（message 层生命周期）。携带完整 assistant 消息快照，
            // 消费者无需自行拼接增量即可获得最终消息。同时开启本轮工具结果累积器
            // （供 TurnEnd 携带，含实际执行与占位 skipped）。
            yield AgentEvent::MessageEnd(assistant.clone());
            let mut turn_tool_results: Vec<agent_core::ToolResultMessage> = Vec::new();

            let tool_calls: Vec<(String, String, serde_json::Value)> = assistant
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall { id, name, arguments } => {
                        Some((id.clone(), name.clone(), arguments.clone()))
                    }
                    _ => None,
                })
                .collect();

            context
                .append(agent_core::AgentMessage::Assistant(assistant.clone()))
                .await;

            // 截断自愈 & 流中断自愈：
            // - finish_reason=length：输出被 max_tokens 截断
            // - stop_reason=None：SSE 流在收到 [DONE] 前异常结束（网络中断/服务端断连），
            //   此时 assistant 可能仅有残缺的 thinking 而无 text/tools —— 直接判定「任务完成」
            //   会把一份中断的思考输出误认为成功终止。
            // 两种情况下若均无 tool_calls，注入「续写」指令并进入下一轮，让模型从中断处补全；
            // 受顶部 max_turns 硬上限保护，不会无限循环。
            let truncated =
                assistant.stop_reason == Some(StopReason::Length) || assistant.stop_reason.is_none();
            if truncated && tool_calls.is_empty() {
                let reason = if assistant.stop_reason == Some(StopReason::Length) {
                    "输出达到 token 上限被截断"
                } else {
                    "响应流异常中断（未收到终止标记）"
                };
                yield AgentEvent::Say(StatusMessage {
                    text: format!("{reason}，正在自动续写…"),
                    kind: StatusKind::Warning,
                });
                context
                    .append(agent_core::AgentMessage::user_text(
                        "（你的上一条回复因输出达到长度上限被截断或流异常中断，请直接从中断处继续输出剩余内容，不要重复已生成的部分。）",
                    ))
                    .await;
                // P1-E：截断续写（无工具），turn 结束 → 继续。
                yield AgentEvent::TurnEnd {
                    message: assistant.clone(),
                    tool_results: std::mem::take(&mut turn_tool_results),
                    will_continue: true,
                };
                continue;
            } else if truncated && !tool_calls.is_empty() {
                // P0-B：输出被截断但已含 ToolCall —— 参数可能残缺（JSON 被截断），**不执行**：
                // 为每个 tool_call 回填占位 result 维持 tool_use/tool_result 配对（严格校验的
                // provider 如 GLM/Z.ai 缺结果会 400），注入续写指令让模型补全（受 max_turns
                // 保护，不会无限循环）。移植 oh-my-pi `runLoopBody` 的 length skip 分支。
                let reason = if assistant.stop_reason == Some(StopReason::Length) {
                    "输出达到 token 上限被截断"
                } else {
                    "响应流异常中断（未收到终止标记）"
                };
                yield AgentEvent::Say(StatusMessage {
                    text: format!("{reason}，工具调用未执行（参数可能不完整），正在续写…"),
                    kind: StatusKind::Warning,
                });
                for (id, _name, _args) in &tool_calls {
                    let msg = ToolResultMessage {
                        tool_call_id: id.clone(),
                        result: ToolResult::Error {
                            recoverable: true,
                            message: "输出被截断，工具参数可能不完整，未执行；请重新发起完整的工具调用".into(),
                        },
                    };
                    turn_tool_results.push(msg.clone());
                    context
                        .append(agent_core::AgentMessage::ToolResult(msg))
                        .await;
                }
                context
                    .append(agent_core::AgentMessage::user_text(
                        "（你的上一条回复因输出达到长度上限被截断或流异常中断，含未完成的工具调用，均已回滚未执行。请直接从中断处继续输出剩余内容，不要重复已生成的部分，也不要重复未完成的工具调用。）",
                    ))
                    .await;
                // P1-E：截断续写（含未完成工具占位），turn 结束 → 继续。
                yield AgentEvent::TurnEnd {
                    message: assistant.clone(),
                    tool_results: std::mem::take(&mut turn_tool_results),
                    will_continue: true,
                };
                continue;
            } else if matches!(assistant.stop_reason, Some(StopReason::Error) | Some(StopReason::Aborted))
                && !tool_calls.is_empty()
            {
                // P0-B：Error/Aborted 且含 tool_calls —— 终止错误（API refusal / 中止），**不执行**
                // 工具：为每个回填占位 result 维持 tool_use/tool_result 配对（严格校验的 provider
                // 缺结果会 400），以 success=false 立即停止（不续写、不无限循环）。移植 oh-my-pi
                // `runLoopBody` 的 error/aborted 占位分支。
                yield AgentEvent::Say(StatusMessage {
                    text: format!(
                        "助手消息以 {:?} 结束且含工具调用，工具未执行（占位补全配对），停止",
                        assistant.stop_reason
                    ),
                    kind: StatusKind::Warning,
                });
                for (id, _name, _args) in &tool_calls {
                    let msg = ToolResultMessage {
                        tool_call_id: id.clone(),
                        result: ToolResult::Error {
                            recoverable: false,
                            message: "助手消息以错误中止，工具调用未执行".into(),
                        },
                    };
                    turn_tool_results.push(msg.clone());
                    context
                        .append(agent_core::AgentMessage::ToolResult(msg))
                        .await;
                }
                // P1-E：error/aborted 终止，turn 结束 → 停止（will_continue: false）。
                fire_on_turn_end(&hooks, &assistant, &turn_tool_results, false).await;
                yield AgentEvent::TurnEnd {
                    message: assistant.clone(),
                    tool_results: std::mem::take(&mut turn_tool_results),
                    will_continue: false,
                };
                yield AgentEvent::StateChanged(AgentState::Idle);
                summary.success = false;
                for h in &hooks {
                    h.on_event(&HookEvent::Stop { success: false }).await;
                }
                record_run_end(&invoke_span, &summary, run_start);
                yield AgentEvent::Done(summary);
                return;
            }

            // pause_turn：provider 结束响应但未终止轮次（非终止停顿，如分段输出/进度更新）。
            // 在上限内重新采样让模型继续；超过上限则按正常停止收尾，避免无限自旋
            // （移植 oh-my-pi MAX_PAUSED_TURN_CONTINUATIONS）。
            if assistant.stop_reason == Some(StopReason::Pause) && tool_calls.is_empty() {
                if paused_continuations < MAX_PAUSED_CONTINUATIONS {
                    paused_continuations += 1;
                    yield AgentEvent::Say(StatusMessage {
                        text: "模型未结束本轮（pause），继续采样…".into(),
                        kind: StatusKind::Info,
                    });
                    // P1-E：pause 续写，turn 结束 → 继续。
                    yield AgentEvent::TurnEnd {
                        message: assistant.clone(),
                        tool_results: std::mem::take(&mut turn_tool_results),
                        will_continue: true,
                    };
                    continue;
                }
                yield AgentEvent::Say(StatusMessage {
                    text: "暂停续写次数达上限，按完成停止".into(),
                    kind: StatusKind::Warning,
                });
                // 落到下方「任务完成」收尾。
            }

            // 有工具调用 → 真实工作轮，重置暂停计数。
            if !tool_calls.is_empty() {
                paused_continuations = 0;
            }

            // 无工具调用 → 任务完成（停止边界）。
            //
            // P1-G：真正收尾前重新探测一次 steering——移植 oh-my-pi `runLoopBody` 外层「停-续」
            // 语义：agent 本该停止时若发现停止边界期间到达的新消息（用户在最后一轮模型调用 /
            // 工具执行期间发的消息），则注入并续跑，而非结束——避免消息被搁置到下次手动 prompt。
            // cancel / deadline 时不 drain（abort 不消费队列，防「消息落地历史、agent 永不响应」
            // 的搁浅 hazard，见 oh-my-pi agent-loop.ts 第 1045-1050 行注释）。
            if tool_calls.is_empty() {
                let deadline_exceeded =
                    deadline_at.is_some_and(|d| std::time::Instant::now() >= d);
                if !cancel.is_cancelled() && !deadline_exceeded {
                    let mut pending_steering = false;
                    {
                        let mut guard_steer = steer_rx.lock().await;
                        if let Some(rx) = guard_steer.as_mut() {
                            if let Ok(msg) = rx.try_recv() {
                                context.append(msg).await;
                                pending_steering = true;
                                while let Ok(more) = rx.try_recv() {
                                    context.append(more).await;
                                }
                            }
                        }
                    }
                    if pending_steering {
                        yield AgentEvent::Say(StatusMessage {
                            text: "已注入停止边界 steering 消息，继续…".into(),
                            kind: StatusKind::Info,
                        });
                        // P1-E：停止边界 steering 续跑，turn 结束 → 继续。
                        yield AgentEvent::TurnEnd {
                            message: assistant.clone(),
                            tool_results: std::mem::take(&mut turn_tool_results),
                            will_continue: true,
                        };
                        continue;
                    }
                    // aside：停止边界若无 steering，再探测被动通知。aside 也触发续跑
                    //（让模型响应后台完成 / 延迟 diagnostics），但优先级低于 steering；
                    // 同样不打断在途工具（此处已无在途工具——tool_calls 为空才到停止边界）。
                    let mut pending_aside = false;
                    {
                        let mut guard_aside = aside_rx.lock().await;
                        if let Some(rx) = guard_aside.as_mut() {
                            if let Ok(msg) = rx.try_recv() {
                                context.append(msg).await;
                                pending_aside = true;
                                while let Ok(more) = rx.try_recv() {
                                    context.append(more).await;
                                }
                            }
                        }
                    }
                    if pending_aside {
                        yield AgentEvent::Say(StatusMessage {
                            text: "已注入停止边界 aside 消息，继续…".into(),
                            kind: StatusKind::Info,
                        });
                        yield AgentEvent::TurnEnd {
                            message: assistant.clone(),
                            tool_results: std::mem::take(&mut turn_tool_results),
                            will_continue: true,
                        };
                        continue;
                    }
                }

                // P1-E：正常停止，turn 结束 → 停止（will_continue: false）。本轮无工具调用，
                // turn_tool_results 为空。
                fire_on_turn_end(&hooks, &assistant, &turn_tool_results, false).await;
                yield AgentEvent::TurnEnd {
                    message: assistant.clone(),
                    tool_results: std::mem::take(&mut turn_tool_results),
                    will_continue: false,
                };
                yield AgentEvent::StateChanged(AgentState::Idle);
                summary.success = true;

                // 捕获隔离变更（若启用）
                if workspace.is_isolated() {
                    if let Some(Ok(diff)) = workspace.diff().await {
                        summary.iso_diff = Some(diff.unified_text());
                    }
                    let _ = workspace.close_isolation();
                }

                for h in &hooks {
                    h.on_event(&HookEvent::Stop { success: true }).await;
                }
                record_run_end(&invoke_span, &summary, run_start);
                yield AgentEvent::Done(summary);
                return;
            }

            // P1-C：软需求 pending 时的非合规处理。合规 = 调用了工具且全部都是所需工具
            //（移植 oh-my-pi `calledOnlyRequiredTool`）。非合规（含 detour 或空）→ detour 不执行、
            // 配 skipped 占位、强制下轮；连续 MAX_SOFT_TOOL_ESCALATIONS 次仍非合规则 abort。
            if let Some(req) = &soft_tool_name_this_turn {
                let compliant = !tool_calls.is_empty()
                    && tool_calls.iter().all(|(_, n, _)| n == req);
                if !compliant {
                    soft_escalations = soft_escalations.saturating_add(1);
                    if soft_escalations > MAX_SOFT_TOOL_ESCALATIONS {
                        yield AgentEvent::Say(StatusMessage {
                            text: format!(
                                "软工具需求 '{req}' 经 {MAX_SOFT_TOOL_ESCALATIONS} 次强制仍未满足，中止"
                            ),
                            kind: StatusKind::Warning,
                        });
                        for (id, name, _args) in &tool_calls {
                            let msg = ToolResultMessage {
                                tool_call_id: id.clone(),
                                result: ToolResult::Error {
                                    recoverable: true,
                                    message: format!(
                                        "软需求 '{req}' 未满足，{name} 未执行（中止）"
                                    ),
                                },
                            };
                            turn_tool_results.push(msg.clone());
                            context
                                .append(agent_core::AgentMessage::ToolResult(msg))
                                .await;
                        }
                        // P1-E：软需求达上限中止，turn 结束 → 停止。
                        yield AgentEvent::TurnEnd {
                            message: assistant.clone(),
                            tool_results: std::mem::take(&mut turn_tool_results),
                            will_continue: false,
                        };
                        yield AgentEvent::StateChanged(AgentState::Idle);
                        summary.success = false;
                        for h in &hooks {
                            h.on_event(&HookEvent::Stop { success: false }).await;
                        }
                        record_run_end(&invoke_span, &summary, run_start);
                        yield AgentEvent::Done(summary);
                        return;
                    }
                    yield AgentEvent::Say(StatusMessage {
                        text: format!(
                            "软需求 '{req}' 待满足，跳过本轮工具调用（detour 未执行），下轮强制（{soft_escalations}/{MAX_SOFT_TOOL_ESCALATIONS}）"
                        ),
                        kind: StatusKind::Info,
                    });
                    for (id, name, _args) in &tool_calls {
                        let msg = ToolResultMessage {
                            tool_call_id: id.clone(),
                            result: ToolResult::Error {
                                recoverable: true,
                                message: format!(
                                    "请先调用所需工具 '{req}'（detour '{name}' 未执行）"
                                ),
                            },
                        };
                        turn_tool_results.push(msg.clone());
                        context
                            .append(agent_core::AgentMessage::ToolResult(msg))
                            .await;
                    }
                    escalate_soft = true;
                    // P1-E：软需求非合规跳过，turn 结束 → 继续。
                    yield AgentEvent::TurnEnd {
                        message: assistant.clone(),
                        tool_results: std::mem::take(&mut turn_tool_results),
                        will_continue: true,
                    };
                    continue;
                }
            }

            // 工具执行：审批串行（Ask 一次一个，不能并发）→ 执行按 shared/exclusive 调度
            // （Shared 工具并发；Exclusive 作屏障串行，避免写/执行类工具相互或与读竞态）。
            let workspace_ref = &workspace;
            let approval_ref = &approval;
            // 软工具需求：本轮是否调用了所需工具（未调用则下一轮升级为强制）。
            let soft_called = soft_tool_name_this_turn
                .as_deref()
                .map_or(true, |req| tool_calls.iter().any(|(_, n, _)| n == req));

            // ── 阶段一：审批门禁（串行；Ask 阻塞等待用户，故必须逐个处理）。──
            let mut runnable: Vec<PendingTask> = Vec::new();
            for (order, (id, name, args)) in tool_calls.into_iter().enumerate() {
                summary.tool_calls += 1;
                let Some(tool) = tools.get(&name) else {
                    // 未知工具：作为可恢复错误回填到上下文，让模型在下一轮自我纠正。
                    // 不计入 `mistakes`（终止计数器），否则模型在同一轮调用 N 个未知工具
                    // 会立即触发 max_mistakes 终止，丧失根据反馈重试的机会。
                    let msg = format!("未知工具: {name}");
                    context
                        .append(agent_core::AgentMessage::ToolResult(ToolResultMessage {
                            tool_call_id: id,
                            result: ToolResult::Error { recoverable: true, message: msg.clone() },
                        }))
                        .await;
                    yield AgentEvent::Error(msg);
                    continue;
                };

                // 审批门禁
                let areq = tool.describe(&args);
                match approval.decide(&areq) {
                    ApprovalDecision::Deny(reason) => {
                        context
                            .append(agent_core::AgentMessage::ToolResult(ToolResultMessage {
                                tool_call_id: id,
                                result: ToolResult::Error {
                                    recoverable: true,
                                    message: format!("被拒绝: {reason}"),
                                },
                            }))
                            .await;
                        yield AgentEvent::Say(StatusMessage {
                            text: format!("已拒绝 {name}: {reason}"),
                            kind: StatusKind::Warning,
                        });
                        continue;
                    }
                    ApprovalDecision::Ask => {
                        let ask = AskMessage {
                            id: id.clone(),
                            kind: AskKind::Tool { tool: name.clone() },
                            prompt: approval_prompt(&name, &args),
                        };
                        yield AgentEvent::StateChanged(AgentState::WaitingForInput);
                        yield AgentEvent::Ask(ask.clone());
                        let allowed = matches!(approval_ref.prompt(&ask).await, Ok(AskResponse::Yes | AskResponse::Text(_)));
                        yield AgentEvent::StateChanged(AgentState::Running);
                        if !allowed {
                            context
                                .append(agent_core::AgentMessage::ToolResult(ToolResultMessage {
                                    tool_call_id: id,
                                    result: ToolResult::Error {
                                        recoverable: true,
                                        message: "用户拒绝".into(),
                                    },
                                }))
                                .await;
                            continue;
                        }
                    }
                    ApprovalDecision::Allow => {}
                }

                // P1-E：工具执行开始（已通过审批，即将执行）。审批拒绝 / Ask 拒绝 / 未知工具
                // 等未执行路径不发 ToolExecution 事件（它们有 Say/Error 信号）；消费者见
                // ToolExecutionStart→ToolExecutionEnd 即表示工具真实执行。
                yield AgentEvent::ToolExecutionStart {
                    tool_call_id: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                };
                runnable.push(PendingTask {
                    order,
                    id,
                    name: name.clone(),
                    args,
                    exclusive: matches!(tool.concurrency(), Concurrency::Exclusive),
                });
            }

            // ── 阶段二：执行（Shared 并发 / Exclusive 作屏障串行）。──
            // P1-I：批级 cancel token 是 run-cancel 的 child——run 级取消向下传播；
            // Immediate 模式下若 batch 含 interruptible 工具，steering 命中会单独触发它，
            // 中断在途工具而不影响 run 级取消语义（steering 随后在下轮顶部/停止边界 drain）。
            // 批级 token 始终创建（廉价）：Wait 模式或无非 interruptible 工具时不触发，行为不变。
            let batch_token = cancel.child_token();
            let need_steering_poll = matches!(agent.interrupt_mode, InterruptMode::Immediate)
                && runnable
                    .iter()
                    .any(|t| matches!(tools.get(&t.name), Some(tool) if tool.interruptible()));
            // P1-F：工具流式 partial 回调 channel。工具 execute 内经 tcx.update_tx 推送
            // ToolUpdate；下方 select! 边执行边 drain，发射 ToolExecutionUpdate 事件。
            // 移植 oh-my-pi `AgentToolUpdateCallback` 的 partialResult。
            let (update_tx, mut update_rx) =
                tokio::sync::mpsc::unbounded_channel::<agent_tools::ToolUpdate>();
            let tcx = ToolContext {
                workspace: workspace_ref,
                approval: approval_ref.as_ref(),
                cancel: &batch_token,
                skills: catalog
                    .as_ref()
                    .map(|c| c.as_ref() as &dyn agent_core::SkillResolver),
                memory: memory
                    .as_ref()
                    .map(|m| m.as_ref() as &dyn agent_core::MemoryStore),
                resources: resources
                    .as_ref()
                    .map(|r| r.as_ref() as &dyn agent_core::ResourceResolver),
                write_effect: write_effect.as_ref().map(std::sync::Arc::as_ref),
                update_tx: Some(&update_tx),
            };
            // 调度执行（Shared 并发 / Exclusive 屏障串行），结果按原始顺序返回。
            // Immediate 模式 + batch 含 interruptible 工具时，边执行边轮询 steering 队列，
            // 命中即 batch_token.cancel() 中断在途工具（移植 oh-my-pi `watchSteeringWhileRunning`）。
            // 用 async 块统一两分支的 future 类型，便于下方 select! 边等边 drain partial。
            // P1-D：GenAI execute_tool span——覆盖整个工具批次执行（含 Immediate 模式的
            // steering 轮询）。包在 async 块上 instrument（Send-safe，避免 enter guard 跨 await
            // 破坏 stream 的 Send；server tokio::spawn 消费要求 Send）。
            let tool_count = runnable.len();
            let tool_span = tracing::info_span!(
                parent: &invoke_span,
                "gen_ai.execute_tool",
                gen_ai.operation.name = "execute_tool",
                gen_ai.tool.count = tool_count,
                gyre.turn = summary.turns,
            );
            let run_fut = async {
                if need_steering_poll {
                    poll_and_run(
                        schedule_and_run(runnable, &tools, &tcx, &hooks),
                        &batch_token,
                        steer_rx,
                    )
                    .await
                } else {
                    schedule_and_run(runnable, &tools, &tcx, &hooks).await
                }
            }
            .instrument(tool_span);
            tokio::pin!(run_fut);
            // P1-F：边执行边 drain 工具流式 partial。biased 先轮询 run_fut（完成即 break），
            // 否则收 update → 发射 ToolExecutionUpdate；循环至 batch 完成。
            let completed = loop {
                tokio::select! {
                    biased;
                    out = &mut run_fut => break out,
                    update = update_rx.recv() => {
                        if let Some(u) = update {
                            yield AgentEvent::ToolExecutionUpdate {
                                tool_call_id: u.tool_call_id,
                                name: u.name,
                                partial: u.partial,
                            };
                        }
                    }
                }
            };
            // 兜底 drain：batch 完成与最后一次 update 入队存在竞态，break 后可能仍有 buffered。
            while let Ok(u) = update_rx.try_recv() {
                yield AgentEvent::ToolExecutionUpdate {
                    tool_call_id: u.tool_call_id,
                    name: u.name,
                    partial: u.partial,
                };
            }
            // 按原始调用顺序回填结果 + 发射事件（确定性顺序，便于观测与重放）。
            for (_order, id, name, result, mistake_inc) in completed {
                if mistake_inc {
                    mistakes += 1;
                }
                // P2-K：按工具名记录执行结果（ok/error 计数 + invoked 集合）。
                summary.record_tool(&name, &result);
                // P1-E：工具执行结束（tool 层生命周期）。先判定 is_error 并构造持久化消息，
                // 再发射事件 + 回填上下文（消息 move 进 context；事件与 turn 累积器持 clone）。
                let is_error = matches!(&result, ToolResult::Error { .. });
                let msg = ToolResultMessage {
                    tool_call_id: id.clone(),
                    result: result.clone(),
                };
                turn_tool_results.push(msg.clone());
                yield AgentEvent::ToolExecutionEnd {
                    tool_call_id: id.clone(),
                    name: name.clone(),
                    result: result.clone(),
                    is_error,
                };
                let preview = result.to_llm_text();
                yield AgentEvent::ToolExec {
                    name,
                    output: preview.chars().take(200).collect(),
                };
                context
                    .append(agent_core::AgentMessage::ToolResult(msg))
                    .await;
            }

            // 软工具需求：本轮未调用所需工具 → 下一轮升级为强制（修复 escalate 死代码）。
            escalate_soft = !soft_called && soft_tool_name_this_turn.is_some();
            // P1-C：合规（调用了所需工具）后重置升级计数，避免累计误中止。
            if soft_called {
                soft_escalations = 0;
            }

            if mistakes >= max_mistakes {
                // P1-E：连续错误达上限，turn 结束 → 停止。
                yield AgentEvent::TurnEnd {
                    message: assistant.clone(),
                    tool_results: std::mem::take(&mut turn_tool_results),
                    will_continue: false,
                };
                yield AgentEvent::Error(format!("连续错误达到上限 {max_mistakes}，停止"));
                yield AgentEvent::StateChanged(AgentState::Idle);
                return;
            }
            // P1-E：工具执行完毕，turn 结束 → 继续（工具结果已回填，模型再次推理）。
            fire_on_turn_end(&hooks, &assistant, &turn_tool_results, true).await;
            yield AgentEvent::TurnEnd {
                message: assistant.clone(),
                tool_results: std::mem::take(&mut turn_tool_results),
                will_continue: true,
            };
            // 继续下一轮（工具结果已回填，模型再次推理）
        }
    }
}

// ============================================================================
// 工具并发执行支持
// ============================================================================

/// 已通过审批、待执行的工具任务。
struct PendingTask {
    /// 原始调用顺序（结果回填按此排序，保证确定性）。
    order: usize,
    id: String,
    name: String,
    args: serde_json::Value,
    /// 是否 Exclusive（屏障串行）。
    exclusive: bool,
}

/// 执行单个任务：before 钩子 → 执行 → after 钩子。
///
/// 返回 `(结果, 是否计入 mistakes)`。仅「不可恢复」错误计入 mistakes；可恢复错误
/// （如某文件不存在）回填让模型自我纠正，避免多工具轮次中提前触顶。
async fn run_pending_task(
    task: &PendingTask,
    tools: &Arc<dyn ToolRegistry>,
    tcx: &ToolContext<'_>,
    hooks: &[Arc<dyn Hook>],
) -> (ToolResult, bool) {
    use futures::FutureExt;
    for h in hooks {
        h.on_event(&HookEvent::BeforeTool {
            tool: task.name.clone(),
            args: task.args.clone(),
        })
        .await;
    }
    let Some(tool) = tools.get(&task.name) else {
        return (
            ToolResult::Error {
                recoverable: true,
                message: format!("未知工具: {}", task.name),
            },
            false,
        );
    };
    // P2-I：before 拦截——任一钩子返回 Some(reason) 即阻止执行：回填可恢复错误（不调用 execute），
    // 仍发 AfterTool 观察事件（观察到的是拦截结果）。区别于交互式审批 ApprovalPolicy——钩子是
    // 程序化门禁，扩展 / MCP 可按模式自动拦截危险工具。
    for h in hooks {
        if let Some(reason) = h.before_tool_intercept(&task.name, &task.args).await {
            let result = ToolResult::Error {
                recoverable: true,
                message: format!("被钩子拦截: {reason}"),
            };
            for hook in hooks {
                hook.on_event(&HookEvent::AfterTool {
                    tool: task.name.clone(),
                    result: result.clone(),
                })
                .await;
            }
            return (result, false);
        }
    }
    // P1-D：catch_unwind 防 `tool.execute` panic（第三方工具/MCP 的 unwrap None、越界等失控）
    // 传播终止整个 agent run。panic 归一化为不可恢复 Error result（不污染会话文件、不悬空）。
    let outcome = std::panic::AssertUnwindSafe(tool.execute(task.args.clone(), tcx))
        .catch_unwind()
        .await;
    let (mut result, mistake_inc) = match outcome {
        Ok(Ok(r)) => (r, false),
        Ok(Err(e)) => {
            let recoverable = e.is_recoverable();
            (
                ToolResult::Error {
                    recoverable,
                    message: e.to_string(),
                },
                !recoverable,
            )
        }
        Err(panic_payload) => {
            // panic payload 通常是 &'static str 或 String；尽力提取消息，否则占位。
            let msg = panic_payload
                .downcast_ref::<&'static str>()
                .copied()
                .map(str::to_string)
                .or_else(|| panic_payload.downcast_ref::<String>().map(String::clone))
                .unwrap_or_else(|| "<非字符串 panic payload>".to_string());
            (
                ToolResult::Error {
                    recoverable: false,
                    message: format!("工具执行 panic: {msg}"),
                },
                true,
            )
        }
    };
    // P2-I：after 改写——钩子可替换 result（脱敏 / 归一化 / 附加纠正提示），在 AfterTool 观察前
    // 应用，故观察事件与回填给模型的都是最终结果。
    for h in hooks {
        if let Some(new_result) = h.after_tool_override(&task.name, &result).await {
            result = new_result;
        }
    }
    for h in hooks {
        h.on_event(&HookEvent::AfterTool {
            tool: task.name.clone(),
            result: result.clone(),
        })
        .await;
    }
    (result, mistake_inc)
}

/// 调度执行一批已审批任务：Shared 工具在相邻 Exclusive 之间并发；Exclusive 作屏障串行
/// （先排空前一批 Shared，再单独执行自身）。返回结果按原始调用顺序排序，便于确定性回填。
async fn schedule_and_run(
    runnable: Vec<PendingTask>,
    tools: &Arc<dyn ToolRegistry>,
    tcx: &ToolContext<'_>,
    hooks: &[Arc<dyn Hook>],
) -> Vec<(usize, String, String, ToolResult, bool)> {
    let mut shared: Vec<PendingTask> = Vec::new();
    let mut completed: Vec<(usize, String, String, ToolResult, bool)> = Vec::new();
    for t in runnable {
        if t.exclusive {
            // 屏障：先排空当前 Shared 批次（并发），再单独串行执行该 Exclusive 工具，
            // 确保写/执行类工具不与其它工具交叠。
            if !shared.is_empty() {
                let mut r = run_batch(std::mem::take(&mut shared), tools, tcx, hooks).await;
                completed.append(&mut r);
            }
            let (result, mistake_inc) = run_pending_task(&t, tools, tcx, hooks).await;
            completed.push((t.order, t.id.clone(), t.name.clone(), result, mistake_inc));
        } else {
            shared.push(t);
        }
    }
    if !shared.is_empty() {
        let mut r = run_batch(std::mem::take(&mut shared), tools, tcx, hooks).await;
        completed.append(&mut r);
    }
    completed.sort_unstable_by_key(|(order, _, _, _, _)| *order);
    completed
}

/// 并发执行一批 Shared 任务（`join_all`，同一任务上交错推进 I/O）。
///
/// 返回 `(order, id, name, result, mistake_inc)` 列表；调用方按 `order` 排序回填。
async fn run_batch(
    batch: Vec<PendingTask>,
    tools: &Arc<dyn ToolRegistry>,
    tcx: &ToolContext<'_>,
    hooks: &[Arc<dyn Hook>],
) -> Vec<(usize, String, String, ToolResult, bool)> {
    let futs = batch.into_iter().map(|t| async move {
        let (result, mistake_inc) = run_pending_task(&t, tools, tcx, hooks).await;
        (t.order, t.id.clone(), t.name.clone(), result, mistake_inc)
    });
    futures::future::join_all(futs).await
}

/// Immediate 模式下轮询 steering 队列的间隔（移植 oh-my-pi `STEERING_INTERRUPT_POLL_MS`）。
///
/// 一次同步的队列长度检查，延迟上界为一个轮询周期。
const STEERING_INTERRUPT_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// 边执行工具批次边轮询 steering 队列：每 [`STEERING_INTERRUPT_POLL`] 用
/// [`tokio::sync::mpsc::UnboundedReceiver::len`]（**非消费 peek**）检查一次，命中即
/// `batch_token.cancel()` 中断在途工具；工具批次完成后立即返回（停止轮询）。
///
/// 移植 oh-my-pi `watchSteeringWhileRunning`：仅当 Immediate 模式且 batch 含
/// [`Tool::interruptible`] 工具时由调用方启用。`batch_token` 是 run-cancel 的 child，故中断
/// 只影响本轮在途工具，不传播到 run 级取消（steering 随后在下一轮顶部 / 停止边界被 drain）。
async fn poll_and_run<Fut>(
    run: Fut,
    batch_token: &tokio_util::sync::CancellationToken,
    steer_rx: &tokio::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedReceiver<agent_core::AgentMessage>>,
    >,
) -> Fut::Output
where
    Fut: std::future::Future,
{
    tokio::pin!(run);
    loop {
        tokio::select! {
            // 工具批次完成（正常或被中断后）→ 返回结果。
            out = &mut run => return out,
            // 周期性非消费探测 steering 队列。
            () = tokio::time::sleep(STEERING_INTERRUPT_POLL) => {
                let pending = steer_rx
                    .lock()
                    .await
                    .as_ref()
                    .map_or(0, |rx| rx.len());
                if pending > 0 {
                    batch_token.cancel();
                    // run 继续被 select 轮询直到完成（工具应观察 batch_token 尽快退出）。
                }
            }
        }
    }
}

/// P1-D：把 agent run 终态 record 到 invoke_agent span（OTel GenAI 约定）。
///
/// 在每个 `yield AgentEvent::Done(summary)` 前调用，使 invoke_agent span 携带 success/
/// turns/usage/duration。因 async_stream! 的 Send 约束无法用 enter guard 覆盖整段 run，
/// invoke_agent 作为「属性载体 span」，chat/execute_tool 子 span 经 `parent` 链关联。
fn record_run_end(span: &tracing::Span, summary: &AgentRunSummary, start: std::time::Instant) {
    span.record("gyre.success", &summary.success);
    span.record("gyre.turns", &summary.turns);
    span.record("gen_ai.usage.input_tokens", &summary.usage.input_tokens);
    span.record("gen_ai.usage.output_tokens", &summary.usage.output_tokens);
    span.record("gyre.duration", &tracing::field::debug(start.elapsed()));
}

/// 流式中断兜底持久化：把已累积的文本增量作为一条被中断的 assistant 消息落盘。
///
/// 仅在流式被取消或异常断开（未到达 [`AssistantEvent::MessageEnd`]）时调用，避免
/// 已显示给用户的回复因未落盘而在 resume 会话时丢失。仅保留 `Text` 块：
/// - `Thinking` 的 signature 在流式中不可靠，持久化后重放可能导致 provider 校验失败；
/// - `ToolCall` 的参数 JSON 可能残缺，会产生悬空工具调用（无对应 tool 结果）。
/// 故二者丢弃。`stop_reason` 置 `None` 标记此条为中断产物。
async fn persist_interrupted(
    context: &Arc<dyn ContextManager>,
    acc_text: &mut String,
    model: &agent_core::Model,
    usage: &Usage,
) {
    if acc_text.is_empty() {
        return;
    }
    let text = std::mem::take(acc_text);
    tracing::info!(bytes = text.len(), "持久化被中断的部分回复");
    context
        .append(agent_core::AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text }],
            usage: usage.clone(),
            model: model.id.clone(),
            stop_reason: None,
            stop_details: None,
        }))
        .await;
}

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
}
