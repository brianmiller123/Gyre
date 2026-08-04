//! # `agent --rpc`：NDJSON 行协议模式
//!
//! 供外部语言 / 机器人集成的 stdio 服务面（对标 oh-my-pi `--mode rpc` 的最小集）：
//! stdin 逐行读 JSON 请求，stdout 逐行写 JSON 事件 / 响应。进程内一个 Agent 实例与同一份
//! Context 跨多个 prompt 请求复用（同一会话累积上下文）。协议契约见 `docs/rpc.md`。
//!
//! 实现要点：
//! - 装配镜像 `main()` 的单次任务路径（Provider / Tools / Context / Approval / TTSR /
//!   advisor / 长期记忆），复用 crate 根共享的 `assemble_builtin_tools`、
//!   `optional_context_files`、`load_skill_catalog`、`apply_model_switch` 等辅助。
//! - 取消：每轮经 [`agent::Agent::run_with_cancel`] 派生独立取消令牌，`cancel` 请求 /
//!   SIGINT 命中即中断流式与在途工具（Agent 与 Context 均可继续下一轮）。
//! - 审批：stdin 是协议通道，无法交互审批——一律拒绝（写工具会失败并反映在 `tool_result`）；
//!   需要全自动写权限时以 `--approval-mode yolo` 启动。
//! - 所有日志走 stderr（telemetry 已确保写 stderr），stdout 仅协议行，单行 JSON（无内嵌换行）。

use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{AgentEvent, AskResponse, Usage};
use agent_i18n::t;
use anyhow::{Context as _, Result};
use futures::StreamExt;
use secrecy::ExposeSecret;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::compiled_minimizer;
use crate::{
    ConsolidateHook, apply_model_switch, assemble_builtin_tools, load_skill_catalog,
    optional_context_files,
};

// ──────────────────────────────────────────────────────────────────────────────
// 协议层（纯数据 / 纯函数，可单测）
// ──────────────────────────────────────────────────────────────────────────────

/// 单个 stdin 请求行。
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
struct RpcRequest {
    /// 请求类型：prompt / cancel / ping（未知类型由调用方按 id 回错误）。
    #[serde(rename = "type")]
    ty: String,
    /// 请求 id（响应 / 事件原样回带）。
    id: u64,
    /// prompt 的文本（prompt 请求必填，非空）。
    #[serde(default)]
    text: Option<String>,
    /// 模型别名（可选；非空时 prompt 前尝试切换 profile）。
    #[serde(default)]
    model: Option<String>,
}

/// 单个 stdout 响应行（所有行均为单行 JSON，字段按类型取舍）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct RpcEnvelope {
    #[serde(rename = "type")]
    ty: &'static str,
    id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl RpcEnvelope {
    /// 事件行：`{"type":"event","id":N,"event":{…}}`。
    fn event(id: u64, event: serde_json::Value) -> Self {
        Self {
            ty: "event",
            id,
            event: Some(event),
            ok: None,
            usage: None,
            turns: None,
            error: None,
            message: None,
        }
    }

    /// 成功收尾：`{"type":"done","id":N,"ok":true,"usage":{…},"turns":M}`。
    fn done_ok(id: u64, usage: &Usage, turns: u64) -> Self {
        Self {
            ty: "done",
            id,
            event: None,
            ok: Some(true),
            usage: Some(usage_json(usage)),
            turns: Some(turns),
            error: None,
            message: None,
        }
    }

    /// 失败收尾：`{"type":"done","id":N,"ok":false,"error":"…"}`。
    fn done_err(id: u64, error: impl Into<String>) -> Self {
        Self {
            ty: "done",
            id,
            event: None,
            ok: Some(false),
            usage: None,
            turns: None,
            error: Some(error.into()),
            message: None,
        }
    }

    /// 错误响应（未知类型 / 模型切换失败 / 无效 JSON 等）：`{"type":"error","id":N,"message":"…"}`。
    fn error(id: u64, message: impl Into<String>) -> Self {
        Self {
            ty: "error",
            id,
            event: None,
            ok: None,
            usage: None,
            turns: None,
            error: None,
            message: Some(message.into()),
        }
    }

    /// ping 应答：`{"type":"pong","id":N}`。
    fn pong(id: u64) -> Self {
        Self {
            ty: "pong",
            id,
            event: None,
            ok: None,
            usage: None,
            turns: None,
            error: None,
            message: None,
        }
    }
}

/// 用量对象：`{"input":N,"output":N,"cache_read":N,"cache_write":N,"cost":f}`。
/// cost 非有限值（NaN/Inf）时归零，保证单行 JSON 可序列化。
fn usage_json(u: &Usage) -> serde_json::Value {
    serde_json::json!({
        "input": u.input_tokens,
        "output": u.output_tokens,
        "cache_read": u.cache_read_tokens,
        "cache_write": u.cache_write_tokens,
        "cost": if u.cost_usd.is_finite() { u.cost_usd } else { 0.0 },
    })
}

/// 把 agent 事件映射为 RPC 事件对象；`None` 表示该事件不对外暴露。
///
/// 暴露面（与契约一致）：`text_delta` / `thinking_delta` / `tool_call` / `tool_result` /
/// `status` / `usage`。生命周期事件（StateChanged / Turn* / Message* / ToolExecutionUpdate /
/// Ask / Error / Done 等）不在此暴露——turn 的成败由最终 done 行承载。
fn map_event(ev: &AgentEvent) -> Option<serde_json::Value> {
    match ev {
        AgentEvent::TextDelta(t) => Some(serde_json::json!({"kind": "text_delta", "text": t})),
        AgentEvent::ThinkingDelta(t) => {
            Some(serde_json::json!({"kind": "thinking_delta", "text": t}))
        }
        AgentEvent::Say(s) => Some(serde_json::json!({"kind": "status", "text": s.text})),
        AgentEvent::ToolExecutionStart { name, args, .. } => {
            Some(serde_json::json!({"kind": "tool_call", "name": name, "args": args}))
        }
        AgentEvent::ToolExecutionEnd { name, is_error, .. } => {
            Some(serde_json::json!({"kind": "tool_result", "name": name, "ok": !is_error}))
        }
        AgentEvent::Usage(u) => {
            let mut v = usage_json(u);
            v["kind"] = serde_json::Value::String("usage".into());
            Some(v)
        }
        _ => None,
    }
}

/// 序列化为单行 JSON（文本内换行由 serde 自动转义为 `\n`，保证一行一条消息）。
fn encode_line(env: &RpcEnvelope) -> Result<String> {
    Ok(serde_json::to_string(env).context("序列化 RPC 响应失败")? + "\n")
}

/// 解析一行请求（仅 JSON 语法层；未知 `type` 由调用方按契约回错误）。
fn parse_request(line: &str) -> serde_json::Result<RpcRequest> {
    serde_json::from_str(line)
}

// ──────────────────────────────────────────────────────────────────────────────
// 驱动层（stdin / stdout 行循环）
// ──────────────────────────────────────────────────────────────────────────────

/// 共享 stdin 协议通道（主循环与运行中 turn 共用同一缓冲，保证多字节行不被拆分）。
type StdinReader = Arc<tokio::sync::Mutex<tokio::io::BufReader<tokio::io::Stdin>>>;

/// 从共享协议通道读一行（自动清空缓冲）。
async fn read_line(reader: &StdinReader, buf: &mut String) -> std::io::Result<usize> {
    let mut r = reader.lock().await;
    buf.clear();
    r.read_line(buf).await
}

/// 写一行响应并立即 flush（客户端实时可见）。
async fn write_line(
    out: &mut tokio::io::BufWriter<tokio::io::Stdout>,
    env: &RpcEnvelope,
) -> Result<()> {
    let line = encode_line(env)?;
    out.write_all(line.as_bytes()).await?;
    out.flush().await?;
    Ok(())
}

/// 转发一个 agent 事件为 RPC event 行，并更新 turn 状态（用量 / 轮数 / 成败 / 错误）。
async fn handle_turn_event(
    ev: &AgentEvent,
    id: u64,
    out: &mut tokio::io::BufWriter<tokio::io::Stdout>,
    usage: &mut Usage,
    turns: &mut u64,
    success: &mut Option<bool>,
    last_error: &mut Option<String>,
) -> Result<()> {
    if let Some(v) = map_event(ev) {
        write_line(out, &RpcEnvelope::event(id, v)).await?;
    }
    match ev {
        AgentEvent::Usage(u) => usage.add(u),
        AgentEvent::TurnStart => *turns += 1,
        AgentEvent::Done(summary) => {
            *success = Some(summary.success);
            *usage = summary.usage.clone();
            *turns = summary.turns;
        }
        AgentEvent::Error(e) => *last_error = Some(e.clone()),
        _ => {}
    }
    Ok(())
}

/// 运行一个 prompt 请求对应的完整 turn：事件流式转发为 event 行，最终以 done 行收尾
/// （任何路径——正常结束 / 取消 / 流中断——都保证恰好一条 done）。
///
/// 运行期间并发读 stdin：`cancel` 取消当前轮、`ping` 即时应答、重复 `prompt` 回 busy 错误。
/// 返回 `Ok(true)` 表示进程应退出（stdin 关闭或收到 SIGINT）；`Ok(false)` 继续服务。
async fn run_turn_rpc(
    agent: &agent::Agent,
    id: u64,
    text: &str,
    stdin_reader: &StdinReader,
    out: &mut tokio::io::BufWriter<tokio::io::Stdout>,
    sigint: &mut tokio::sync::mpsc::Receiver<()>,
) -> Result<bool> {
    // 每轮独立取消作用域：cancel 请求 / SIGINT 命中即中断流式与在途工具，Agent 可继续下一轮。
    let cancel = tokio_util::sync::CancellationToken::new();
    let events = agent.run_with_cancel(text, cancel.clone());
    tokio::pin!(events);

    let mut success: Option<bool> = None;
    let mut usage = Usage::default();
    let mut turns: u64 = 0;
    let mut cancelled = false;
    let mut last_error: Option<String> = None;
    let mut stdin_eof = false;
    let mut exit_after = false;
    let mut buf = String::new();

    loop {
        if stdin_eof {
            // 协议通道已关闭：不再并发读，专心排空事件流。
            match events.next().await {
                Some(ev) => {
                    handle_turn_event(
                        &ev,
                        id,
                        out,
                        &mut usage,
                        &mut turns,
                        &mut success,
                        &mut last_error,
                    )
                    .await?;
                }
                None => break,
            }
            continue;
        }
        tokio::select! {
            biased;
            ev = events.next() => match ev {
                Some(ev) => {
                    handle_turn_event(&ev, id, out, &mut usage, &mut turns, &mut success, &mut last_error)
                        .await?;
                }
                None => break,
            },
            n = read_line(stdin_reader, &mut buf) => match n {
                Ok(0) => stdin_eof = true,
                Err(e) => return Err(e.into()),
                Ok(_) => {
                    if buf.trim().is_empty() {
                        continue;
                    }
                    let line = buf.trim().to_string();
                    match parse_request(&line) {
                        Ok(r) if r.ty == "cancel" => {
                            cancel.cancel();
                            cancelled = true;
                        }
                        Ok(r) if r.ty == "ping" => {
                            write_line(out, &RpcEnvelope::pong(r.id)).await?;
                        }
                        Ok(r) if r.ty == "prompt" => {
                            write_line(
                                out,
                                &RpcEnvelope::error(
                                    r.id,
                                    "a turn is already running (send cancel first)",
                                ),
                            )
                            .await?;
                        }
                        Ok(r) => {
                            write_line(out, &RpcEnvelope::error(r.id, "unknown rpc message type"))
                                .await?;
                        }
                        Err(_) => {
                            write_line(out, &RpcEnvelope::error(0, "invalid json request")).await?;
                        }
                    }
                }
            },
            _ = sigint.recv() => {
                // Ctrl-C：取消当前 turn，收尾后退出进程。
                cancel.cancel();
                cancelled = true;
                exit_after = true;
            }
        }
    }

    // 收尾：以一条 done 结束本轮。
    let env = match success {
        Some(s) if s => RpcEnvelope::done_ok(id, &usage, turns),
        Some(_) => {
            RpcEnvelope::done_err(id, last_error.unwrap_or_else(|| "turn failed".to_string()))
        }
        None => {
            let err = if cancelled {
                "cancelled".to_string()
            } else {
                last_error.unwrap_or_else(|| "stream ended unexpectedly".to_string())
            };
            RpcEnvelope::done_err(id, err)
        }
    };
    write_line(out, &env).await?;
    Ok(exit_after || stdin_eof)
}

/// 主入口：装配（镜像 `main()` 的单次任务路径）后进入 NDJSON 行循环。
pub async fn run_rpc(
    cfg: agent_config::Config,
    cwd: PathBuf,
    socks_override: Option<bool>,
) -> Result<()> {
    // ── 装配：模型 profile（默认链）→ Provider → Tools / Context / Prompt ──
    let chain = cfg
        .resolve_chain(None)
        .context("解析默认模型 profile 失败")?;
    let profile = chain[0];
    let api_key: String = profile.resolve_api_key().expose_secret().to_string();
    let model = profile.to_model();
    let fallback_models: Vec<agent_core::Model> =
        chain.iter().skip(1).map(|p| p.to_model()).collect();
    let key_rings: std::collections::HashMap<String, Vec<String>> =
        chain.iter().map(|p| (p.id.clone(), p.key_ring())).collect();

    // SOCKS5 代理控制器（仅影响后端出站请求；开关经 --socks / 配置 / sidecar）。
    let socks5 = super::build_socks5_controller(&cfg, &cwd, socks_override);
    super::print_socks5_status(&socks5);
    // 共享 HTTP 客户端：仅设连接超时 + keepalive，不设整条请求总超时（专供流式 LLM 调用）。
    let client = agent_proxy::build_http_client(socks5, |b| {
        b.tcp_keepalive(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
    })
    .context("构建 HTTP 客户端失败")?;
    let mut registry = agent_llm::ProviderRegistry::new();
    for p in agent_llm::collect_providers(client) {
        registry.register(p);
    }
    let provider: Arc<dyn agent_core::LlmProvider> = agent_llm::wrap_inband_if(
        Arc::new(registry),
        std::env::var("GYRE_INBAND_TOOLS").ok().as_deref(),
    );
    let provider_ctx = agent_core::ProviderCallContext {
        api_key: Some(api_key.clone()),
        base_url: Some(profile.base_url.clone()),
        max_in_flight: None,
    };

    let mode = cfg.agent.mode;
    let prompts = Arc::new(agent_prompt::PromptCatalog::new());
    // 会话持久化：一个进程一个会话（Context 跨 prompt 请求复用，累积上下文）。
    let session_store = agent_context::SessionStore::for_cwd(&cwd);
    let session_id = agent_context::SessionStore::new_id();
    eprintln!("{}", t!("session.label", id = session_id));
    let session_path = session_store.path_for(&session_id);
    let pctx =
        agent_context::PersistentContext::open(prompts.system_with_platform(mode), &session_path)
            .await
            .context("打开持久化上下文失败")?;
    pctx.set_summarizer(Box::new(
        agent_context::compaction::LlmSummaryProvider::new(
            Arc::clone(&provider),
            model.clone(),
            provider_ctx.clone(),
            cfg.compaction.remote_endpoint.clone(),
        ),
    ))
    .await;
    // Shake 归档落盘到 <cwd>/.gyre/artifacts（与 CLI 单次任务一致）。
    pctx.set_shake_sink(Arc::new(agent_context::compaction::DirSink::new(
        cwd.join(".gyre").join("artifacts"),
    )))
    .await;
    let context: Arc<dyn agent_core::ContextManager> = Arc::new(pctx);
    let workspace = Arc::new(agent_core::Workspace::new(cwd.clone()));
    let mcp: Arc<agent_mcp::McpRegistry> = Arc::new(agent_mcp::McpRegistry::load(&cfg.mcp).await);
    let mut optional: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for p in agent_tools::OPTIONAL_TOOL_PROMPTS {
        optional.insert(p.key.to_string(), cfg.tools.effective(p.key, p.default));
    }
    optional.insert(
        "hashline".to_string(),
        cfg.tools.effective("hashline", true),
    );
    optional.insert("pty".to_string(), cfg.tools.effective("pty", false));
    // P1-1：DAP 调试器可选组（[tools.enabled] debug）。
    optional.insert("debug".to_string(), cfg.tools.effective("debug", false));
    // P2：SSH / browser 可选组（[tools.enabled] ssh / browser）。
    optional.insert("ssh".to_string(), cfg.tools.effective("ssh", false));
    optional.insert("browser".to_string(), cfg.tools.effective("browser", false));
    let (mut sub_reg, _) = assemble_builtin_tools(
        &optional,
        cfg.github.enabled,
        cfg.github.allow_write,
        cfg.agent.commands.interceptor.enabled,
        compiled_minimizer(&cfg),
    );
    for t in mcp.tools() {
        sub_reg = sub_reg.with(Box::new(t.clone()));
    }
    let sub_tools: Arc<dyn agent_tools::ToolRegistry> = Arc::new(sub_reg);
    let sub_context_factory: agent::ContextFactory = Arc::new(|| {
        Arc::new(agent_context::InMemoryContext::new(vec![])) as Arc<dyn agent_core::ContextManager>
    });

    // Skill 目录 + 上下文约定（AGENTS.md 等，注入 system prompt）。
    let skill_catalog = Arc::new(load_skill_catalog(&cfg, &cwd).await);
    let foreign_sections: Vec<String> = agent_discovery::discover(&cwd)
        .iter()
        .map(agent_discovery::render_section)
        .collect();
    let mut base_context_files = agent_config::discover_context_files(&cwd);
    base_context_files.extend(foreign_sections);

    // 长期记忆（可选；按 cwd 项目作用域，backend 可切换；RPC 无 LLM 合并钩子）。
    let mut memory: Option<Arc<dyn agent_core::MemoryStore>> = None;
    let mut local_memory: Option<Arc<agent_memory::LocalMemoryStore>> = None;
    if cfg.memory.enabled {
        match cfg.memory.backend {
            agent_config::MemoryBackend::Local => {
                let store = Arc::new(
                    agent_memory::LocalMemoryStore::new(&cwd)
                        .with_mental_models_config(agent_memory::MentalModelsConfig::default()),
                );
                local_memory = Some(Arc::clone(&store));
                memory = Some(store);
            }
            agent_config::MemoryBackend::Structured => {
                memory = Some(Arc::new(
                    agent_memory::StructuredMemoryStore::new(&cwd)
                        .with_mental_models_config(agent_memory::MentalModelsConfig::default())
                        .with_embedder(agent_memory::default_embedder()),
                ));
            }
        }
    }

    // 审批：RPC 模式下 stdin 是协议通道，无法交互审批——一律拒绝（写工具会失败并反映在
    // tool_result 中）；需要全自动写权限时以 `--approval-mode yolo` 启动。
    let prompt_resolver: agent_config::PromptResolver = Arc::new(|ask: agent_core::AskMessage| {
        Box::pin(async move {
            eprintln!("[rpc] 审批被自动拒绝（stdin 为协议通道）：{}", ask.prompt);
            Ok(AskResponse::No)
        })
    });

    // 可变运行时状态（prompt 的 model 字段可覆盖）。
    let max_mistakes = cfg.agent.max_mistakes;
    let context_guard = cfg.agent.context_window_guard;
    let enable_thinking = cfg.agent.enable_thinking;
    let reasoning_budget = cfg.agent.reasoning_budget;
    let auto_thinking = cfg.agent.auto_thinking;
    let auto_thinking_model = cfg.agent.auto_thinking_model.clone();
    let auto_consolidate = cfg.memory.auto_consolidate;
    let subagent_enabled = cfg.subagent.enabled;
    let subagent_max_concurrent = cfg.subagent.max_concurrent;
    let subagent_inherit = cfg.subagent.inherit_parent;
    let subagent_max_output_override = cfg.subagent.max_output_tokens;
    let profile_temperature = profile.temperature;
    let mut github_enabled = cfg.github.enabled;
    let mut github_allow_write = cfg.github.allow_write;
    let supervisor = agent_supervisor::Supervisor::new();

    // P0-3：goals 目标预算共享状态（跨 Agent 重建保持；与 main() 单次任务路径一致）。
    // 预算为空（两者均为 0）时不注入，行为与未配置完全一致。
    let goal_state: Option<Arc<std::sync::Mutex<agent::GoalState>>> =
        if cfg.goals.token_budget > 0 || cfg.goals.time_budget_secs > 0 {
            Some(Arc::new(std::sync::Mutex::new(agent::GoalState::new(
                agent::GoalBudget {
                    token_budget: cfg.goals.token_budget,
                    time_budget: std::time::Duration::from_secs(cfg.goals.time_budget_secs),
                    hard_stop: cfg.goals.hard_stop,
                },
            ))))
        } else {
            None
        };

    let mut current_mode = mode;
    let mut current_model = model;
    let mut current_api_key = api_key;
    let mut current_base_url = profile.base_url.clone();
    let mut current_max_output = profile.max_output_tokens.unwrap_or(4096);
    let mut current_provider_ctx = provider_ctx;

    // Agent 构造闭包：与 main() 的单次任务路径一致（含 TTSR / advisor / 长期记忆 / 子 Agent）。
    #[allow(clippy::too_many_arguments)]
    let build_agent = |mode: agent_core::Mode,
                       model: agent_core::Model,
                       provider_ctx: agent_core::ProviderCallContext,
                       max_output: usize,
                       context: Arc<dyn agent_core::ContextManager>,
                       github_enabled: bool,
                       github_allow_write: bool,
                       optional: &std::collections::HashMap<String, bool>|
     -> agent::Agent {
        let mut agent_cfg = cfg.agent.clone();
        agent_cfg.mode = mode;
        let rules = agent_config::RulesEngine::new(Arc::new(agent_cfg))
            .with_workspace_root(Some(workspace.root().to_path_buf()));
        let approval: Arc<dyn agent_core::ApprovalPolicy> = Arc::new(
            agent_config::RulesApprovalPolicy::new(rules, Arc::clone(&prompt_resolver)),
        );
        let sub_temperature = if subagent_inherit {
            profile_temperature
        } else {
            None
        };
        let sub_thinking = if subagent_inherit && enable_thinking {
            Some(agent_core::ThinkingConfig::new(
                reasoning_budget.unwrap_or(16_000),
            ))
        } else {
            None
        };
        let sub_max_output = subagent_max_output_override.unwrap_or(max_output);
        let (mut tool_registry, lsp_pool) = assemble_builtin_tools(
            optional,
            github_enabled,
            github_allow_write,
            cfg.agent.commands.interceptor.enabled,
            compiled_minimizer(&cfg),
        );
        for t in mcp.tools() {
            tool_registry = tool_registry.with(Box::new(t.clone()));
        }
        if subagent_enabled {
            let task_tool = agent::TaskTool::new(
                Arc::clone(&provider),
                Arc::clone(&sub_tools),
                Arc::clone(&prompts),
                Arc::clone(&workspace),
                model.clone(),
                provider_ctx.clone(),
                mode,
                max_mistakes,
                context_guard,
                sub_max_output,
                Arc::clone(&sub_context_factory),
                sub_temperature,
                sub_thinking,
                subagent_max_concurrent,
            )
            .with_supervisor(supervisor.clone())
            .with_approval(Arc::clone(&approval));
            tool_registry = tool_registry.with(Box::new(task_tool));
        }
        let tools: Arc<dyn agent_tools::ToolRegistry> = Arc::new(tool_registry);

        // TTSR 流规则：发现 `<cwd>/.gyre/rules/*.md` 并装配协调器（与 main() 一致）。
        let ttsr = if cfg.ttsr.enabled.unwrap_or(true) {
            let rules = agent_ttsr::discover_rules(&workspace.root().join(".gyre/rules"));
            if rules.is_empty() {
                None
            } else {
                let names: Vec<&str> = rules.iter().map(|r| r.name.as_str()).collect();
                eprintln!("已加载 {} 条 TTSR 规则: {}", rules.len(), names.join(", "));
                Some(Arc::new(agent_ttsr::TtsrCoordinator::new(
                    agent_ttsr::TtsrConfig {
                        disabled_rules: cfg.ttsr.disabled_rules.clone(),
                        ..Default::default()
                    },
                    rules,
                )))
            }
        } else {
            None
        };

        // P1-L：advisor 独立评审（env `GYRE_ADVISOR=1` 启用；与 main() 一致）。
        let advisor = if std::env::var("GYRE_ADVISOR").is_ok_and(|v| v == "1") {
            let watchdog = agent_advisor::render_watchdog(&workspace.root());
            let adv = agent_advisor::Advisor::new(Arc::clone(&provider), model.clone());
            let adv = if watchdog.is_empty() {
                adv
            } else {
                adv.with_watchdog(watchdog)
            };
            eprintln!("已启用 advisor（每 {} 轮独立评审）", 4);
            Some(Arc::new(adv))
        } else {
            None
        };

        let builder = agent::Agent::builder(model.clone())
            .steering()
            .provider(Arc::clone(&provider))
            .tools(tools)
            .context(Arc::clone(&context))
            .prompts(Arc::clone(&prompts))
            .approval(Arc::clone(&approval))
            .workspace(Arc::clone(&workspace))
            .provider_ctx(provider_ctx.clone())
            .fallbacks(fallback_models.clone())
            .key_rings(key_rings.clone())
            .mode(mode)
            .ttsr(ttsr)
            .advisor(advisor)
            .max_output_tokens(model.max_output_tokens)
            .max_mistakes(max_mistakes)
            .context_guard(context_guard)
            .catalog(Arc::clone(&skill_catalog))
            .context_files(optional_context_files(
                &base_context_files,
                optional,
                github_enabled,
            ))
            .resources(Arc::clone(&mcp) as Arc<dyn agent_core::ResourceResolver>);
        // P2：压缩后端——snapcompact 要求视觉模型（与 main() 一致；每次重建按当前 model 判断）。
        let compaction_backend = {
            let backend = agent_config::parse_compaction_backend(&cfg.compaction.backend);
            if backend == agent_core::CompactionBackend::Snapcompact {
                let vision = cfg.compaction.vision_models.is_empty()
                    || cfg
                        .compaction
                        .vision_models
                        .iter()
                        .any(|p| agent_config::wildcard_match(p, &model.id));
                if vision {
                    backend
                } else {
                    agent_core::CompactionBackend::Summarize
                }
            } else {
                backend
            }
        };
        let builder = builder
            .compaction_backend(compaction_backend)
            .compaction_max_frames(cfg.compaction.max_frames);
        // P0-3：goals 预算注入（共享状态，与 main() 一致）。
        let builder = if let Some(gs) = &goal_state {
            builder.goals_state(Arc::clone(gs))
        } else {
            builder
        };
        // 编辑后 LSP writethrough：lsp 启用（lsp_pool 为 Some）且 edit 开启时注入（与 main() 一致）。
        let builder = if lsp_pool.is_some()
            && (cfg.agent.tools.edit.format_on_write || cfg.agent.tools.edit.diagnostics_on_write)
        {
            match &lsp_pool {
                Some(pool) => {
                    builder.write_effect(std::sync::Arc::new(agent_tools::LspWriteEffect::new(
                        workspace.root().to_path_buf(),
                        std::sync::Arc::clone(pool),
                        cfg.agent.tools.edit.format_on_write,
                        cfg.agent.tools.edit.diagnostics_on_write,
                        cfg.agent.tools.edit.diagnostics_deduplicate,
                    ))
                        as std::sync::Arc<dyn agent_core::WriteEffect>)
                }
                None => builder, // 理论不可达：外层已检查 lsp_pool.is_some()
            }
        } else {
            builder
        };
        let builder = if let Some(m) = &memory {
            builder.memory(Arc::clone(m))
        } else {
            builder
        };
        // LLM 合并钩子仅 local 后端有（structured 逐条积累，无合并语义）。
        let builder = if let Some(m) = &local_memory {
            let hook = ConsolidateHook {
                store: Arc::clone(m),
                provider: Arc::clone(&provider),
                model: model.clone(),
                provider_ctx: provider_ctx.clone(),
                auto_consolidate,
            };
            builder.hooks(vec![Arc::new(hook) as Arc<dyn agent_core::Hook>])
        } else {
            builder
        };
        if enable_thinking {
            let static_cfg = agent_core::ThinkingConfig::new(reasoning_budget.unwrap_or(16_000));
            if auto_thinking {
                if let Some(tiny_id) = auto_thinking_model.clone() {
                    let mut tiny = model.clone();
                    tiny.id = tiny_id;
                    let classifier = Arc::new(agent_llm::LlmThinkingClassifier::new(
                        Arc::clone(&provider),
                        tiny,
                        provider_ctx.clone(),
                    ));
                    builder
                        .thinking_policy(agent_core::ThinkingPolicy::auto(classifier, static_cfg))
                        .build()
                } else {
                    tracing::warn!(
                        "auto_thinking 已启用但 auto_thinking_model 未配置，回退静态思考预算"
                    );
                    builder.thinking(static_cfg).build()
                }
            } else {
                builder.thinking(static_cfg).build()
            }
        } else {
            builder.build()
        }
    };

    // ── SIGINT：持久注册句柄，避免信号在 select 轮询间隙丢失 ──
    let (sigint_tx, mut sigint_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = sigint_tx.send(()).await;
    });

    // ── 主循环：逐行读 stdin 请求，逐行写 stdout 响应 ──
    let stdin_reader: StdinReader = Arc::new(tokio::sync::Mutex::new(tokio::io::BufReader::new(
        tokio::io::stdin(),
    )));
    let mut out = tokio::io::BufWriter::new(tokio::io::stdout());
    let mut buf = String::new();
    let mut agent: Option<agent::Agent> = None;

    loop {
        tokio::select! {
            biased;
            n = read_line(&stdin_reader, &mut buf) => {
                let n = n?;
                if n == 0 {
                    break; // stdin EOF → 正常退出
                }
                if buf.trim().is_empty() {
                    continue;
                }
            }
            _ = sigint_rx.recv() => break, // 空闲时 Ctrl-C → 直接退出
        }
        let line = buf.trim().to_string();
        let req = match parse_request(&line) {
            Ok(r) => r,
            Err(_) => {
                write_line(&mut out, &RpcEnvelope::error(0, "invalid json request")).await?;
                continue;
            }
        };
        match req.ty.as_str() {
            "ping" => {
                write_line(&mut out, &RpcEnvelope::pong(req.id)).await?;
            }
            "cancel" => {
                // 空闲时无 turn 可取消 → 错误响应（运行中由 turn 循环处理）。
                write_line(&mut out, &RpcEnvelope::error(req.id, "no running turn")).await?;
            }
            "prompt" => {
                let Some(text) = req.text.clone().filter(|t| !t.is_empty()) else {
                    write_line(
                        &mut out,
                        &RpcEnvelope::error(req.id, "prompt requires non-empty text"),
                    )
                    .await?;
                    continue;
                };
                // 可选模型切换（失败 → error 响应，不执行本轮；成功 → 重建 Agent）。
                if let Some(alias) = req.model.clone().filter(|m| !m.is_empty()) {
                    if !apply_model_switch(
                        &alias,
                        &cfg,
                        &mut current_api_key,
                        &mut current_base_url,
                        &mut current_max_output,
                        &mut current_model,
                        &mut current_provider_ctx,
                    ) {
                        write_line(
                            &mut out,
                            &RpcEnvelope::error(req.id, format!("model switch failed: {alias}")),
                        )
                        .await?;
                        continue;
                    }
                    agent = Some(build_agent(
                        current_mode,
                        current_model.clone(),
                        current_provider_ctx.clone(),
                        current_max_output,
                        Arc::clone(&context),
                        github_enabled,
                        github_allow_write,
                        &optional,
                    ));
                } else if agent.is_none() {
                    // 惰性构建：首次 prompt 前，模型/模式取配置默认。
                    agent = Some(build_agent(
                        current_mode,
                        current_model.clone(),
                        current_provider_ctx.clone(),
                        current_max_output,
                        Arc::clone(&context),
                        github_enabled,
                        github_allow_write,
                        &optional,
                    ));
                }
                let Some(a) = agent.as_ref() else {
                    anyhow::bail!("构建 agent 失败（不可达）");
                };
                if run_turn_rpc(a, req.id, &text, &stdin_reader, &mut out, &mut sigint_rx).await? {
                    break; // stdin EOF 或 SIGINT：本轮已收尾，退出进程
                }
            }
            _ => {
                write_line(
                    &mut out,
                    &RpcEnvelope::error(req.id, "unknown rpc message type"),
                )
                .await?;
            }
        }
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// 单测：协议编解码纯函数
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AgentRunSummary, StatusKind, ToolResult};

    fn usage() -> Usage {
        Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
            cost_usd: 0.5,
        }
    }

    #[test]
    fn parses_prompt_request_with_optional_model() {
        let r = parse_request(r#"{"type":"prompt","id":7,"text":"hello","model":"ds"}"#).unwrap();
        assert_eq!(r.ty, "prompt");
        assert_eq!(r.id, 7);
        assert_eq!(r.text.as_deref(), Some("hello"));
        assert_eq!(r.model.as_deref(), Some("ds"));
    }

    #[test]
    fn parses_request_without_optional_fields() {
        let r = parse_request(r#"{"type":"cancel","id":2}"#).unwrap();
        assert_eq!(r.ty, "cancel");
        assert_eq!(r.id, 2);
        assert_eq!(r.text, None);
        assert_eq!(r.model, None);
    }

    #[test]
    fn parses_unknown_type_keeping_id() {
        let r = parse_request(r#"{"type":"explode","id":9}"#).unwrap();
        assert_eq!(r.ty, "explode");
        assert_eq!(r.id, 9);
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_request("not json").is_err());
        assert!(parse_request(r#"{"type":"ping"}"#.trim_end_matches('}')).is_err());
    }

    #[test]
    fn encode_line_is_single_line_json() {
        let env = RpcEnvelope::event(
            1,
            serde_json::json!({"kind": "text_delta", "text": "a\nb\t\"c\""}),
        );
        let line = encode_line(&env).unwrap();
        // 恰好一个行尾换行；文本内换行已被转义。
        assert_eq!(line.chars().filter(|&c| c == '\n').count(), 1);
        assert!(line.ends_with('\n'));
        // 可回读，且结构与事件一致。
        let back: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(back["type"], "event");
        assert_eq!(back["id"], 1);
        assert_eq!(back["event"]["kind"], "text_delta");
        assert_eq!(back["event"]["text"], "a\nb\t\"c\"");
    }

    #[test]
    fn envelope_shapes_match_contract() {
        // done ok：带 usage 与 turns，无 error/message。
        let v: serde_json::Value =
            serde_json::to_value(RpcEnvelope::done_ok(1, &usage(), 3)).unwrap();
        assert_eq!(v["type"], "done");
        assert_eq!(v["id"], 1);
        assert_eq!(v["ok"], true);
        assert_eq!(v["turns"], 3);
        assert_eq!(v["usage"]["input"], 1);
        assert_eq!(v["usage"]["output"], 2);
        assert_eq!(v["usage"]["cache_read"], 3);
        assert_eq!(v["usage"]["cache_write"], 4);
        assert_eq!(v["usage"]["cost"], 0.5);
        assert!(v.get("error").is_none());
        assert!(v.get("message").is_none());
        assert!(v.get("event").is_none());

        // done err：ok=false + error，无 usage/turns。
        let v: serde_json::Value =
            serde_json::to_value(RpcEnvelope::done_err(1, "cancelled")).unwrap();
        assert_eq!(v["type"], "done");
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "cancelled");
        assert!(v.get("usage").is_none());
        assert!(v.get("turns").is_none());

        // error 响应：message 字段。
        let v: serde_json::Value =
            serde_json::to_value(RpcEnvelope::error(2, "unknown rpc message type")).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["id"], 2);
        assert_eq!(v["message"], "unknown rpc message type");

        // pong。
        let v: serde_json::Value = serde_json::to_value(RpcEnvelope::pong(3)).unwrap();
        assert_eq!(v["type"], "pong");
        assert_eq!(v["id"], 3);
    }

    #[test]
    fn maps_text_and_thinking_deltas() {
        let v = map_event(&AgentEvent::TextDelta("hi".into())).unwrap();
        assert_eq!(v["kind"], "text_delta");
        assert_eq!(v["text"], "hi");
        let v = map_event(&AgentEvent::ThinkingDelta("t".into())).unwrap();
        assert_eq!(v["kind"], "thinking_delta");
        assert_eq!(v["text"], "t");
    }

    #[test]
    fn maps_status_and_tool_lifecycle() {
        let v = map_event(&AgentEvent::Say(agent_core::StatusMessage {
            text: "working".into(),
            kind: StatusKind::Info,
        }))
        .unwrap();
        assert_eq!(v["kind"], "status");
        assert_eq!(v["text"], "working");

        let v = map_event(&AgentEvent::ToolExecutionStart {
            tool_call_id: "1".into(),
            name: "run_command".into(),
            args: serde_json::json!({"cmd": "ls"}),
        })
        .unwrap();
        assert_eq!(v["kind"], "tool_call");
        assert_eq!(v["name"], "run_command");
        assert_eq!(v["args"]["cmd"], "ls");

        let ok = map_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "1".into(),
            name: "run_command".into(),
            result: ToolResult::text("done"),
            is_error: false,
        })
        .unwrap();
        assert_eq!(ok["kind"], "tool_result");
        assert_eq!(ok["name"], "run_command");
        assert_eq!(ok["ok"], true);

        let err = map_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "1".into(),
            name: "run_command".into(),
            result: ToolResult::text("boom"),
            is_error: true,
        })
        .unwrap();
        assert_eq!(err["ok"], false);
    }

    #[test]
    fn maps_usage_with_all_five_fields() {
        let v = map_event(&AgentEvent::Usage(usage())).unwrap();
        assert_eq!(v["kind"], "usage");
        assert_eq!(v["input"], 1);
        assert_eq!(v["output"], 2);
        assert_eq!(v["cache_read"], 3);
        assert_eq!(v["cache_write"], 4);
        assert_eq!(v["cost"], 0.5);
    }

    #[test]
    fn hides_lifecycle_events() {
        assert!(map_event(&AgentEvent::TurnStart).is_none());
        assert!(map_event(&AgentEvent::Done(AgentRunSummary::default())).is_none());
        assert!(map_event(&AgentEvent::Error("x".into())).is_none());
        assert!(map_event(&AgentEvent::StateChanged(agent_core::AgentState::Idle)).is_none());
    }

    #[test]
    fn sanitizes_non_finite_cost() {
        let mut u = usage();
        u.cost_usd = f64::NAN;
        let v = usage_json(&u);
        assert_eq!(v["cost"], 0.0);
    }
}
