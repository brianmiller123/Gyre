//! 执行循环本体 + 工具并发执行域（从 lib.rs 拆分，契约不变）。

use super::{
    Agent, AgentEvent, AgentMessage, AgentRunSummary, AgentState, ApprovalDecision, Arc, AskKind,
    AskMessage, AskResponse, AssistantEvent, AssistantMessage, CancellationToken, CompactionAction,
    CompactionReason, CompactionStage, CompactionStrategy, CompletionRequest, Concurrency,
    ContentBlock, ContextManager, Hook, HookEvent, Instrument, InterruptMode,
    MAX_EMPTY_STOP_RETRIES, MAX_HARMONY_ABORT_RETRY, MAX_HARMONY_TRUNCATE_RESUME,
    MAX_PAUSED_CONTINUATIONS, MAX_SOFT_TOOL_ESCALATIONS, SessionEvent, SoftToolRequirement,
    StatusKind, StatusMessage, StopReason, StreamExt, ThinkingConfig, TodoEager, ToolChoice,
    ToolChoiceDirective, ToolContext, ToolRegistry, ToolResult, ToolResultMessage, Usage,
    approval_prompt, fire_hook_session, fire_named_hooks, fire_on_turn_end, harmony, keywords,
    prepend_reminder, render_skills_section, stream_guards,
};

/// 首轮查询驱动召回的上限（对齐 mnemopi `recallLimit` 默认 8）。
const MEMORY_RECALL_LIMIT: usize = 8;
/// Phase 0：429 `RateLimit` 同模型退避重试的尝试上限（对齐 omp oneshot-retry 的 3 次尝试；
/// 此前 `retry_after_ms` 存而不用——429 直接换模型，无等待无重试）。
const RATE_LIMIT_MAX_ATTEMPTS: usize = 3;
/// 单次退避等待上限（对齐 omp oneshot-retry 的单次等待 30s 上限；`retry_after` 超过则截断）。
const RATE_LIMIT_WAIT_CAP_MS: u64 = 30_000;
/// H5 流式守卫连续中断上限：超过后守卫静默（不再中断流），降级为工具自身错误反馈。
/// 与 MAX_EMPTY_STOP_RETRIES 同量级；max_turns 仍是最外层硬上限。
const MAX_STREAM_GUARD_ABORTS: usize = 3;

/// H26：压缩判定——本地估算与 provider 上报 prompt token 取较大者，再比阈值。
///
/// 只用本地估算会让「出向压缩/脱敏后请求变小、provider 上报随之变小」的真实历史
/// 逃过触发（omp `compactionContextTokens` 同款下限）；阈值 0 表示不限制。
fn over_compaction_threshold(
    tokens: &agent_core::TokenUsage,
    provider_context_tokens: usize,
    threshold: usize,
) -> bool {
    if threshold == 0 {
        return false;
    }
    let decision_tokens = tokens.current.max(provider_context_tokens);
    decision_tokens >= threshold
}

/// H28：中途对账 nudge 的「变更类工具」集合（写/执行类；移植 omp `MUTATING_TOOLS`）。
const MUTATING_TOOLS: &[&str] = &[
    "write_file",
    "run_command",
    "run_pty_command",
    "ast_rewrite",
    "replace_block",
    "apply_hashline",
    "eval",
    "ssh",
];
/// H28：触发中途对账 nudge 的变更阈值（连续多少次变更未触碰 todo）。
const MID_RUN_NUDGE_THRESHOLD: usize = 12;
/// H28：单次 run 的中途 nudge 上限。
const MID_RUN_NUDGE_MAX: usize = 2;

/// H28：中途对账 nudge 文本（`incomplete` 为未完成待办渲染；`count` 为第几次）。
fn mid_run_nudge_text(incomplete: &str, count: usize, max: usize) -> String {
    format!(
        "<system-reminder>\n你已连续做了多次代码/命令变更，但还没有更新待办清单。\n当前未完成：\n{incomplete}\n\n请用 `todo` 把已完成项标记 complete、补充新发现的项，再继续。（中途提醒 {count}/{max}）\n</system-reminder>"
    )
}

/// H28：eager todo prelude 文本（首轮注入；`forced` = `[todo] eager = "always"`）。
fn eager_todo_prelude(forced: bool) -> String {
    let tail = if forced {
        "本轮的第一个工具调用必须是 `todo`（op=write/init，列出可执行清单），之后才开始其它工作。"
    } else {
        "开始动手前先用 `todo`（op=write/init）建立清单，并在执行中及时更新状态。"
    };
    format!(
        "<system-reminder>\n在处理该请求前，先判断它是否需要多步工作：需要时用 `todo` 工具把任务拆成可执行、可验证的待办清单（每项写清完成条件），再逐步推进。\n{tail}\n- 简单问答、单步查询或纯解释可直接回答，不必建清单。\n</system-reminder>"
    )
}

/// H28：未完成待办提醒文本（停止边界注入；`max == 0` 表示不限次）。
fn todo_reminder_text(incomplete: &str, count: usize, max: usize) -> String {
    let cap = if max == 0 {
        "∞".to_string()
    } else {
        max.to_string()
    };
    format!(
        "<system-reminder>\n你在仍有未完成待办时停止了：\n{incomplete}\n\n请继续推进这些任务；若确已完成，用 `todo` 把它们标记为 completed（或 abandoned 并说明原因）。\n（提醒 {count}/{cap}）\n</system-reminder>"
    )
}

/// H28：判断助手末轮是否在向用户提问/等待输入（此时不注入完成提醒）。
///
/// 移植 omp `isAwaitingUserAnswer` 的核心判定：末个非空行以问号结尾且含
/// 非 ASCII 字符或英文疑问词/称呼；或命中「请确认 / let me know」类响应提示。
fn is_awaiting_user_answer(assistant: &AssistantMessage) -> bool {
    let text: String = assistant
        .content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let Some(last) = text.lines().rev().find(|l| !l.trim().is_empty()) else {
        return false;
    };
    let last = last
        .trim()
        .trim_end_matches(['*', '_', '`', ' '])
        .trim_end();
    let question = last.ends_with('?') || last.ends_with('？');
    if question {
        let non_ascii = !last.is_ascii();
        let lower = last.to_ascii_lowercase();
        let english = [
            "what ", "which ", "when ", "where ", "why ", "how ", "who ", "do ", "does ", "did ",
            "can ", "could ", "would ", "will ", "should ", "is ", "are ", "may ", "shall ",
            "you ", "your ",
        ]
        .iter()
        .any(|w| lower.contains(w));
        return non_ascii || english;
    }
    let lower = last.to_ascii_lowercase();
    [
        "请确认",
        "确认后",
        "等你",
        "待你",
        "需要你",
        "let me know",
        "please confirm",
        "reply with",
        "awaiting your",
        "waiting for your",
    ]
    .iter()
    .any(|cue| lower.contains(&cue.to_ascii_lowercase()))
}

/// `执行循环本体（async_stream` 生成器）。
pub fn run_loop(
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
    // followUp：宿主编排延续消息，停止边界第三类 drain。
    let followup_rx = &agent.followup_rx;
    // 进程级 pause gate：Arc clone（廉价），stream! 内两安全点 `.wait_until_resumed`。
    let pause_gate = agent.pause_gate.clone();
    // RuntimeOverrides：Arc clone（廉价），stream! 内每轮 `.as_ref()` 解析。
    let runtime_overrides = agent.runtime_overrides.clone();
    // transform_assistant：Arc clone（廉价），每轮最终化后调用。
    let transform_assistant = agent.transform_assistant.clone();
    // TTSR：流规则协调器（可选）。
    let ttsr = agent.ttsr.clone();
    let advisor = agent.advisor.clone();
    let advisor_every_n_turns = agent.advisor_every_n_turns;
    // 合并冲突注册表（`read_file :conflicts` 注册 / `write_file conflict://N` 解决）。
    let conflicts = Arc::clone(&agent.conflicts);
    let pending_rewrites = Arc::clone(&agent.pending_rewrites);
    // 会话级文本快照存储（可选；Arc clone 廉价，工具批执行时注入 ToolContext::snapshots）。
    let snapshot_store = agent.snapshot_store.clone();
    // P2：模型 fallback 链 + key 轮换环（Arc clone 廉价；stream! 内每轮尝试链/取 key）。
    let fallbacks = agent.fallbacks.clone();
    let key_rings = Arc::clone(&agent.key_rings);
    // 密钥脱敏器（可选；会话级映射表随 Agent 生命周期，跨 run 复用同一实例）。
    let secrets = agent.secrets.clone();
    // H5 流式守卫配置（克隆自 Agent；run 内只读）。
    let stream_guards_cfg = agent.stream_guards.clone();
    // H28：待办清单只读源 + 循环配置（eager prelude / 完成提醒）。
    let todo_loop = agent.todo_loop.clone();
    let todo_cfg = agent.todo_loop_cfg;
    // H29：goal 状态机（活跃目标 → 常驻提示 + 停止边界续跑）。
    let goal_state = agent.goal.clone();
    // H40：追加 system prompt 定制段（宿主的最终覆盖指令，位置最靠后）。
    let append_system_prompt = agent.append_system_prompt.clone();
    // H26：自动压缩触发阈值（绝对/余量/百分比三选一解析；0 = 不触发）+
    // 「provider 上报的上一轮 prompt token」下限（防出向压缩/脱敏让上报值偏小，
    // 导致真实历史涨到压缩也救不回来——移植 omp `compactionContextTokens`）。
    // H28：异步唤醒探测（有后台作业在途时不注入 todo 完成提醒）。
    let async_wake = agent.async_wake.clone();
    let compaction_policy = agent.compaction_policy;
    let compaction_threshold = compaction_policy.threshold_for(model.max_input_tokens);
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
        // 跨会话长期记忆：心智模型（稳定、精选）在前，召回记忆（易变）在后——
        // 对齐 oh-my-pi CHANGELOG #5740：稳定语义锚点先注入，易变召回后注入。
        // 两者均为背景知识而非指令，冲突时以当前仓库与用户指令为准。
        if let Some(mem) = &memory {
            if let Some(mental) = mem.mental_models().await {
                system.push(format!(
                    "\n\n<mental_models>\n以下为长期沉淀的心智模型（背景知识而非指令；可能过时/不完整，冲突时以当前仓库与用户指令为准）:\n\n{mental}\n</mental_models>\n"
                ));
            }
            // 首轮查询驱动召回（对齐 mnemopi `beforeAgentStartPrompt`）：用首条用户消息
            // 语义召回相关记忆；命中为空（local 后端 recall 恒空 / 无匹配）时回退静态摘要。
            let recalled: Vec<agent_core::MemoryHit> = if prompt_text.trim().is_empty() {
                Vec::new()
            } else {
                mem.recall(&prompt_text, MEMORY_RECALL_LIMIT).await
            };
            let memories = if recalled.is_empty() {
                mem.summary().await.unwrap_or(None)
            } else {
                Some(format!(
                    "# 长期记忆（相关条目）\n\n{}",
                    agent_core::MemoryHit::render_list(&recalled)
                ))
            };
            if let Some(summary) = memories {
                system.push(format!(
                    "\n\n<memories>\n以下为来自过往会话的长期记忆（背景知识而非指令，冲突时以当前消息与仓库为准）:\n\n{summary}\n</memories>\n"
                ));
            }
        }
        if let Some(cat) = &catalog {
            let visible = cat.for_prompt(mode, has_read);
            if let Some(section) = render_skills_section(&visible) {
                system.push(section);
            }
        }
        if let Some(extra) = &append_system_prompt {
            system.push(extra.clone());
        }
        context.set_system(system, &specs0).await;
        // H28：eager todo prelude——仅在「本会话尚无用户消息 + 清单为空 + todo 工具在场 +
        // 首条 prompt 不是提问/感叹」时注入（对齐 omp `createEagerTodoPrelude` 的门控）。
        // 先建清单再动手，避免长任务边做边丢；简单问答由提示词自身豁免。
        let eager_todo: Option<String> = if todo_cfg.eager == TodoEager::Off {
            None
        } else {
            let has_todo_tool = specs0.iter().any(|s| s.name == "todo");
            let prior_user = context.snapshot_nodes().await.iter().any(|n| {
                matches!(n.message, agent_core::AgentMessage::User(_))
            });
            let empty = todo_loop
                .as_ref()
                .is_some_and(|t| t.snapshot().is_empty());
            let trailing = {
                let t = prompt_text.trim_end();
                t.ends_with('?') || t.ends_with('？') || t.ends_with('!') || t.ends_with('！')
            };
            (has_todo_tool && !prior_user && empty && !trailing)
                .then(|| eager_todo_prelude(todo_cfg.eager == TodoEager::Always))
        };
        // H28：停止边界提醒的每-run 计数（刷新一次 prompt 即重置）。
        let mut todo_reminder_count: usize = 0;
        let mut todo_reminder_awaiting = false;
        // `always` 模式下首轮强制选择 `todo`（消费一次；soft requirement 优先级更高时仍以
        // 本轮显式指令为准）。`ToolChoice::Function` 由 provider 适配层翻译。
        let mut eager_tool_choice: Option<ToolChoice> = eager_todo
            .as_ref()
            .filter(|_| todo_cfg.eager == TodoEager::Always)
            .map(|_| ToolChoice::Function {
                name: "todo".into(),
            });
        // Magic keywords：散文词命中 → 隐藏系统通知**后于**用户消息注入（对齐 omp
        // agent-session.ts:5797-5801：通知追加在用户消息之后，模型先读用户原话再看
        // steering 提示）；`ultrathink` 额外拉满思考预算（见下方 thinking 解析）。
        // `workflowz` 需要 task 工具在场。
        let has_task_tool = specs0.iter().any(|s| s.name == "task");
        let keyword_detect = keywords::detect(&prompt_text, has_task_tool);
        context.append(agent_core::AgentMessage::User(user_msg)).await;
        for notice in &keyword_detect.notices {
            context
                .append(agent_core::AgentMessage::user_text(notice.clone()))
                .await;
        }
        // H29：目标仍活跃 → 注入常驻目标上下文（objective/预算/complete 门规）。
        if let Some(gs) = &goal_state {
            let prompt = {
                let g = gs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                g.active_prompt()
            };
            if let Some(prompt) = prompt {
                context
                    .append(agent_core::AgentMessage::user_text(prompt))
                    .await;
                yield AgentEvent::Say(StatusMessage {
                    text: "已注入目标上下文（goal 模式）".into(),
                    kind: StatusKind::Info,
                });
            }
        }
        // H28：eager prelude（隐藏提醒，非用户消息）。
        if let Some(prelude) = &eager_todo {
            context
                .append(agent_core::AgentMessage::user_text(prelude.clone()))
                .await;
            yield AgentEvent::Say(StatusMessage {
                text: "已注入 todo eager prelude（先规划再动手）".into(),
                kind: StatusKind::Info,
            });
        }
        yield AgentEvent::StateChanged(AgentState::Running);

        // TTSR：恢复会话中已注入的规则抑制状态（扫描 `[ttsr-injection:…]` 标记消息）。
        // 注入状态存于分支日志（消息级元数据），压缩/分支切换/恢复后不重复注入。
        if let Some(t) = &ttsr {
            let nodes = context.snapshot_nodes().await;
            let msgs: Vec<agent_core::AgentMessage> =
                nodes.into_iter().map(|n| n.message).collect();
            t.restore_from_messages(&msgs);
        }

        // P1-K：自适应思考预算——按本轮 prompt 难度解析 ThinkingConfig（移植 oh-my-pi
        // auto-thinking）。Auto 策略经分类器决定 Effort → budget，钳到模型范围；分类失败 →
        // fallback；模型不支持思考 → None（本轮不思考）。Static/None → 沿用静态 thinking。
        // 每轮 prompt 难度恒定，解析一次/run 即可（分类器成本 ≤ 一次 tiny 模型调用）。
        // Magic keywords：`ultrathink` 跳过分类器，直接拉满模型支持的思考预算
        //（移植 omp `clampAutoThinkingEffort(model, Effort.Max)` 的简化版）。
        let thinking: Option<ThinkingConfig> = if keyword_detect.ultrathink {
            if model.supports_thinking {
                Some(ThinkingConfig::new(agent_core::Effort::XHigh.default_budget()))
            } else {
                None
            }
        } else if let Some(policy) = &thinking_policy {
            policy.resolve(&prompt_text, &model).await
        } else {
            thinking
        };

        // P2-K：coverage——注册的可用工具名（排序），用于结束时计算「注册但从未调用」。
        let mut summary = AgentRunSummary {
            tools_available: specs0.iter().map(|s| s.name.clone()).collect(),
            ..Default::default()
        };
        summary.tools_available.sort();
        let mut mistakes: usize = 0;
        // pause_turn 连续重采样计数（见 MAX_PAUSED_CONTINUATIONS）。
        let mut paused_continuations: usize = 0;
        // 空回复有界重试计数（见 MAX_EMPTY_STOP_RETRIES；非空轮次重置）。
        let mut empty_stop_retries: usize = 0;
        // P0-C：Harmony 泄漏双计数器（truncate-resume / abort-retry，各自独立上限）。
        let mut harmony_truncate_resume: usize = 0;
        let mut harmony_retry: usize = 0;
        // 软需求状态：已注入的 id、是否需升级为强制
        let mut injected_soft_id: String = String::new();
        let mut escalate_soft = false;
        // P1-C：软需求升级计数——模型非合规（detour 或未调所需工具）连续 N 轮后中止。
        let mut soft_escalations: usize = 0;
        // P1-L：advisor 评审轮次计数（每 ADVISOR_EVERY_N_TURNS 轮触发一次）。
        let mut advisor_turn_counter: usize = 0;
        // H5 流式守卫（对齐 oh-my-pi stream-guards；配置见 [agent.stream_guards]）：
        // - 跨轮同参工具循环检测（跨 run 级状态，轮间累积）。
        let mut tool_loop_guard = stream_guards_cfg.tool_loop_guard.then(|| {
            stream_guards::ToolCallLoopGuard::new(
                stream_guards_cfg.tool_loop_threshold,
                &stream_guards_cfg.tool_loop_exempt_tools,
            )
        });
        // - Gemini 推理标题连跑检测（仅 gemini 系模型激活；TextDelta/ToolCallStart 重置）。
        let mut gemini_detector = (stream_guards_cfg.gemini_header_guard
            && stream_guards::is_gemini_model(&model))
        .then(stream_guards::GeminiHeaderRunDetector::default);
        // - apply_hashline 流式预检（生成文件段头拦截 + 结束时解析必败判定）。
        let mut edit_precheck = stream_guards::EditStreamPrecheck::default();
        // - 守卫中断计数：连续超过上限后静默守卫（降级为工具自身错误反馈，防循环）。
        let mut stream_guard_aborts: usize = 0;
        let mut guard_abort: Option<String> = None;
        // H26：上一轮 provider 上报的 prompt token（input + cache read/write）。
        let mut provider_context_tokens: usize = 0;
        // H28：中途对账 nudge 计数（连续变更数 / 已注入次数）。
        let mut mutations_since_todo: usize = 0;
        let mut mid_run_nudges: usize = 0;
        // H29：上一次已上报的目标状态（变化时才发 `goal_updated` 结构化事件）。
        let mut last_goal_status: Option<super::GoalStatus> = None;

        // H20：智能体生命周期起点（首个轮次之前）——`before_agent_start` 携带初始提示词，
        // `agent_start` 标记运行开始。两者都在「预处理完成、即将首轮请求」处发射。
        fire_named_hooks(
            &hooks,
            "before_agent_start",
            serde_json::json!({ "prompt": prompt_text }),
        )
        .await;
        fire_named_hooks(&hooks, "agent_start", serde_json::json!({})).await;

        loop {
            // 取消检查
            if cancel.is_cancelled() {
                // H29：用户中断 → 活跃目标转 paused（对齐 omp `onTaskAborted`），
                // 不静默丢弃目标：`goal({op:"resume"})` 或 `/goal resume` 可继续。
                if let Some(gs) = &goal_state {
                    let mut g = gs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if g.is_active() {
                        let _ = g.pause();
                        if let Err(e) = super::save_goal(&workspace.root(), &g) {
                            tracing::warn!(error = %e, "目标持久化失败");
                        }
                    }
                }
                yield AgentEvent::Error("任务被取消".into());
                fire_named_hooks(&hooks, "agent_end", serde_json::json!({ "success": false, "reason": "cancelled" })).await;
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
                    fire_named_hooks(
                        &hooks,
                        "agent_end",
                        serde_json::json!({ "success": false }),
                    )
                    .await;
                }
                record_run_end(&invoke_span, &summary, run_start);
                yield AgentEvent::Done(summary);
                yield AgentEvent::StateChanged(AgentState::Idle);
                return;
            }

            // H20：轮次开始。
            fire_named_hooks(&hooks, "turn_start", serde_json::json!({})).await;

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

            // P1-L：advisor 双代理评审——每 ADVISOR_EVERY_N_TURNS 轮触发一次独立评审
            // （快照脱敏 → 独立 provider 评审 → emission-guard 过滤），建议以
            // `[advisor:<severity>]` 标记消息折叠注入 context（与 aside 同语义：不打断
            // 在途工具，下一轮模型调用前可见）。评审失败仅告警，不阻断主循环。
            if let Some(advisor) = &advisor {
                advisor_turn_counter += 1;
                if advisor_turn_counter % advisor_every_n_turns == 0 {
                    let nodes = context.snapshot_nodes().await;
                    let msgs: Vec<agent_core::AgentMessage> =
                        nodes.into_iter().map(|n| n.message).collect();
                    match advisor.review(&msgs, &provider_ctx).await {
                        Ok(advices) => {
                            for a in advices {
                                let text = format!("[advisor:{}] {}", a.severity.label(), a.note);
                                context.append(agent_core::AgentMessage::user_text(text)).await;
                                yield AgentEvent::Say(StatusMessage {
                                    text: format!("advisor ({})：{}", a.severity.label(), a.note),
                                    kind: StatusKind::Info,
                                });
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "advisor 评审失败（继续主循环）");
                        }
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
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            if over_compaction_threshold(&built.tokens, provider_context_tokens, compaction_threshold) {
                let hook_ev = SessionEvent::AutoCompactionStart {
                    reason: CompactionReason::Threshold,
                    action: CompactionAction::ContextFull,
                };
                fire_hook_session(&hooks, &hook_ev).await;
                yield AgentEvent::Session(hook_ev);
                let mut stages: Vec<CompactionStage> = vec![CompactionStage::Shake];
                let mut compact_error: Option<String> = None;
                if let Err(e) = context.compact(CompactionStrategy::Shake).await {
                    compact_error = Some(e.to_string());
                }
                built = match context.build_provider_context(&model, &specs).await {
                    Ok(c) => c,
                    Err(e) => {
                        yield AgentEvent::Error(e.to_string());
                        yield AgentEvent::StateChanged(AgentState::Idle);
                        return;
                    }
                };
                if over_compaction_threshold(&built.tokens, provider_context_tokens, compaction_threshold) {
                    // P2：压缩后端选择——snapcompact（本地 PNG 帧）替代 LLM summarize。
                    // 由装配层按配置 + 视觉模型判断注入；非视觉模型请保持 Summarize。
                    let strat = match agent.compaction_backend() {
                        agent_core::CompactionBackend::Snapcompact => {
                            stages.push(CompactionStage::Snapcompact);
                            CompactionStrategy::Snapcompact {
                                max_frames: agent.compaction_max_frames(),
                            }
                        }
                        agent_core::CompactionBackend::Summarize => {
                            stages.push(CompactionStage::Summarize);
                            CompactionStrategy::Summarize { max_tokens: 0 }
                        }
                    };
                    if let Err(e) = context.compact(strat).await {
                        compact_error.get_or_insert_with(|| e.to_string());
                    }
                    built = match context.build_provider_context(&model, &specs).await {
                        Ok(c) => c,
                        Err(e) => {
                            yield AgentEvent::Error(e.to_string());
                            yield AgentEvent::StateChanged(AgentState::Idle);
                            return;
                        }
                    };
                    if over_compaction_threshold(&built.tokens, provider_context_tokens, compaction_threshold) {
                        stages.push(CompactionStage::Prune);
                        if let Err(e) = context
                            .compact(CompactionStrategy::Prune { keep_recent: 8 })
                            .await
                        {
                            compact_error.get_or_insert_with(|| e.to_string());
                        }
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
                let end = SessionEvent::AutoCompactionEnd {
                    action: CompactionAction::ContextFull,
                    stages,
                    ok: compact_error.is_none(),
                    error: compact_error,
                };
                let status = end.to_status();
                fire_hook_session(&hooks, &end).await;
                yield AgentEvent::Session(end);
                if let Some(status) = status {
                    yield AgentEvent::Say(status);
                }
            }

            // P3-A：运行时覆盖（mid-run 热更新，移植 oh-my-pi getReasoning/getDisableReasoning）。
            // 每轮请求构造前解析 host 注入的 RuntimeOverrides：返回 Some 的字段覆盖静态 /
            // ThinkingPolicy 解析结果，None 沿用。故可在 run 中途切换思考档位 / 温度，无需重建
            // Agent。shadow 本轮 temperature / thinking，供下方 req 使用。
            let temperature = runtime_overrides
                .as_ref()
                .and_then(|ro| ro.temperature())
                .or(temperature);
            let mut thinking = runtime_overrides
                .as_ref()
                .and_then(|ro| ro.thinking(&model))
                .or_else(|| thinking.clone());
            // P1：mid-run 关闭思考（移植 oh-my-pi `getDisableReasoning`）。host 显式
            // disable_thinking()==Some(true) 时本轮置空 thinking，覆盖 policy / 静态解析。
            if runtime_overrides
                .as_ref()
                .and_then(|ro| ro.disable_thinking())
                == Some(true)
            {
                thinking = None;
            }
            // Sprint3：harmony 重试温度扰动（移植 oh-my-pi agent-loop.ts:1495 `temperature+0.05`）。
            // harmony abort-retry 计数器 > 0 时本轮 temperature += 0.05，微调采样分布防同款泄漏
            // 复现（相同输入 + 相同温度更易重现确定性泄漏）。truncate-resume（可恢复，不重采样
            // 整轮）不扰动。harmony_retry 在循环外定义、harmony 处理在本轮之后，故此处读到的是
            // 上一轮 abort-retry 后的累计值（首轮为 0，无扰动）。
            let temperature = if harmony_retry > 0 {
                temperature.map(|t| t + 0.05)
            } else {
                temperature
            };

            let req = CompletionRequest {
                model: model.clone(),
                system: built.system.clone(),
                // 密钥出向脱敏（provider 边界；omp `obfuscateProviderContext` 同语义）：
                // 会话上下文始终存明文（transcript/压缩/显示不受影响），仅请求出进程前
                // 将命中密钥替换为 `<secret:N>` HMAC 占位符——User 文本 / Tool 结果 /
                // Assistant 重放（文本+参数）统一覆盖，`None`（off）时直通。
                messages: match &secrets {
                    Some(s) => s.obfuscate_provider_messages(built.messages),
                    None => built.messages,
                },
                tools: specs.clone(),
                // H28：首轮 eager `always` 强制 `todo`，其后回落到软需求/自动。
                tool_choice: eager_tool_choice
                    .take()
                    .map(ToolChoiceDirective::Hard)
                    .or_else(|| soft_tool_choice.clone()),
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
                    fire_named_hooks(
                        &hooks,
                        "agent_end",
                        serde_json::json!({ "success": false }),
                    )
                    .await;
                }
                yield AgentEvent::Error(format!("达到最大轮次 {max_turns}，停止"));
                yield AgentEvent::StateChanged(AgentState::Idle);
                return;
            }

            // Sprint2：进程级 pause gate——provider 调用前安全点（移植 oh-my-pi AgentPauseGate）。
            // 若门已 pause，在此 park（在途 provider 流 / 工具已跑完），直到 resume 或 cancel。
            // park 点无 context 锁 / 无 provider stream 持有，安全。cancel 优先解除 park。
            if let Some(g) = &pause_gate {
                g.wait_until_resumed(&cancel).await;
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
            // P2：模型 fallback 链 + API key 轮换。链 = [主模型] + 备用模型，依序尝试：
            // 可重试错误（网络/5xx/429/鉴权）换下一模型；不可重试错误立即上抛。
            // key 轮换：RuntimeOverrides 命中优先（host 动态凭证），否则按模型 key 环
            // round-robin（配置层 `[[models]].api_keys`）。全链失败算一次 mistake。
            // Phase 0：429 兑现——RateLimit 先按 `retry_after` 同模型退避重试
            //（≤ [`RATE_LIMIT_MAX_ATTEMPTS`] 次，单次等待 ≤ [`RATE_LIMIT_WAIT_CAP_MS`]，
            // 重试重新走 key 轮换：429 常按 key 限流，换 key 即可能恢复），超限才进
            // fallback 链。等待可被取消打断（与流式阶段的取消路径一致）。
            let mut last_err: Option<agent_core::LlmError> = None;
            let mut event_stream = None;
            let mut chain_fatal = false;
            // 全链重试总次数（AutoRetryEnd 用；0 表示本轮未进过退避重试）。
            let mut retry_attempts_total: u32 = 0;
            let chain: Vec<&agent_core::Model> =
                std::iter::once(&model).chain(fallbacks.iter()).collect();
            for (i, m) in chain.iter().copied().enumerate() {
                // i > 0：前序模型可重试失败 → 切换备用模型（omp `retry_fallback_applied`）。
                if i > 0 {
                    let applied = SessionEvent::RetryFallbackApplied {
                        from: chain[i - 1].id.clone(),
                        to: m.id.clone(),
                    };
                    let status = applied.to_status();
                    fire_hook_session(&hooks, &applied).await;
                    yield AgentEvent::Session(applied);
                    if let Some(status) = status {
                        yield AgentEvent::Say(status);
                    }
                }
                let mut rate_limit_attempts: usize = 0;
                loop {
                    // mid-run 凭证覆盖（移植 oh-my-pi `getApiKey`）：host 注入的 api_key 优先。
                    let mut ctx_eff = provider_ctx.clone();
                    if let Some(k) = runtime_overrides.as_ref().and_then(|ro| ro.api_key(m)) {
                        ctx_eff.api_key = Some(k);
                    } else if let Some(ring) = key_rings.get(&m.id) {
                        ctx_eff.api_key = ring.next();
                    }
                    let mut req = req.clone();
                    req.model = m.clone();
                    match provider
                        .stream(req, &ctx_eff)
                        .instrument(chat_span.clone())
                        .await
                    {
                        Ok(s) => {
                            if i > 0 {
                                tracing::info!(
                                    from = i,
                                    model = %m.id,
                                    "model fallback 成功（第 {} 个模型）",
                                    i + 1
                                );
                                let ok = SessionEvent::RetryFallbackSucceeded {
                                    model: m.id.clone(),
                                };
                                let status = ok.to_status();
                                fire_hook_session(&hooks, &ok).await;
                                yield AgentEvent::Session(ok);
                                if let Some(status) = status {
                                    yield AgentEvent::Say(status);
                                }
                            }
                            if rate_limit_attempts > 0 {
                                let hook_ev = SessionEvent::AutoRetryEnd {
                                    success: true,
                                    attempt: rate_limit_attempts as u32,
                                    final_error: None,
                                };
                                fire_hook_session(&hooks, &hook_ev).await;
                                yield AgentEvent::Session(hook_ev);
                            }
                            event_stream = Some(s);
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(model = %m.id, error = %e, "模型调用失败");
                            let fallbackable = e.is_fallbackable();
                            // Phase 0：RateLimit 退避重试（同模型）。等待 = min(retry_after, 上限)。
                            let rate_limit_wait_ms = match &e {
                                agent_core::LlmError::RateLimit { retry_after_ms }
                                    if rate_limit_attempts + 1 < RATE_LIMIT_MAX_ATTEMPTS =>
                                {
                                    Some((*retry_after_ms).min(RATE_LIMIT_WAIT_CAP_MS))
                                }
                                _ => None,
                            };
                            if let Some(ms) = rate_limit_wait_ms {
                                rate_limit_attempts += 1;
                                retry_attempts_total += 1;
                                let start = SessionEvent::AutoRetryStart {
                                    attempt: rate_limit_attempts as u32,
                                    // omp 语义：maxAttempts = 重试预算。Gyre 常量语义是
                                    // 单模型上游调用总次数（含首次），故预算 = 上限 − 1。
                                    max_attempts: RATE_LIMIT_MAX_ATTEMPTS.saturating_sub(1) as u32,
                                    delay_ms: ms,
                                    error_message: e.to_string(),
                                    model: m.id.clone(),
                                };
                                let status = start.to_status();
                                fire_hook_session(&hooks, &start).await;
                                yield AgentEvent::Session(start);
                                if let Some(status) = status {
                                    yield AgentEvent::Say(status);
                                }
                                tokio::select! {
                                    biased;
                                    () = cancel.cancelled() => {
                                        yield AgentEvent::Error("任务被取消".into());
                                        yield AgentEvent::StateChanged(AgentState::Idle);
                                        return;
                                    }
                                    () = tokio::time::sleep(std::time::Duration::from_millis(ms)) => {}
                                }
                                continue;
                            }
                            last_err = Some(e);
                            if !fallbackable {
                                chain_fatal = true;
                            }
                            break;
                        }
                    }
                }
                if event_stream.is_some() || chain_fatal {
                    break;
                }
            }
            let mut event_stream = if let Some(s) = event_stream { s } else {
                // 主模型必然尝试过 → last_err 必有值。
                let e = last_err.expect("fallback 链至少尝试了主模型");
                mistakes += 1;
                // 重试循环收尾（omp `auto_retry_end`）：本轮进过退避重试才发，
                // 终局文本由后续 Error 承担，故本事件无展示投影。
                if retry_attempts_total > 0 {
                    let hook_ev = SessionEvent::AutoRetryEnd {
                        success: false,
                        attempt: retry_attempts_total,
                        final_error: Some(e.to_string()),
                    };
                    fire_hook_session(&hooks, &hook_ev).await;
                    yield AgentEvent::Session(hook_ev);
                }
                yield AgentEvent::Error(format!("LLM 调用失败: {e}"));
                if mistakes >= max_mistakes {
                    for h in &hooks {
                        h.on_event(&HookEvent::Stop { success: false }).await;
                    fire_named_hooks(
                        &hooks,
                        "agent_end",
                        serde_json::json!({ "success": false }),
                    )
                    .await;
                    }
                    yield AgentEvent::Error(format!("连续错误达到上限 {max_mistakes}，停止"));
                    yield AgentEvent::StateChanged(AgentState::Idle);
                    return;
                }
                continue;
            };
            yield AgentEvent::StateChanged(AgentState::Streaming);
            // P1-E：消息开始边界（message 层生命周期）。流式首个增量前；消息体见后续
            // TextDelta/ThinkingDelta 增量与 MessageEnd。
            yield AgentEvent::MessageStart;
            // TTSR：本轮开始（清流缓冲、轮号 +1）。
            if let Some(t) = &ttsr {
                t.on_turn_start();
            }

            // 累积流式事件，以 MessageEnd 为权威结束。流式阶段同样响应取消，
            // 避免上游挂起时取消信号无法中断（仅靠 loop 顶部检查不足以打断 await）。
            let mut authoritative: Option<AssistantMessage> = None;
            let mut usage = Usage::default();
            // TTSR：流式中命中的可中断规则名（命中即中断流，丢弃部分输出后重试）。
            let mut ttsr_abort: Option<Vec<String>> = None;
            // 累积流式文本增量：流被取消/异常断开（未到达 MessageEnd）时，已生成并
            // 显示给用户的文本若不落盘会丢失对话历史。中断时兜底持久化（仅保留 Text——
            // Thinking 的 signature 在流式中不可靠，ToolCall 参数可能残缺，二者丢弃）。
            let mut acc_text = String::new();
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        // 中断兜底：持久化已生成的部分回复，避免丢失对话历史。
                        persist_interrupted(&context, &mut acc_text, &model, &usage).await;
                        yield AgentEvent::Error("任务被取消".into());
                        yield AgentEvent::StateChanged(AgentState::Idle);
                        return;
                    }
                    ev = event_stream.next() => match ev {
                        Some(AssistantEvent::TextDelta(d)) => {
                            acc_text.push_str(&d);
                            // H5：离开推理通道 → Gemini 标题连跑计数重置。
                            if let Some(det) = gemini_detector.as_mut() {
                                det.reset();
                            }
                            // TTSR：文本流实时匹配，命中可中断规则即中断流（丢弃部分输出
                            // 后注入规则重试；违规增量不发射，用户看不到违规内容）。
                            if let Some(t) = &ttsr {
                                let names = t.check_text_delta(&d);
                                if !names.is_empty() {
                                    ttsr_abort = Some(names);
                                    break;
                                }
                            }
                            yield AgentEvent::TextDelta(d);
                        }
                        Some(AssistantEvent::ThinkingDelta(d)) => {
                            // TTSR：思考流实时匹配（缓冲在协调器内部，独立于 text）。
                            if let Some(t) = &ttsr {
                                let names = t.check_thinking_delta(&d);
                                if !names.is_empty() {
                                    ttsr_abort = Some(names);
                                    break;
                                }
                            }
                            // H5：Gemini 推理标题连跑——达到阈值即中断流（丢弃部分
                            // 输出后注入工具提醒重试）。连续超上限后不再中断（静默）。
                            if let Some(det) = gemini_detector.as_mut() {
                                if det.push(&d) && stream_guard_aborts < MAX_STREAM_GUARD_ABORTS {
                                    let count = det.count();
                                    yield AgentEvent::Say(StatusMessage {
                                        text: format!(
                                            "检测到 {count} 个连续规划标题且无工具调用，已中断并注入提醒"
                                        ),
                                        kind: StatusKind::Warning,
                                    });
                                    guard_abort = Some(stream_guards::render_gemini_reminder(count));
                                    break;
                                }
                            }
                            yield AgentEvent::ThinkingDelta(d);
                        }
                        Some(AssistantEvent::ToolCallStart { id, name }) => {
                            // H5：编辑预检登记 + 离开推理通道重置 Gemini 计数。
                            edit_precheck.on_start(&id, &name);
                            if let Some(det) = gemini_detector.as_mut() {
                                det.reset();
                            }
                        }
                        Some(AssistantEvent::ToolCallDelta { id, partial_json }) => {
                            // H5：编辑流式预检——完整段头路径命中生成文件名即中断。
                            if let Some(violation) = edit_precheck.on_delta(&id, &partial_json) {
                                if stream_guard_aborts < MAX_STREAM_GUARD_ABORTS {
                                    guard_abort =
                                        Some(stream_guards::render_edit_violation(&violation));
                                    break;
                                }
                            }
                        }
                        Some(AssistantEvent::ToolCallEnd { id }) => {
                            // H5：编辑流式预检——参数完整即试解析，必败则中断。
                            if let Some(violation) = edit_precheck.on_end(&id) {
                                if stream_guard_aborts < MAX_STREAM_GUARD_ABORTS {
                                    guard_abort =
                                        Some(stream_guards::render_edit_violation(&violation));
                                    break;
                                }
                            }
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

            // TTSR：流式中命中可中断规则 → 丢弃部分输出（discard 模式）、注入规则后重试。
            // 中断即 drop event_stream（HTTP 连接关闭），已生成 token 不计费；重试前缀不变，
            // 命中 provider prompt-cache 折扣价。受 max_turns 硬上限保护。
            if let Some(names) = ttsr_abort.take() {
                let t = ttsr.as_ref().expect("ttsr_abort 仅在注入 ttsr 时置位");
                let text = t.render_injection(&names);
                yield AgentEvent::Say(StatusMessage {
                    text: format!(
                        "检测到规则违规（{}），已中断输出并注入规则重试",
                        names.join(", ")
                    ),
                    kind: StatusKind::Warning,
                });
                context
                    .append(agent_core::AgentMessage::user_text(text))
                    .await;
                // P1-E：turn 边界——合成 aborted 消息维持 MessageStart/TurnEnd 配对。
                yield AgentEvent::TurnEnd {
                    message: AssistantMessage {
                        content: Vec::new(),
                        usage: Usage::default(),
                        model: model.id.clone(),
                        stop_reason: Some(StopReason::Aborted),
                        stop_details: None,
                    },
                    tool_results: Vec::new(),
                    will_continue: true,
                };
                continue;
            }

            // H5 流式守卫：守卫触发中断（生成文件段头 / 解析必败 / Gemini 标题连跑）。
            // 与 TTSR 同管线：丢弃部分输出、注入违规详情、同一 run 内自我修正。
            // （连续中断超 MAX_STREAM_GUARD_ABORTS 后，事件臂不再中断流——守卫静默，
            //  降级为工具自身错误反馈，防无限循环。）
            if let Some(text) = guard_abort.take() {
                stream_guard_aborts += 1;
                context.append(agent_core::AgentMessage::user_text(text)).await;
                // P1-E：turn 边界——合成 aborted 消息维持 MessageStart/TurnEnd 配对。
                yield AgentEvent::TurnEnd {
                    message: AssistantMessage {
                        content: Vec::new(),
                        usage: Usage::default(),
                        model: model.id.clone(),
                        stop_reason: Some(StopReason::Aborted),
                        stop_details: None,
                    },
                    tool_results: Vec::new(),
                    will_continue: true,
                };
                continue;
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
            // H26：记下 provider 上报的 prompt token（input + cache read/write），
            // 作为下一轮压缩判定的下限（出向压缩/脱敏可能让上报值小于真实历史）。
            provider_context_tokens = (assistant.usage.input_tokens
                + assistant.usage.cache_read_tokens
                + assistant.usage.cache_write_tokens)
                as usize;
            // P0-3：goals 记账——首次超限置位停止边界注入标记（一次性，防重复注入循环）。
            if let Some(goal) = &agent.goal {
                let newly_exceeded = {
                    let mut g = goal.lock().unwrap();
                    g.note_usage(&assistant.usage)
                };
                if newly_exceeded {
                    agent
                        .goal_pending
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
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
            // 密钥入向还原（须在持久化与工具分发**前**——放在 P1-M transform 之前的单一
            // 真相源点位）：模型回显的 `<secret:N>` 还原为明文，context 持久化、MessageEnd、
            // 工具参数分发全部看到真实字节；下一轮重放时出向 transform 再统一脱敏（闭环）。
            if let Some(s) = &secrets {
                s.restore_assistant_message(&mut assistant);
            }
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
            // P0-自愈：瞬时错误恢复（移植 oh-my-pi `recoverTransientErrorToolTurn`）。
            // 仅当 stop_reason=Error 且 stop_details 为**瞬时流错误类**（经
            // [`agent_core::StopDetails::is_transient_stream_error`] 白名单：stream_read_error /
            // stream_parse_error / stream_interrupted / transient）时才恢复——对齐 oh-my-pi 的
            // 瞬时错误白名单语义。refusal/sensitive 类（[`AssistantMessage::is_provider_refusal`]）
            // 与无 stop_details 的泛化 Error 均不恢复（保守，避免对未知错误形态误执行副作用工具）。
            // 另要求本轮已含**已知工具**且参数完整（非 null）。满足时改写 stop_reason 为 ToolUse，
            // 让循环**执行已完成的工具**而非整轮废弃停止——避免浪费已消耗的 token 与已完成推理。
            // Aborted（主动中止）不在此列，仍走下方占位 + 停止分支。
            // 须在 MessageEnd yield 前改写，保证事件 / 上下文 / 工具分发看到统一终态
            // （ToolUse），后续截断 / Error / 停止边界判定据此自然 fall through 到工具执行。
            // 先 immutable 借用算出判定、释放后再 mutable 改写，规避借用冲突。
            let transient_recoverable = assistant.stop_reason == Some(StopReason::Error)
                && !assistant.is_provider_refusal()
                && assistant
                    .stop_details
                    .as_ref()
                    .is_some_and(agent_core::StopDetails::is_transient_stream_error)
                && assistant
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
                && assistant.content.iter().all(|b| match b {
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => tools.get(name).is_some() && !arguments.is_null(),
                    _ => true,
                });
            if transient_recoverable {
                assistant.stop_reason = Some(StopReason::ToolUse);
                assistant.stop_details = None;
                yield AgentEvent::Say(StatusMessage {
                    text: "检测到瞬时错误但本轮已含完整工具调用，恢复执行已完成的工具（不废弃本轮）".into(),
                    kind: StatusKind::Warning,
                });
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
                    ContentBlock::ToolCall { id, name, arguments, .. } => {
                        Some((id.clone(), name.clone(), arguments.clone()))
                    }
                    _ => None,
                })
                .collect();

            // TTSR：MessageEnd 对工具载荷做快照匹配（matcherDigest）。Always 规则 → 丢弃
            // 整条 assistant（不 append）并注入重试；Never 规则 → 折叠提醒进工具结果
            //（回填时 `take_reminder`）。
            if let Some(t) = &ttsr {
                if let agent_ttsr::ToolOutcome::Abort(names) = t.check_tool_calls(&assistant) {
                    let text = t.render_injection(&names);
                    yield AgentEvent::Say(StatusMessage {
                        text: format!(
                            "检测到工具调用违反规则（{}），丢弃本轮重试",
                            names.join(", ")
                        ),
                        kind: StatusKind::Warning,
                    });
                    context
                        .append(agent_core::AgentMessage::user_text(text))
                        .await;
                    yield AgentEvent::TurnEnd {
                        message: assistant.clone(),
                        tool_results: Vec::new(),
                        will_continue: true,
                    };
                    continue;
                }
            }

            // P0：空回复有界重试（移植 oh-my-pi turn-recovery 的 empty-stop retry，
            // EMPTY_STOP_MAX_RETRIES=3）。普通停止（`Stop`/孤立 `ToolUse`）却无任何可交付
            // 内容（无文本、无工具调用）时：不持久化空 assistant（对齐 omp
            // dropAssistantTurn——避免重试后上下文堆积空消息）、注入 user 角色续跑提醒后
            // 重采样。cancel / deadline 时不重试（保持中止语义）；连续超限则按现状完成
            //（落到下方正常收尾路径）。
            let deadline_exceeded_now =
                deadline_at.is_some_and(|d| std::time::Instant::now() >= d);
            if is_empty_assistant_stop(&assistant)
                && tool_calls.is_empty()
                && !cancel.is_cancelled()
                && !deadline_exceeded_now
            {
                if empty_stop_retries < MAX_EMPTY_STOP_RETRIES {
                    empty_stop_retries += 1;
                    yield AgentEvent::Say(StatusMessage {
                        text: format!(
                            "检测到空回复，重试 {empty_stop_retries}/{MAX_EMPTY_STOP_RETRIES}"
                        ),
                        kind: StatusKind::Warning,
                    });
                    context
                        .append(agent_core::AgentMessage::user_text(format!(
                            "<system-injection>\n\
                             上一轮未产出任何可交付内容（无文本、无工具调用）即停止，任务尚未完成。\
                             请继续：给出用户可见的最终回答，或发起下一个必需的工具调用。\
                             （Attempt #{empty_stop_retries}/{MAX_EMPTY_STOP_RETRIES}）\n\
                             </system-injection>"
                        )))
                        .await;
                    // P1-E：空回复重试，turn 结束 → 继续（本轮 assistant 未入 context）。
                    yield AgentEvent::TurnEnd {
                        message: assistant.clone(),
                        tool_results: std::mem::take(&mut turn_tool_results),
                        will_continue: true,
                    };
                    continue;
                }
                yield AgentEvent::Say(StatusMessage {
                    text: format!("空回复重试达上限（{MAX_EMPTY_STOP_RETRIES}），按完成停止"),
                    kind: StatusKind::Warning,
                });
            } else {
                // 非空轮次重置计数（对齐 omp `#emptyStopRetryCount = 0`）。
                empty_stop_retries = 0;
            }
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
            } else if matches!(assistant.stop_reason, Some(StopReason::Error | StopReason::Aborted))
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
                fire_named_hooks(
                    &hooks,
                    "turn_end",
                    serde_json::json!({
                        "message": assistant,
                        "willContinue": false,
                    }),
                )
                .await;
                yield AgentEvent::TurnEnd {
                    message: assistant.clone(),
                    tool_results: std::mem::take(&mut turn_tool_results),
                    will_continue: false,
                };
                yield AgentEvent::StateChanged(AgentState::Idle);
                summary.success = false;
                for h in &hooks {
                    h.on_event(&HookEvent::Stop { success: false }).await;
                    fire_named_hooks(
                        &hooks,
                        "agent_end",
                        serde_json::json!({ "success": false }),
                    )
                    .await;
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
                // H28：模型已有实质动作 → 解除「提醒待响应」静默（允许后续再提醒）。
                todo_reminder_awaiting = false;
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
                    // followUp：宿主编排的延续消息（停止边界第三类）。steering / aside 均无
                    // 消息时再探测，触发续跑——移植 oh-my-pi `getFollowUpMessages`。区别于
                    // steering（中断）/ aside（被动）：宿主主动「让 agent 继续」（子代理完成等）。
                    let mut pending_followup = false;
                    {
                        let mut guard_followup = followup_rx.lock().await;
                        if let Some(rx) = guard_followup.as_mut() {
                            if let Ok(msg) = rx.try_recv() {
                                context.append(msg).await;
                                pending_followup = true;
                                while let Ok(more) = rx.try_recv() {
                                    context.append(more).await;
                                }
                            }
                        }
                    }
                    if pending_followup {
                        yield AgentEvent::Say(StatusMessage {
                            text: "已注入停止边界 followUp 消息，继续…".into(),
                            kind: StatusKind::Info,
                        });
                        yield AgentEvent::TurnEnd {
                            message: assistant.clone(),
                            tool_results: std::mem::take(&mut turn_tool_results),
                            will_continue: true,
                        };
                        continue;
                    }
                    // P0-3：goals 预算超限（记账置位）。软模式：注入提醒后续跑一轮
                    //（模型收尾）；硬模式：不注入，走下方正常停止（Done 摘要可见用量）。
                    if agent.goal_pending.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        let goal = agent
                            .goal
                            .as_ref()
                            .expect("goal_pending 仅在注入 goal 时置位");
                        let msg = {
                            let g = goal.lock().unwrap();
                            format!(
                                "⚠ 目标预算已用尽（{}）。若确需继续，请先让用户确认或调整预算；否则请尽快收尾并总结。",
                                g.summary()
                            )
                        };
                        if !goal.lock().unwrap().budget.hard_stop {
                            context.append(agent_core::AgentMessage::user_text(msg)).await;
                            yield AgentEvent::Say(StatusMessage {
                                text: "已注入目标预算提醒，继续…".into(),
                                kind: StatusKind::Warning,
                            });
                            yield AgentEvent::TurnEnd {
                                message: assistant.clone(),
                                tool_results: std::mem::take(&mut turn_tool_results),
                                will_continue: true,
                            };
                            continue;
                        }
                    }
                }

                // H29：目标状态变化 → 结构化事件 + 展示文本（双通道同源；工具改动在本
                // 停止边界被一次性捕获），并落盘持久化。
                if let Some(gs) = &goal_state {
                    let change = {
                        let g = gs
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if last_goal_status != Some(g.status) {
                            last_goal_status = Some(g.status);
                            g.snapshot().map(|s| (s.status, Some(s.objective), s.continuations))
                        } else {
                            None
                        }
                    };
                    if let Some((status, objective, continuations)) = change {
                        let ev = agent_core::SessionEvent::GoalUpdated {
                            status: status.clone(),
                            objective,
                            continuations,
                        };
                        context
                            .append(agent_core::AgentMessage::user_text(format!(
                                "<system-reminder>目标状态已变更为 {status}。若目标仍活跃，继续推进；\n若已 complete，请在最终回答中汇报预算使用情况；若已 paused/budget-limited，请收尾。</system-reminder>",
                            )))
                            .await;
                        if let Some(st) = ev.to_status() {
                            yield AgentEvent::Session(ev);
                            yield AgentEvent::Say(st);
                        }
                    }
                }
                // H29：goal 续跑——目标仍活跃时注入 continuation 提示并继续下一轮
                //（`note_continuation` 设上限，防无限循环；`complete`/`budget-limited`
                // 后 `is_active()` 为假，自然停止）。
                if let Some(gs) = &goal_state {
                    let prompt = {
                        let g = gs
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        g.continuation_prompt()
                    };
                    if let Some(prompt) = prompt {
                        let allowed = {
                            let mut g = gs
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            g.note_continuation()
                        };
                        if allowed {
                            // H29：续跑数变化即落盘（跨重启保留进度）。
                            if let Ok(g) = gs.lock() {
                                if let Err(e) = super::save_goal(&workspace.root(), &g) {
                                    tracing::warn!(error = %e, "目标持久化失败");
                                }
                            }
                            context
                                .append(agent_core::AgentMessage::user_text(prompt))
                                .await;
                            yield AgentEvent::Say(StatusMessage {
                                text: "已注入目标续跑提示，继续…".into(),
                                kind: StatusKind::Info,
                            });
                            yield AgentEvent::TurnEnd {
                                message: assistant.clone(),
                                tool_results: std::mem::take(&mut turn_tool_results),
                                will_continue: true,
                            };
                            continue;
                        }
                        yield AgentEvent::Say(StatusMessage {
                            text: "目标续跑达上限，停止（可用 goal resume 继续）".into(),
                            kind: StatusKind::Warning,
                        });
                    }
                }
                // H28：完成提醒——模型在仍有未完成待办时停止 → 注入提醒并续跑
                //（上限 `reminders_max`；同一提醒未获实质动作前保持静默，避免刷屏；
                // 末行是向用户提问则视为等待输入，不打断）。
                // H28：后台作业在途时不提醒——它们完成会经投递/hub 唤醒循环，重复催办会打断
                // 正常的「等结果 → 继续」节奏（移植 omp `hasPendingAsyncWake` 护栏）。
                let pending_async = async_wake
                    .as_ref()
                    .is_some_and(|w| w.has_pending_async());
                if todo_cfg.reminders
                    && !todo_reminder_awaiting
                    && !pending_async
                    && (todo_cfg.reminders_max == 0 || todo_reminder_count < todo_cfg.reminders_max)
                    && let Some(src) = &todo_loop
                {
                    let snapshot = src.snapshot();
                    if let Some(list) = snapshot.render_incomplete()
                        && !is_awaiting_user_answer(&assistant)
                    {
                        todo_reminder_count += 1;
                        todo_reminder_awaiting = true;
                        context
                            .append(agent_core::AgentMessage::user_text(todo_reminder_text(
                                &list,
                                todo_reminder_count,
                                todo_cfg.reminders_max,
                            )))
                            .await;
                        yield AgentEvent::Say(StatusMessage {
                            text: format!(
                                "已注入未完成待办提醒（{}/{}），继续…",
                                todo_reminder_count,
                                if todo_cfg.reminders_max == 0 {
                                    "∞".to_string()
                                } else {
                                    todo_cfg.reminders_max.to_string()
                                }
                            ),
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
                fire_named_hooks(
                    &hooks,
                    "turn_end",
                    serde_json::json!({
                        "message": assistant,
                        "willContinue": false,
                    }),
                )
                .await;
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
                // H20：智能体运行结束（成功路径）。
                fire_named_hooks(
                    &hooks,
                    "agent_end",
                    serde_json::json!({
                        "success": true,
                        "turns": summary.turns,
                        "toolCalls": summary.tool_calls,
                    }),
                )
                .await;
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
                    fire_named_hooks(
                        &hooks,
                        "agent_end",
                        serde_json::json!({ "success": false }),
                    )
                    .await;
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
                .is_none_or(|req| tool_calls.iter().any(|(_, n, _)| n == req));
            // H28：本轮是否触碰待办清单 + 变更类工具数量（`tool_calls` 随后会被消费）。
            let touched_todo_this_turn = tool_calls.iter().any(|(_, n, _)| n == "todo");
            let mutating_this_turn = tool_calls
                .iter()
                .filter(|(_, n, _)| MUTATING_TOOLS.contains(&n.as_str()))
                .count();

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

                // H5 流式守卫（执行侧兜底）：apply_hashline 补丁段头路径（含 fallback
                // path）命中生成文件判定（文件名模式 + 头部内容 marker）→ 回填可恢复
                // 错误、跳过执行。流式早拦只覆盖文件名模式（零 IO）；内容 marker 在此
                // 拦截（对齐 omp assertEditableFile 的完整语义）。
                if stream_guards_cfg.block_auto_generated && name == "apply_hashline" {
                    if let Some(err) = check_generated_edit_args(&args, &workspace).await {
                        context
                            .append(agent_core::AgentMessage::ToolResult(ToolResultMessage {
                                tool_call_id: id,
                                result: ToolResult::Error {
                                    recoverable: true,
                                    message: err.clone(),
                                },
                            }))
                            .await;
                        yield AgentEvent::Say(StatusMessage {
                            text: format!("已拦截对生成文件的编辑：{err}"),
                            kind: StatusKind::Warning,
                        });
                        continue;
                    }
                }

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

            // Sprint2：进程级 pause gate——工具批执行前安全点。审批门禁已过、batch_token
            // 未创建时 park（无在途工具），resume 后继续执行；cancel 优先解除 park。
            if let Some(g) = &pause_gate {
                g.wait_until_resumed(&cancel).await;
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
                conflicts: Some(&conflicts),
                pending_rewrites: Some(&pending_rewrites),
                context: Some(context.as_ref()),
                snapshots: snapshot_store.as_ref(),
                tool_call_id: None,
            };
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
                // TTSR：折叠非打断规则提醒（Never 规则命中时前置 system-reminder 到结果）。
                let effective_result = match &ttsr {
                    Some(t) => t.take_reminder(&id).map_or_else(
                        || result.clone(),
                        |reminder| prepend_reminder(&result, &reminder),
                    ),
                    None => result.clone(),
                };
                let msg = ToolResultMessage {
                    tool_call_id: id.clone(),
                    result: effective_result,
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

            // H5 流式守卫：跨轮同参工具循环检测（对齐 omp LoopGuards.recordTurn——
            // 注入发生在结果回填后、下一轮推理前，模型下一轮即见重定向）。
            if let Some(g) = tool_loop_guard.as_mut() {
                if let Some(detection) = g.record_turn(&assistant, &turn_tool_results) {
                    tracing::warn!(
                        tool = %detection.tool_name,
                        count = detection.count,
                        "cross-turn tool-call loop detected"
                    );
                    yield AgentEvent::Say(StatusMessage {
                        text: format!(
                            "检测到跨轮同参工具循环（{} 连续 {} 次），已注入重定向提示",
                            detection.tool_name, detection.count
                        ),
                        kind: StatusKind::Warning,
                    });
                    context
                        .append(agent_core::AgentMessage::user_text(
                            stream_guards::render_loop_redirect(&detection),
                        ))
                        .await;
                }
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
            // H28：中途对账 nudge——连续多次变更类工具调用仍未触碰 `todo` 时提醒一次
            // （移植 omp `takeMidRunNudge`：阈值 12、每次 run 上限 2）。提醒在工具结果之后、
            // 下一次模型调用之前注入，不打断在途工具。
            if todo_cfg.mid_run_nudge
                && let Some(src) = &todo_loop
            {
                if touched_todo_this_turn {
                    mutations_since_todo = 0;
                } else {
                    mutations_since_todo += mutating_this_turn;
                    if mutations_since_todo >= MID_RUN_NUDGE_THRESHOLD
                        && mid_run_nudges < MID_RUN_NUDGE_MAX
                        && let Some(incomplete) = src.snapshot().render_incomplete()
                    {
                        mutations_since_todo = 0;
                        mid_run_nudges += 1;
                        context
                            .append(agent_core::AgentMessage::user_text(mid_run_nudge_text(
                                &incomplete,
                                mid_run_nudges,
                                MID_RUN_NUDGE_MAX,
                            )))
                            .await;
                        yield AgentEvent::Say(StatusMessage {
                            text: format!(
                                "已注入中途待办对账提醒（{mid_run_nudges}/{MID_RUN_NUDGE_MAX}）"
                            ),
                            kind: StatusKind::Info,
                        });
                    }
                }
            }
            // P1-E：工具执行完毕，turn 结束 → 继续（工具结果已回填，模型再次推理）。
            fire_on_turn_end(&hooks, &assistant, &turn_tool_results, true).await;
            // H20：轮次结束（通知型 hook）——与 `on_turn_end` 编程钩子同点发射。
            fire_named_hooks(
                &hooks,
                "turn_end",
                serde_json::json!({ "message": assistant, "willContinue": true }),
            )
            .await;
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
pub struct PendingTask {
    /// 原始调用顺序（结果回填按此排序，保证确定性）。
    pub(crate) order: usize,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) args: serde_json::Value,
    /// 是否 Exclusive（屏障串行）。
    pub(crate) exclusive: bool,
}

/// 执行单个任务：before 钩子 → 执行 → after 钩子。
///
/// 返回 `(结果, 是否计入 mistakes)`。仅「不可恢复」错误计入 mistakes；可恢复错误
/// （如某文件不存在）回填让模型自我纠正，避免多工具轮次中提前触顶。
pub async fn run_pending_task(
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
    // 契约#2：按工具调用派生上下文——`tool_call_id` 绑定单次调用，工具发 partial 时据此
    // 与 ToolExecutionStart/End 配对；其余字段照抄批次级引用（全为 Copy 引用，构造廉价）。
    // cancel 语义不变：仍指向批次级 token（run 级取消 / steering 中断对批内工具共享生效）。
    let call_tcx = ToolContext {
        workspace: tcx.workspace,
        approval: tcx.approval,
        cancel: tcx.cancel,
        skills: tcx.skills,
        memory: tcx.memory,
        resources: tcx.resources,
        write_effect: tcx.write_effect,
        update_tx: tcx.update_tx,
        conflicts: tcx.conflicts,
        pending_rewrites: tcx.pending_rewrites,
        context: tcx.context,
        snapshots: tcx.snapshots,
        tool_call_id: Some(&task.id),
    };
    let outcome = std::panic::AssertUnwindSafe(tool.execute(task.args.clone(), &call_tcx))
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
                .or_else(|| panic_payload.downcast_ref::<String>().cloned())
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
pub async fn schedule_and_run(
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
pub async fn run_batch(
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

/// 空停止判定（移植 oh-my-pi `isEmptyAssistantStop`）：`Stop` / 孤立 `ToolUse` 停止却无
/// 任何可交付内容——文本块全空白、thinking 无签名（不可回放），且无工具调用。其余停止
/// 原因不算空停止：Length 截断 / 流中断有专属续写路径，Error / Aborted / Pause 各有分支。
/// H5 流式守卫（执行侧）：解析 apply_hashline 参数并对全部涉及路径做生成文件判定
///（文件名模式 + 头部内容 marker，对齐 omp `assertEditableFile`）。
/// 任一路径命中即返回错误文案；无命中 / 参数不可解析（交由工具自身报错）返回 `None`。
async fn check_generated_edit_args(
    args: &serde_json::Value,
    workspace: &agent_core::Workspace,
) -> Option<String> {
    let patch = args.get("patch").and_then(serde_json::Value::as_str)?;
    let mut paths: Vec<String> = match agent_hashline::parse_hashline(patch) {
        Ok(sections) => sections.into_iter().map(|s| s.path).collect(),
        // 解析失败不在此拦（edit_parse_guard 流式预检 / 工具自身错误覆盖）。
        Err(_) => return None,
    };
    if let Some(fallback) = args.get("path").and_then(serde_json::Value::as_str) {
        paths.push(fallback.to_string());
    }
    for path in paths {
        let display = path.clone();
        let resolved = workspace.resolve(std::path::Path::new(&path));
        if let Err(err) =
            agent_tools::generated_guard::assert_editable(&resolved.display().to_string(), &display)
                .await
        {
            return Some(err);
        }
    }
    None
}

/// 空停止判定（移植 oh-my-pi `isEmptyAssistantStop`）：`Stop` / 孤立 `ToolUse` 停止却无
fn is_empty_assistant_stop(msg: &AssistantMessage) -> bool {
    if !matches!(
        msg.stop_reason,
        Some(StopReason::Stop | StopReason::ToolUse)
    ) {
        return false;
    }
    !msg.content.iter().any(|b| match b {
        ContentBlock::Text { text } => !text.trim().is_empty(),
        // thinking 仅在携带签名（可回放校验）时算可交付内容（对齐 omp isActionableContent）。
        ContentBlock::Thinking { signature, .. } => {
            signature.as_deref().is_some_and(|s| !s.trim().is_empty())
        }
        ContentBlock::ToolCall { .. } => true,
    })
}

/// Immediate 模式下轮询 steering 队列的间隔（移植 oh-my-pi `STEERING_INTERRUPT_POLL_MS`）。
///
/// 一次同步的队列长度检查，延迟上界为一个轮询周期。
pub const STEERING_INTERRUPT_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// 边执行工具批次边轮询 steering 队列：每 [`STEERING_INTERRUPT_POLL`] 用
/// [`tokio::sync::mpsc::UnboundedReceiver::len`]（**非消费 peek**）检查一次，命中即
/// `batch_token.cancel()` 中断在途工具；工具批次完成后立即返回（停止轮询）。
///
/// 移植 oh-my-pi `watchSteeringWhileRunning`：仅当 Immediate 模式且 batch 含
/// [`Tool::interruptible`] 工具时由调用方启用。`batch_token` 是 run-cancel 的 child，故中断
/// 只影响本轮在途工具，不传播到 run 级取消（steering 随后在下一轮顶部 / 停止边界被 drain）。
pub async fn poll_and_run<Fut>(
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
                    .map_or(0, tokio::sync::mpsc::UnboundedReceiver::len);
                if pending > 0 {
                    batch_token.cancel();
                    // run 继续被 select 轮询直到完成（工具应观察 batch_token 尽快退出）。
                }
            }
        }
    }
}

/// P1-D：把 agent run 终态 record 到 `invoke_agent` span（OTel `GenAI` 约定）。
///
/// 在每个 `yield AgentEvent::Done(summary)` 前调用，使 `invoke_agent` span 携带 success/
/// turns/usage/duration。因 `async_stream`! 的 Send 约束无法用 enter guard 覆盖整段 run，
/// `invoke_agent` 作为「属性载体 `span」，chat/execute_tool` 子 span 经 `parent` 链关联。
pub fn record_run_end(span: &tracing::Span, summary: &AgentRunSummary, start: std::time::Instant) {
    span.record("gyre.success", summary.success);
    span.record("gyre.turns", summary.turns);
    span.record("gen_ai.usage.input_tokens", summary.usage.input_tokens);
    span.record("gen_ai.usage.output_tokens", summary.usage.output_tokens);
    span.record("gyre.duration", tracing::field::debug(start.elapsed()));
}

/// 流式中断兜底持久化：把已累积的文本增量作为一条被中断的 assistant 消息落盘。
///
/// 仅在流式被取消或异常断开（未到达 [`AssistantEvent::MessageEnd`]）时调用，避免
/// 已显示给用户的回复因未落盘而在 resume 会话时丢失。仅保留 `Text` 块：
/// - `Thinking` 的 signature 在流式中不可靠，持久化后重放可能导致 provider 校验失败；
/// - `ToolCall` 的参数 JSON 可能残缺，会产生悬空工具调用（无对应 tool 结果）。
///   故二者丢弃。`stop_reason` 置 `None` 标记此条为中断产物。
pub async fn persist_interrupted(
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
