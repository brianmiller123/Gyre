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
    AgentEvent, AgentRunSummary, AgentState, ApprovalDecision, ApprovalPolicy, AskKind, AskMessage,
    AgentMessage, AskResponse, AssistantEvent, AssistantMessage, CompactionStrategy, CompletionRequest,
    ContextManager, ContentBlock, Hook, HookEvent, LlmProvider, MemoryStore, Mode, ProviderCallContext,
    ResourceResolver, SoftToolRequirement, StatusKind, StatusMessage, StopReason, ThinkingConfig, ToolResult, ToolResultMessage,
    ToolChoice, ToolChoiceDirective, Usage, Workspace, WriteEffect,
};
use agent_prompt::PromptCatalog;
use agent_skills::{render_skills_section, SkillCatalog};
use agent_tools::{ToolContext, ToolRegistry};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

mod task_tool;
pub use task_tool::{ContextFactory, TaskTool};

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
    context_guard: f32,
    max_output_tokens: usize,
    temperature: Option<f32>,
    thinking: Option<ThinkingConfig>,
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
    steer_rx: tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<agent_core::AgentMessage>>>,
    /// 写入效果（编辑后 LSP format/diagnostics 钩子；装配层注入 `LspWriteEffect`）。
    write_effect: Option<Arc<dyn WriteEffect>>,
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
            context_guard: 0.8,
            max_output_tokens: 4096,
            temperature: None,
            thinking: None,
            cancel: CancellationToken::new(),
            catalog: None,
            context_files: Vec::new(),
            hooks: Vec::new(),
            memory: None,
            resources: None,
            soft_requirement: None,
            steer_rx: None,
            write_effect: None,
        }
    }

    /// 取消句柄（外部中止）。
    #[must_use]
    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// 运行一个任务，产出事件流。取消作用域为 Agent 自身的 `cancel`。
    pub fn run(&self, task: &str) -> impl futures::Stream<Item = AgentEvent> + '_ {
        run_loop(self, agent_core::UserMessage::from_text(task), self.cancel.clone())
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
    context_guard: f32,
    max_output_tokens: usize,
    temperature: Option<f32>,
    thinking: Option<ThinkingConfig>,
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
    /// 写入效果（编辑后 LSP format/diagnostics）。
    write_effect: Option<Arc<dyn WriteEffect>>,
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
            context_guard: self.context_guard,
            max_output_tokens: self.max_output_tokens,
            temperature: self.temperature,
            thinking: self.thinking,
            cancel: self.cancel,
            catalog: self.catalog,
            context_files: self.context_files,
            hooks: self.hooks,
            memory: self.memory,
            resources: self.resources,
            write_effect: self.write_effect,
            soft_requirement: Arc::new(std::sync::Mutex::new(self.soft_requirement)),
            steer_rx: tokio::sync::Mutex::new(self.steer_rx),
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
        "str_replace" => format!("Agent 想替换文件 {} 中的内容", s("path")),
        "apply_diff" => format!("Agent 想对文件 {} 应用差异编辑", s("path")),
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
    let guard = agent.context_guard;
    let max_tokens = agent.max_output_tokens;
    let temperature = agent.temperature;
    let thinking = agent.thinking.clone();
    let catalog = agent.catalog.clone();
    let context_files = agent.context_files.clone();
    let hooks = agent.hooks.clone();
    let memory = agent.memory.clone();
    let resources = agent.resources.clone();
    let soft_requirement = Arc::clone(&agent.soft_requirement);
    let steer_rx = &agent.steer_rx;

    async_stream::stream! {
        // 设置稳定前缀（system prompt + tool spec 指纹冻结）。
        // 若注入了 Skill 目录，则追加 `<skills>` 段（仅当 read_file 工具可用时）。
        let specs0 = tools.specs();
        let has_read = specs0.iter().any(|s| s.name == "read_file");
        let mut system = prompts.system_with_platform(mode);
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

        let mut summary = AgentRunSummary::default();
        let mut mistakes: usize = 0;
        // 软需求状态：已注入的 id、是否需升级为强制
        let mut injected_soft_id: String = String::new();
        let mut escalate_soft = false;

        loop {
            // 取消检查
            if cancel.is_cancelled() {
                yield AgentEvent::Error("任务被取消".into());
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

            // 上下文压缩（接近上限）
            if built.tokens.near_limit(guard) {
                // 逐级压缩：shake（去冗余）→ summarize（LLM handoff 摘要）→ prune（窗口兜底）
                let _ = context.compact(CompactionStrategy::Shake).await;
                let _ = context
                    .compact(CompactionStrategy::Summarize { max_tokens: 0 })
                    .await;
                let _ = context.compact(CompactionStrategy::Prune { keep_recent: 8 }).await;
                yield AgentEvent::Say(StatusMessage {
                    text: "上下文接近上限，已触发压缩（shake + summarize + prune）".into(),
                    kind: StatusKind::Warning,
                });
                // 压缩后重新构建上下文，确保本轮请求使用压缩后的快照
                // （修复「压缩滞后一轮」——否则本轮仍发送超限 messages 触发 API 错误）。
                built = match context.build_provider_context(&model, &specs).await {
                    Ok(c) => c,
                    Err(e) => {
                        yield AgentEvent::Error(e.to_string());
                        yield AgentEvent::StateChanged(AgentState::Idle);
                        return;
                    }
                };
            }

            let req = CompletionRequest {
                model: model.clone(),
                system: built.system.clone(),
                messages: built.messages,
                tools: specs.clone(),
                tool_choice: soft_tool_choice.clone(),
                max_tokens,
                temperature,
                thinking: thinking.clone(),
                cache_key: None,
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

            let mut event_stream = match provider.stream(req, &provider_ctx).await {
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
                continue;
            }

            // 无工具调用 → 任务完成
            if tool_calls.is_empty() {
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
                yield AgentEvent::Done(summary);
                return;
            }

            // 逐工具执行（含 say/ask 审批门禁）
            let cancel_ref = &cancel;
            let workspace_ref = &workspace;
            let approval_ref = &approval;
            // 软工具需求：本轮是否调用了所需工具（未调用则下一轮升级为强制）。
            let soft_called = soft_tool_name_this_turn
                .as_deref()
                .map_or(true, |req| tool_calls.iter().any(|(_, n, _)| n == req));
            for (id, name, args) in tool_calls {
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

                // 执行
                let tcx = ToolContext {
                    workspace: workspace_ref,
                    approval: approval_ref.as_ref(),
                    cancel: cancel_ref,
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
                };
                for h in &hooks {
                    h.on_event(&HookEvent::BeforeTool { tool: name.clone(), args: args.clone() }).await;
                }
                let result = match tool.execute(args.clone(), &tcx).await {
                    Ok(r) => r,
                    Err(e) => {
                        // 仅「不可恢复」错误累加 mistakes 终止计数；可恢复错误（如某文件
                        // 不存在）应让模型据回填自我纠正，避免多工具轮次中提前触顶。
                        if !e.is_recoverable() {
                            mistakes += 1;
                        }
                        ToolResult::Error {
                            recoverable: e.is_recoverable(),
                            message: e.to_string(),
                        }
                    }
                };
                for h in &hooks {
                    h.on_event(&HookEvent::AfterTool { tool: name.clone(), result: result.clone() }).await;
                }
                let preview = result.to_llm_text();
                yield AgentEvent::ToolExec {
                    name: name.clone(),
                    output: preview.chars().take(200).collect(),
                };
                context
                    .append(agent_core::AgentMessage::ToolResult(ToolResultMessage {
                        tool_call_id: id,
                        result,
                    }))
                    .await;
            }

            // 软工具需求：本轮未调用所需工具 → 下一轮升级为强制（修复 escalate 死代码）。
            escalate_soft = !soft_called && soft_tool_name_this_turn.is_some();

            if mistakes >= max_mistakes {
                yield AgentEvent::Error(format!("连续错误达到上限 {max_mistakes}，停止"));
                yield AgentEvent::StateChanged(AgentState::Idle);
                return;
            }
            // 继续下一轮（工具结果已回填，模型再次推理）
        }
    }
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
        }))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_context::InMemoryContext;
    use agent_core::ContextManager;

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
}
