//! # agent CLI
//!
//! 二进制入口：加载配置（TOML）→ 装配 Provider/Tools/Context/Approval → 驱动 [`agent::Agent`]。
//! 体现 Ports & Adapters：所有具体实现在此装配，智能体循环只依赖 trait。

mod agents_view;
mod markdown;
mod repl;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{
    AgentEvent, AskResponse, CompactionStrategy, Hook, HookEvent, MemoryStore, Model, StatusKind,
    Usage,
};
use anyhow::{Context as _, Result};
use clap::Parser;
use futures::StreamExt;
use rustyline::Editor;
use rustyline::history::DefaultHistory;
use secrecy::ExposeSecret;

/// i18n 取词宏（编译期内嵌 locale 目录，运行期按系统/配置语言激活）。
use agent_i18n::t;

use repl::{
    CommandContext, CommandOutcome, ReplHelper, all_command_names, handle_command, model_choices,
    session_history_lines,
};

/// 高性能 Rust 智能体 CLI。
#[derive(Parser, Debug)]
#[command(
    name = "agent",
    version,
    about = "高性能 Rust 智能体（融合 Zoo-Code + oh-my-pi）"
)]
struct Cli {
    /// 任务文本（省略则从 stdin 读取一行）。
    task: Option<String>,
    /// 模型别名（对应 config 的 `[[models]] alias`）。
    #[arg(long)]
    model: Option<String>,
    /// 工作目录（默认当前目录）。
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// 审批模式覆盖：always-ask / write / yolo。
    #[arg(long)]
    approval_mode: Option<String>,
    /// 界面语言覆盖（en / zh / ru / ja）；省略则用 config 或系统语言。
    #[arg(long)]
    lang: Option<String>,
    /// 启动 Web 服务（HTTP + WebSocket + 前端），而非运行单次任务。
    /// 不带值时监听配置文件 `[server].bind` 地址；带值则覆盖监听地址（如 `--serve 0.0.0.0:80`）。
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    serve: Option<String>,
    /// ACP（Agent Client Protocol）。
    ///
    /// - 单独使用（不带 `--serve`）：以纯 stdio 模式运行 ACP（供编辑器作为子进程调用；
    ///   stdin 读 JSON-RPC，stdout 写事件，无需 HTTP 端口）。最高优先级，不启动 HTTP。
    /// - 与 `--serve` 配合：额外启用 HTTP+SSE 端点（亦受 `[acp].enabled` 控制）。
    #[arg(long)]
    acp: bool,
    /// 恢复历史会话（会话 id；用 --list-sessions 查看）。
    #[arg(long)]
    resume: Option<String>,
    /// 列出历史会话后退出。
    #[arg(long)]
    list_sessions: bool,
    /// 复制会话为新 id 后继续（fork）。
    #[arg(long)]
    fork: Option<String>,
    /// OpenTelemetry OTLP 端点（如 http://localhost:4317）；省略则仅本地日志。
    #[arg(long)]
    otlp: Option<String>,
}

/// 启动 Web 服务。`acp` 为 true（或配置启用）时合并 ACP HTTP+SSE 路由。
async fn run_server(cfg: agent_config::Config, cwd: PathBuf, acp: bool) -> Result<()> {
    let bind = cfg.server.bind.clone();
    let acp_enabled = acp || cfg.acp.enabled;
    // 服务端共享 HTTP 客户端：仅设连接超时 + keepalive，不设整条请求总超时。该客户端
    // 专供流式 LLM 调用——总超时会切断仍在正常输出的慢速长流（收不到 `data: [DONE]`
    // 终止帧而误判「未收到结束标记」）；真正的「上游挂起」由各 SSE 适配器的按 chunk
    // 空闲读超时（agent_llm::STREAM_IDLE_TIMEOUT）兜底，连接阶段挂起由 connect_timeout 兜底。
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .context(t!("error.build_http"))?;
    let state = agent_server::SessionManager::new(Arc::new(cfg), http, Arc::new(cwd));
    // ACP 路由在组装层合并（agent-acp 依赖 agent-server，故不能在 server crate 内 merge，
    // 否则循环依赖）。
    let app = if acp_enabled {
        agent_server::app(state.clone()).merge(agent_acp::acp_routes(state))
    } else {
        agent_server::app(state)
    };
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .context(t!("error.bind_failed"))?;
    eprintln!("{}", t!("server.started", bind = bind));
    eprintln!("{}", t!("server.route.sessions"));
    eprintln!("{}", t!("server.route.ws"));
    eprintln!("{}", t!("server.route.static"));
    if acp_enabled {
        eprintln!("  • ACP  JSON-RPC  POST /acp/rpc");
        eprintln!("           SSE 事件  GET  /acp/sse/{{session_id}}");
    }
    axum::serve(listener, app).await?;
    Ok(())
}

/// 纯 stdio 模式运行 ACP（编辑器作为子进程调用：stdin 读 JSON-RPC，stdout 写事件）。
async fn run_acp_stdio(cfg: agent_config::Config, cwd: PathBuf) -> Result<()> {
    // 流式 LLM 客户端：不设整条请求总超时（会误杀慢速长流），上游静默由按 chunk 空闲
    // 读超时（agent_llm::STREAM_IDLE_TIMEOUT）兜底，连接阶段挂起由 connect_timeout 兜底。
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .context(t!("error.build_http"))?;
    let state = agent_server::SessionManager::new(Arc::new(cfg), http, Arc::new(cwd));
    agent_acp::run_stdio(state)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // i18n：先用系统语言初始化（配置加载前的报错也能本地化）。
    agent_i18n::init(None);

    // 遥测初始化：--otlp 启用 OTLP span 导出，否则仅本地日志
    let _telemetry_guard =
        agent_telemetry::init(cli.otlp.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let cwd = cli
        .cwd
        .unwrap_or_else(|| std::env::current_dir().expect("无法获取当前目录"));

    // 1. 加载配置（分层 TOML）
    let mut cfg = agent_config::Config::load(&cwd).context(t!("error.load_config"))?;
    // 配置加载后：按优先级 --lang > config > 系统语言 选择激活语言。
    agent_i18n::init(cli.lang.as_deref().or(cfg.language.as_deref()));
    // fuzzy 配置 → 全局覆盖（fuzzy_match 经 resolve_opts 读取；非 auto 时覆盖 env）。
    {
        let mut opts = agent_tools::FuzzyOpts::from_env();
        match cfg.agent.tools.edit.fuzzy.as_str() {
            "on" | "1" | "true" => opts.enabled = true,
            "off" | "0" | "false" => opts.enabled = false,
            _ => {} // "auto"：保留 env
        }
        opts.threshold = cfg.agent.tools.edit.fuzzy_threshold;
        agent_tools::set_fuzzy_opts(opts);
    }
    if let Some(mode) = &cli.approval_mode {
        cfg.agent.approval_mode = match mode.as_str() {
            "yolo" => agent_core::ApprovalMode::Yolo,
            "write" => agent_core::ApprovalMode::Write,
            _ => agent_core::ApprovalMode::AlwaysAsk,
        };
    }

    // 纯 stdio ACP 模式（--acp 且未指定 --serve）：编辑器作为子进程调用，不启动 HTTP。
    // 与 `--serve --acp`（HTTP+SSE）区分——stdio 优先，仅在没有 serve 时触发。
    if cli.acp && cli.serve.is_none() {
        return run_acp_stdio(cfg, cwd).await;
    }

    // Web 服务模式
    if let Some(serve_addr) = &cli.serve {
        // --serve 不带值 → 沿用配置文件 `[server].bind`；
        // 带完整地址（如 `0.0.0.0:80`）→ 直接覆盖；
        // 仅端口形式（如 `:80`）→ 取配置 bind 的 host 部分补全（`:80` → `127.0.0.1:80`）。
        if !serve_addr.is_empty() {
            let resolved = if let Some(port) = serve_addr.strip_prefix(':') {
                let host = cfg
                    .server
                    .bind
                    .rsplit_once(':')
                    .map(|(h, _)| h)
                    .unwrap_or("127.0.0.1");
                format!("{host}:{port}")
            } else {
                serve_addr.clone()
            };
            cfg.server.bind = resolved;
        }
        return run_server(cfg, cwd, cli.acp).await;
    }

    // 会话持久化：按 cwd 项目隔离（/sessions 只列出当前项目的历史会话）
    let session_store = agent_context::SessionStore::for_cwd(&cwd);
    if cli.list_sessions {
        let sessions = session_store.list();
        if sessions.is_empty() {
            eprintln!("{}", t!("session.none_history"));
        } else {
            for s in &sessions {
                let ts = s
                    .mtime
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                eprintln!(
                    "{}",
                    t!("session.list_entry", id = s.id, bytes = s.bytes, ts = ts)
                );
            }
        }
        return Ok(());
    }
    let mut session_id = if let Some(id) = &cli.resume {
        id.clone()
    } else if let Some(src) = &cli.fork {
        session_store.fork(src).context(t!("session.fork_failed"))?
    } else {
        agent_context::SessionStore::new_id()
    };
    eprintln!("{}", t!("session.label", id = session_id));

    // 2. 解析模型 profile
    let profile = cfg
        .resolve_model(cli.model.as_deref())
        .context(t!("error.resolve_model"))?;
    let api_key: String = profile.resolve_api_key().expose_secret().to_string();
    let model = agent_core::Model {
        id: profile.id.clone(),
        provider: "openai-compatible".into(),
        api: profile.api,
        max_input_tokens: profile.effective_max_input_tokens(),
        max_output_tokens: profile.max_output_tokens.unwrap_or(4096),
        supports_tools: true,
        supports_streaming: true,
        supports_thinking: false,
        extra_body: profile.extra_body.clone(),
    };

    // 3. 装配 Provider（registry + OpenAI Chat Completions 适配器）
    // 共享 HTTP 客户端：仅设连接超时 + keepalive，不设整条请求总超时——该客户端专供流式
    // LLM 调用，总超时会切断仍在正常输出的慢速长流（收不到终止帧而误判「未收到结束标记」）；
    // 真正的「上游挂起」由各 SSE 适配器的按 chunk 空闲读超时（STREAM_IDLE_TIMEOUT）兜底。
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .context(t!("error.build_http"))?;
    let mut registry = agent_llm::ProviderRegistry::new();
    for p in agent_llm::collect_providers(client) {
        registry.register(p);
    }
    // 环境变量 opt-in in-band 工具调用（GYRE_INBAND_TOOLS=1）：对 function-calling 不稳的
    // 模型（GLM/DeepSeek 等）改用「提示词 + 文本协议」调用工具。
    let provider: Arc<dyn agent_core::LlmProvider> = agent_llm::wrap_inband_if(
        Arc::new(registry),
        std::env::var("GYRE_INBAND_TOOLS").ok().as_deref(),
    );

    let provider_ctx = agent_core::ProviderCallContext {
        api_key: Some(api_key.clone()),
        base_url: Some(profile.base_url.clone()),
        max_in_flight: None,
    };

    // 4. 装配 Tools / Context / Prompt
    let mode = cfg.agent.mode;
    let prompts = Arc::new(agent_prompt::PromptCatalog::new());
    let session_path = session_store.path_for(&session_id);
    let pctx =
        agent_context::PersistentContext::open(prompts.system_with_platform(mode), &session_path)
            .await
            .context(t!("error.open_persistence"))?;
    pctx.set_summarizer(Box::new(
        agent_context::compaction::LlmSummaryProvider::new(
            Arc::clone(&provider),
            model.clone(),
            provider_ctx.clone(),
        ),
    ))
    .await;
    // Shake 归档落盘到 <cwd>/.gyre/artifacts，使被压缩的大块可经 read_file artifact:// 回读。
    pctx.set_shake_sink(Arc::new(agent_context::compaction::DirSink::new(
        cwd.join(".gyre").join("artifacts"),
    )))
    .await;
    let mut context: Arc<dyn agent_core::ContextManager> = Arc::new(pctx);
    let workspace = Arc::new(agent_core::Workspace::new(cwd.clone()));
    // MCP 注册表（多 server 工具；包 Arc 供 build_agent 与 /mcp 共享）
    let mcp: Arc<agent_mcp::McpRegistry> = Arc::new(agent_mcp::McpRegistry::load(&cfg.mcp).await);
    // 可选工具开关运行时快照：初值取自配置 [tools].enabled（覆盖各组默认 false）。
    // 后续可由 `/tools <key> on|off` 动态切换；切换后重建 Agent 以反映新工具集与提示词。
    let mut optional: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for p in agent_tools::OPTIONAL_TOOL_PROMPTS {
        optional.insert(p.key.to_string(), cfg.tools.effective(p.key, p.default));
    }
    // hashline 默认启用（主推编辑格式）；用户可在 [tools].enabled 显式置 false 关闭。
    optional.insert(
        "hashline".to_string(),
        cfg.tools.effective("hashline", true),
    );
    optional.insert("pty".to_string(), cfg.tools.effective("pty", false));
    // 子 Agent 工具集（按启用态装配的可选工具 + MCP，不含 task 以防递归；与模型无关，构建一次）
    let (mut sub_reg, _) =
        assemble_builtin_tools(&optional, cfg.github.enabled, cfg.github.allow_write);
    for t in mcp.tools() {
        sub_reg = sub_reg.with(Box::new(t.clone()));
    }
    let sub_tools: Arc<dyn agent_tools::ToolRegistry> = Arc::new(sub_reg);
    // 子 Agent 的空上下文工厂（task 工具用）
    let sub_context_factory: agent::ContextFactory = Arc::new(|| {
        Arc::new(agent_context::InMemoryContext::new(vec![])) as Arc<dyn agent_core::ContextManager>
    });

    // 4b. 装配 Skill 目录（发现 + 过滤；失败不阻断启动）
    let skill_catalog = Arc::new(load_skill_catalog(&cfg, &cwd).await);
    // 4c. 装配上下文约定（AGENTS.md，注入 system prompt）+ 自定义 slash 命令
    // GitHub 提示词按启用态在 build_agent 内动态注入（见 github_context_files）；
    // 未启用时完全不进入 system prompt，零额外 Token 开销（按需加载）。
    // 启用态可由配置 [github] 初始化，或经 /github 命令运行时切换。
    let base_context_files = agent_config::discover_context_files(&cwd);
    let commands = agent_config::discover_commands(&cwd);
    if !commands.is_empty() {
        eprintln!("{}", t!("commands.loaded", count = commands.len()));
    }
    // 4d. 装配长期记忆（可选；按 cwd 项目作用域）
    let memory: Option<Arc<agent_memory::LocalMemoryStore>> = if cfg.memory.enabled {
        let store = Arc::new(agent_memory::LocalMemoryStore::new(&cwd));
        if let Ok(Some(_)) = store.summary().await {
            eprintln!("{}", t!("memory.injected"));
        }
        Some(store)
    } else {
        None
    };
    // 5. 装配 Approval（规则引擎 + stdin 交互回调）
    let prompt_resolver: agent_config::PromptResolver = Arc::new(|ask: agent_core::AskMessage| {
        Box::pin(async move {
            eprint!("{}", t!("approval.prompt", prompt = ask.prompt));
            let _ = std::io::stderr().flush();
            let answer = tokio::task::spawn_blocking(|| {
                let mut s = String::new();
                let _ = std::io::stdin().read_line(&mut s);
                s.trim().to_ascii_lowercase()
            })
            .await
            .unwrap_or_default();
            if answer.starts_with('y') {
                Ok(AskResponse::Yes)
            } else {
                Ok(AskResponse::No)
            }
        })
    });
    // 审批策略（RulesEngine + 上方 resolver）在下方 build_agent 内按当前 mode 构造，
    // 使 `/mode` 切换后审批门槛立即跟随（code/debug 写类放行，ask/architect 写类询问）。

    // 6. 可变运行时状态（/model、/mode 可热切换）
    let max_mistakes = cfg.agent.max_mistakes;
    let context_guard = cfg.agent.context_window_guard;
    let enable_thinking = cfg.agent.enable_thinking;
    let reasoning_budget = cfg.agent.reasoning_budget;
    // P1-K：自适应思考配置（closure 内按 mode 重建时复用）。
    let auto_thinking = cfg.agent.auto_thinking;
    let auto_thinking_model = cfg.agent.auto_thinking_model.clone();
    let auto_consolidate = cfg.memory.auto_consolidate;
    // 子 Agent 配置（[subagent]：开关 / 并发护栏 / 继承父 temperature·thinking / 独立 token 预算）
    let subagent_enabled = cfg.subagent.enabled;
    let subagent_max_concurrent = cfg.subagent.max_concurrent;
    let subagent_inherit = cfg.subagent.inherit_parent;
    let subagent_max_output_override = cfg.subagent.max_output_tokens;
    let profile_temperature = profile.temperature;
    // GitHub 运行时开关：可由 `/github` 动态切换（初值取自配置 [github]）。
    let mut github_enabled = cfg.github.enabled;
    let mut github_allow_write = cfg.github.allow_write;

    let mut current_mode = mode;
    let mut current_model = model;
    let mut current_api_key = api_key;
    let mut current_base_url = profile.base_url.clone();
    let mut current_max_output = profile.max_output_tokens.unwrap_or(4096);
    let mut current_provider_ctx = provider_ctx;

    // Agent 构造闭包：参数化 mode/model/provider_ctx/max_output，热切换后可重建。
    // task 工具内嵌 model/provider_ctx，故随模型/模式一起重建。
    // 子 Agent 监控总线：与 TaskTool / /agents 仪表盘共享同一份进程内状态。
    let supervisor = agent_supervisor::Supervisor::new();

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
        // 审批策略按当前 mode 重建（code/debug 写类放行，ask/architect 写类询问），
        // 使 `/mode` 切换后审批门槛立即跟随——与 server assemble 一致。
        let mut agent_cfg = cfg.agent.clone();
        agent_cfg.mode = mode;
        let rules = agent_config::RulesEngine::new(Arc::new(agent_cfg));
        let approval: Arc<dyn agent_core::ApprovalPolicy> = Arc::new(
            agent_config::RulesApprovalPolicy::new(rules, Arc::clone(&prompt_resolver)),
        );
        // 子 Agent 继承父 temperature/thinking（受 [subagent].inherit_parent 控制）
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
        // 父 Agent 工具集 = 按启用态装配的可选工具 + MCP + task（task 受 [subagent].enabled 控制）
        let (mut tool_registry, lsp_pool) =
            assemble_builtin_tools(optional, github_enabled, github_allow_write);
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

        let builder = agent::Agent::builder(model.clone())
            .provider(Arc::clone(&provider))
            .tools(tools)
            .context(Arc::clone(&context))
            .prompts(Arc::clone(&prompts))
            .approval(Arc::clone(&approval))
            .workspace(Arc::clone(&workspace))
            .provider_ctx(provider_ctx.clone())
            .mode(mode)
            // 与 server assemble 一致：把模型输出预算下发给 Agent 作为每轮请求 max_tokens，
            // 否则回落到硬编码 4096，长回复被截断（finish_reason=length）→ 误报「任务完成」。
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
        // 编辑后 LSP writethrough：lsp 启用（lsp_pool 为 Some）且 edit 开启时，共享 LspTool 的 pool 注入。
        let builder = if lsp_pool.is_some()
            && (cfg.agent.tools.edit.format_on_write || cfg.agent.tools.edit.diagnostics_on_write)
        {
            builder.write_effect(std::sync::Arc::new(agent_tools::LspWriteEffect::new(
                workspace.root().to_path_buf(),
                std::sync::Arc::clone(lsp_pool.as_ref().expect("lsp_pool 已检查 Some")),
                cfg.agent.tools.edit.format_on_write,
                cfg.agent.tools.edit.diagnostics_on_write,
                cfg.agent.tools.edit.diagnostics_deduplicate,
            ))
                as std::sync::Arc<dyn agent_core::WriteEffect>)
        } else {
            builder
        };
        let builder = if let Some(m) = &memory {
            builder.memory(Arc::clone(m) as Arc<dyn agent_core::MemoryStore>)
        } else {
            builder
        };
        let builder = if let Some(m) = &memory {
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
                    // P1-K：tiny 模型分类 prompt 难度 → Effort → 钳位 budget（移植 oh-my-pi
                    // auto-thinking）。分类失败回退 static_cfg；模型不支持思考 → 本轮不思考。
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
    let mut agent = build_agent(
        current_mode,
        current_model.clone(),
        current_provider_ctx.clone(),
        current_max_output,
        Arc::clone(&context),
        github_enabled,
        github_allow_write,
        &optional,
    );

    // 7. 累计用量（跨轮次，供 /status 展示）
    let accumulated: Arc<std::sync::Mutex<Usage>> =
        Arc::new(std::sync::Mutex::new(Usage::default()));

    // 8. 单次任务（非交互）：执行后退出
    if let Some(task) = cli.task.clone() {
        run_turn(&agent, &task, &accumulated).await?;
        return Ok(());
    }

    // 交互 REPL：rustyline 行编辑 + Tab 补全。
    //
    // 防御性检测：若 stdin 非终端（被管道 / 编辑器子进程调用）且未显式指定
    // --acp / --serve / 位置 task，极可能是 ACP 客户端（如 Zed）调用时漏了
    // --acp——此时 REPL 会把 JSON-RPC 请求当用户提问发给 LLM，污染 stdout。
    // 在 stderr 给出明确提示，避免反复调试仍误判为「启动失败」。
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!(
            "提示：stdin 非终端（检测到管道/子进程输入）。若作为 ACP/LSP 服务端被编辑器\
             （如 Zed）调用，须加 --acp 参数；否则此处进入交互 REPL。"
        );
    }
    let helper = ReplHelper::new(
        all_command_names(&commands),
        model_choices(&cfg),
        skill_catalog
            .skills
            .iter()
            .map(|s| s.name.clone())
            .collect(),
        session_store.list().iter().map(|s| s.id.clone()).collect(),
    );
    let mut rl: Editor<ReplHelper, DefaultHistory> = Editor::new()?;
    rl.set_helper(Some(helper));
    let prompt = "\n> ";

    loop {
        let line = match rl.readline(prompt) {
            Ok(l) => l,
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!(e)),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(line);

        // 命令分派：先在独立作用域内构造只读上下文并取得动作，避免借用冲突
        let mut pending_msg: Option<agent_core::UserMessage> = None;
        let (next_task, skip_run) = if line.starts_with('/') {
            let outcome = {
                let acc = accumulated.lock().expect("用量锁中毒");
                let ctx = CommandContext {
                    model: &current_model,
                    mode: current_mode,
                    context: context.as_ref(),
                    accumulated: &acc,
                    mcp: mcp.as_ref(),
                    skills: skill_catalog.as_ref(),
                    config: &cfg,
                    commands: &commands,
                    sessions: &session_store,
                    session_id: &session_id,
                    guard: context_guard,
                    cwd: &cwd,
                    github_enabled,
                    github_allow_write,
                    optional: &optional,
                };
                handle_command(line, &ctx)
            };
            match outcome {
                CommandOutcome::Handled => (String::new(), true),
                CommandOutcome::Inject(t) => (t, false),
                CommandOutcome::SwitchModel(alias) => {
                    if apply_model_switch(
                        &alias,
                        &cfg,
                        &mut current_api_key,
                        &mut current_base_url,
                        &mut current_max_output,
                        &mut current_model,
                        &mut current_provider_ctx,
                    ) {
                        agent = build_agent(
                            current_mode,
                            current_model.clone(),
                            current_provider_ctx.clone(),
                            current_max_output,
                            Arc::clone(&context),
                            github_enabled,
                            github_allow_write,
                            &optional,
                        );
                        eprintln!("{}", t!("model.switched", id = current_model.id));
                    }
                    (String::new(), true)
                }
                CommandOutcome::SwitchMode(m) => {
                    current_mode = m;
                    agent = build_agent(
                        current_mode,
                        current_model.clone(),
                        current_provider_ctx.clone(),
                        current_max_output,
                        Arc::clone(&context),
                        github_enabled,
                        github_allow_write,
                        &optional,
                    );
                    eprintln!(
                        "{}",
                        t!("mode.switched", mode = format!("{current_mode:?}"))
                    );
                    (String::new(), true)
                }
                CommandOutcome::Resume(id) => {
                    let path = session_store.path_for(&id);
                    if !path.exists() {
                        eprintln!("{}", t!("session.not_exist", id = id));
                    } else {
                        match agent_context::PersistentContext::open(
                            prompts.system_with_platform(current_mode),
                            &path,
                        )
                        .await
                        {
                            Ok(new_pctx) => {
                                new_pctx
                                    .set_summarizer(Box::new(
                                        agent_context::compaction::LlmSummaryProvider::new(
                                            Arc::clone(&provider),
                                            current_model.clone(),
                                            current_provider_ctx.clone(),
                                        ),
                                    ))
                                    .await;
                                new_pctx
                                    .set_shake_sink(Arc::new(
                                        agent_context::compaction::DirSink::new(
                                            cwd.join(".gyre").join("artifacts"),
                                        ),
                                    ))
                                    .await;
                                let new_context: Arc<dyn agent_core::ContextManager> =
                                    Arc::new(new_pctx);
                                context = new_context;
                                session_id = id.clone();
                                agent = build_agent(
                                    current_mode,
                                    current_model.clone(),
                                    current_provider_ctx.clone(),
                                    current_max_output,
                                    Arc::clone(&context),
                                    github_enabled,
                                    github_allow_write,
                                    &optional,
                                );
                                let u = context.token_usage();
                                eprintln!(
                                    "{}",
                                    t!("session.resumed", id = session_id, tokens = u.current)
                                );
                                print_session_history(&path);
                            }
                            Err(e) => eprintln!("{}", t!("session.resume_failed", e = e)),
                        }
                    }
                    (String::new(), true)
                }
                CommandOutcome::Compact => {
                    compact_context(context.as_ref()).await;
                    (String::new(), true)
                }
                CommandOutcome::Paste { prompt, image } => {
                    let mut content = vec![image];
                    if !prompt.is_empty() {
                        content.push(agent_core::UserContent::Text { text: prompt });
                    }
                    pending_msg = Some(agent_core::UserMessage { content });
                    (String::new(), true)
                }
                CommandOutcome::Swarm(target) => {
                    if !subagent_enabled {
                        eprintln!("{}", t!("swarm.disabled"));
                        (String::new(), true)
                    } else {
                        // swarm 子 Agent 同样继承父 temperature/thinking（受 inherit_parent 控制）
                        let swarm_temperature = if subagent_inherit {
                            profile_temperature
                        } else {
                            None
                        };
                        let swarm_thinking = if subagent_inherit && enable_thinking {
                            Some(agent_core::ThinkingConfig::new(
                                reasoning_budget.unwrap_or(16_000),
                            ))
                        } else {
                            None
                        };
                        run_swarm_yaml(
                            &target,
                            &provider,
                            &sub_tools,
                            &prompts,
                            &workspace,
                            &current_model,
                            &current_provider_ctx,
                            current_mode,
                            max_mistakes,
                            context_guard,
                            current_max_output,
                            &sub_context_factory,
                            swarm_temperature,
                            swarm_thinking,
                            subagent_max_concurrent,
                        )
                        .await;
                        (String::new(), true)
                    }
                }
                CommandOutcome::Agents => {
                    if let Err(e) = agents_view::run_dashboard(&supervisor).await {
                        eprintln!("{}", t!("agents.dashboard_error", e = e));
                    }
                    (String::new(), true)
                }
                CommandOutcome::SetGithub {
                    enabled,
                    allow_write,
                } => {
                    github_enabled = enabled;
                    if let Some(w) = allow_write {
                        github_allow_write = w;
                    }
                    agent = build_agent(
                        current_mode,
                        current_model.clone(),
                        current_provider_ctx.clone(),
                        current_max_output,
                        Arc::clone(&context),
                        github_enabled,
                        github_allow_write,
                        &optional,
                    );
                    let token_set = std::env::var("GH_TOKEN")
                        .or_else(|_| std::env::var("GITHUB_TOKEN"))
                        .ok()
                        .filter(|s| !s.is_empty())
                        .is_some();
                    eprintln!(
                        "[github] {}",
                        if github_enabled {
                            t!(
                                "github.toggle_enabled",
                                write = if github_allow_write {
                                    t!("github.write_on")
                                } else {
                                    t!("github.write_off")
                                },
                                token = if token_set {
                                    t!("github.token_set_suffix")
                                } else {
                                    t!("github.token_unset_suffix")
                                }
                            )
                        } else {
                            t!("github.toggle_disabled")
                        }
                    );
                    (String::new(), true)
                }
                CommandOutcome::SetTool { key, enabled } => {
                    let valid = if key == "github" {
                        github_enabled = enabled;
                        true
                    } else if is_known_optional_key(&key) {
                        optional.insert(key.clone(), enabled);
                        true
                    } else {
                        eprintln!("{}", t!("tools.unknown_group", key = key));
                        false
                    };
                    if valid {
                        agent = build_agent(
                            current_mode,
                            current_model.clone(),
                            current_provider_ctx.clone(),
                            current_max_output,
                            Arc::clone(&context),
                            github_enabled,
                            github_allow_write,
                            &optional,
                        );
                        eprintln!(
                            "{}",
                            if enabled {
                                t!("tools.enabled", key = key)
                            } else {
                                t!("tools.disabled", key = key)
                            }
                        );
                    }
                    (String::new(), true)
                }
                CommandOutcome::Quit => break,
            }
        } else {
            (line.to_string(), false)
        };
        if skip_run {
            if let Some(msg) = pending_msg.take() {
                run_turn_message(&agent, msg, &accumulated).await?;
            }
            continue;
        }

        run_turn(&agent, &next_task, &accumulated).await?;
    }

    Ok(())
}

/// 按可选工具启用态组装 context_files：仅启用组的操作提示词进入 system prompt，
/// 未启用完全屏蔽（零额外 Token 开销）。
///
/// 这是按需加载的核心——ast/lsp/image 来自 [`agent_tools::OPTIONAL_TOOL_PROMPTS`]，
/// hashline/pty/github 来自各 crate 的 `PROMPT_SECTION`。统一原则：**启用才注入，禁用屏蔽**。
#[must_use]
fn optional_context_files(
    base: &[String],
    optional: &std::collections::HashMap<String, bool>,
    github_enabled: bool,
) -> Vec<String> {
    let mut files = base.to_vec();
    for p in agent_tools::OPTIONAL_TOOL_PROMPTS {
        if *optional.get(p.key).unwrap_or(&false) {
            files.push(p.prompt.to_string());
        }
    }
    if *optional.get("hashline").unwrap_or(&false) {
        files.push(agent_hashline::PROMPT_SECTION.to_string());
    }
    if *optional.get("pty").unwrap_or(&false) {
        files.push(agent_pty::PROMPT_SECTION.to_string());
    }
    if github_enabled {
        files.push(agent_tools::PROMPT_SECTION.to_string());
    }
    files
}

/// 装配内置工具集：核心工具（始终启用）+ 按启用态追加的可选组（ast/lsp/image/hashline/pty/github）。
///
/// 与 [`optional_context_files`] 配对：同一开关同时决定「工具是否注册」与「提示词是否注入」，
/// 确保未启用工具既不出现在 LLM 工具列表，也不占 system prompt Token。
#[must_use]
fn assemble_builtin_tools(
    optional: &std::collections::HashMap<String, bool>,
    github_enabled: bool,
    github_allow_write: bool,
) -> (
    agent_tools::DefaultToolRegistry,
    Option<agent_tools::LspPool>,
) {
    let mut reg = agent_tools::core_tools();
    let mut lsp_pool: Option<agent_tools::LspPool> = None;
    if *optional.get("ast").unwrap_or(&false) {
        reg = agent_tools::ast_tools(reg);
    }
    if *optional.get("image").unwrap_or(&false) {
        reg = agent_tools::image_tools(reg);
    }
    if *optional.get("lsp").unwrap_or(&false) {
        // 取 LspTool 的共享 pool，供 LspWriteEffect 复用同一套语言服务器（避免两套 LSP）。
        let lsp = agent_tools::LspTool::new();
        lsp_pool = Some(lsp.pool());
        reg = reg.with(Box::new(lsp));
    }
    if *optional.get("hashline").unwrap_or(&false) {
        reg = reg.with(Box::new(agent_hashline::HashlineTool::new()));
    }
    if *optional.get("pty").unwrap_or(&false) {
        reg = reg.with(Box::new(agent_pty::RunPtyTool));
    }
    if github_enabled {
        reg = reg.with(Box::new(agent_tools::GithubTool::new(github_allow_write)));
    }
    (reg, lsp_pool)
}

/// 可选工具组 key 白名单（不含 `github`；github 由独立字段管理）。
const OPTIONAL_TOOL_KEYS: &[&str] = &["ast", "lsp", "image", "hashline", "pty"];

/// 判断 key 是否为已知可选工具组（不含 github）。
#[must_use]
fn is_known_optional_key(key: &str) -> bool {
    OPTIONAL_TOOL_KEYS.contains(&key)
}

/// `/model <alias>` 热切换：解析 profile 并更新运行时模型状态。成功返回 true。
fn apply_model_switch(
    alias: &str,
    cfg: &agent_config::Config,
    current_api_key: &mut String,
    current_base_url: &mut String,
    current_max_output: &mut usize,
    current_model: &mut agent_core::Model,
    current_provider_ctx: &mut agent_core::ProviderCallContext,
) -> bool {
    match cfg.resolve_model(Some(alias)) {
        Ok(profile) => {
            *current_api_key = profile.resolve_api_key().expose_secret().to_string();
            *current_base_url = profile.base_url.clone();
            *current_max_output = profile.max_output_tokens.unwrap_or(4096);
            *current_model = agent_core::Model {
                id: profile.id.clone(),
                provider: "openai-compatible".into(),
                api: profile.api,
                max_input_tokens: profile.effective_max_input_tokens(),
                max_output_tokens: *current_max_output,
                supports_tools: true,
                supports_streaming: true,
                supports_thinking: false,
                extra_body: profile.extra_body.clone(),
            };
            *current_provider_ctx = agent_core::ProviderCallContext {
                api_key: Some(current_api_key.clone()),
                base_url: Some(current_base_url.clone()),
                max_in_flight: None,
            };
            true
        }
        Err(e) => {
            eprintln!("{}", t!("model.switch_failed", e = e));
            false
        }
    }
}

/// `/compact` 手动压缩：shake → summarize → prune（与循环内自动压缩一致）。
async fn compact_context(context: &dyn agent_core::ContextManager) {
    eprintln!("{}", t!("compact.compressing"));
    let _ = context.compact(CompactionStrategy::Shake).await;
    let _ = context
        .compact(CompactionStrategy::Summarize { max_tokens: 0 })
        .await;
    let _ = context
        .compact(CompactionStrategy::Prune { keep_recent: 8 })
        .await;
    let u = context.token_usage();
    eprintln!(
        "{}",
        t!("compact.done", current = u.current, limit = u.limit)
    );
}

/// 恢复会话后回显对话历史（仅 user/assistant 轮次；超长只显示最近 60 条）。
fn print_session_history(path: &std::path::Path) {
    let history = session_history_lines(path);
    let total = history.len();
    const MAX_HISTORY: usize = 60;
    if total == 0 {
        eprintln!("{}", t!("session.no_history"));
        return;
    }
    if total > MAX_HISTORY {
        eprintln!(
            "{}",
            t!(
                "session.history_truncated",
                total = total,
                max = MAX_HISTORY
            )
        );
    } else {
        eprintln!("{}", t!("session.history_count", count = total));
    }
    for h in &history[total.saturating_sub(MAX_HISTORY)..] {
        eprintln!("{h}");
    }
    eprintln!("{}", t!("session.history_continue"));
}

/// `/swarm <yaml>`：读取 swarm 定义文件并运行多代理编排（DAG 波内并行）。
#[allow(clippy::too_many_arguments)]
async fn run_swarm_yaml(
    target: &str,
    provider: &Arc<dyn agent_core::LlmProvider>,
    tools: &Arc<dyn agent_tools::ToolRegistry>,
    prompts: &Arc<agent_prompt::PromptCatalog>,
    workspace: &Arc<agent_core::Workspace>,
    model: &agent_core::Model,
    provider_ctx: &agent_core::ProviderCallContext,
    mode: agent_core::Mode,
    max_mistakes: usize,
    context_guard: f32,
    max_output: usize,
    context_factory: &agent::ContextFactory,
    temperature: Option<f32>,
    thinking: Option<agent_core::ThinkingConfig>,
    max_concurrent: usize,
) {
    let yaml = match tokio::fs::read_to_string(target).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", t!("swarm.read_failed", target = target, e = e));
            return;
        }
    };
    let runner: Arc<dyn agent_swarm::SwarmAgentRunner> =
        Arc::new(agent_swarm::AgentSwarmRunner::new(
            Arc::clone(provider),
            Arc::clone(tools),
            Arc::clone(prompts),
            Arc::clone(workspace),
            model.clone(),
            provider_ctx.clone(),
            mode,
            max_mistakes,
            context_guard,
            max_output,
            Arc::clone(context_factory),
            temperature,
            thinking,
        ));
    let progress: agent_swarm::ProgressFn = Arc::new(|name: &str, msg: &str| {
        eprintln!("{}", t!("swarm.progress", name = name, msg = msg))
    });
    let cancel = tokio_util::sync::CancellationToken::new();
    let workspace_path: Arc<std::path::Path> = Arc::from(workspace.root());
    let options = agent_swarm::SwarmRunOptions {
        workspace: workspace_path,
        cancel,
        model_override: None,
        on_progress: Some(progress),
        max_concurrent: max_concurrent.max(1),
    };
    eprintln!("{}", t!("swarm.starting", target = target));
    match agent_swarm::run_swarm(&yaml, &runner, options).await {
        Ok(result) => print_swarm_result(&result),
        Err(e) => eprintln!("{}", t!("swarm.run_failed", e = e)),
    }
}

/// 打印 swarm 运行结果摘要。
fn print_swarm_result(result: &agent_swarm::PipelineResult) {
    eprintln!(
        "{}",
        t!(
            "swarm.result_summary",
            status = format!("{:?}", result.status),
            iters = result.iterations,
            agents = result.agent_results.len()
        )
    );
    for (name, runs) in &result.agent_results {
        let summary = match runs.last() {
            Some(r) if r.exit_code == 0 => t!("swarm.output_chars", n = r.output.chars().count()),
            Some(r) => t!(
                "swarm.failed_short",
                e = r
                    .error
                    .clone()
                    .unwrap_or_else(|| t!("swarm.failed_default").to_string())
            ),
            None => t!("swarm.not_run"),
        };
        eprintln!(
            "{}",
            t!(
                "swarm.agent_line",
                name = name,
                rounds = runs.len(),
                summary = summary
            )
        );
    }
    if !result.errors.is_empty() {
        eprintln!("{}", t!("swarm.errors", errors = result.errors.join("; ")));
    }
}

/// 长期记忆合并 Hook：任务成功结束时触发 LLM consolidate。
#[derive(Clone)]
struct ConsolidateHook {
    store: Arc<agent_memory::LocalMemoryStore>,
    provider: Arc<dyn agent_core::LlmProvider>,
    model: Model,
    provider_ctx: agent_core::ProviderCallContext,
    auto_consolidate: bool,
}

#[async_trait::async_trait]
impl Hook for ConsolidateHook {
    async fn on_event(&self, event: &HookEvent) {
        if self.auto_consolidate && matches!(event, HookEvent::Stop { success: true }) {
            if let Err(e) = self
                .store
                .consolidate(&self.provider, &self.model, &self.provider_ctx)
                .await
            {
                eprintln!("{}", t!("memory.merge_failed", e = e));
            }
        }
    }
}

/// 加载 Skill 目录：按 `[skills]` 配置发现、去重、过滤；失败时返回空集合并告警。
async fn load_skill_catalog(
    cfg: &agent_config::Config,
    cwd: &std::path::Path,
) -> agent_skills::SkillCatalog {
    let opts = cfg.skills.to_load_options();
    if !opts.enabled {
        return agent_skills::SkillCatalog::default();
    }
    match agent_skills::SkillRegistry::native(cwd.to_path_buf())
        .load(&opts)
        .await
    {
        Ok(cat) => {
            if !cat.warnings.is_empty() {
                eprintln!(
                    "{}",
                    t!("skill.load_warn", warnings = cat.warnings.join("; "))
                );
            }
            if cat.is_empty() {
                eprintln!("{}", t!("skill.not_found_msg"));
            } else {
                let names: Vec<&str> = cat.skills.iter().map(|s| s.name.as_str()).collect();
                eprintln!(
                    "{}",
                    t!(
                        "skill.loaded",
                        count = cat.skills.len(),
                        names = names.join(", ")
                    )
                );
            }
            cat
        }
        Err(e) => {
            eprintln!("{}", t!("skill.load_failed", e = e));
            agent_skills::SkillCatalog::default()
        }
    }
}

/// 运行一个任务轮次，消费事件流并打印，并把用量累加到 `accumulated`。返回是否成功完成。
async fn run_turn(
    agent: &agent::Agent,
    task: &str,
    accumulated: &Arc<std::sync::Mutex<Usage>>,
) -> Result<bool> {
    consume_stream(agent.run(task), accumulated).await
}

/// 运行一条带图像等多模态内容块的用户消息（`/paste`）。
async fn run_turn_message(
    agent: &agent::Agent,
    msg: agent_core::UserMessage,
    accumulated: &Arc<std::sync::Mutex<Usage>>,
) -> Result<bool> {
    consume_stream(agent.run_message(msg), accumulated).await
}

/// 消费 agent 事件流并打印（`run_turn` / `run_turn_message` 共用）。
async fn consume_stream<S>(events: S, accumulated: &Arc<std::sync::Mutex<Usage>>) -> Result<bool>
where
    S: futures::Stream<Item = AgentEvent>,
{
    tokio::pin!(events);
    let mut success = false;
    // 流式 Markdown 美化：仅 TTY 输出着色/高亮，管道透传原文。
    let mut md =
        markdown::MarkdownRenderer::new(std::io::IsTerminal::is_terminal(&std::io::stdout()));
    while let Some(ev) = events.next().await {
        match ev {
            AgentEvent::TextDelta(t) => {
                let rendered = md.push(&t);
                if !rendered.is_empty() {
                    print!("{rendered}");
                    let _ = std::io::stdout().flush();
                }
            }
            AgentEvent::ThinkingDelta(t) => {
                eprint!("\x1b[2m{t}\x1b[0m");
                let _ = std::io::stderr().flush();
            }
            AgentEvent::Say(s) => {
                let tag = match s.kind {
                    StatusKind::Info => t!("event.info"),
                    StatusKind::Thinking => t!("event.think"),
                    StatusKind::Success => t!("event.ok"),
                    StatusKind::Warning => t!("event.warn"),
                    StatusKind::Error => t!("event.err"),
                };
                eprintln!("\n[{tag}] {}", s.text);
            }
            AgentEvent::ToolExec { name, output } => {
                eprintln!("\n{}", t!("event.tool", name = name, output = output));
            }
            AgentEvent::Usage(u) => {
                eprintln!(
                    "\n{}",
                    t!("event.usage", input = u.input_tokens, out = u.output_tokens)
                );
            }
            AgentEvent::StateChanged(st) => {
                eprintln!("\n{}", t!("event.state", state = format!("{st:?}")));
            }
            AgentEvent::Error(e) => {
                eprintln!("\n{}", t!("event.error", e = e));
            }
            AgentEvent::Done(summary) => {
                // flush 末尾残留（未闭合代码块 / 行缓冲）
                let tail = md.finish();
                if !tail.is_empty() {
                    print!("{tail}");
                    let _ = std::io::stdout().flush();
                }
                success = summary.success;
                if let Ok(mut acc) = accumulated.lock() {
                    acc.add(&summary.usage);
                }
                eprintln!(
                    "\n{}",
                    t!(
                        "event.done",
                        turns = summary.turns,
                        tools = summary.tool_calls,
                        success = summary.success,
                        cost = format!("{:.6}", summary.usage.cost_usd)
                    )
                );
            }
            AgentEvent::Ask(_)
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
    Ok(success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_tools::ToolRegistry;

    /// 默认（无任何可选工具启用、github 关）→ 仅核心工具，无可选工具与提示词。
    #[test]
    fn assemble_defaults_to_core_only() {
        let optional = std::collections::HashMap::new();
        let (reg, _) = assemble_builtin_tools(&optional, false, false);
        let specs = reg.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"run_command"));
        for opt in [
            "replace_block",
            "ast_search",
            "read_image",
            "image_gen",
            "lsp",
            "apply_hashline",
            "run_pty_command",
            "github",
        ] {
            assert!(!names.contains(&opt), "默认应关闭可选工具 {opt}");
        }
    }

    #[test]
    fn assemble_enables_ast_group() {
        let mut optional = std::collections::HashMap::new();
        optional.insert("ast".to_string(), true);
        let (reg, _) = assemble_builtin_tools(&optional, false, false);
        let specs = reg.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"replace_block"));
        assert!(names.contains(&"ast_search"));
        // 其他可选组仍关闭
        assert!(!names.contains(&"read_image"));
        assert!(!names.contains(&"lsp"));
    }

    #[test]
    fn assemble_github_independent_of_optional_map() {
        let optional = std::collections::HashMap::new();
        let (reg, _) = assemble_builtin_tools(&optional, true, true);
        let specs = reg.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"github"),
            "github 由独立参数控制，不受 optional map 影响"
        );
    }

    /// 仅启用组进入 system prompt；未启用组（含 github）完全屏蔽。
    #[test]
    fn context_files_inject_only_enabled() {
        let base = vec!["项目约定".to_string()];
        let mut optional = std::collections::HashMap::new();
        optional.insert("lsp".to_string(), true);
        let files = optional_context_files(&base, &optional, false);
        assert_eq!(files.len(), 2, "base + 1 个启用组");
        assert!(files[0].contains("项目约定"));
        assert!(files[1].contains("<lsp>"));
        // 未启用组不注入
        assert!(!files.iter().any(|f| f.contains("<ast>")));
        assert!(!files.iter().any(|f| f.contains("<github>")));
    }

    #[test]
    fn context_files_github_toggle() {
        let optional = std::collections::HashMap::new();
        let on = optional_context_files(&[], &optional, true);
        assert!(on.iter().any(|f| f.contains("<github>")));
        let off = optional_context_files(&[], &optional, false);
        assert!(!off.iter().any(|f| f.contains("<github>")));
    }

    #[test]
    fn known_optional_key_excludes_github() {
        assert!(is_known_optional_key("ast"));
        assert!(is_known_optional_key("pty"));
        // github 不在此白名单（由独立字段管理）
        assert!(!is_known_optional_key("github"));
        assert!(!is_known_optional_key("bogus"));
    }
}
