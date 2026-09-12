//! # agent CLI
//!
//! 二进制入口：加载配置（TOML）→ 装配 Provider/Tools/Context/Approval → 驱动 [`agent::Agent`]。
//! 体现 Ports & Adapters：所有具体实现在此装配，智能体循环只依赖 trait。

mod agents_view;
mod markdown;
mod repl;
mod review;
mod rpc;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{
    AgentEvent, AskResponse, CompactionStrategy, Hook, HookEvent, Model, StatusKind, Usage,
};
use anyhow::{Context as _, Result};
use clap::{CommandFactory, FromArgMatches, Parser};
use futures::StreamExt;
use rustyline::Editor;
use rustyline::history::DefaultHistory;
use secrecy::ExposeSecret;
use tokio::io::AsyncBufReadExt;

/// i18n 取词宏（编译期内嵌 locale 目录，运行期按系统/配置语言激活）。
use agent_i18n::t;

/// 管理子命令面（models / auth / config）：路由识别与执行在 `agent_cli::manage`。
use agent_cli::manage::{ManageCmd, Route};

use repl::{
    CommandContext, CommandOutcome, ReplHelper, handle_command, model_choices,
    session_history_lines,
};

/// High-performance Rust agent CLI.
#[derive(Parser, Debug)]
#[command(
    name = "agent",
    version,
    about = "High-performance Rust agent (Zoo-Code + oh-my-pi)"
)]
struct Cli {
    /// Task text (reads one line from stdin if omitted).
    ///
    /// 收集首参数起的全部剩余词（对齐 oh-my-pi：`agent list all my files` 整句作 prompt；
    /// flag 须置于任务文本之前）。配合保留顶层词防误注入 gate（reserved_top_level_word_hint）。
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
    task: Vec<String>,
    /// Model alias (matches `[[models]] alias` in config).
    #[arg(long)]
    model: Option<String>,
    /// Working directory (defaults to the current directory).
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Approval mode override: always-ask / write / yolo.
    #[arg(long)]
    approval_mode: Option<String>,
    /// UI language override (en / zh / ru / ja); falls back to config or system language.
    #[arg(long)]
    lang: Option<String>,
    /// Start the web service (HTTP + WebSocket + frontend) instead of running a one-shot task.
    /// Without a value, binds to `[server].bind` in config; with a value, overrides the bind
    /// address (e.g. `--serve 0.0.0.0:80`).
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    serve: Option<String>,
    /// ACP (Agent Client Protocol).
    ///
    /// - Standalone (without `--serve`): runs ACP in pure stdio mode (for editors invoking it
    ///   as a subprocess; reads JSON-RPC from stdin, writes events to stdout, no HTTP port).
    ///   Highest priority; does not start HTTP.
    /// - With `--serve`: additionally enables HTTP+SSE endpoints (also controlled by
    ///   `[acp].enabled`).
    #[arg(long)]
    acp: bool,
    /// NDJSON line protocol mode (external language/robot integration, see docs/rpc.md): reads
    /// JSON requests line by line from stdin, writes JSON events/responses line by line to
    /// stdout; one Agent with the same Context is reused across prompts in-process.
    /// Mutually exclusive with --serve / --acp (stdout is the protocol channel).
    #[arg(long)]
    rpc: bool,
    /// Forward approvals/followup questions to the RPC host as `request` frames
    /// (host answers with `response` frames). Default off: approvals are auto-denied
    /// (compat with existing embedders).
    #[arg(long)]
    rpc_forward_ask: bool,
    /// Append custom text to the system prompt (H40).
    ///
    /// Value semantics (aligned with oh-my-pi `resolvePromptInput`): text containing a
    /// newline is used verbatim; otherwise, if the value names a readable file, that
    /// file's contents are appended; otherwise the value is appended as-is.
    /// Equivalent config key: `[agent] append_system_prompt`.
    #[arg(long, value_name = "TEXT|FILE")]
    append_system_prompt: Option<String>,
    /// Resume a historical session (session id; list with --list-sessions).
    #[arg(long)]
    resume: Option<String>,
    /// Continue the most recent session in this project (H31): prefers the `.last`
    /// breadcrumb written by the last used session, falls back to newest by mtime.
    /// Mutually exclusive with --resume / --fork (they take precedence).
    #[arg(long)]
    r#continue: bool,
    /// List historical sessions and exit.
    #[arg(long)]
    list_sessions: bool,
    /// Copy a session to a new id and continue (fork).
    #[arg(long)]
    fork: Option<String>,
    /// OpenTelemetry OTLP endpoint (e.g. http://localhost:4317); omitted → local logs only.
    #[arg(long)]
    otlp: Option<String>,
    /// SOCKS5 proxy (`host:port`, e.g. `127.0.0.1:1080`; IPv6 uses brackets `[::1]:1080`).
    /// Overrides `[socks5].host/.port` in config; presence implies enabled (overrides the
    /// persisted toggle and config default).
    /// Only affects backend outbound HTTP/HTTPS requests; the frontend is never proxied.
    #[arg(long, value_name = "HOST:PORT")]
    socks: Option<String>,
    /// SOCKS5 proxy username (optional; setting it enables RFC1929 auth).
    #[arg(long)]
    socks_user: Option<String>,
    /// SOCKS5 proxy password (optional; in-process CLI only, never persisted or logged).
    #[arg(long)]
    socks_pass: Option<String>,
}

/// clap 参数 id → i18n 词条 key（仅非 en 语言使用；en 直接用 derive 英文注释）。
/// 注意：clap derive 的参数 id 默认是字段名（snake_case），与 long flag（kebab-case）不同。
const CLI_HELP_KEYS: &[(&str, &str)] = &[
    ("task", "cli.help.arg.task"),
    ("model", "cli.help.arg.model"),
    ("cwd", "cli.help.arg.cwd"),
    ("approval_mode", "cli.help.arg.approval-mode"),
    ("lang", "cli.help.arg.lang"),
    ("serve", "cli.help.arg.serve"),
    ("acp", "cli.help.arg.acp"),
    ("rpc", "cli.help.arg.rpc"),
    ("append_system_prompt", "cli.help.arg.append-system-prompt"),
    ("resume", "cli.help.arg.resume"),
    ("continue", "cli.help.arg.continue"),
    ("list_sessions", "cli.help.arg.list-sessions"),
    ("fork", "cli.help.arg.fork"),
    ("otlp", "cli.help.arg.otlp"),
    ("socks", "cli.help.arg.socks"),
    ("socks_user", "cli.help.arg.socks-user"),
    ("socks_pass", "cli.help.arg.socks-pass"),
];

/// 按激活语言本地化 clap 帮助文本。英文直接用 derive 注释（en 是词表回退语言，无需词条）；
/// 其余语言（zh/ru/ja）从 i18n 词表替换 about 与各参数 help。
fn localize_cli_help(mut cmd: clap::Command) -> clap::Command {
    if agent_i18n::current_locale() == "en" {
        return cmd;
    }
    cmd = cmd.about(agent_i18n::t!("cli.help.about"));
    // 收集 id 后逐个 mut_arg 替换（避免在不可变迭代期间修改命令）。
    let ids: Vec<String> = cmd
        .get_arguments()
        .map(|a| a.get_id().to_string())
        .collect();
    for id in ids {
        if let Some((_, key)) = CLI_HELP_KEYS.iter().find(|(i, _)| *i == id) {
            cmd = cmd.mut_arg(&id, |arg| arg.help(agent_i18n::tr(key, &[])));
        }
    }
    cmd
}

/// H40：解析 `--append-system-prompt` 取值（对齐 omp `resolvePromptInput`）：
/// 含换行 → 字面文本；否则可读文件 → 文件内容；否则字面文本。
fn resolve_prompt_input(value: &str) -> String {
    if value.contains('\n') {
        return value.to_string();
    }
    std::fs::read_to_string(value).unwrap_or_else(|_| value.to_string())
}

/// 解析 CLI 参数：帮助文本先按系统语言本地化（--lang 覆盖运行期文本，帮助跟随系统语言）。
///
/// 解析前先做一次「未知 flag」预检（H1）：`task` 位置参数带
/// `trailing_var_arg + allow_hyphen_values`，否则 `agent --smoke-test` 这类未知 flag 会被
/// 静默收进 prompt 送 LLM 计费。预检逻辑见 [`validate_argv_flags`]。
fn parse_cli() -> Cli {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    // 预检报错也要跟随 `--lang`（i18n 在 parse 前已按系统语言初始化；此处按 argv 提前覆盖）。
    if let Some(lang) = scan_lang_flag(&raw) {
        agent_i18n::init(Some(&lang));
    }
    let mut cmd = localize_cli_help(Cli::command());
    // build() 补齐自动生成的 --help/--version（预检表需要完整 flag 面）。
    cmd.build();
    if let Err(msg) = validate_argv_flags(&cmd, &raw) {
        eprintln!("{msg}");
        std::process::exit(2); // 用法错误（对齐 omp reportUnrecognizedFlags 的非零退出）
    }
    let matches = cmd.get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

/// 从原始 argv 提取 `--lang <v>` / `--lang=<v>`（仅用于 parse 前激活 i18n）。
fn scan_lang_flag(raw: &[String]) -> Option<String> {
    let mut it = raw.iter();
    while let Some(tok) = it.next() {
        if let Some(v) = tok.strip_prefix("--lang=") {
            return (!v.is_empty()).then(|| v.to_string());
        }
        if tok == "--lang" {
            return it.next().filter(|v| !v.starts_with('-')).cloned();
        }
    }
    None
}

/// flag 取值形态（预检时决定是否连带消费下一个 token）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlagKind {
    /// 布尔开关（不取值）。
    Switch,
    /// 必须取值（`--model x` / `--model=x`）。
    TakesValue,
    /// 可选值（`--serve` / `--serve 0.0.0.0:80`）。
    OptionalValue,
}

/// 长旗标表（旗标名 → 取值形态）。
type LongFlagTable = Vec<(String, FlagKind)>;
/// 短旗标表（字符 → 取值形态）。
type ShortFlagTable = Vec<(char, FlagKind)>;

/// 从 clap `Command` 抽取 flag 表：`(长旗标 → 形态, 短旗标 → 形态)`。
fn collect_flag_kinds(cmd: &clap::Command) -> (LongFlagTable, ShortFlagTable) {
    let mut longs: Vec<(String, FlagKind)> = Vec::new();
    let mut shorts: Vec<(char, FlagKind)> = Vec::new();
    for arg in cmd.get_arguments() {
        let kind = match arg.get_action() {
            clap::ArgAction::SetTrue
            | clap::ArgAction::SetFalse
            | clap::ArgAction::Count
            | clap::ArgAction::Help
            | clap::ArgAction::Version => FlagKind::Switch,
            _ => match arg.get_num_args() {
                Some(r) if r.min_values() == 0 && r.max_values() == 1 => FlagKind::OptionalValue,
                _ => FlagKind::TakesValue,
            },
        };
        if let Some(l) = arg.get_long() {
            longs.push((l.to_string(), kind));
        }
        for l in arg.get_all_aliases().into_iter().flatten() {
            longs.push((l.to_string(), kind));
        }
        if let Some(s) = arg.get_short() {
            shorts.push((s, kind));
        }
    }
    (longs, shorts)
}

/// argv 预检：未知 flag → `Err`（调用方 exit 2）。
///
/// 规则（对齐 oh-my-pi `reportUnrecognizedFlags`）：
/// - 未知 `-`/`--` 开头的 token 一律报错，**绝不**沉入 prompt；
/// - 已知 flag 出现在 prompt 之后也报错（clap 的 `trailing_var_arg` 会把它当 prompt 文本，
///   静默忽略配置比报错更糟）——用户可用 `--` 显式分隔：`agent -- do it --model x`；
/// - `--` 之后的 token 全部视为字面 prompt，不再校验（转义出口）；
/// - 纯数字 token（`-1`、`-1.5`）视为 prompt 词，不当作短旗标。
fn validate_argv_flags(cmd: &clap::Command, raw: &[String]) -> Result<(), String> {
    let (longs, shorts) = collect_flag_kinds(cmd);
    let mut unknown: Vec<String> = Vec::new();
    let mut late: Vec<String> = Vec::new();
    let mut task_started = false;
    let mut i = 0usize;

    while i < raw.len() {
        let tok = &raw[i];
        if tok == "--" {
            break; // 显式分隔符：其后一律字面 prompt
        }
        if let Some(rest) = tok.strip_prefix("--") {
            let (name, has_eq) = match rest.split_once('=') {
                Some((n, _)) => (n, true),
                None => (rest, false),
            };
            match longs.iter().find(|(l, _)| l == name) {
                Some((_, kind)) => {
                    // --help/--version 交给 clap（全局）或 manage::route（子命令）处理，不算「迟到 flag」。
                    if task_started && !is_meta_flag(name) {
                        late.push(tok.clone());
                    }
                    if !has_eq {
                        match kind {
                            FlagKind::TakesValue => i += 1,
                            FlagKind::OptionalValue => {
                                if raw
                                    .get(i + 1)
                                    .is_some_and(|n| !n.starts_with('-') || is_numeric_token(n))
                                {
                                    i += 1;
                                }
                            }
                            FlagKind::Switch => {}
                        }
                    }
                }
                None => unknown.push(tok.clone()),
            }
        } else if tok.len() > 1 && tok.starts_with('-') {
            if is_numeric_token(tok) {
                task_started = true;
            } else {
                let chars: Vec<char> = tok[1..].chars().collect();
                let (mut ok, mut consume_next) = (true, false);
                for (idx, c) in chars.iter().enumerate() {
                    match shorts.iter().find(|(s, _)| s == c) {
                        Some((_, FlagKind::Switch)) => continue,
                        Some((_, FlagKind::TakesValue)) => {
                            consume_next = idx + 1 == chars.len();
                            break;
                        }
                        Some((_, FlagKind::OptionalValue)) => break,
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    unknown.push(tok.clone());
                } else {
                    if task_started && !chars.iter().any(|c| matches!(c, 'h' | 'V')) {
                        late.push(tok.clone());
                    }
                    if consume_next {
                        i += 1;
                    }
                }
            }
        } else {
            task_started = true;
        }
        i += 1;
    }

    if !unknown.is_empty() {
        return Err(t!(
            "cli.error.unknown_flags",
            flags = unknown.join(", "),
            plural = if unknown.len() == 1 { "" } else { "s" }
        ));
    }
    if !late.is_empty() {
        return Err(t!("cli.error.flags_after_task", flags = late.join(", ")));
    }
    Ok(())
}

/// `--help` / `--version`：clap 内建元 flag，出现位置不受「flag 须在 task 之前」限制
/// （子命令帮助由 `agent_cli::manage::route` 消费，见 H2）。
fn is_meta_flag(name: &str) -> bool {
    matches!(name, "help" | "version")
}

/// `-1` / `-1.5` / `-.5` 之类的纯数字 token：按 prompt 词处理，不当作短旗标。
fn is_numeric_token(tok: &str) -> bool {
    let body = tok.strip_prefix('-').unwrap_or(tok);
    !body.is_empty() && body.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// H24：模型 profile 的自定义请求头（`${ENV}` 展开）。
fn profile_headers(profile: &agent_config::ModelProfile) -> Vec<(String, String)> {
    profile
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), agent_config::expand_env(v)))
        .collect()
}

/// 装配 SOCKS5 运行时控制器（共享单例）。
///
/// 优先级：CLI `--socks` 显式值 ＞ `.gyre/socks5.state` 持久化开关 ＞ 配置默认。
/// 配置不完整（host/port 缺失）→ 内部告警并返回 `None`（代理自动禁用，不阻断启动）。
fn build_socks5_controller(
    cfg: &agent_config::Config,
    cwd: &std::path::Path,
    cli_override: Option<bool>,
) -> Option<Arc<agent_proxy::Socks5Controller>> {
    agent_proxy::Socks5Controller::new(
        &cfg.socks5,
        Some(cwd.join(".gyre").join("socks5.state")),
        cli_override,
    )
}

/// 解析 `--socks` 的 `host:port`（IPv6 需方括号 `[::1]:1080`）。
/// 返回 `(host, port)`；host 去方括号，port 必须为 1..=65535。
fn parse_socks_spec(spec: &str) -> Result<(String, u16), String> {
    let spec = spec.trim();
    let (host, port_str) = spec
        .rsplit_once(':')
        .ok_or_else(|| "缺少端口，期望 host:port（如 127.0.0.1:1080）".to_string())?;
    let port: u16 = port_str
        .parse()
        .map_err(|_| format!("端口无效: {port_str:?}"))?;
    if port == 0 {
        return Err("端口必须 > 0".into());
    }
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if host.is_empty() {
        return Err("host 为空".into());
    }
    Ok((host.to_string(), port))
}

/// 启动时打印脱敏的 SOCKS5 代理状态（绝不出现密码；文案随激活语言）。
fn print_socks5_status(socks5: &Option<Arc<agent_proxy::Socks5Controller>>) {
    match socks5 {
        Some(c) => eprintln!(
            "{}",
            agent_i18n::t!(
                "socks5.status",
                url = c.redacted(),
                state = if c.enabled() {
                    agent_i18n::t!("socks5.state_enabled")
                } else {
                    agent_i18n::t!("socks5.state_disabled")
                }
            )
        ),
        None => eprintln!("{}", agent_i18n::t!("socks5.status_unconfigured")),
    }
}

/// 启动时打印生效的审批模式（含来源：持久化/CLI 覆盖时可见）。
fn print_approval_status(cfg: &agent_config::Config) {
    let mode = match cfg.agent.approval_mode {
        agent_core::ApprovalMode::AlwaysAsk => agent_i18n::t!("approval.mode.always-ask"),
        agent_core::ApprovalMode::Write => agent_i18n::t!("approval.mode.write"),
        agent_core::ApprovalMode::Yolo => agent_i18n::t!("approval.mode.yolo"),
    };
    eprintln!("{}", agent_i18n::t!("approval.status", mode = mode));
}

/// ACP `session/available_commands` 清单：与 REPL 内置斜杠命令对齐（名称 + 简述）。
/// 描述面向编辑器命令菜单；新增 REPL 命令时同步此表。
/// 从 `help.*` 词条提取描述文本。
///
/// 词条形态：`"  /status          显示状态（…）"`、`"  /h, /help        显示此帮助"`。
/// 做法：按空白切 token，跳过命令段（以 `/` 开头或带尾逗号），其余即描述。
fn help_entry_description(raw: &str) -> String {
    let mut rest = raw.trim();
    while let Some((head, tail)) = rest.split_once(char::is_whitespace) {
        if head.starts_with('/') || head.ends_with(',') {
            rest = tail.trim_start();
            continue;
        }
        break;
    }
    rest.trim().to_string()
}

fn acp_command_catalog(cwd: &std::path::Path) -> Vec<(String, String)> {
    // H13：清单**从 REPL 同一注册表派生**（内置 + 项目自定义命令），不再手写 15 条硬编码
    // ——此前 ACP 菜单比 REPL 实际命令面少一半，且新增命令要两处登记（必然漂移）。
    // 描述取 `help.<name>` 词条（缺词条则留空，不编造）。
    let custom = agent_config::discover_commands(cwd);
    repl::all_command_names(&custom)
        .into_iter()
        .map(|full| {
            // 别名/同义命令复用同一份既有词条（不新造文案）。
            let bare = full.trim_start_matches('/');
            let key = match bare {
                "?" | "h" | "help" => "help.h".to_string(),
                "quit" | "exit" => "help.exit".to_string(),
                "skills" => "help.skill".to_string(),
                "resume" => "help.session".to_string(),
                other => format!("help.{other}"),
            };
            let raw = agent_i18n::tr(&key, &[]);
            let desc = if raw == key {
                // 无词条（如项目自定义命令）：留空，不编造描述。
                String::new()
            } else {
                help_entry_description(&raw)
            };
            (full, desc)
        })
        .collect()
}

/// 启动 Web 服务。`acp` 为 true（或配置启用）时合并 ACP HTTP+SSE 路由。
async fn run_server(
    cfg: agent_config::Config,
    cwd: PathBuf,
    acp: bool,
    socks_override: Option<bool>,
) -> Result<()> {
    let bind = cfg.server.bind.clone();
    let acp_enabled = acp || cfg.acp.enabled;
    // SOCKS5 代理控制器（Web 设置页开关经 /api/socks5 驱动；切换实时生效、落盘持久化）。
    let socks5 = build_socks5_controller(&cfg, &cwd, socks_override);
    print_socks5_status(&socks5);
    print_approval_status(&cfg);
    // 服务端共享 HTTP 客户端：仅设连接超时 + keepalive，不设整条请求总超时。该客户端
    // 专供流式 LLM 调用——总超时会切断仍在正常输出的慢速长流（收不到 `data: [DONE]`
    // 终止帧而误判「未收到结束标记」）；真正的「上游挂起」由各 SSE 适配器的按 chunk
    // 空闲读超时（agent_llm::STREAM_IDLE_TIMEOUT）兜底，连接阶段挂起由 connect_timeout 兜底。
    let http = agent_proxy::build_http_client(socks5.clone(), cfg.user_agent.as_deref(), |b| {
        b.tcp_keepalive(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
    })
    .context(t!("error.build_http"))?;
    // ACP `session/available_commands` 数据源（H13）：先取清单再移交 cwd 所有权。
    let acp_commands = acp_command_catalog(&cwd);
    let state = agent_server::SessionManager::new(Arc::new(cfg), http, Arc::from(cwd), socks5);
    state.set_available_commands(acp_commands).await;
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
async fn run_acp_stdio(
    cfg: agent_config::Config,
    cwd: PathBuf,
    socks_override: Option<bool>,
) -> Result<()> {
    let socks5 = build_socks5_controller(&cfg, &cwd, socks_override);
    print_socks5_status(&socks5);
    print_approval_status(&cfg);
    // 流式 LLM 客户端：不设整条请求总超时（会误杀慢速长流），上游静默由按 chunk 空闲
    // 读超时（agent_llm::STREAM_IDLE_TIMEOUT）兜底，连接阶段挂起由 connect_timeout 兜底。
    let http = agent_proxy::build_http_client(socks5.clone(), cfg.user_agent.as_deref(), |b| {
        b.tcp_keepalive(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
    })
    .context(t!("error.build_http"))?;
    let acp_commands = acp_command_catalog(&cwd);
    let state = agent_server::SessionManager::new(Arc::new(cfg), http, Arc::from(cwd), socks5);
    state.set_available_commands(acp_commands).await;
    agent_acp::run_stdio(state)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// 用户级配置目录（`platform::config_dir()`；不可得时退当前目录，与 auth 链一致）。
fn manage_config_dir() -> PathBuf {
    agent_core::platform::config_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// 执行管理子命令（语法识别在 [`agent_cli::manage::route`]；此处只装配 IO 与退出码）。
///
/// 返回进程退出码：0 成功；1 运行错（经 `Err` 亦以 1 退出）；2 用法错（路由层已拦截）。
/// 依赖配置的命令在此处自行 `Config::load`——路由位于 main 的启动装载之前，
/// config.toml 损坏/缺失时 `config check` / `auth save` 等仍须可用。
async fn run_manage_cmd(cmd: ManageCmd, cwd: &std::path::Path) -> Result<i32> {
    use std::io::Read as _;
    let config_dir = manage_config_dir();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match cmd {
        ManageCmd::ModelsList => {
            let cfg = agent_config::Config::load(cwd).context(t!("error.load_config"))?;
            agent_cli::manage::models_list(&cfg, &mut out)?;
            Ok(0)
        }
        // H25：内置模型目录（离线；无需 provider 与网络）。
        ManageCmd::ModelsCatalog(filter) => {
            agent_cli::manage::models_catalog(&filter, &mut out)?;
            Ok(0)
        }
        ManageCmd::AuthList => {
            let cfg = agent_config::Config::load(cwd).context(t!("error.load_config"))?;
            agent_cli::manage::auth_list(&cfg, &config_dir, &mut out)?;
            Ok(0)
        }
        ManageCmd::AuthSave { provider } => {
            // 密钥只经 stdin 读入（--stdin 已在路由层强制）；不回显、不落日志。
            let mut key = String::new();
            std::io::stdin()
                .read_to_string(&mut key)
                .context(t!("manage.auth.stdin_read_failed"))?;
            let path = agent_cli::manage::auth_save(&config_dir, &provider, &key)?;
            eprintln!(
                "{}",
                t!(
                    "manage.auth.saved",
                    provider = provider,
                    path = path.display().to_string()
                )
            );
            Ok(0)
        }
        ManageCmd::AuthRemove { provider } => {
            agent_cli::manage::auth_remove(&config_dir, &provider)?;
            eprintln!(
                "{}",
                t!(
                    "manage.auth.removed",
                    provider = provider,
                    path = agent_config::auth_path(&config_dir).display().to_string()
                )
            );
            Ok(0)
        }
        ManageCmd::AuthLogin { id } => {
            agent_cli::manage::auth_login(&id, &config_dir).await?;
            Ok(0)
        }
        ManageCmd::AuthLogout { provider } => {
            let outcome = agent_cli::manage::auth_logout(&config_dir, &provider)?;
            if outcome.oauth_removed {
                eprintln!(
                    "{}",
                    t!(
                        "manage.auth.logout.oauth_removed",
                        provider = provider,
                        path = agent_config::oauth_path(&config_dir).display().to_string()
                    )
                );
            }
            if outcome.api_key_removed {
                eprintln!(
                    "{}",
                    t!(
                        "manage.auth.logout.key_removed",
                        provider = provider,
                        path = agent_config::auth_path(&config_dir).display().to_string()
                    )
                );
            }
            Ok(0)
        }
        ManageCmd::McpLogin { server } => {
            agent_cli::manage::mcp_login(&server, &config_dir).await?;
            Ok(0)
        }
        ManageCmd::McpLogout { server } => {
            agent_cli::manage::mcp_logout(&server, &config_dir)?;
            Ok(0)
        }
        // MCP 配置管理（add / remove / enable / disable / list / status）：实现集中在
        // `manage::run_mcp_cmd`，与 REPL `/mcp` 子命令共用，避免两处漂移。
        ManageCmd::McpAdd(_)
        | ManageCmd::McpRemove { .. }
        | ManageCmd::McpEnable { .. }
        | ManageCmd::McpDisable { .. }
        | ManageCmd::McpList
        | ManageCmd::McpStatus => {
            agent_cli::manage::run_mcp_cmd(&cmd, cwd, &config_dir, &mut out)?;
            Ok(0)
        }
        ManageCmd::ConfigPath => {
            agent_cli::manage::config_path(&config_dir, &mut out)?;
            Ok(0)
        }
        ManageCmd::ConfigShow => {
            agent_cli::manage::config_show(cwd, &mut out)?;
            Ok(0)
        }
        ManageCmd::ConfigGet { key } => {
            agent_cli::manage::config_get(cwd, &key, &mut out)?;
            Ok(0)
        }
        ManageCmd::ConfigSet { key, value } => {
            agent_cli::manage::config_set(cwd, &key, &value, &mut out)?;
            Ok(0)
        }
        ManageCmd::ConfigKeys { prefix } => {
            agent_cli::manage::config_keys(prefix.as_deref(), &mut out)?;
            Ok(0)
        }
        ManageCmd::ConfigCheck => match agent_config::Config::load_with_warnings(cwd) {
            Ok((cfg, warnings)) => {
                agent_cli::manage::config_check_summary(&cfg, &mut out)?;
                agent_cli::manage::config_check_warnings(&warnings, &mut out)?;
                Ok(0)
            }
            // 校验失败：打印错误并以非零码（运行错 1）退出。
            Err(err) => {
                eprintln!("{}", agent_cli::manage::config_check_error(&err));
                Ok(1)
            }
        },
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // i18n：parse 前先按系统语言初始化（clap 帮助文本本地化需要；配置加载后 --lang 覆盖在下方）。
    agent_i18n::init(None);
    let cli = parse_cli();

    // 遥测初始化：--otlp 启用 OTLP span 导出，否则仅本地日志
    let _telemetry_guard =
        agent_telemetry::init(cli.otlp.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let cwd = cli
        .cwd
        .unwrap_or_else(|| std::env::current_dir().expect("无法获取当前目录"));

    // H36：加载 `.env`（项目 → 配置目录 → HOME；真实环境优先），随后 `${VAR}` /
    // `${VAR:-default}` 展开即可命中 dotenv 值。摘要一行上 stderr，便于排查「变量没生效」。
    {
        let dotenv = agent_config::load_dotenv(&cwd);
        if !dotenv.is_empty() {
            eprintln!(
                "已加载 {} 个环境变量（来源 {}）",
                dotenv.len(),
                dotenv
                    .files()
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    // 管理子命令路由（对齐 oh-my-pi cli-commands.ts 的显式注册表语义）：
    // 识别成功即执行并退出；语法不匹配（裸保留词 / 未知子动作 / 真实多词 prompt）
    // 一律回落原路径——下方 prompt 路径上的 reserved 顶层词提示（#4845 防误注入）
    // 与 prompt 行为完全不变。
    // 路由必须位于 Config::load 之前：config.toml 损坏/缺失时 config check 与
    // auth save 等管理命令仍须可用（启动装载在下方会先 bail）。
    // i18n 先按 --lang 激活（管理命令输出跟随 --lang；config 语言在装载后才可得）。
    agent_i18n::init(cli.lang.as_deref());
    match agent_cli::manage::route(&cli.task) {
        Route::Fallthrough => {}
        Route::Usage(msg) => {
            eprintln!("{msg}");
            std::process::exit(2); // 用法错误
        }
        Route::Help(text) => {
            // H2：子命令帮助 → stdout + 退出 0（与 `--help` 惯例一致）。
            println!("{text}");
            return Ok(());
        }
        Route::Run(cmd) => {
            let code = run_manage_cmd(cmd, &cwd).await?;
            if code != 0 {
                std::process::exit(code);
            }
            return Ok(());
        }
    }

    // 1. 加载配置（分层 TOML）
    // H23：未知配置键在启动期显式提示（只告警，不影响加载）。
    let (mut cfg, config_warnings) =
        agent_config::Config::load_with_warnings(&cwd).context(t!("error.load_config"))?;
    agent_cli::manage::report_config_warnings(&config_warnings);
    // H40：`--append-system-prompt` 覆盖配置值（含换行 = 字面文本；否则先试文件）。
    if let Some(raw) = cli.append_system_prompt.as_deref() {
        cfg.agent.append_system_prompt = Some(resolve_prompt_input(raw));
    }
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
    // H33：PTY 执行器接入（`run_command(pty: true)` 依赖；一次性注入，进程级共享）。
    agent_tools::set_pty_executor(agent_pty::executor());
    // 审批模式：CLI --approval-mode 显式值 ＞ `.gyre/approval-mode.state` 持久化 ＞ 配置默认。
    // （Web 设置页开关写入 sidecar——忘记每次指定时下次启动自动恢复；CLI 显式给出时压过它。）
    let persisted_approval = agent_config::ApprovalModeController::new(Some(
        cwd.join(".gyre").join("approval-mode.state"),
    ))
    .current();
    if let Some(mode) = &cli.approval_mode {
        cfg.agent.approval_mode = match mode.as_str() {
            "yolo" => agent_core::ApprovalMode::Yolo,
            "write" => agent_core::ApprovalMode::Write,
            _ => agent_core::ApprovalMode::AlwaysAsk,
        };
    } else if let Some(m) = persisted_approval {
        cfg.agent.approval_mode = m;
    }
    // SOCKS5 CLI 覆盖（--socks 给出即覆盖 host/port 并隐含启用；`--socks-user/--socks-pass`
    // 覆盖认证字段。启用覆盖在装配层以 cli_override 传入控制器，避免压过 `.gyre/socks5.state`
    // 持久化开关的读取逻辑——CLI 显式给出时 CLI 优先，未给时 sidecar 生效）。
    let socks_cli_override = cli.socks.as_ref().map(|_| true); // 给出 --socks 即视为显式启用
    if let Some(spec) = &cli.socks {
        let (host, port) = parse_socks_spec(spec)
            .map_err(|e| anyhow::anyhow!("--socks 格式无效: {spec:?} — {e}"))?;
        cfg.socks5.host = host;
        cfg.socks5.port = Some(port);
    }
    if let Some(u) = &cli.socks_user {
        cfg.socks5.username = Some(u.clone());
    }
    if let Some(p) = &cli.socks_pass {
        cfg.socks5.password = secrecy::SecretString::from(p.clone());
    }

    // NDJSON RPC 模式（--rpc）：外部语言/机器人集成的 stdio 行协议（见 docs/rpc.md）。
    // stdout 是协议通道，与 --serve（HTTP）及 --acp（JSON-RPC stdio）互斥。
    if cli.rpc && (cli.serve.is_some() || cli.acp) {
        anyhow::bail!("--rpc 与 --serve / --acp 互斥，不能同时指定");
    }
    if cli.rpc {
        return rpc::run_rpc(cfg, cwd, socks_cli_override, cli.rpc_forward_ask).await;
    }

    // 纯 stdio ACP 模式（--acp 且未指定 --serve）：编辑器作为子进程调用，不启动 HTTP。
    // 与 `--serve --acp`（HTTP+SSE）区分——stdio 优先，仅在没有 serve 时触发。
    if cli.acp && cli.serve.is_none() {
        return run_acp_stdio(cfg, cwd, socks_cli_override).await;
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
        return run_server(cfg, cwd, cli.acp, socks_cli_override).await;
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
    } else if cli.r#continue {
        // H31：`--continue` = 最近使用的会话（面包屑优先，mtime 回退）。
        match session_store.resolve_last() {
            Some(id) => {
                eprintln!("{}", t!("session.continue_resolved", id = id));
                id
            }
            None => anyhow::bail!(t!("session.continue_none")),
        }
    } else {
        agent_context::SessionStore::new_id()
    };
    // H31：记录面包屑（本次实际使用的会话），供下次 `--continue` 精确命中。
    session_store.mark_last(&session_id);
    eprintln!("{}", t!("session.label", id = session_id));
    // eval 内核的会话标签：RwLock 共享（/resume 换会话后重建 Agent 时读到新标签，
    // 内核池按新 session_key 隔离，避免跨会话共享命名空间）。
    let session_label = Arc::new(std::sync::RwLock::new(session_id.clone()));

    // P1：checkpoint/rewind 共享状态（单活跃检查点，跨 Agent 重建存活）。
    let checkpoint_state = agent_tools::CheckpointState::new().shared();
    // Phase 0：security_scan 共享状态（v1 仅记录最近一次扫描摘要；跨 Agent 重建存活）。
    let security_scan_state = agent_tools::SecurityScanState::new().shared();
    // P0：todo 清单共享状态（todo 工具与 /todo 命令共用；.gyre/todo.json 持久化，跨 Agent 重建存活）。
    let todo_state = agent_tools::TodoState::load(cwd.join(".gyre").join("todo.json")).shared();
    // P1：进程内消息总线（hub 工具以 "main" 入册；跨 Agent 重建存活）。
    let hub = agent_core::hub::Hub::new().shared();
    // H41：名册容量（0 = 不限；+1 计父 agent 自身）→ `task` 满员时在派生前预拒。
    hub.set_capacity(cfg.subagent.max_registry.saturating_add(1));

    // 2. 解析模型 profile（P2：含 fallback 链——主模型失败且错误可重试时依序换备用模型）
    let chain = cfg
        .resolve_chain(cli.model.as_deref())
        .context(t!("error.resolve_model"))?;
    let profile = chain[0];
    // 3. 装配 Provider（registry + OpenAI Chat Completions 适配器）
    // SOCKS5 代理控制器（仅影响后端出站请求；开关经 --socks / 配置 / sidecar）。
    let socks5 = build_socks5_controller(&cfg, &cwd, socks_cli_override);
    print_socks5_status(&socks5);
    print_approval_status(&cfg);
    // 共享 HTTP 客户端：仅设连接超时 + keepalive，不设整条请求总超时——该客户端专供流式
    // LLM 调用，总超时会切断仍在正常输出的慢速长流（收不到终止帧而误判「未收到结束标记」）；
    // 真正的「上游挂起」由各 SSE 适配器的按 chunk 空闲读超时（STREAM_IDLE_TIMEOUT）兜底。
    // （构建顺序前移：OAuth「先刷后用」解析需要该客户端。）
    let client = agent_proxy::build_http_client(socks5, cfg.user_agent.as_deref(), |b| {
        b.tcp_keepalive(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
    })
    .context(t!("error.build_http"))?;
    let oauth_client = client.clone(); // collect_providers 会移动 client；热切换解析复用。
    // 认证链分层（config 值含 ${ENV} 展开 → oauth.toml 有效/先刷后用 → auth.toml
    // → GYRE_<PROVIDER>_API_KEY env）。注意：load 期望「配置目录」（内部自拼
    // auth.toml），误传文件路径会永远读空。
    let config_dir = agent_core::platform::config_dir().unwrap_or_else(|| PathBuf::from("."));
    // H24：`auth = "none"` 时不解析（也不发送）内置鉴权头；`oauth` 只认 OAuth 存储。
    let api_key: Option<String> = agent_llm::oauth::resolve_profile_api_key(
        &client,
        &config_dir,
        profile.auth,
        profile.api.as_str(),
        &profile.id,
        profile.api_key.expose_secret().is_empty(),
        profile.resolve_api_key().expose_secret(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let model = profile.to_model(cfg.agent.enable_thinking);
    let fallback_models: Vec<agent_core::Model> = chain
        .iter()
        .skip(1)
        .map(|p| p.to_model(cfg.agent.enable_thinking))
        .collect();
    // P2：API key 轮换环（`api_keys` 多 key 列表；空 = 单 key 不轮换）。
    let key_rings: std::collections::HashMap<String, Vec<String>> =
        chain.iter().map(|p| (p.id.clone(), p.key_ring())).collect();

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
        api_key: api_key.clone(),
        base_url: Some(profile.effective_base_url()),
        max_in_flight: None,
        headers: profile_headers(profile),
        quirks: profile.effective_quirks(),
        auth: profile.auth,
    };

    // 4. 装配 Tools / Context / Prompt
    let mode = cfg.agent.mode;
    let prompts = Arc::new(agent_prompt::PromptCatalog::new());
    let session_path = session_store.path_for(&session_id);
    // H31：header 身份字段（cwd / 父会话 / prompt-cache key）。
    let mut session_header = agent_context::SessionHeader::new("gyre").with_path(&session_path);
    session_header.cwd = Some(cwd.display().to_string());
    session_header.parent_session = cli.fork.clone().or_else(|| cli.resume.clone());
    session_header.provider_prompt_cache_key = Some(session_id.clone());
    let pctx = agent_context::PersistentContext::open_with_header(
        prompts.system_with_platform(mode),
        &session_path,
        session_header.clone(),
    )
    .await
    .context(t!("error.open_persistence"))?;
    // 4d. 装配长期记忆（可选；按 cwd 项目作用域，backend 可切换）。
    // local：markdown 管道 + LLM 合并（P1 既有）；structured：records.jsonl + 向量融合检索。
    // 两者都带心智模型注入（P1-6，seeds + 项目 mental_models.md）。
    // 提前到 summarizer 之前：压缩前召回（preCompactionContext）需把记忆挂到摘要提供器。
    let mut memory: Option<Arc<dyn agent_core::MemoryStore>> = None;
    let mut local_memory: Option<Arc<agent_memory::LocalMemoryStore>> = None;
    let mut structured_memory: Option<Arc<agent_memory::StructuredMemoryStore>> = None;
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
                // 向量记忆：L2 fastembed 语义嵌入（vec-embed feature 编译时）或 L1 投影（离线），
                // 懒加载 + 失败降级，永不 panic。
                let store = Arc::new(
                    agent_memory::StructuredMemoryStore::new(&cwd)
                        .with_mental_models_config(agent_memory::MentalModelsConfig::default())
                        .with_embedder(agent_memory::default_embedder()),
                );
                structured_memory = Some(Arc::clone(&store));
                memory = Some(store);
            }
        }
        if let Ok(Some(_)) = memory.as_ref().unwrap().summary().await {
            eprintln!("{}", t!("memory.injected"));
        }
    }
    let mut summarizer = agent_context::compaction::LlmSummaryProvider::new(
        Arc::clone(&provider),
        model.clone(),
        provider_ctx.clone(),
        cfg.compaction.remote_endpoint.clone(),
        cfg.user_agent.clone(),
    );
    if let Some(m) = &memory {
        // P2-5：压缩前召回（对齐 mnemopi `preCompactionContext`）——挂记忆到摘要提供器。
        summarizer = summarizer.with_memory(Arc::clone(m));
    }
    pctx.set_summarizer(Box::new(summarizer)).await;
    // Shake 归档落盘到 <cwd>/.gyre/artifacts，使被压缩的大块可经 read_file artifact:// 回读。
    pctx.set_shake_sink(Arc::new(agent_context::compaction::DirSink::new(
        cwd.join(".gyre").join("artifacts"),
    )))
    .await;
    let mut context: Arc<dyn agent_core::ContextManager> = Arc::new(pctx);
    let workspace = Arc::new(agent_core::Workspace::new(cwd.clone()));
    // MCP 注册表（多 server 工具；包 Arc 供 build_agent 与 /mcp 共享）。
    // 启动预算（默认 250ms）外的 server 转后台连接：有适用缓存快照者立即以 Deferred
    // 工具注册（元信息可见、执行时等连接就绪），不再让慢 server 拖住启动（对齐 omp
    // issue #2100）；快照落盘由注册表自理（版本 + 配置指纹 + TTL，30 天）。
    let mcp_cache_dir = agent_core::platform::config_dir().unwrap_or_else(|| PathBuf::from("."));
    let mcp: Arc<agent_mcp::McpRegistry> = Arc::new(
        agent_mcp::McpRegistry::load(
            &cfg.mcp,
            &agent_mcp::McpLoadOptions {
                cache_dir: Some(mcp_cache_dir),
                // H16：把会话工作目录登记为 MCP root —— server 的 roots/list 才能拿到
                // 工作区范围（此前 server 请求一律被丢弃）。
                roots: vec![agent_mcp::McpRoot::file(&cwd, "workspace")],
                ..agent_mcp::McpLoadOptions::default()
            },
        )
        .await,
    );
    // MCP 工具动态源：装配层只持这一份句柄，父/子 Agent 注册表都挂它，运行期
    // server 端工具增删无需重建 Agent 即可见（对齐 omp `setOnToolsChanged`）。
    let mcp_source: Arc<dyn agent_tools::ToolSource> =
        Arc::new(agent_mcp::McpToolSource::new(Arc::clone(&mcp)));
    // 可选工具开关运行时快照：初值取自配置 [tools].enabled（覆盖各组默认 false）。
    // 后续可由 `/tools <key> on|off` 动态切换；切换后重建 Agent 以反映新工具集与提示词。
    let mut optional = agent_sdk::optional_tool_switches(&cfg);
    // 会话级文本快照存储：read_file（经 engine 的 ToolContext::snapshots 记录）与
    // apply_hashline（HashlineTool::with_snapshots 写前自存）共享同一 Arc——read 侧版本
    // 对 stale-hash 恢复可见。跨 Agent 重建（/model、/mode 热切换）保持，会话级单例。
    let snapshot_store = agent_sdk::new_snapshot_store();
    // H3 异步后台作业管理器：bash async:true 后台化 + 完成投递回会话 + hub 作业面。
    // 会话级单例；禁用（[agent] async_enabled=false）时 run_command async 参数报错。
    let job_manager = if cfg.agent.async_enabled {
        Some(agent_core::jobs::AsyncJobManager::with_max_running(
            cfg.agent.async_max_jobs,
        ))
    } else {
        None
    };

    // H3 投递 sink：完结作业结果以 async-result 通知形式追加进会话上下文
    //（对齐 omp registerDeliverySink 语义：owner 路由 + 恰好一次 + 失败退避重试）。
    let _job_sink_guard = job_manager.as_ref().map(|jm| {
        let ctx = Arc::clone(&context);
        let sink: agent_core::jobs::DeliverySink = Arc::new(move |job_id, text, job| {
            let ctx = Arc::clone(&ctx);
            Box::pin(async move {
                let label = job.as_ref().map(|j| j.label.clone()).unwrap_or_default();
                let status = job
                    .as_ref()
                    .map(|j| j.status.as_str())
                    .unwrap_or("completed");
                let body = format!(
                    "<system-notice>\n后台作业 {job_id}（{label}，{status}）已完结。请基于以下结果继续工作。\n\n{text}\n</system-notice>"
                );
                ctx.append(agent_core::AgentMessage::user_text(body)).await;
                Ok(())
            })
        });
        {
            let resumed = jm.register_delivery_sink("main", sink);
            // H27：Agent/会话重建后，把先前因 `watch ids` 被抑制的投递重新入队——
            // `enqueue_delivery` 在抑制状态下不入队，不恢复就会永久静默丢失结果。
            let suppressed = jm.suppressed_job_ids(Some("main"));
            if !suppressed.is_empty() {
                let refs: Vec<&str> = suppressed.iter().map(String::as_str).collect();
                jm.resume_deliveries(&refs);
                tracing::info!(jobs = suppressed.len(), "已恢复被抑制的作业投递");
            }
            resumed
        }
    });
    // 子 Agent 工具集（按启用态装配的可选工具 + MCP，不含 task 以防递归；与模型无关，构建一次）
    let (mut sub_reg, _) = assemble_builtin_tools(
        &optional,
        cfg.github.enabled,
        cfg.github.allow_write,
        cfg.agent.commands.interceptor.enabled,
        compiled_minimizer(&cfg),
        Some(Arc::clone(&snapshot_store)),
        job_manager.clone(),
    );
    // MCP 工具走动态源：server 端清单增删在子 Agent 下一轮即可见。
    sub_reg = sub_reg.with_source(Arc::clone(&mcp_source));
    let sub_tools: Arc<dyn agent_tools::ToolRegistry> = Arc::new(sub_reg);
    // 子 Agent 的空上下文工厂（task 工具用）
    let sub_context_factory: agent::ContextFactory = Arc::new(|| {
        Arc::new(agent_context::InMemoryContext::new(vec![])) as Arc<dyn agent_core::ContextManager>
    });
    // H18：命名子代理定义（项目 `.agent/agents/*.md` → 用户 → 内置 task/scout）与其模型覆盖表。
    let sub_agents = agent::discover_agents(&cwd);
    let sub_model_overrides: std::collections::HashMap<String, agent_core::Model> = chain
        .iter()
        .map(|p| (p.id.clone(), p.to_model(cfg.agent.enable_thinking)))
        .chain(chain.iter().filter_map(|p| {
            p.alias
                .clone()
                .map(|a| (a, p.to_model(cfg.agent.enable_thinking)))
        }))
        .collect();

    // 4b. 装配 Skill 目录（发现 + 过滤；失败不阻断启动）
    let skill_catalog = Arc::new(load_skill_catalog(&cfg, &cwd).await);
    // 4c. 装配上下文约定（AGENTS.md，注入 system prompt）+ 自定义 slash 命令
    // GitHub 提示词按启用态在 build_agent 内动态注入（见 github_context_files）；
    // 未启用时完全不进入 system prompt，零额外 Token 开销（按需加载）。
    // 启用态可由配置 [github] 初始化，或经 /github 命令运行时切换。
    // 上下文基座（H19/H42 收敛点）：行为杠杆四段（§ Tool Policy / # Delegation / § Workflow /
    // § Delivery + § Critical 收尾）在最前，其后依次是项目上下文（AGENTS.md/外来配置）、
    // 按启用态注入的可选工具提示、security_scan 指引、MCP server 指引。
    // 顺序与内容由 `agent_sdk::compose_context_files` 唯一决定——REPL / RPC / server 三处共用同一份。
    let mcp_instructions = mcp.server_instructions();
    let mut composed = agent_sdk::compose_context_files(agent_sdk::ContextAssembly {
        cwd: &cwd,
        mcp_instructions: &mcp_instructions,
    });
    // H18：把可用命名子代理清单注入父级提示词（模型据此选择 `task(agent=…)`）。
    if let Some(section) = agent::render_agent_catalog(&sub_agents) {
        composed.files.push(section);
    }
    if composed.foreign_count > 0 {
        eprintln!(
            "{}",
            t!("context.foreign_loaded", count = composed.foreign_count)
        );
    }
    let base_context_files = composed.files;
    let commands = agent_config::discover_commands(&cwd);
    if !commands.is_empty() {
        eprintln!("{}", t!("commands.loaded", count = commands.len()));
    }
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
    // 鉴权模式 `none` 时为空串（`list_models` / 热切换都据此不发鉴权头）。
    let mut current_api_key = api_key.clone().unwrap_or_default();
    let mut current_base_url = profile.effective_base_url();
    let mut current_max_output = profile.max_output_tokens.unwrap_or(4096);
    let mut current_provider_ctx = provider_ctx;

    // Agent 构造闭包：参数化 mode/model/provider_ctx/max_output，热切换后可重建。
    // task 工具内嵌 model/provider_ctx，故随模型/模式一起重建。
    // 子 Agent 监控总线：与 TaskTool / /agents 仪表盘共享同一份进程内状态。
    let supervisor = Arc::new(agent_supervisor::Supervisor::new());

    // P0-3 + H29：goals 状态共享句柄（跨 Agent 重建保持；`/goal` 与 `goal` 工具共用）。
    // H29 起恒在场：预算为 0 时按不限记账，`goal({op:"create"})` 与目标续跑无需额外配置
    //（与 omp 一致：目标模式不依赖预算开关，预算只是额度）。
    let goal_state: Option<Arc<std::sync::Mutex<agent::GoalState>>> =
        Some(Arc::new(std::sync::Mutex::new({
            let mut st = agent::GoalState::new(agent::GoalBudget {
                token_budget: cfg.goals.token_budget,
                time_budget: std::time::Duration::from_secs(cfg.goals.time_budget_secs),
                hard_stop: cfg.goals.hard_stop,
            });
            // H29：恢复上次会话持久化的目标（`.gyre/goal.json`）。`active` 恢复为
            // `paused`——重启不自动续跑，由用户显式 `goal resume`（`/goal resume`）。
            if let Some(snap) = agent::load_goal(&cwd) {
                let restored_active = snap.status == "active";
                st.restore(snap);
                if restored_active {
                    eprintln!(
                        "已恢复目标（状态 paused，需 `goal resume` 继续）：{}",
                        st.goal_summary().unwrap_or_default()
                    );
                } else {
                    eprintln!("已恢复目标：{}", st.goal_summary().unwrap_or_default());
                }
            }
            st
        })));

    // P0-1：eval 内核管理器（`[eval] enabled` 或 env `GYRE_EVAL=1` 启用）。
    // 内核池跨 Agent 重建存活（/model、/mode 切换不丢命名空间）；环回桥由 EvalTool
    // 首次 execute 时按当前审批懒加载 spawn。
    let eval_mgr: Option<Arc<agent_eval::EvalManager>> =
        if cfg.eval.enabled || std::env::var("GYRE_EVAL").is_ok_and(|v| v == "1") {
            let mgr = Arc::new(agent_eval::EvalManager::new(
                agent_eval::EvalSettings::from_parts(
                    cfg.eval.python.clone(),
                    cfg.eval.idle_timeout_secs,
                ),
            ));
            mgr.ensure_sweeper();
            Some(mgr)
        } else {
            None
        };

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
        // 审批策略按当前 mode 重建（code/debug 写类放行；ask 全只读；architect 仅 plans/ 下
        // markdown 可写），使 `/mode` 切换后审批门槛立即跟随——与 server assemble 一致。
        let mut agent_cfg = cfg.agent.clone();
        agent_cfg.mode = mode;
        let rules = agent_config::RulesEngine::new(Arc::new(agent_cfg))
            .with_workspace_root(Some(workspace.root().to_path_buf()));
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
        let (mut tool_registry, lsp_pool) = assemble_builtin_tools(
            optional,
            github_enabled,
            github_allow_write,
            cfg.agent.commands.interceptor.enabled,
            compiled_minimizer(&cfg),
            Some(Arc::clone(&snapshot_store)),
            job_manager.clone(),
        );
        // MCP 工具走动态源（每次 specs()/get() 实时求值）：运行中 Agent 无需重建即可
        // 看到 server 端工具增删（tools/list_changed / 后台连接完成 / 重连恢复）。
        tool_registry = tool_registry.with_source(Arc::clone(&mcp_source));
        // P1：hub 消息总线（以 "main" 入册；子 Agent 不含——其消费面留待 task 集成）。
        let processes = Arc::new(agent_supervisor::ProcessManager::new());
        // H29：goal 工具（要求 goals 状态在场；无 `[goals]` 配置时 `goal_state` 为 None，
        // 模型仍可用 op=create 建立目标——会话预算按 unlimited 记账）。
        if let Some(gs) = &goal_state {
            tool_registry = tool_registry.with(Box::new(agent::GoalTool::new(Arc::clone(gs))));
        }
        tool_registry = tool_registry.with(Box::new(
            agent_tools::HubTool::register(Arc::clone(&hub), "main")
                .with_supervision(supervisor.clone())
                .with_processes(processes)
                .with_jobs_option(job_manager.clone()),
        ));
        // Phase 0：todo / ask / checkpoint / rewind 接线（修复 CLI 前端工具面缺失——
        // 此前仅 server 装配，双前端工具面不一致；状态取闭包外共享句柄，跨 Agent
        // 重建存活）。ask 经 ApprovalPolicy::prompt 通道走 CLI 的 stdin resolver
        //（见上方 prompt_resolver），宿主零新增接线。
        tool_registry = tool_registry
            .with(Box::new(agent_tools::TodoTool::new(Arc::clone(
                &todo_state,
            ))))
            .with(Box::new(agent_tools::AskUserTool::new()))
            .with(Box::new(agent_tools::CheckpointTool::new(Arc::clone(
                &checkpoint_state,
            ))))
            .with(Box::new(agent_tools::RewindTool::new(Arc::clone(
                &checkpoint_state,
            ))));
        // Phase 0：security_scan 接线（两端装配修复：此前工具已实现但无任何前端注册）。
        tool_registry = tool_registry.with(Box::new(agent_tools::SecurityScanTool::new(
            Arc::clone(&security_scan_state),
        )));
        // 记忆工具面（P0-1/P0-1b）：recall / retain / reflect + memory_edit / learn
        // 经 ToolContext.memory 访问同一存储；未启用记忆时不注册（零上下文成本）。
        // 子 Agent 工具集（sub_reg）不含记忆工具。
        if let Some(m) = &memory {
            tool_registry = tool_registry
                .with(Box::new(agent_tools::MemoryRecallTool::new(Arc::clone(m))))
                .with(Box::new(agent_tools::MemoryRetainTool::new(Arc::clone(m))))
                .with(Box::new(agent_tools::MemoryReflectTool::new(Arc::clone(m))))
                .with(Box::new(agent_tools::MemoryEditTool::new(Arc::clone(m))))
                .with(Box::new(agent_tools::MemoryLearnTool::new(Arc::clone(m))));
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
            .with_supervisor(Arc::clone(&supervisor))
            .with_approval(Arc::clone(&approval))
            .with_agents(sub_agents.clone())
            .with_model_overrides(sub_model_overrides.clone())
            // H43：子代理以 task id 入册，可被 `hub send` 寻址（消息在轮次边界注入）。
            .with_hub(Arc::clone(&hub));
            tool_registry = tool_registry.with(Box::new(task_tool));
        }
        // P0-1：eval 工具（懒加载环回桥）。桥回调宿主工具用的注册表为「不含 eval」的快照
        //（防自我递归）；eval 经 WithEval 适配器附加到 Agent 注册表之上。会话标签经
        // RwLock 读取，/resume 换会话后重建 Agent 即用新标签隔离内核池。
        let tools: Arc<dyn agent_tools::ToolRegistry> = if let Some(eval_mgr) = &eval_mgr {
            let bridge_registry: Arc<dyn agent_tools::ToolRegistry> = Arc::new(tool_registry);
            let eval_tool = agent_eval::EvalTool::with_session(
                Arc::clone(eval_mgr),
                Arc::clone(&bridge_registry),
                Arc::clone(&workspace),
                Arc::clone(&approval),
                session_label
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            );
            Arc::new(WithEval {
                inner: bridge_registry,
                eval: Arc::new(eval_tool),
            })
        } else {
            Arc::new(tool_registry)
        };

        // TTSR 流规则：发现 `<cwd>/.gyre/rules/*.md` 并装配协调器（缺省有规则即启用；
        // `[ttsr] enabled = false` 或 `disabled_rules` 可关闭/过滤）。规则零上下文成本：
        // 命中才注入，违规输出中断重试（见 agent-ttsr crate 文档）。
        let ttsr = if cfg.ttsr.enabled.unwrap_or(true) {
            // H44：内置规则集（27 条语言约定）默认加载，同名项目规则覆盖之。
            let builtin_enabled = cfg.ttsr.builtin_rules.unwrap_or(true);
            let rules =
                agent_ttsr::load_rules(&workspace.root().join(".gyre/rules"), builtin_enabled);
            if rules.is_empty() {
                None
            } else {
                report_ttsr_rules(&rules);
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

        // P1-L：advisor 独立评审（env `GYRE_ADVISOR=1` 启用；WATCHDOG.md 准则自动发现）。
        // 复用主 provider（独立 LLM 调用），快照脱敏后喂评审；建议经 `[advisor:…]` 注入。
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
            .stream_guards(cfg.agent.stream_guards.clone())
            .advisor(advisor)
            // 与 server assemble 一致：把模型输出预算下发给 Agent 作为每轮请求 max_tokens，
            // 否则回落到硬编码 4096，长回复被截断（finish_reason=length）→ 误报「任务完成」。
            .max_output_tokens(model.max_output_tokens)
            .max_mistakes(max_mistakes)
            .context_guard(context_guard)
            .compaction_policy(agent::AgentBuilder::compaction_policy_from_config(
                context_guard,
                cfg.agent.compaction_threshold_tokens,
                cfg.agent.compaction_reserve_tokens,
            ))
            .catalog(Arc::clone(&skill_catalog))
            .context_files(optional_context_files(
                &base_context_files,
                optional,
                github_enabled,
            ))
            // 与 HashlineTool 共享会话快照存储（见上方 snapshot_store 单例）。
            .snapshot_store(Some(Arc::clone(&snapshot_store)))
            .resources(Arc::clone(&mcp) as Arc<dyn agent_core::ResourceResolver>);
        // P2：压缩后端——snapcompact 要求视觉模型（[compaction].vision_models 通配匹配，
        // 空列表 = 任何模型启用）；不匹配回退 summarize（每次重建按当前 model 判断）。
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
                    eprintln!(
                        "[compaction] backend=snapcompact 但模型 {} 不匹配 vision_models，回退 summarize",
                        model.id
                    );
                    agent_core::CompactionBackend::Summarize
                }
            } else {
                backend
            }
        };
        let builder = builder
            .compaction_backend(compaction_backend)
            .compaction_max_frames(cfg.compaction.max_frames);
        // P0-3：goals 预算注入（共享状态，`/goal` 可运行时调整）。
        let builder = if let Some(gs) = &goal_state {
            builder.goals_state(Arc::clone(gs))
        } else {
            builder
        };
        // H40：追加 system prompt 定制段（CLI flag / `[agent] append_system_prompt`）。
        let builder = if let Some(extra) = &cfg.agent.append_system_prompt {
            builder.append_system_prompt(extra.clone())
        } else {
            builder
        };
        // H28 尾项：注入异步唤醒探测（有后台作业在途时不在停止边界提醒待办）。
        let builder = if let Some(jm) = &job_manager {
            builder.async_wake(Arc::clone(jm) as Arc<dyn agent_core::AsyncWakeProbe>)
        } else {
            builder
        };
        // H28：todo 循环接线（eager prelude + 完成提醒；与 `todo` 工具共享同一清单）。
        let builder = builder
            .todo_loop_source(Arc::clone(&todo_state) as Arc<dyn agent_core::TodoLoopSource>)
            .todo_loop_config(agent::TodoLoopConfig::from_config(
                cfg.todo.eager.as_deref(),
                cfg.todo.reminders,
                cfg.todo.reminders_max,
                cfg.todo.mid_run_nudge,
            ));
        // 编辑后 LSP writethrough：lsp 启用（lsp_pool 为 Some）且 edit 开启时，共享 LspTool 的 pool 注入。
        let builder = if let Some(pool) = lsp_pool.as_ref().filter(|_| {
            cfg.agent.tools.edit.format_on_write || cfg.agent.tools.edit.diagnostics_on_write
        }) {
            builder.write_effect(std::sync::Arc::new(agent_tools::LspWriteEffect::new(
                workspace.root().to_path_buf(),
                std::sync::Arc::clone(pool),
                cfg.agent.tools.edit.format_on_write,
                cfg.agent.tools.edit.diagnostics_on_write,
                cfg.agent.tools.edit.diagnostics_deduplicate,
            ))
                as std::sync::Arc<dyn agent_core::WriteEffect>)
        } else {
            builder
        };
        let builder = if let Some(m) = &memory {
            builder.memory(Arc::clone(m))
        } else {
            builder
        };
        // 记忆 hooks（P0-2/P0-4）：auto-retain（每 N 个停止轮沉淀本轮输出）+ 会话末维护。
        // local：ConsolidateHook（LLM 合并 raw notes → MEMORY.md + 心智模型提炼）；
        // structured：StructuredSleepHook（清理过期 + 去重 + 模型变更重嵌）。
        let mut hooks: Vec<Arc<dyn agent_core::Hook>> = Vec::new();
        if let Some(m) = &local_memory {
            hooks.push(Arc::new(ConsolidateHook {
                store: Arc::clone(m),
                provider: Arc::clone(&provider),
                model: model.clone(),
                provider_ctx: provider_ctx.clone(),
                auto_consolidate,
                background: cfg.memory.background_consolidate,
            }));
        }
        if let Some(m) = &structured_memory {
            hooks.push(Arc::new(StructuredSleepHook {
                store: Arc::clone(m),
                auto_consolidate: cfg.memory.auto_consolidate,
                provider: Some(Arc::clone(&provider)),
                model: Some(model.clone()),
                provider_ctx: Some(provider_ctx.clone()),
                background: cfg.memory.background_consolidate,
            }));
        }
        if let Some(m) = &memory {
            if cfg.memory.auto_retain_every_n_turns > 0 {
                hooks.push(Arc::new(AutoRetainHook {
                    store: Arc::clone(m),
                    every_n_turns: cfg.memory.auto_retain_every_n_turns,
                    final_rounds: std::sync::atomic::AtomicUsize::new(0),
                }));
            }
        }
        // 文件式 hook（[[hooks]]）：shell 命令钩子（before_tool 可 deny/allow；其余通知型）。
        hooks.extend(agent_cli::hooks_cfg::shell_hooks_from_config(&cfg));
        let builder = if hooks.is_empty() {
            builder
        } else {
            builder.hooks(hooks)
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

    // 8. 单次任务（非交互）：执行后退出。非交互路径没有运行期队列（stdin 非 tty 时
    // `consume_stream` 也不会读输入），给一个空队列满足签名。
    if !cli.task.is_empty() {
        let task = cli.task.join(" ");
        // 保留顶层词防误注入（移植 oh-my-pi cli-commands.ts #4845 语义）：
        // 裸管理动词/插件语法不作为 prompt 发给 LLM 计费。
        if let Some(hint) = reserved_top_level_word_hint(&task) {
            anyhow::bail!("{hint}");
        }
        let no_queue: Arc<std::sync::Mutex<agent_cli::queue::MessageQueue>> =
            Arc::new(std::sync::Mutex::new(agent_cli::queue::MessageQueue::new()));
        run_turn(&agent, &task, &accumulated, &no_queue).await?;
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
            "提示：stdin 非终端（检测到管道/子进程输入）。若作为 ACP 服务端被编辑器\
             （如 Zed）调用，须加 --acp 参数；否则此处进入交互 REPL。"
        );
    }
    let model_alias_choices = model_choices(&cfg);
    let skill_names: Vec<String> = skill_catalog
        .skills
        .iter()
        .map(|s| s.name.clone())
        .collect();
    let mut rl: Editor<ReplHelper, DefaultHistory> = Editor::new()?;
    let prompt = "\n> ";
    // H4：运行期消息队列（运行中键入在此排队；空闲时 `/queue pop` 或 Ctrl-Y 取回）。
    let message_queue: Arc<std::sync::Mutex<agent_cli::queue::MessageQueue>> =
        Arc::new(std::sync::Mutex::new(agent_cli::queue::MessageQueue::new()));
    // H5：键位表（默认 + `[keybindings]` 覆盖）——解析后一次性绑到 rustyline。
    let keybindings = agent_cli::keybindings::resolve(&cfg.keybindings.bindings);
    for diag in keybindings
        .unknown_actions
        .iter()
        .map(|a| format!("未知键位动作 {a}（已忽略）"))
        .chain(
            keybindings
                .invalid_keys
                .iter()
                .map(|(a, e)| format!("键位 {a} 非法（已回退默认）：{e}")),
        )
    {
        eprintln!("[keybindings] {diag}");
    }
    apply_keybindings(&mut rl, &keybindings, &message_queue);
    // 上一轮结束后若有排队消息，下一轮直接消费它（自动续跑），不再等用户输入。
    let mut pending_task: Option<String> = None;

    loop {
        // 每次读行前重建补全器：MCP 提示词随 server 连接/清单刷新随时出现，
        // 命令名集合必须比启动时的一次性快照新（`/mcp` 之外的动态命令面）。
        rl.set_helper(Some(ReplHelper::new(
            repl::all_command_names_with_prompts(&commands, &mcp.prompts()),
            model_alias_choices.clone(),
            skill_names.clone(),
            session_store.list().iter().map(|s| s.id.clone()).collect(),
        )));
        let line = if let Some(queued) = pending_task.take() {
            eprintln!(
                "{}",
                t!("queue.running_next", text = queued.replace('\n', " ⏎ "))
            );
            queued
        } else {
            match rl.readline(prompt) {
                Ok(l) => l,
                Err(rustyline::error::ReadlineError::Interrupted) => continue,
                Err(rustyline::error::ReadlineError::Eof) => break,
                Err(e) => return Err(anyhow::anyhow!(e)),
            }
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
                    goal: goal_state.clone(),
                    todo: Arc::clone(&todo_state),
                    jobs: job_manager.as_ref(),
                    queue: &message_queue,
                    keybindings: &keybindings,
                };
                handle_command(line, &ctx)
            };
            match outcome {
                CommandOutcome::Handled => (String::new(), true),
                CommandOutcome::Inject(t) => (t, false),
                CommandOutcome::McpPrompt {
                    server,
                    prompt,
                    args,
                } => match mcp.execute_prompt(&server, &prompt, &args).await {
                    // 提示词内容即下一轮任务（模板由 server 侧渲染）。
                    Ok(text) => (text, false),
                    Err(e) => {
                        eprintln!("{e}");
                        (String::new(), true)
                    }
                },
                CommandOutcome::SwitchModel(alias) => {
                    if apply_model_switch(
                        &alias,
                        &cfg,
                        &oauth_client,
                        &config_dir,
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
                                    .set_summarizer(Box::new({
                                        let mut s =
                                            agent_context::compaction::LlmSummaryProvider::new(
                                                Arc::clone(&provider),
                                                current_model.clone(),
                                                current_provider_ctx.clone(),
                                                cfg.compaction.remote_endpoint.clone(),
                                                cfg.user_agent.clone(),
                                            );
                                        if let Some(m) = &memory {
                                            s = s.with_memory(Arc::clone(m));
                                        }
                                        s
                                    }))
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
                                *session_label.write().unwrap_or_else(|e| e.into_inner()) =
                                    id.clone();
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
                CommandOutcome::Fresh => {
                    // P1-4：全新会话（新 id + 空上下文），保留模型/模式；本地历史记录不动。
                    let fresh_id = agent_context::SessionStore::new_id();
                    let path = session_store.path_for(&fresh_id);
                    match agent_context::PersistentContext::open(
                        prompts.system_with_platform(current_mode),
                        &path,
                    )
                    .await
                    {
                        Ok(new_pctx) => {
                            new_pctx
                                .set_summarizer(Box::new({
                                    let mut s = agent_context::compaction::LlmSummaryProvider::new(
                                        Arc::clone(&provider),
                                        current_model.clone(),
                                        current_provider_ctx.clone(),
                                        cfg.compaction.remote_endpoint.clone(),
                                        cfg.user_agent.clone(),
                                    );
                                    if let Some(m) = &memory {
                                        s = s.with_memory(Arc::clone(m));
                                    }
                                    s
                                }))
                                .await;
                            new_pctx
                                .set_shake_sink(Arc::new(agent_context::compaction::DirSink::new(
                                    cwd.join(".gyre").join("artifacts"),
                                )))
                                .await;
                            context = Arc::new(new_pctx);
                            session_id = fresh_id.clone();
                            *session_label.write().unwrap_or_else(|e| e.into_inner()) =
                                fresh_id.clone();
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
                            eprintln!("{}", t!("session.fresh", id = session_id));
                        }
                        Err(e) => eprintln!("{}", t!("session.fresh_failed", e = e)),
                    }
                    (String::new(), true)
                }
                CommandOutcome::Clear => {
                    // H34：原地清空上下文（保留会话 id 与系统提示词）；逐条删索引 0。
                    let mut removed = 0usize;
                    for _ in 0..10_000 {
                        match context.delete_message_at(0).await {
                            Ok(0) => break,
                            Ok(n) => removed += n,
                            Err(e) => {
                                eprintln!("清空上下文失败：{e}");
                                break;
                            }
                        }
                    }
                    eprintln!(
                        "已清空上下文（{removed} 条消息；会话 {session_id} 保留，历史文件未删）"
                    );
                    (String::new(), true)
                }
                CommandOutcome::Context => {
                    // H34：上下文占用 + 消息构成报告。
                    let nodes = context.snapshot_nodes().await;
                    let usage = context.token_usage();
                    let b = repl::context_breakdown(&nodes);
                    let pct = if usage.limit > 0 {
                        #[allow(clippy::cast_precision_loss)]
                        {
                            usage.current as f64 / usage.limit as f64 * 100.0
                        }
                    } else {
                        0.0
                    };
                    eprintln!("{}", t!("context.title"));
                    eprintln!(
                        "{}",
                        t!(
                            "context.nodes",
                            total = b.total,
                            user = b.user,
                            assistant = b.assistant,
                            tool = b.tool,
                            other = b.other
                        )
                    );
                    eprintln!(
                        "{}",
                        t!(
                            "context.tokens",
                            current = usage.current,
                            limit = usage.limit,
                            pct = format!("{pct:.1}")
                        )
                    );
                    if usage.limit > 0 {
                        eprintln!(
                            "{}",
                            t!(
                                "context.guard",
                                pct = format!("{:.0}", context_guard * 100.0)
                            )
                        );
                    }
                    eprintln!("{}", t!("context.footer"));
                    (String::new(), true)
                }
                CommandOutcome::Retry => {
                    // H34：重发最近一条真实用户输入（跳过注入提醒）。
                    let nodes = context.snapshot_nodes().await;
                    match repl::last_user_prompt(&nodes) {
                        Some(text) => {
                            eprintln!("{}", t!("retry.reusing"));
                            (text, false)
                        }
                        None => {
                            eprintln!("{}", t!("retry.none"));
                            (String::new(), true)
                        }
                    }
                }
                CommandOutcome::Diff { staged, ref_name } => {
                    // P1-4：git diff 展示（工作区 / --staged / 指定 ref），长输出截断。
                    let diff = run_git_diff(&workspace.root(), staged, ref_name.as_deref());
                    eprintln!("{}", t!("diff.title"));
                    eprintln!("{diff}");
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
                CommandOutcome::ShowTree => {
                    let nodes = context.snapshot_nodes().await;
                    let active = context.active_leaf().await;
                    eprintln!("{}", t!("tree.title"));
                    eprintln!(
                        "{}",
                        agent_cli::tree_ui::render_tree(&nodes, active.as_deref())
                    );
                    (String::new(), true)
                }
                CommandOutcome::SwitchBranch { node, handoff } => {
                    let nodes = context.snapshot_nodes().await;
                    match agent_cli::tree_ui::parse_node_target(&node, &nodes) {
                        None => {
                            eprintln!(
                                "{}",
                                t!(
                                    "tree.switch_failed",
                                    reason = format!("节点不存在或前缀歧义: {node}")
                                )
                            );
                        }
                        Some(target) if handoff => match context
                            .switch_branch_with_handoff(&target)
                            .await
                        {
                            Ok(true) => {
                                eprintln!("{}", t!("tree.switched", node = target));
                            }
                            Ok(false) => {
                                eprintln!(
                                    "{}",
                                    t!(
                                        "tree.switch_failed",
                                        reason = format!("节点不存在: {target}")
                                    )
                                );
                            }
                            Err(e) => {
                                eprintln!("{}", t!("tree.switch_failed", reason = e.to_string()));
                            }
                        },
                        Some(target) => {
                            if context.set_active_leaf(&target).await {
                                eprintln!("{}", t!("tree.switched", node = target));
                            } else {
                                eprintln!(
                                    "{}",
                                    t!(
                                        "tree.switch_failed",
                                        reason = format!("节点不存在: {target}")
                                    )
                                );
                            }
                        }
                    }
                    (String::new(), true)
                }
                CommandOutcome::ListModels => {
                    let http = reqwest::Client::builder()
                        .connect_timeout(std::time::Duration::from_secs(10))
                        .build()
                        .unwrap_or_default();
                    match agent_llm::list_models(
                        current_model.api,
                        &current_base_url,
                        &current_api_key,
                        http,
                    )
                    .await
                    {
                        Ok(list) if list.is_empty() => {
                            eprintln!("{}", t!("models.empty"));
                        }
                        Ok(list) => {
                            eprintln!(
                                "{}",
                                t!(
                                    "models.title",
                                    source = format!(
                                        "{} @ {}",
                                        current_model.api.as_str(),
                                        current_base_url
                                    )
                                )
                            );
                            for dm in &list {
                                let owner = dm
                                    .owned_by
                                    .as_deref()
                                    .map(|o| format!("  ({o})"))
                                    .unwrap_or_default();
                                eprintln!("  {}{owner}", dm.id);
                            }
                        }
                        Err(e) => {
                            eprintln!("{}", t!("models.failed", reason = e.to_string()));
                        }
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
                CommandOutcome::Enhance { draft } => {
                    // Roo-Code 风格：单一 LLM 增强（模板作为用户消息，见 agent_prompt::enhance）。
                    match agent_prompt::enhance::enhance_collect(
                        &draft,
                        provider.as_ref(),
                        &current_provider_ctx,
                        &current_model,
                    )
                    .await
                    {
                        Ok(r) => println!("✨ Enhanced:\n{r}"),
                        Err(e) => eprintln!("✨ 增强失败：{e}"),
                    }
                    (String::new(), true)
                }
                CommandOutcome::Suggest { query } => {
                    // 与服务器一致：find_files（尊重 .gitignore）枚举，score_files 评分。
                    let files: Vec<String> = agent_search::find_files(&cwd, None, None, 400)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect();
                    let suggestions = agent_prompt::suggest::score_files(&query, &files, 8);
                    if suggestions.is_empty() {
                        eprintln!("未找到与「{query}」相关的文件");
                    } else {
                        for s in &suggestions {
                            println!("  {:<40} {:.1}  {}", s.path, s.score, s.reason);
                        }
                    }
                    (String::new(), true)
                }
                CommandOutcome::Review { staged, reviewers } => {
                    if !subagent_enabled {
                        eprintln!("[review] 子 Agent 已由 [subagent].enabled = false 禁用");
                        (String::new(), true)
                    } else {
                        // 评审子代理审批：Ask 模式规则（写操作硬拒绝，评审只读）；执行类经
                        // TaskTool 委派审批自动放行（prompt 自动 Yes），避免子代理运行期阻塞在 stdin。
                        let mut agent_cfg = cfg.agent.clone();
                        agent_cfg.mode = agent_core::Mode::Ask;
                        let rules = agent_config::RulesEngine::new(Arc::new(agent_cfg))
                            .with_workspace_root(Some(workspace.root().to_path_buf()));
                        let review_approval: Arc<dyn agent_core::ApprovalPolicy> =
                            Arc::new(agent_config::RulesApprovalPolicy::new(
                                rules,
                                Arc::clone(&prompt_resolver),
                            ));
                        let review_env = review::ReviewEnv {
                            cwd: &cwd,
                            provider: &provider,
                            sub_tools: &sub_tools,
                            prompts: &prompts,
                            workspace: &workspace,
                            model: &current_model,
                            provider_ctx: &current_provider_ctx,
                            max_mistakes,
                            context_guard,
                            max_output: current_max_output,
                            context_factory: &sub_context_factory,
                            temperature: if subagent_inherit {
                                profile_temperature
                            } else {
                                None
                            },
                            thinking: if subagent_inherit && enable_thinking {
                                Some(agent_core::ThinkingConfig::new(
                                    reasoning_budget.unwrap_or(16_000),
                                ))
                            } else {
                                None
                            },
                            max_concurrent: subagent_max_concurrent,
                            supervisor: supervisor.clone(),
                            approval: review_approval,
                        };
                        review::run_review(staged, reviewers, &review_env).await;
                        (String::new(), true)
                    }
                }
                CommandOutcome::Quit => break,
            }
        } else {
            // 非 slash 命令：作为任务发送前，先展开行内 @file 提及（与 Web 对齐）。
            // 仅当文本含 @ 提及时才触发（零提及则原样返回，正常提示无额外开销）。
            let mut text = line.to_string();
            let mentions = agent_prompt::mentions::parse_mentions(&text);
            if !mentions.is_empty() {
                let mut blocks: Vec<String> = Vec::new();
                for agent_prompt::mentions::Mention::File(p) in &mentions {
                    match std::fs::read_to_string(cwd.join(p)) {
                        Ok(content) => {
                            blocks.push(agent_prompt::mentions::format_file_block(p, &content));
                        }
                        Err(e) => eprintln!("@file {p} 读取失败，已跳过：{e}"),
                    }
                }
                if !blocks.is_empty() {
                    text = agent_prompt::mentions::render_attached(&text, &blocks);
                }
            }
            (text, false)
        };
        if skip_run {
            if let Some(msg) = pending_msg.take() {
                run_turn_message(&agent, msg, &accumulated, &message_queue).await?;
                // H4：本轮结束后把队首消息排为下一轮任务（自动续跑，不再等输入）。
                pending_task = pop_queued(&message_queue);
            }
            continue;
        }

        run_turn(&agent, &next_task, &accumulated, &message_queue).await?;
        // H4：同上——排队消息在本轮结束后依次执行。
        pending_task = pop_queued(&message_queue);
    }

    Ok(())
}

/// 按可选工具启用态组装 context_files：仅启用组的操作提示词进入 system prompt，
/// 未启用完全屏蔽（零额外 Token 开销）。
///
/// 这是按需加载的核心——清单本体收敛在 [`agent_sdk::optional_tool_sections`]
/// （REPL / RPC / server 共用同一份，避免工具注册面与提示词面漂移）。统一原则：
/// **启用才注入，禁用屏蔽**。
#[must_use]
fn optional_context_files(
    base: &[String],
    optional: &std::collections::HashMap<String, bool>,
    github_enabled: bool,
) -> Vec<String> {
    let mut files = base.to_vec();
    files.extend(agent_sdk::optional_tool_sections(optional, github_enabled));
    files
}

/// 保留顶层词（移植 oh-my-pi `cli-commands.ts` 的 `RESERVED_TOP_LEVEL_WORDS`，并按
/// Gyre 子命令路线图扩充）：这些词规划为未来 CLI 子命令（models/config/usage/…
/// 与插件动词族）。裸用作任务首词时，用户意图极可能是「调管理命令」而非「发这句
/// prompt」——按 omp #4845 防误注入语义拦截并提示，避免整句被发给 LLM 误计费。
const RESERVED_TOP_LEVEL_WORDS: &[&str] = &[
    "models",
    "config",
    "usage",
    "stats",
    "plugin",
    "marketplace",
    "extensions",
    "mcp",
    "auth",
    "login",
    "logout",
    "update",
    "upgrade",
    "doctor",
    "completions",
    "gc",
    "install",
    "uninstall",
    "enable",
    "disable",
    "list",
    "remove",
    "discover",
];

/// 插件/市场管理语法子动作（对齐 omp `MARKETPLACE_SUBCOMMANDS`）：
/// `agent marketplace add x` / `agent plugin remove y` 视为管理命令语法。
const MARKETPLACE_SUBCOMMANDS: &[&str] = &["add", "remove", "rm", "update", "list"];

/// 保留顶层词提示（移植 oh-my-pi `reservedTopLevelWordMessage` 的触发语法）：
///
/// - 首词带 `-` / `@` 前缀 → 非管理命令（flag / @file 语法），放行。
/// - 非保留词首词 → 放行。
/// - 裸保留词（`agent models`）→ 提示。
/// - `marketplace|plugin <子动作> …` → 提示。
/// - 后续任一参数含 `@`（`name@marketplace` 插件 id 语法，如 `agent uninstall foo@bar`）→ 提示。
/// - 其余多词形态（如 `list all my files`、`upgrade the deps`）→ 放行为正常 prompt。
#[must_use]
fn reserved_top_level_word_hint(task: &str) -> Option<String> {
    let mut tokens = task.split_whitespace();
    let first = tokens.next()?;
    if first.starts_with('-') || first.starts_with('@') {
        return None;
    }
    if !RESERVED_TOP_LEVEL_WORDS.contains(&first) {
        return None;
    }
    let rest: Vec<&str> = tokens.collect();
    let management_grammar = match rest.first() {
        // 裸保留词。
        None => true,
        Some(second) => {
            ((first == "marketplace" || first == "plugin")
                && MARKETPLACE_SUBCOMMANDS.contains(second))
                || rest.iter().any(|a| !a.starts_with('-') && a.contains('@'))
        }
    };
    management_grammar.then(|| {
        format!(
            "「agent {first}」是保留的管理命令字（该词已按 oh-my-pi #4845 防误注入语义\
             拦截，不作为任务发送，避免被误计费）。可用子命令：`agent models list`、\
             `agent auth list|save|remove`、`agent mcp list|status|add|remove|enable|disable`、\
             `agent config path|check`。若确要以此文本作为任务，请调整首词措辞，或在交互 \
             REPL 中直接输入。"
        )
    })
}

/// 装配内置工具集：核心工具（始终启用）+ 按启用态追加的可选组（ast/lsp/image/hashline/pty/github）。
///
/// 与 [`optional_context_files`] 配对：同一开关同时决定「工具是否注册」与「提示词是否注入」，
/// P0-1：把 `eval` 工具附加到既有注册表之上的适配器。
///
/// 桥侧注册表（`inner`）不含 eval 自身（防 python 内 `tool.eval` 自我递归）；
/// Agent 侧经本适配器看到完整工具面（含 eval）。
struct WithEval {
    inner: Arc<dyn agent_tools::ToolRegistry>,
    eval: Arc<dyn agent_tools::Tool>,
}

impl agent_tools::ToolRegistry for WithEval {
    fn specs(&self) -> Vec<agent_core::ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(agent_core::ToolSpec::new(
            self.eval.name(),
            self.eval.description(),
            self.eval.schema(),
        ));
        specs
    }

    fn get(&self, name: &str) -> Option<Arc<dyn agent_tools::Tool>> {
        if name == self.eval.name() {
            Some(Arc::clone(&self.eval))
        } else {
            self.inner.get(name)
        }
    }
}

#[cfg(test)]
mod with_eval_tests {
    use super::*;
    use agent_tools::ToolRegistry as _; // specs/get 方法解析

    /// 桩工具：仅用于验证 WithEval 的 spec/get 分发与 eval 名隔离。
    struct DummyTool;
    #[async_trait::async_trait]
    impl agent_tools::Tool for DummyTool {
        fn name(&self) -> &str {
            "dummy"
        }
        fn description(&self) -> &str {
            "dummy tool"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability(&self) -> agent_core::CapabilityTier {
            agent_core::CapabilityTier::ReadOnly
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &agent_tools::ToolContext<'_>,
        ) -> Result<agent_core::ToolResult, agent_core::ToolError> {
            Ok(agent_core::ToolResult::text("ok"))
        }
    }

    struct EvalStub;
    #[async_trait::async_trait]
    impl agent_tools::Tool for EvalStub {
        fn name(&self) -> &str {
            "eval"
        }
        fn description(&self) -> &str {
            "eval stub"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability(&self) -> agent_core::CapabilityTier {
            agent_core::CapabilityTier::Execute
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &agent_tools::ToolContext<'_>,
        ) -> Result<agent_core::ToolResult, agent_core::ToolError> {
            Ok(agent_core::ToolResult::text("ok"))
        }
    }

    #[test]
    fn with_eval_attaches_eval_and_delegates_rest() {
        let inner = agent_tools::DefaultToolRegistry::new().with(Box::new(DummyTool));
        let inner: Arc<dyn agent_tools::ToolRegistry> = Arc::new(inner);
        let wrapped = WithEval {
            inner: Arc::clone(&inner),
            eval: Arc::new(EvalStub),
        };
        let specs = wrapped.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"dummy"), "桥注册表工具仍可见");
        assert!(names.contains(&"eval"), "eval 附加到 Agent 侧");
        assert!(wrapped.get("dummy").is_some(), "非 eval 名委托给桥注册表");
        assert!(wrapped.get("eval").is_some(), "eval 名由适配器提供");
    }
}

/// 打印 TTSR 规则加载摘要（H44）：内置条数 + 项目规则名（避免把 27 条内置规则全刷屏）。
fn report_ttsr_rules(rules: &[agent_ttsr::Rule]) {
    let is_builtin = |name: &str| {
        agent_ttsr::BUILTIN_RULE_SOURCES
            .iter()
            .any(|(n, _)| *n == name)
    };
    let project: Vec<&str> = rules
        .iter()
        .filter(|r| !is_builtin(&r.name))
        .map(|r| r.name.as_str())
        .collect();
    if project.is_empty() {
        eprintln!("已加载 {} 条 TTSR 规则（全部为内置）", rules.len());
    } else {
        eprintln!(
            "已加载 {} 条 TTSR 规则（内置 {} 条 + 项目 {} 条: {}）",
            rules.len(),
            rules.len() - project.len(),
            project.len(),
            project.join(", ")
        );
    }
}

/// 从配置构造输出最小化器（`[agent.commands.minimizer] enabled/max_lines`）。
/// 未启用时返回 [`agent_tools::disabled`]（apply 恒 None，零开销）。
#[must_use]
fn compiled_minimizer(cfg: &agent_config::Config) -> agent_tools::Minimizer {
    let m = &cfg.agent.commands.minimizer;
    if m.enabled {
        agent_tools::Minimizer::new(agent_tools::default_filters(), m.max_lines)
    } else {
        agent_tools::disabled()
    }
}

// H42：可选工具组白名单/装配入口收敛到 `agent-sdk`（单一实现，三前端共用）。
// 这里保留同名再导出，`crate::assemble_builtin_tools` 等既有调用点（rpc/测试）不受影响。
pub use agent_sdk::{OPTIONAL_TOOL_KEYS, assemble_builtin_tools, is_known_optional_key};

/// `/model <alias|role>` 热切换：解析 profile（先 alias/id，未命中再查 `[models.roles]`
/// 角色）并更新运行时模型状态；密钥走认证链分层（config → auth.toml → env）。
/// 成功返回 true。
#[allow(clippy::too_many_arguments)] // 模型热切换状态注入面，同 run_swarm 先例。
fn apply_model_switch(
    alias: &str,
    cfg: &agent_config::Config,
    oauth_client: &reqwest::Client,
    config_dir: &std::path::Path,
    current_api_key: &mut String,
    current_base_url: &mut String,
    current_max_output: &mut usize,
    current_model: &mut agent_core::Model,
    current_provider_ctx: &mut agent_core::ProviderCallContext,
) -> bool {
    let resolved = cfg
        .resolve_model(Some(alias))
        .ok()
        .or_else(|| cfg.resolve_role(alias));
    let Some(profile) = resolved else {
        eprintln!(
            "{}",
            t!(
                "model.switch_failed",
                e = agent_core::ConfigError::ModelNotFound(alias.to_string())
            )
        );
        return false;
    };
    // 密钥链解析：config 值非空优先，否则 oauth.toml（有效/先刷后用）→
    // auth.toml → 环境变量回退；全空回落 profile 内联值（历史行为兜底）。
    // H24：热切换同样尊重 profile 的 `auth`（none → 空串 = 不发鉴权头）。
    let key = agent_llm::oauth::resolve_profile_api_key(
        oauth_client,
        config_dir,
        profile.auth,
        profile.api.as_str(),
        &profile.id,
        profile.api_key.expose_secret().is_empty(),
        profile.resolve_api_key().expose_secret(),
    )
    .ok()
    .flatten()
    .unwrap_or_default();
    *current_api_key = if key.is_empty() {
        profile.resolve_api_key().expose_secret().to_string()
    } else {
        key
    };
    *current_base_url = profile.effective_base_url();
    *current_max_output = profile.max_output_tokens.unwrap_or(4096);
    *current_model = agent_core::Model {
        id: profile.id.clone(),
        provider: "openai-compatible".into(),
        api: profile.api,
        max_input_tokens: profile.effective_max_input_tokens(),
        max_output_tokens: *current_max_output,
        supports_tools: true,
        supports_streaming: true,
        supports_thinking: cfg.agent.enable_thinking,
        extra_body: profile.extra_body.clone(),
        tokenizer: profile.tokenizer.clone(),
    };
    *current_provider_ctx = agent_core::ProviderCallContext {
        api_key: Some(current_api_key.clone()),
        base_url: Some(current_base_url.clone()),
        max_in_flight: None,
        headers: profile_headers(profile),
        quirks: profile.effective_quirks(),
        auth: profile.auth,
    };
    true
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

/// `/diff` 执行 git diff 并截断长输出（工作区 / `--staged` / 指定 ref）。
/// 非 git 仓库返回提示文本。
#[must_use]
fn run_git_diff(root: &std::path::Path, staged: bool, ref_name: Option<&str>) -> String {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("diff");
    if staged {
        cmd.arg("--cached");
    }
    if let Some(r) = ref_name {
        cmd.arg(r);
    }
    let out = match cmd.current_dir(root).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) if o.status.code() == Some(128) => {
            return "当前目录不是 git 仓库（git diff 退出码 128）".to_string();
        }
        Ok(o) => format!(
            "git diff 失败（exit {}）: {}",
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(e) => return format!("无法执行 git: {e}"),
    };
    // 长输出截断：保留前 300 行 + 省略标记。
    const MAX_LINES: usize = 300;
    let lines: Vec<&str> = out.lines().collect();
    if lines.len() > MAX_LINES {
        let head: Vec<&str> = lines.iter().take(MAX_LINES).copied().collect();
        format!(
            "{}\n… [diff 过长，已截断 {} 行；可用完整命令重跑] …",
            head.join("\n"),
            lines.len() - MAX_LINES
        )
    } else {
        out
    }
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

/// 循环内 auto-retain Hook（P0-2）：每 N 个「停止轮」（will_continue=false，即用户轮次
/// 的最终模型答复）把本轮 assistant 输出沉淀到记忆；工具轮（will_continue=true）跳过
/// （中间推理噪音）。对齐 oh-my-pi `retainEveryNTurns` 语义的 Hook 层实现——
/// 转写 strip `<memories>`/`<mental_models>` 块，防记忆自反馈回路。
struct AutoRetainHook {
    store: Arc<dyn agent_core::MemoryStore>,
    every_n_turns: usize,
    final_rounds: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl Hook for AutoRetainHook {
    async fn on_event(&self, _event: &HookEvent) {}

    async fn on_turn_end(&self, ctx: &agent_core::TurnEndContext<'_>) {
        if ctx.will_continue {
            return; // 工具轮：中间推理噪音，不沉淀
        }
        let n = self
            .final_rounds
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if n % self.every_n_turns != 0 {
            return;
        }
        // 组装沉淀内容：assistant 最终答复（strip 记忆块）+ 工具调用摘要。
        let mut parts: Vec<String> = Vec::new();
        let text = strip_memory_blocks(&ctx.message.text());
        if !text.trim().is_empty() {
            parts.push(text.trim().to_string());
        }
        for (id, name, args) in ctx.message.tool_calls() {
            let _ = id;
            let arg_preview: String = args.to_string().chars().take(120).collect();
            parts.push(format!("- tool {name}({arg_preview})"));
        }
        if parts.is_empty() {
            return;
        }
        // 防爆库：单条上限 4000 字符（对齐 mnemopi 长内容 head 保留）。
        let content: String = parts.join("\n").chars().take(4000).collect();
        let note = agent_core::MemoryNote {
            content,
            source: "auto-retain".into(),
        };
        if let Err(e) = self.store.append_note(&note).await {
            tracing::warn!(error = %e, "auto-retain 写入失败");
        }
    }
}

/// 结构化记忆会话末维护 Hook（P0-4/P0-5）：任务成功结束时 LLM 沉淀未沉淀记录
/// （working → episodic，`auto_consolidate` 开启且 provider 可用时），再清理过期记录
/// + 内容去重 + 模型变更重嵌。对齐 mnemopi `sleep` 的完整语义（consolidate + cleanup）。
struct StructuredSleepHook {
    store: Arc<agent_memory::StructuredMemoryStore>,
    auto_consolidate: bool,
    provider: Option<Arc<dyn agent_core::LlmProvider>>,
    model: Option<Model>,
    provider_ctx: Option<agent_core::ProviderCallContext>,
    /// H30：后台执行（同 [`ConsolidateHook`]）。
    background: bool,
}

#[async_trait::async_trait]
impl Hook for StructuredSleepHook {
    async fn on_event(&self, event: &HookEvent) {
        if matches!(event, HookEvent::Stop { success: true }) {
            if self.auto_consolidate {
                if let (Some(p), Some(m), Some(ctx)) =
                    (&self.provider, &self.model, &self.provider_ctx)
                {
                    let store = Arc::clone(&self.store);
                    let provider = Arc::clone(p);
                    let model = m.clone();
                    let ctx = ctx.clone();
                    let run = async move {
                        match store.consolidate(&provider, &model, &ctx).await {
                            Ok(report) if report.skipped => {
                                tracing::debug!("记忆沉淀跳过：租约被其它会话持有（H30）")
                            }
                            Ok(report) => {
                                if report.distilled > 0 {
                                    tracing::info!(
                                        absorbed = report.absorbed,
                                        distilled = report.distilled,
                                        banks = report.banks,
                                        "记忆 LLM 沉淀完成"
                                    );
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "记忆 LLM 沉淀失败"),
                        }
                    };
                    if self.background {
                        tokio::spawn(run);
                    } else {
                        run.await;
                    }
                }
            }
            match self.store.sleep() {
                Ok(report) => {
                    tracing::info!(
                        banks = report.banks,
                        expired_removed = report.expired_removed,
                        duplicates_removed = report.duplicates_removed,
                        "记忆会话末维护完成"
                    );
                }
                Err(e) => tracing::warn!(error = %e, "记忆会话末维护失败"),
            }
        }
    }
}

/// 剥除 `<memories>…</memories>` 与 `<mental_models>…</mental_models>` 块（非贪婪，
/// 大小写不敏感）——防记忆自反馈回路（omp `hindsight/content.ts` 的 strip 语义）。
fn strip_memory_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let lower = text.to_lowercase();
    let markers: [&str; 2] = ["<memories>", "<mental_models>"];
    let mut pos = 0usize;
    while pos < text.len() {
        // 找下一个开启标记（大小写不敏感，取原文的实际偏移）。
        let mut open: Option<(usize, &str)> = None;
        for m in &markers {
            if let Some(rel) = lower[pos..].find(m) {
                let cand = pos + rel;
                if open.is_none_or(|(o, _)| cand < o) {
                    open = Some((cand, m));
                }
            }
        }
        let Some((start, m)) = open else {
            out.push_str(&text[pos..]);
            break;
        };
        out.push_str(&text[pos..start]);
        // 找对应结束标记 `</memories>` / `</mental_models>`。
        let close_tag = format!("</{}>", &m[1..m.len() - 1]);
        let rest = &lower[start + m.len()..];
        let close_len = rest.find(&close_tag).map_or(0, |c| c + close_tag.len());
        pos = start + m.len() + close_len;
    }
    out
}

/// 长期记忆合并 Hook：任务成功结束时触发 LLM consolidate（仅 local 后端）。
#[derive(Clone)]
struct ConsolidateHook {
    store: Arc<agent_memory::LocalMemoryStore>,
    provider: Arc<dyn agent_core::LlmProvider>,
    model: Model,
    provider_ctx: agent_core::ProviderCallContext,
    auto_consolidate: bool,
    /// H30：后台执行（true 时停止边界不被 LLM 沉淀阻塞；租约仍保证互斥）。
    background: bool,
}

#[async_trait::async_trait]
impl Hook for ConsolidateHook {
    async fn on_event(&self, event: &HookEvent) {
        if self.auto_consolidate && matches!(event, HookEvent::Stop { success: true }) {
            let store = Arc::clone(&self.store);
            let provider = Arc::clone(&self.provider);
            let model = self.model.clone();
            let ctx = self.provider_ctx.clone();
            let run = async move {
                if let Err(e) = store.consolidate(&provider, &model, &ctx).await {
                    eprintln!("{}", t!("memory.merge_failed", e = e));
                }
                // P1-6：心智模型 LLM 合并（去重/分组/提炼），独立于 MEMORY.md 合并。
                if let Err(e) = store
                    .consolidate_mental_models(&provider, &model, &ctx)
                    .await
                {
                    eprintln!("{}", t!("memory.merge_failed", e = e));
                }
            };
            if self.background {
                // H30：后台 rollout——不阻塞停止边界；租约保证并发只有一个真正沉淀。
                tokio::spawn(run);
            } else {
                run.await;
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
    match agent_skills::SkillRegistry::cross_tool(
        cwd.to_path_buf(),
        &cfg.skills.to_provider_toggles(),
    )
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

// ──────────────────────────────────────────────────────────────────────────────
// Ctrl-C（SIGINT）路由（H3）
// ──────────────────────────────────────────────────────────────────────────────

/// 进程级 SIGINT 路由器：把 Ctrl-C 定向到「当前在跑的 turn」的取消令牌。
///
/// 背景（差距报告 H3）：此前只有 `--rpc` 路径装 `tokio::signal::ctrl_c()`；REPL /
/// 单次任务在流式输出中按 Ctrl-C 会走默认动作**直接杀进程**——不落盘、不回滚在途工具、
/// 已生成内容丢失。提示符处的 Ctrl-C 由 rustyline 自行处理（raw 模式读 `\x03`，不产生
/// SIGINT），因此本路由器只需覆盖「turn 运行中」这一路。
///
/// 语义：
/// - 有活动 turn → 取消其令牌（流式中断 + 在途工具收到取消；会话与上下文保留，回到提示符）；
/// - 无活动 turn（极端时序：SIGINT 在提示符之外到达）→ 退出码 130（与 shell 惯例一致）。
pub struct SigintRouter {
    /// 当前活动 turn 的取消令牌（`None` = 空闲）。
    current: std::sync::Mutex<Option<tokio_util::sync::CancellationToken>>,
}

impl SigintRouter {
    /// 登记一个 turn 的取消令牌（返回守卫，Drop 时自动摘除）。
    fn begin_turn(
        self: &Arc<Self>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> SigintTurnGuard {
        *self.current.lock().expect("SIGINT 令牌锁中毒") = Some(cancel);
        SigintTurnGuard {
            router: Arc::clone(self),
        }
    }

    /// 处理一次 SIGINT：有活动 turn → 取消并返回 [`SigintAction::Cancelled`]；
    /// 空闲 → [`SigintAction::Idle`]（由调用方决定退出，便于单测两个分支）。
    fn interrupt(&self) -> SigintAction {
        let token = self.current.lock().expect("SIGINT 令牌锁中毒").clone();
        match token {
            Some(token) => {
                eprintln!("\n{}", t!("repl.interrupted"));
                token.cancel();
                SigintAction::Cancelled
            }
            None => {
                eprintln!("{}", t!("repl.interrupt_exit"));
                SigintAction::Idle
            }
        }
    }
}

/// 一次 SIGINT 的路由结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigintAction {
    /// 取消了活动 turn。
    Cancelled,
    /// 空闲（无活动 turn）。
    Idle,
}

/// turn 守卫：Drop 时清空路由器上的当前令牌（无需手动清理）。
pub struct SigintTurnGuard {
    router: Arc<SigintRouter>,
}

impl Drop for SigintTurnGuard {
    fn drop(&mut self) {
        *self.router.current.lock().expect("SIGINT 令牌锁中毒") = None;
    }
}

/// 安装 SIGINT 处理器（进程内一次；重复调用返回既有路由器）。
fn install_sigint_router() -> Arc<SigintRouter> {
    static ROUTER: std::sync::OnceLock<Arc<SigintRouter>> = std::sync::OnceLock::new();
    Arc::clone(ROUTER.get_or_init(|| {
        let router = Arc::new(SigintRouter {
            current: std::sync::Mutex::new(None),
        });
        let handler = Arc::clone(&router);
        // `tokio::signal::ctrl_c()` 每次 await 只交付一次信号 → 循环常驻。
        tokio::spawn(async move {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    break; // 信号注册失败（非 Unix 极端场景）：放弃路由，退回默认行为
                }
                if handler.interrupt() == SigintAction::Idle {
                    std::process::exit(130); // 空闲态 Ctrl-C：与 shell 惯例一致的退出码
                }
            }
        });
        router
    }))
}

/// H5：把生效键位表绑到 rustyline 编辑器。
///
/// 原生编辑动作用对应 [`rustyline::Cmd`]；插入型动作（`/retry` 等）插入命令文本；
/// `app.message.dequeue` 用带队列状态的 handler（H4）；中断/退出由信号与 EOF 路径处理，
/// 这里只登记展示（`/hotkeys`）。
fn apply_keybindings(
    rl: &mut Editor<ReplHelper, DefaultHistory>,
    resolved: &agent_cli::keybindings::Resolved,
    queue: &Arc<std::sync::Mutex<agent_cli::queue::MessageQueue>>,
) {
    use agent_cli::keybindings::Action;
    for b in &resolved.bindings {
        let handler: Option<rustyline::EventHandler> = match b.action {
            Action::Clear => Some(rustyline::Cmd::ClearScreen.into()),
            Action::HistorySearch => Some(rustyline::Cmd::ReverseSearchHistory.into()),
            Action::Complete => Some(rustyline::Cmd::Complete.into()),
            Action::Dequeue => Some(rustyline::EventHandler::Conditional(Box::new(
                DequeueHandler {
                    queue: Arc::clone(queue),
                },
            ))),
            Action::Retry | Action::SessionNew | Action::QueueList | Action::Settings => b
                .action
                .inserts()
                .map(|text| rustyline::Cmd::Insert(1, text.to_string()).into()),
            // 中断（Ctrl-C）与退出（Ctrl-D）由信号路由 / EOF 路径消费。
            Action::Interrupt | Action::Exit => None,
        };
        if let Some(h) = handler {
            rl.bind_sequence(b.key, h);
        }
    }
}

/// H4：取出队首排队消息（无/锁中毒 → `None`）；用作下一轮任务。
fn pop_queued(queue: &Arc<std::sync::Mutex<agent_cli::queue::MessageQueue>>) -> Option<String> {
    queue.lock().ok().and_then(|mut q| q.pop_front())
}

/// H4：`Ctrl-Y` 键处理——把队首排队消息插入当前编辑行（空队列时无操作）。
struct DequeueHandler {
    queue: Arc<std::sync::Mutex<agent_cli::queue::MessageQueue>>,
}

impl rustyline::ConditionalEventHandler for DequeueHandler {
    fn handle(
        &self,
        _evt: &rustyline::Event,
        _n: rustyline::RepeatCount,
        _positive: bool,
        _ctx: &rustyline::EventContext,
    ) -> Option<rustyline::Cmd> {
        let text = self.queue.lock().ok().and_then(|mut q| q.pop_front())?;
        eprintln!("\n[dequeued] 已取回排队消息（Enter 提交，或继续编辑）");
        Some(rustyline::Cmd::Insert(1, text))
    }
}

/// 运行一个任务轮次，消费事件流并打印，并把用量累加到 `accumulated`。返回是否成功完成。
///
/// Ctrl-C（H3）经 [`install_sigint_router`] 的取消令牌优雅中止本轮：流式中断、
/// 在途工具收到取消，会话与上下文保留（可继续下一轮）。
async fn run_turn(
    agent: &agent::Agent,
    task: &str,
    accumulated: &Arc<std::sync::Mutex<Usage>>,
    queue: &Arc<std::sync::Mutex<agent_cli::queue::MessageQueue>>,
) -> Result<bool> {
    let cancel = tokio_util::sync::CancellationToken::new();
    let guard = install_sigint_router().begin_turn(cancel.clone());
    let result = consume_stream(
        agent.run_with_cancel(task, cancel),
        accumulated,
        Some(agent),
        queue,
    )
    .await;
    drop(guard);
    result
}

/// 运行一条带图像等多模态内容块的用户消息（`/paste`）。
async fn run_turn_message(
    agent: &agent::Agent,
    msg: agent_core::UserMessage,
    accumulated: &Arc<std::sync::Mutex<Usage>>,
    queue: &Arc<std::sync::Mutex<agent_cli::queue::MessageQueue>>,
) -> Result<bool> {
    let cancel = tokio_util::sync::CancellationToken::new();
    let guard = install_sigint_router().begin_turn(cancel.clone());
    let result = consume_stream(
        agent.run_message_with_cancel(msg, cancel),
        accumulated,
        Some(agent),
        queue,
    )
    .await;
    drop(guard);
    result
}

/// 渲染单个 agent 事件到终端（流式 Markdown / 状态行 / 工具 / 用量 / done）。
/// 返回 `Some(success)` 当且仅当事件为 `Done`（此时已 flush 残留并累加用量），其余返回 `None`。
fn render_event(
    ev: AgentEvent,
    md: &mut markdown::MarkdownRenderer,
    accumulated: &Arc<std::sync::Mutex<Usage>>,
) -> Option<bool> {
    match ev {
        AgentEvent::TextDelta(t) => {
            let rendered = md.push(&t);
            if !rendered.is_empty() {
                print!("{rendered}");
                let _ = std::io::stdout().flush();
            }
            None
        }
        AgentEvent::ThinkingDelta(t) => {
            eprint!("\x1b[2m{t}\x1b[0m");
            let _ = std::io::stderr().flush();
            None
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
            None
        }
        AgentEvent::ToolExec { name, output } => {
            eprintln!("\n{}", t!("event.tool", name = name, output = output));
            None
        }
        AgentEvent::Usage(u) => {
            eprintln!(
                "\n{}",
                t!("event.usage", input = u.input_tokens, out = u.output_tokens)
            );
            None
        }
        AgentEvent::StateChanged(st) => {
            eprintln!("\n{}", t!("event.state", state = format!("{st:?}")));
            None
        }
        AgentEvent::Error(e) => {
            eprintln!("\n{}", t!("event.error", e = e));
            None
        }
        AgentEvent::Done(summary) => {
            // flush 末尾残留（未闭合代码块 / 行缓冲）
            let tail = md.finish();
            if !tail.is_empty() {
                print!("{tail}");
                let _ = std::io::stdout().flush();
            }
            let success = summary.success;
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
            Some(success)
        }
        AgentEvent::Ask(_)
        | AgentEvent::Assistant(_)
        | AgentEvent::TurnStart
        | AgentEvent::TurnEnd { .. }
        | AgentEvent::MessageStart
        | AgentEvent::MessageEnd(_)
        | AgentEvent::ToolExecutionStart { .. }
        | AgentEvent::ToolExecutionUpdate { .. }
        | AgentEvent::ToolExecutionEnd { .. }
        // 结构化会话事件：展示文本已由配对的 Say 打印（双通道同源）。
        | AgentEvent::Session(_) => None,
    }
}

/// 消费 agent 事件流并打印（`run_turn` / `run_turn_message` 共用）。
///
/// `steer` 为 `Some(agent)` 时（交互 REPL），运行期间并发读 stdin：用户键入的每一行经
/// [`agent::Agent::steer`] 即时投递给运行中的 agent——Immediate 策略会尽快打断在途的可中断
/// 工具，并在下一注入边界把消息投给模型（移植 oh-my-pi `Agent.steer()`）。rustyline 仅在空闲态
/// 持有终端（行编辑/补全），运行期间终端处于 cooked 模式，故此处 `read_line` 与空闲态的
/// rustyline 互斥、不抢字节；非交互（stdin 非 tty）时退化为纯事件循环。
async fn consume_stream<S>(
    events: S,
    accumulated: &Arc<std::sync::Mutex<Usage>>,
    steer: Option<&agent::Agent>,
    queue: &Arc<std::sync::Mutex<agent_cli::queue::MessageQueue>>,
) -> Result<bool>
where
    S: futures::Stream<Item = AgentEvent>,
{
    tokio::pin!(events);
    let mut success = false;
    // 流式 Markdown 美化：仅 TTY 输出着色/高亮，管道透传原文。
    let mut md =
        markdown::MarkdownRenderer::new(std::io::IsTerminal::is_terminal(&std::io::stdout()));

    // 纯事件循环（一次性任务 / 非交互 / 未启用 steering）。
    let Some(steer_agent) = steer.filter(|_| std::io::IsTerminal::is_terminal(&std::io::stdin()))
    else {
        while let Some(ev) = events.next().await {
            if let Some(s) = render_event(ev, &mut md, accumulated) {
                success = s;
            }
        }
        return Ok(success);
    };

    // 交互 REPL：事件流与 stdin 并发；运行中键入即 steering。
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut buf = String::new();
    let mut stdin_eof = false;
    loop {
        if stdin_eof {
            // stdin 已 EOF：不再并发读，专心排空事件流。
            match events.next().await {
                Some(ev) => {
                    if let Some(s) = render_event(ev, &mut md, accumulated) {
                        success = s;
                    }
                }
                None => break,
            }
            continue;
        }
        tokio::select! {
            biased;
            ev = events.next() => match ev {
                Some(ev) => {
                    if let Some(s) = render_event(ev, &mut md, accumulated) {
                        success = s;
                    }
                }
                None => break,
            },
            n = stdin.read_line(&mut buf) => match n {
                Ok(0) | Err(_) => stdin_eof = true,
                Ok(_) => {
                    let line = std::mem::take(&mut buf);
                    match repl::handle_running_input(&line, queue) {
                        repl::RunningInput::Ignored => {}
                        repl::RunningInput::Queued(len) => {
                            eprintln!("\n[queued #{len}] 运行中消息已排队（/queue list 查看）");
                        }
                        repl::RunningInput::Steer(text) => {
                            if steer_agent
                                .steer(agent_core::AgentMessage::user_text(text))
                            {
                                eprintln!("\n[steer] 消息已投递给运行中的 agent");
                            }
                        }
                    }
                }
            },
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
        let (reg, _) = assemble_builtin_tools(
            &optional,
            false,
            false,
            false,
            agent_tools::disabled(),
            None,
            None,
        );
        let specs = reg.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"run_command"));
        for opt in [
            "ast_search",
            "read_image",
            "image_gen",
            "lsp",
            "apply_hashline",
            "run_pty_command",
            "shell_session",
            "github",
        ] {
            assert!(!names.contains(&opt), "默认应关闭可选工具 {opt}");
        }
    }

    #[test]
    fn assemble_enables_ast_group() {
        let mut optional = std::collections::HashMap::new();
        optional.insert("ast".to_string(), true);
        let (reg, _) = assemble_builtin_tools(
            &optional,
            false,
            false,
            false,
            agent_tools::disabled(),
            None,
            None,
        );
        let specs = reg.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"replace_block"));
        assert!(names.contains(&"ast_search"));
        // 其他可选组仍关闭
        assert!(!names.contains(&"read_image"));
        assert!(!names.contains(&"lsp"));
    }

    /// H45：`[tools].enabled.pty = true` 同时注册**两个** PTY 工具——一次性执行
    /// （`run_pty_command`）与持久会话（`shell_session`，跨命令保持 cwd/环境）。
    #[test]
    fn assemble_enables_pty_group_with_persistent_session() {
        let mut optional = std::collections::HashMap::new();
        optional.insert("pty".to_string(), true);
        let (reg, _) = assemble_builtin_tools(
            &optional,
            false,
            false,
            false,
            agent_tools::disabled(),
            None,
            None,
        );
        let specs = reg.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"run_pty_command"), "{names:?}");
        assert!(names.contains(&"shell_session"), "{names:?}");
        // 其他可选组仍关闭。
        assert!(!names.contains(&"ast_search"));
        assert!(!names.contains(&"lsp"));
    }

    #[test]
    fn assemble_github_independent_of_optional_map() {
        let optional = std::collections::HashMap::new();
        let (reg, _) = assemble_builtin_tools(
            &optional,
            true,
            true,
            false,
            agent_tools::disabled(),
            None,
            None,
        );
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

    #[test]
    fn parse_socks_spec_ok() {
        assert_eq!(
            parse_socks_spec("127.0.0.1:1080"),
            Ok(("127.0.0.1".into(), 1080))
        );
        assert_eq!(
            parse_socks_spec(" proxy.example.com:8080 "),
            Ok(("proxy.example.com".into(), 8080))
        );
        assert_eq!(parse_socks_spec("[::1]:1080"), Ok(("::1".into(), 1080)));
        assert_eq!(
            parse_socks_spec("1.2.3.4:65535"),
            Ok(("1.2.3.4".into(), 65535))
        );
    }

    #[test]
    fn parse_socks_spec_rejects_bad_input() {
        assert!(parse_socks_spec("127.0.0.1").is_err(), "缺端口应拒绝");
        assert!(parse_socks_spec("127.0.0.1:0").is_err(), "端口 0 应拒绝");
        assert!(
            parse_socks_spec("127.0.0.1:abc").is_err(),
            "非数字端口应拒绝"
        );
        assert!(
            parse_socks_spec("127.0.0.1:70000").is_err(),
            "端口超范围应拒绝"
        );
        assert!(parse_socks_spec(":1080").is_err(), "空 host 应拒绝");
        assert!(parse_socks_spec("host:").is_err(), "空端口应拒绝");
    }

    #[test]
    fn help_localizes_by_locale() {
        // zh：词表替换（与既有中文文案一致）。
        agent_i18n::init(Some("zh"));
        let cmd = localize_cli_help(Cli::command());
        let socks_help = cmd
            .get_arguments()
            .find(|a| a.get_id() == "socks")
            .expect("socks 参数")
            .get_help()
            .expect("help 应存在")
            .to_string();
        assert!(
            socks_help.contains("SOCKS5 代理"),
            "zh help 应为中文: {socks_help}"
        );
        let about = cmd.get_about().expect("about").to_string();
        assert!(about.contains("智能体"), "zh about 应为中文: {about}");

        // en：derive 英文注释（en 为回退语言，不经词表）。
        agent_i18n::init(Some("en"));
        let cmd = localize_cli_help(Cli::command());
        let socks_help = cmd
            .get_arguments()
            .find(|a| a.get_id() == "socks")
            .expect("socks 参数")
            .get_help()
            .expect("help 应存在")
            .to_string();
        assert!(
            socks_help.contains("SOCKS5 proxy"),
            "en help 应为英文: {socks_help}"
        );

        // ru：词表替换，且与 en 不同。
        agent_i18n::init(Some("ru"));
        let cmd = localize_cli_help(Cli::command());
        let socks_help = cmd
            .get_arguments()
            .find(|a| a.get_id() == "socks")
            .expect("socks 参数")
            .get_help()
            .expect("help 应存在")
            .to_string();
        assert!(
            socks_help.contains("SOCKS5-прокси"),
            "ru help 应来自词表: {socks_help}"
        );

        // 恢复系统探测，避免影响其它测试。
        agent_i18n::init(None);
    }

    // ── P0-2：auto-retain hook ──────────────────────────────────────────────

    /// 记录 append_note 的 mock 存储。
    struct MockStore {
        notes: Arc<parking_lot::Mutex<Vec<String>>>,
        root: PathBuf,
    }

    #[async_trait::async_trait]
    impl agent_core::MemoryStore for MockStore {
        async fn summary(&self) -> Result<Option<String>, std::io::Error> {
            Ok(None)
        }
        async fn read_full(&self) -> Result<Option<String>, std::io::Error> {
            Ok(None)
        }
        async fn append_note(&self, note: &agent_core::MemoryNote) -> Result<(), std::io::Error> {
            self.notes.lock().push(note.content.clone());
            Ok(())
        }
        async fn clear(&self) -> Result<(), std::io::Error> {
            Ok(())
        }
        fn root_dir(&self) -> &PathBuf {
            &self.root
        }
        async fn add_mental_model(&self, _text: &str) -> Result<(), std::io::Error> {
            Ok(())
        }
    }

    fn assistant_with_text(text: &str) -> agent_core::AssistantMessage {
        agent_core::AssistantMessage {
            content: vec![agent_core::ContentBlock::Text {
                text: text.to_string(),
            }],
            usage: agent_core::Usage::default(),
            model: "test".into(),
            stop_reason: None,
            stop_details: None,
        }
    }

    fn turn_ctx<'a>(
        msg: &'a agent_core::AssistantMessage,
        will_continue: bool,
    ) -> agent_core::TurnEndContext<'a> {
        agent_core::TurnEndContext {
            message: msg,
            tool_results: &[],
            will_continue,
        }
    }

    #[test]
    fn strip_memory_blocks_removes_memories_and_mental() {
        let text = "前面正文\n<memories>\n1. [0.92] 旧召回\n</memories>\n中间\n<mental_models>种子</mental_models>\n结尾";
        let stripped = strip_memory_blocks(text);
        assert!(stripped.contains("前面正文"));
        assert!(stripped.contains("中间"));
        assert!(stripped.contains("结尾"));
        assert!(!stripped.contains("旧召回"));
        assert!(!stripped.contains("<memories>"));
        assert!(!stripped.contains("<mental_models>"));
        // 大小写不敏感
        let mixed = strip_memory_blocks("<MEMORIES>内容</MEMORIES>保留");
        assert!(!mixed.contains("内容"));
        assert!(mixed.contains("保留"));
        // 无块时不改动
        assert_eq!(strip_memory_blocks("纯文本"), "纯文本");
    }

    #[tokio::test]
    async fn auto_retain_counts_final_rounds_only() {
        let notes = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let store = Arc::new(MockStore {
            notes: Arc::clone(&notes),
            root: PathBuf::from("/tmp/mock"),
        });
        let hook = AutoRetainHook {
            store,
            every_n_turns: 4,
            final_rounds: std::sync::atomic::AtomicUsize::new(0),
        };
        // 工具轮（will_continue=true）不计数不沉淀。
        let tool_msg = assistant_with_text("正在检查…");
        hook.on_turn_end(&turn_ctx(&tool_msg, true)).await;
        // 停止轮 1..=3：不触发。
        for i in 1..=3 {
            let msg = assistant_with_text(&format!("答复 {i}"));
            hook.on_turn_end(&turn_ctx(&msg, false)).await;
        }
        assert!(notes.lock().is_empty(), "前 3 个停止轮不应沉淀");
        // 停止轮 4：触发沉淀（内容 = assistant 文本 + 工具摘要）。
        let mut msg = assistant_with_text("决定用 structured 后端");
        msg.content.push(agent_core::ContentBlock::ToolCall {
            id: "t1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({ "path": "Cargo.toml" }),
            signature: None,
        });
        hook.on_turn_end(&turn_ctx(&msg, false)).await;
        {
            let got = notes.lock();
            assert_eq!(got.len(), 1);
            assert!(got[0].contains("决定用 structured 后端"));
            assert!(got[0].contains("- tool write_file("), "应含工具摘要");
            assert!(got[0].chars().count() <= 4000, "防爆库截断");
        }
        for i in 5..=7 {
            let m = assistant_with_text(&format!("答复 {i}"));
            hook.on_turn_end(&turn_ctx(&m, false)).await;
        }
        assert_eq!(notes.lock().len(), 1);
        let m8 = assistant_with_text("收尾总结");
        hook.on_turn_end(&turn_ctx(&m8, false)).await;
        assert_eq!(notes.lock().len(), 2);
    }

    #[test]
    fn reserved_word_bare_verb_hints() {
        assert!(reserved_top_level_word_hint("models").is_some());
        assert!(reserved_top_level_word_hint("list").is_some());
        assert!(reserved_top_level_word_hint("config").is_some());
    }

    #[test]
    fn reserved_word_multiword_prompt_falls_through() {
        // omp #4845 的原始案例：真实 prompt 恰好以保留词开头仍应放行。
        assert!(reserved_top_level_word_hint("list all my files").is_none());
        assert!(reserved_top_level_word_hint("upgrade the deps").is_none());
        assert!(reserved_top_level_word_hint("explain models of thinking").is_none());
        // 非保留词首词 / flag、@file 前缀一律放行。
        assert!(reserved_top_level_word_hint("explain models").is_none());
        assert!(reserved_top_level_word_hint("-models").is_none());
        assert!(reserved_top_level_word_hint("@file models").is_none());
    }

    #[test]
    fn reserved_word_plugin_grammar_hints() {
        // 管理语法形态：marketplace/plugin + 子动作、插件 id 含 @。
        assert!(reserved_top_level_word_hint("marketplace add x").is_some());
        assert!(reserved_top_level_word_hint("plugin rm x").is_some());
        assert!(reserved_top_level_word_hint("uninstall foo@bar").is_some());
        assert!(reserved_top_level_word_hint("enable foo@bar").is_some());
        // 含 - 开头的参数不算 id 语法。
        assert!(reserved_top_level_word_hint("enable -x").is_none());
    }

    #[test]
    fn manage_route_fallthrough_keeps_reserved_hints() {
        use agent_cli::manage::Route;
        // 裸保留词不由管理路由接管：回落原路径并继续命中既有 reserved 提示
        // （#4845 语义不变）。
        for word in ["models", "auth", "config", "list", "usage"] {
            assert!(
                matches!(
                    agent_cli::manage::route(&[word.to_string()]),
                    Route::Fallthrough
                ),
                "裸保留词 {word} 应回落"
            );
            assert!(
                reserved_top_level_word_hint(word).is_some(),
                "{word} 应提示"
            );
        }
        // 真实多词 prompt 两关都放行。
        assert!(matches!(
            agent_cli::manage::route(&["explain".to_string(), "models".to_string()]),
            Route::Fallthrough
        ));
        assert!(reserved_top_level_word_hint("explain models").is_none());
    }

    /// H3 回归：Ctrl-C 路由——有活动 turn 时取消令牌，空闲时报告 Idle。
    #[test]
    fn sigint_router_cancels_active_turn_then_reports_idle() {
        let router = Arc::new(SigintRouter {
            current: std::sync::Mutex::new(None),
        });
        // 空闲：无令牌可取消。
        assert_eq!(router.interrupt(), SigintAction::Idle);

        let token = tokio_util::sync::CancellationToken::new();
        let guard = router.begin_turn(token.clone());
        assert_eq!(router.interrupt(), SigintAction::Cancelled);
        assert!(token.is_cancelled(), "Ctrl-C 必须取消活动 turn 的令牌");

        // 守卫 Drop 后回到空闲态（下一次 Ctrl-C 不再误取消已结束的 turn）。
        drop(guard);
        assert_eq!(router.interrupt(), SigintAction::Idle);
    }

    /// H1 回归：argv 预检必须拦住「未知 flag 沉入 prompt」的计费风险。
    mod argv_flag_guard {
        use super::*;

        fn built_cmd() -> clap::Command {
            let mut c = Cli::command();
            c.build();
            c
        }

        fn check(raw: &[&str]) -> Result<(), String> {
            let owned: Vec<String> = raw.iter().map(|s| (*s).to_string()).collect();
            validate_argv_flags(&built_cmd(), &owned)
        }

        #[test]
        fn unknown_flag_is_rejected() {
            // 原报告复现路径：--smoke-test 未定义，任何位置都不得成为 prompt 文本。
            let err = check(&["--model", "__nope__", "--smoke-test"]).unwrap_err();
            assert!(err.contains("--smoke-test"), "{err}");
            let err = check(&["--smoke-test"]).unwrap_err();
            assert!(err.contains("unknown flag"), "{err}");
            // 短旗标未知同样拒绝，且合并形态逐个字符校验。
            assert!(check(&["-Z"]).is_err());
            assert!(check(&["-aZ"]).is_err());
        }

        #[test]
        fn known_flags_pass() {
            assert!(check(&["--model", "x", "do", "it"]).is_ok());
            assert!(check(&["--model=x", "do", "it"]).is_ok());
            assert!(check(&["--acp"]).is_ok());
            assert!(check(&["--serve"]).is_ok());
            // --serve 可选值：地址被消费，后续词仍是 prompt。
            assert!(check(&["--serve", "0.0.0.0:80", "do", "it"]).is_ok());
            assert!(check(&["-h"]).is_ok());
            assert!(check(&[]).is_ok());
        }

        #[test]
        fn flag_after_task_is_rejected() {
            // clap 的 trailing_var_arg 会把它们当 prompt 文本静默忽略配置 → 报错更安全。
            let err = check(&["do", "it", "--model", "x"]).unwrap_err();
            assert!(err.contains("--model"), "{err}");
            assert!(check(&["do", "it", "--acp"]).is_err());
        }

        #[test]
        fn double_dash_escapes_literal_flags() {
            assert!(check(&["--", "do", "it", "--model", "x"]).is_ok());
        }

        #[test]
        fn help_after_task_is_allowed_for_subcommands() {
            // H2：`agent models --help` 由 manage::route 消费，预检放行。
            assert!(check(&["models", "--help"]).is_ok());
            assert!(check(&["models", "-h"]).is_ok());
            assert!(check(&["mcp", "add", "--help"]).is_ok());
        }

        #[test]
        fn numeric_tokens_are_prompt_words() {
            assert!(check(&["-1"]).is_ok());
            assert!(check(&["compare", "-1.5", "and", "-.5"]).is_ok());
        }
    }
}
