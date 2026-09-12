//! REPL 辅助：rustyline 行编辑 + Tab 补全，slash 命令分发与展示。
//!
//! 本模块把「命令如何展示/派发」与「命令如何改变运行时状态」解耦：
//! - [`handle_command`] 是纯函数：读取 [`CommandContext`] 快照后返回 [`CommandOutcome`]，
//!   状态变更（切换模型/模式、压缩、退出）交回 `main` 执行。
//! - [`ReplHelper`] 实现 rustyline 的补全：对 `/` 命令名及部分参数（模式、模型、skill）补全。

use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;

use agent_config::{Config, CustomCommand, ModelProfile};
use agent_context::SessionStore;
use agent_core::{ContextManager, Mode, Model, SkillLevel, Usage, UserContent};
/// i18n 取词宏。
use agent_i18n::t;
use agent_mcp::McpRegistry;
use agent_skills::SkillCatalog;
use agent_tools::{TodoItem, TodoPhase, Tool};
use rustyline::completion::Completer;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;

const MODES: [&str; 5] = ["code", "architect", "ask", "debug", "plan"];

/// 命令派发结果：`main` 据此决定是否改变运行时状态。
pub enum CommandOutcome {
    /// 命令已自行处理（已打印），继续等待输入。
    Handled,
    /// 注入文本作为下一轮任务。
    Inject(String),
    /// MCP 提示词模板（`/<server>:<prompt>`）：主循环异步 `prompts/get` 后注入结果文本。
    McpPrompt {
        /// server 名。
        server: String,
        /// 提示词名（server 内唯一）。
        prompt: String,
        /// `key=value` 解析后的参数对象。
        args: serde_json::Value,
    },
    /// 请求切换模型（alias 或 id）。
    SwitchModel(String),
    /// 请求切换模式。
    SwitchMode(Mode),
    /// 请求手动压缩上下文。
    Compact,
    /// 请求恢复历史会话（会话 id）。
    Resume(String),
    /// 请求运行 swarm 多代理编排（参数为 swarm YAML 文件路径）。
    Swarm(String),
    /// 打开子 Agent 监控仪表盘（终端备用屏，自动刷新，Enter 返回）。
    Agents,
    /// `/tree`（无参）：渲染会话树（活跃叶子标记；主循环异步快照后打印）。
    ShowTree,
    /// `/tree <id>` / `/branch <id>`：切换活跃叶子（handoff = 摘要交接分叉）。
    SwitchBranch {
        /// 目标节点（完整 id 或 ≥4 字符前缀）。
        node: String,
        /// true → `switch_branch_with_handoff`（离支摘要注入）。
        handoff: bool,
    },
    /// `/models`：运行时模型发现（当前 provider 的 /models 端点）。
    ListModels,
    /// 粘贴图像（剪贴板或本地文件）作为多模态用户消息发送。
    Paste {
        /// 附带的文本提示（可为空）。
        prompt: String,
        /// 图像内容块（mime + base64 数据）。
        image: UserContent,
    },
    /// 请求退出。
    Quit,
    /// `/diff [--staged] [ref]`：展示 git 工作区/暂存区/与指定 ref 的差异（截断输出）。
    Diff {
        /// 是否只展示暂存区（`--staged`）。
        staged: bool,
        /// 对比 ref（可选；缺省对比 HEAD/工作区）。
        ref_name: Option<String>,
    },
    /// `/fresh`：开启全新会话（新 id + 空上下文，保留模型/模式），不动本地历史记录。
    Fresh,
    /// 按需切换 GitHub 功能开关：动态注册/注销 `GithubTool` 并注入/屏蔽工具提示词。
    ///
    /// 触发 `main` 更新运行时开关并重建 Agent——下一轮 `set_system` 即反映新状态：
    /// 启用则把 [`agent_tools::PROMPT_SECTION`] 注入 system prompt 并注册工具；
    /// 禁用则两者一并移除，零额外 Token 开销。
    SetGithub {
        /// 是否启用 GitHub 工具与提示词。
        enabled: bool,
        /// 可选写操作开关；`Some` 时一并设置 `allow_write`（并隐含 `enabled`）。
        allow_write: Option<bool>,
    },
    /// 按需切换可选工具组（ast/lsp/image/hashline/pty/github）开关：
    /// 动态注册/注销对应工具并注入/屏蔽其操作提示词。
    ///
    /// 与 [`CommandOutcome::SetGithub`] 同构，但适用于所有可选工具组。
    /// 启用则注入提示词并注册工具，禁用则两者一并屏蔽（零额外 Token 开销）。
    SetTool {
        /// 工具组 key（ast/lsp/image/hashline/pty/github）。
        key: String,
        /// 是否启用。
        enabled: bool,
    },
    /// `/enhance <draft>`：Roo-Code 风格的 LLM 草稿增强（单一动作，无 preset）。
    /// `main` 负责打印结果（不注入为任务）。
    Enhance {
        /// 待增强的草稿文本。
        draft: String,
    },
    /// `/suggest <query>`：按相关性给工作区文件评分排序并打印。
    Suggest {
        /// 查询草稿文本。
        query: String,
    },
    /// `/review [--staged] [N]`：评审 git diff（默认 HEAD；`--staged` 只评暂存区），
    /// 按 diff 权重分配 1-N 个 TaskTool 子代理并行评审，聚合 P0-P3 报告（stderr 输出）。
    Review {
        /// 只评 staged diff（默认 HEAD）。
        staged: bool,
        /// 评审子代理数（1-4；`None` 按 diff 权重自动）。
        reviewers: Option<usize>,
    },
    /// H34 `/clear`：原地清空对话上下文（保留当前会话与系统提示词）。
    ///
    /// 需要 async + 逐条删除，故交回 `main` 执行（`handle_command` 是同步纯函数）。
    Clear,
    /// H34 `/context`：上下文占用与消息构成报告（异步取节点快照后打印）。
    Context,
    /// H34 `/retry`：重发最近一条用户输入（异步取上下文里最后一条真实 prompt）。
    Retry,
}

/// 命令执行所需的只读上下文快照。
pub struct CommandContext<'a> {
    /// 当前模型。
    pub model: &'a Model,
    /// 当前模式。
    pub mode: Mode,
    /// 上下文管理器（取 token 用量）。
    pub context: &'a dyn ContextManager,
    /// 本会话累计用量。
    pub accumulated: &'a Usage,
    /// MCP 注册表。
    pub mcp: &'a McpRegistry,
    /// Skill 目录。
    pub skills: &'a SkillCatalog,
    /// 配置（取模型 profile 列表）。
    pub config: &'a Config,
    /// 自定义命令表。
    pub commands: &'a [CustomCommand],
    /// 会话存储。
    pub sessions: &'a SessionStore,
    /// 当前会话 id。
    pub session_id: &'a str,
    /// 上下文窗口阈值。
    pub guard: f32,
    /// 工作目录。
    pub cwd: &'a Path,
    /// GitHub 工具运行时启用态（可由 `/github` 动态切换，初值取自配置 `[github]`）。
    pub github_enabled: bool,
    /// GitHub 写操作运行时开关。
    pub github_allow_write: bool,
    /// 可选工具组运行时开关（ast/lsp/image/hashline/pty；github 用上面两个字段）。
    pub optional: &'a std::collections::HashMap<String, bool>,
    /// goals 目标预算共享状态（`/goal` 查看/调整；未配置时为 `None`）。
    pub goal: Option<Arc<std::sync::Mutex<agent::GoalState>>>,
    /// todo 清单共享状态（`/todo` 查看/编辑清单）。
    pub todo: Arc<agent_tools::TodoState>,
    /// 异步后台作业管理器（`/jobs` 列表与取消；`[agent] async_enabled=false` 时为 `None`）。
    pub jobs: Option<&'a Arc<agent_core::jobs::AsyncJobManager>>,
    /// H4：运行期消息队列（`/queue`；运行中键入的消息在此排队，空闲时取回执行）。
    pub queue: &'a Arc<std::sync::Mutex<agent_cli::queue::MessageQueue>>,
    /// H5：生效键位表（`/hotkeys` 展示真实绑定，而非静态文案）。
    pub keybindings: &'a agent_cli::keybindings::Resolved,
}

/// 内置命令名（带 `/`），用于补全与帮助。
#[must_use]
pub const fn builtin_commands() -> &'static [&'static str] {
    &[
        "/h",
        "/help",
        "/?",
        "/status",
        "/usage",
        "/context",
        "/clear",
        "/new",
        "/retry",
        "/jobs",
        "/queue",
        "/settings",
        "/hotkeys",
        "/todo",
        "/goal",
        "/diff",
        "/fresh",
        "/model",
        "/mode",
        "/plan",
        "/paste",
        "/enhance",
        "/suggest",
        "/review",
        "/compact",
        "/tree",
        "/branch",
        "/models",
        "/mcp",
        "/skill",
        "/skills",
        "/sessions",
        "/session",
        "/resume",
        "/swarm",
        "/agents",
        "/collab",
        "/github",
        "/tools",
        "/lang",
        "/exit",
        "/quit",
    ]
}

/// `/help` 展示顺序（即 [`print_help`] 的渲染清单，也是帮助覆盖回归测试的事实源）。
///
/// 新增内置命令时：先在 [`builtin_commands`] 登记名字，再补一条 `help.*` 文案并在此登记——
/// `help_covers_all_builtin_commands` 会同时校验两侧不漂移。
const HELP_KEYS: &[&str] = &[
    "help.h",
    "help.status",
    "help.usage",
    "help.context",
    "help.clear",
    "help.new",
    "help.retry",
    "help.jobs",
    "help.queue",
    "help.settings",
    "help.hotkeys",
    "help.todo",
    "help.goal",
    "help.diff",
    "help.fresh",
    "help.model",
    "help.mode",
    "help.plan",
    "help.tree",
    "help.branch",
    "help.models",
    "help.paste",
    "help.enhance",
    "help.suggest",
    "help.compact",
    "help.mcp",
    "help.skill",
    "help.skill_colon",
    "help.sessions",
    "help.session",
    "help.swarm",
    "help.agents",
    "help.review",
    "help.collab",
    "help.github",
    "help.tools",
    "help.lang",
    "help.exit",
];

/// 所有可选模型（alias 与 id 并集，去重排序），用于补全。
#[must_use]
pub fn model_choices(config: &Config) -> Vec<String> {
    let mut out = Vec::new();
    out.push(config.default_model.id.clone());
    if let Some(a) = &config.default_model.alias {
        out.push(a.clone());
    }
    for m in &config.models {
        out.push(m.id.clone());
        if let Some(a) = &m.alias {
            out.push(a.clone());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 所有命令名（内置 + 自定义，带 `/`，去重排序），用于补全。
#[must_use]
pub fn all_command_names(custom: &[CustomCommand]) -> Vec<String> {
    let mut out: Vec<String> = builtin_commands()
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    for c in custom {
        out.push(format!("/{}", c.name));
    }
    out.sort();
    out.dedup();
    out
}

/// 内置 + 自定义 + MCP 提示词命令名（补全用；提示词随 server 连接/刷新变化，
/// 调用方每次补全前重建即可拿到最新集合）。
#[must_use]
pub fn all_command_names_with_prompts(
    custom: &[CustomCommand],
    prompts: &[(String, agent_mcp::McpPromptInfo)],
) -> Vec<String> {
    let mut out = all_command_names(custom);
    out.extend(prompts.iter().map(|(s, p)| format!("/{s}:{}", p.name)));
    out.sort();
    out.dedup();
    out
}

/// 解析模式名 → [`Mode`]。
#[must_use]
pub fn parse_mode(arg: &str) -> Option<Mode> {
    match arg.trim() {
        "code" => Some(Mode::Code),
        "architect" => Some(Mode::Architect),
        "ask" => Some(Mode::Ask),
        "debug" => Some(Mode::Debug),
        "plan" => Some(Mode::Plan),
        _ => None,
    }
}

/// `/github` 子命令解析结果（驱动按需加载开关）。
enum GithubToggle {
    /// 显示状态（无参数）。
    Status,
    /// 启用 GitHub 工具与提示词。
    Enable,
    /// 禁用 GitHub 工具与提示词（完全屏蔽）。
    Disable,
    /// 启用并打开写操作。
    Write,
    /// 未识别参数（携带原始串，用于错误提示）。
    Unknown(String),
}

/// 解析 `/github` 子命令参数（大小写不敏感、去除首尾空白；空参数视为查看状态）。
#[must_use]
fn parse_github_subcommand(arg: Option<&str>) -> GithubToggle {
    let Some(a) = arg else {
        return GithubToggle::Status;
    };
    match a.trim().to_ascii_lowercase().as_str() {
        "" => GithubToggle::Status,
        "on" | "enable" | "enabled" => GithubToggle::Enable,
        "off" | "disable" | "disabled" => GithubToggle::Disable,
        "write" => GithubToggle::Write,
        other => GithubToggle::Unknown(other.to_string()),
    }
}

/// 主分发：根据输入返回动作。状态变更交给 `main`。
pub fn handle_command(input: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let name = input.split_whitespace().next().unwrap_or("");
    match name {
        "/h" | "/help" | "/?" => {
            print_help(ctx);
            CommandOutcome::Handled
        }
        "/tree" => match input.split_whitespace().nth(1) {
            Some(id) if !id.is_empty() => CommandOutcome::SwitchBranch {
                node: id.to_string(),
                handoff: false,
            },
            _ => CommandOutcome::ShowTree,
        },
        "/branch" => match input.split_whitespace().nth(1) {
            Some(id) if !id.is_empty() => CommandOutcome::SwitchBranch {
                node: id.to_string(),
                handoff: true,
            },
            _ => {
                eprintln!("{}", t!("tree.usage"));
                CommandOutcome::Handled
            }
        },
        "/models" => CommandOutcome::ListModels,
        "/status" => {
            print_status(ctx);
            CommandOutcome::Handled
        }
        "/usage" => {
            print_usage(ctx);
            CommandOutcome::Handled
        }
        "/context" => CommandOutcome::Context,
        "/clear" => CommandOutcome::Clear,
        // `/new` = 全新会话（与 `/fresh` 同义；对齐 omp「Start a new session」）。
        "/new" => CommandOutcome::Fresh,
        "/retry" => CommandOutcome::Retry,
        "/jobs" => {
            handle_jobs(input, ctx);
            CommandOutcome::Handled
        }
        "/queue" => handle_queue(input, ctx),
        "/settings" => {
            print_settings(ctx);
            CommandOutcome::Handled
        }
        "/hotkeys" => {
            print_hotkeys(ctx);
            CommandOutcome::Handled
        }
        "/todo" => {
            // H34：可写清单（add/start/done/drop/rm/clear；无参 = 查看）。
            handle_todo(input, ctx);
            CommandOutcome::Handled
        }
        "/goal" => {
            handle_goal(input, ctx);
            CommandOutcome::Handled
        }
        "/agents" => CommandOutcome::Agents,
        "/mcp" => {
            handle_mcp(input, ctx);
            CommandOutcome::Handled
        }
        "/skill" | "/skills" => {
            let arg = input.split_whitespace().nth(1);
            match arg {
                Some(n) => inject_skill(n, ctx),
                None => {
                    print_skills(ctx);
                    CommandOutcome::Handled
                }
            }
        }
        "/sessions" => {
            print_sessions(ctx);
            CommandOutcome::Handled
        }
        "/session" => match input.split_whitespace().nth(1) {
            Some(id) if !id.is_empty() => CommandOutcome::Resume(id.to_string()),
            _ => {
                print_sessions(ctx);
                CommandOutcome::Handled
            }
        },
        "/resume" => match input.split_whitespace().nth(1) {
            // H34：`/resume <n>` 按 `/sessions` 列表序号选择；否则按会话 id。
            Some(arg) if !arg.is_empty() => match arg.parse::<usize>() {
                Ok(n) if n >= 1 => {
                    let list = ctx.sessions.list();
                    match list.get(n - 1) {
                        Some(s) => CommandOutcome::Resume(s.id.clone()),
                        None => {
                            eprintln!("{}", t!("resume.bad_index", n = n, count = list.len()));
                            CommandOutcome::Handled
                        }
                    }
                }
                _ => CommandOutcome::Resume(arg.to_string()),
            },
            _ => {
                // 无参：列出带序号的候选（选择器），并提示两种用法。
                print_session_choices(ctx);
                CommandOutcome::Handled
            }
        },
        "/compact" => CommandOutcome::Compact,
        "/paste" => handle_paste(input, ctx),
        "/model" => {
            let arg = input.split_whitespace().nth(1);
            match arg {
                Some(a) if !a.is_empty() => CommandOutcome::SwitchModel(a.to_string()),
                _ => {
                    print_models(ctx);
                    CommandOutcome::Handled
                }
            }
        }
        "/mode" => {
            let arg = input.split_whitespace().nth(1);
            match arg.and_then(parse_mode) {
                Some(m) => CommandOutcome::SwitchMode(m),
                None => {
                    eprintln!("{}", t!("mode.usage", current = mode_label(ctx.mode)));
                    CommandOutcome::Handled
                }
            }
        }
        "/plan" => {
            // P1-5：进入计划模式（仅 plans/*.md 可写，执行类需确认；批准后 /mode code）。
            eprintln!("{}", t!("plan.enter"));
            CommandOutcome::SwitchMode(Mode::Plan)
        }
        "/diff" => {
            // P1-4：git diff（工作区 / --staged / 指定 ref），截断展示。
            let rest = input.strip_prefix("/diff").unwrap_or("").trim();
            let staged = rest.split_whitespace().any(|t| t == "--staged");
            let ref_name = rest
                .split_whitespace()
                .find(|t| !t.starts_with("--"))
                .map(str::to_string);
            CommandOutcome::Diff { staged, ref_name }
        }
        "/fresh" => CommandOutcome::Fresh,
        "/swarm" => match input.split_whitespace().nth(1) {
            Some(f) if !f.is_empty() => CommandOutcome::Swarm(f.to_string()),
            _ => {
                eprintln!("{}", t!("swarm.usage"));
                eprintln!("{}", t!("swarm.usage_fields"));
                eprintln!("{}", t!("swarm.usage_modes"));
                CommandOutcome::Handled
            }
        },
        "/collab" => {
            print_collab(ctx);
            CommandOutcome::Handled
        }
        "/lang" => handle_lang(input),
        "/github" => match parse_github_subcommand(input.split_whitespace().nth(1)) {
            GithubToggle::Status => {
                print_github(ctx);
                CommandOutcome::Handled
            }
            GithubToggle::Enable => CommandOutcome::SetGithub {
                enabled: true,
                allow_write: None,
            },
            GithubToggle::Disable => CommandOutcome::SetGithub {
                enabled: false,
                allow_write: None,
            },
            GithubToggle::Write => CommandOutcome::SetGithub {
                enabled: true,
                allow_write: Some(true),
            },
            GithubToggle::Unknown(o) => {
                eprintln!("{}", t!("github.unknown_arg", arg = o));
                CommandOutcome::Handled
            }
        },
        "/tools" => handle_tools(input, ctx),
        "/enhance" => handle_enhance(input),
        "/suggest" => handle_suggest(input),
        "/review" => handle_review(input),
        "/exit" | "/quit" => CommandOutcome::Quit,
        other if other.starts_with("/skill:") => {
            let skill_name = &other["/skill:".len()..];
            inject_skill(skill_name, ctx)
        }
        other => match resolve_mcp_prompt(other, input, ctx) {
            PromptCmd::Ready(outcome) => outcome,
            // 命中 `server:prompt` 形态但缺必填参数（用法已打印）。
            PromptCmd::UsageError => CommandOutcome::Handled,
            PromptCmd::NotPrompt => match try_custom_command(other, input, ctx.commands) {
                Some(text) => CommandOutcome::Inject(text),
                None => {
                    eprintln!("{}", t!("common.unknown_command", cmd = other));
                    CommandOutcome::Handled
                }
            },
        },
    }
}

/// 解析提示词命令的参数：`key=value` 词元 → JSON 对象 + 缺失的必填项名。
///
/// 纯函数（无注册表/IO 依赖）便于测试：无法解析的词元忽略（omp 同款宽容语义），
/// 必填项缺失只报告、不猜测默认值。
fn parse_prompt_args(
    raw: &str,
    prompt: &agent_mcp::McpPromptInfo,
) -> (serde_json::Value, Vec<String>) {
    let mut args = serde_json::Map::new();
    for tok in raw.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            if !k.is_empty() {
                args.insert(k.to_string(), serde_json::Value::String(v.to_string()));
            }
        }
    }
    let missing: Vec<String> = prompt
        .arguments
        .iter()
        .filter(|a| a.required && !args.contains_key(&a.name))
        .map(|a| a.name.clone())
        .collect();
    (serde_json::Value::Object(args), missing)
}

/// 提示词用法的展示形态（必填 `k=<值>`、可选 `[k]=<值>`）。
fn prompt_usage_args(prompt: &agent_mcp::McpPromptInfo) -> String {
    prompt
        .arguments
        .iter()
        .map(|a| {
            if a.required {
                format!("{}=<值>", a.name)
            } else {
                format!("[{}]=<值>", a.name)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// MCP 提示词命令的解析结果。
enum PromptCmd {
    /// 不是 MCP 提示词（含无 `server:prompt` 形态 / server、提示词不存在）。
    NotPrompt,
    /// 命中但缺必填参数（用法已打印）。
    UsageError,
    /// 命中且参数齐备，交由主循环异步 `prompts/get` 后注入。
    Ready(CommandOutcome),
}

/// MCP 提示词命令解析：`/<server>:<prompt> [key=value …]`（对齐 omp
/// `buildMCPPromptCommands` 的命令名与参数约定）。
fn resolve_mcp_prompt(cmd_token: &str, full: &str, ctx: &CommandContext<'_>) -> PromptCmd {
    let token = cmd_token.trim_start_matches('/');
    let Some((server, prompt_name)) = token.split_once(':') else {
        return PromptCmd::NotPrompt;
    };
    if server.is_empty() || prompt_name.is_empty() {
        return PromptCmd::NotPrompt;
    }
    let Some(prompt) = ctx
        .mcp
        .prompts()
        .into_iter()
        .find(|(s, p)| s == server && p.name == prompt_name)
    else {
        return PromptCmd::NotPrompt;
    };

    let raw = full
        .trim_start_matches('/')
        .strip_prefix(token)
        .unwrap_or("")
        .trim();
    let (args, missing) = parse_prompt_args(raw, &prompt.1);
    if !missing.is_empty() {
        eprintln!(
            "{}",
            t!(
                "mcp.prompt_usage",
                cmd = format!("{server}:{prompt_name}"),
                args = prompt_usage_args(&prompt.1),
                missing = missing.join(", ")
            )
        );
        return PromptCmd::UsageError;
    }
    PromptCmd::Ready(CommandOutcome::McpPrompt {
        server: server.to_string(),
        prompt: prompt_name.to_string(),
        args,
    })
}

// ── 图片粘贴（剪贴板 / 本地文件）──────────────────────────────────────────────

/// `/paste`：从剪贴板或本地文件读取图像，构造多模态用户消息。
///
/// 形式：
/// - `/paste`                  读系统剪贴板图像
/// - `/paste <提示>`           读剪贴板图像并附带文本提示
/// - `/paste <文件路径>`       读本地图片文件（png/jpeg/gif/webp）
/// - `/paste <文件路径> <提示>` 读文件并附带提示
fn handle_paste(input: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let rest = input.strip_prefix("/paste").unwrap_or("").trim();
    let mut tokens = rest.split_whitespace();
    let first = tokens.next();
    let remainder: String = tokens.collect::<Vec<_>>().join(" ");

    // 第一个 token 若指向已存在的文件，则按文件读取。
    if let Some(tok) = first {
        let candidate = ctx.cwd.join(tok);
        if candidate.is_file() {
            return match read_image_from_file(&candidate) {
                Ok(image) => CommandOutcome::Paste {
                    prompt: remainder,
                    image,
                },
                Err(e) => {
                    eprintln!("{}", t!("paste.read_file_failed", e = e));
                    CommandOutcome::Handled
                }
            };
        }
    }

    // 否则读剪贴板；提示取整个剩余文本（含 first）。
    let prompt = first.map_or_else(String::new, |_| rest.to_string());
    match read_clipboard_image() {
        Ok(image) => CommandOutcome::Paste { prompt, image },
        Err(e) => {
            eprintln!("{}", t!("paste.clipboard_failed", e = e));
            CommandOutcome::Handled
        }
    }
}

/// 从本地图片文件读取字节并编码为 base64（保留原始字节，不做解码）。
fn read_image_from_file(path: &Path) -> Result<UserContent, String> {
    const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(t!("paste.image_too_large", n = bytes.len()));
    }
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        other => return Err(t!("paste.unsupported_format", fmt = format!("{other:?}"))),
    };
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(UserContent::Image {
        mime: mime.to_string(),
        data,
    })
}

/// 从系统剪贴板读取图像（RGBA 像素），编码为 PNG 再 base64。
fn read_clipboard_image() -> Result<UserContent, String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let img = cb.get_image().map_err(|e| e.to_string())?;
    let w = u32::try_from(img.width).map_err(|_| t!("paste.cb_width_overflow"))?;
    let h = u32::try_from(img.height).map_err(|_| t!("paste.cb_height_overflow"))?;
    // 防止超大剪贴板图像 OOM：像素总数与原始字节设上限（与 read_image 一致 10 MiB）。
    const MAX_CB_PIXELS: usize = 10 * 1024 * 1024;
    let raw_len = img.bytes.len();
    if raw_len > MAX_CB_PIXELS {
        return Err(t!("paste.image_too_large", n = raw_len));
    }
    let buf: image::ImageBuffer<image::Rgba<u8>, Vec<u8>> =
        image::ImageBuffer::from_raw(w, h, img.bytes.into_owned())
            .ok_or_else(|| t!("paste.cb_invalid"))?;
    let mut png = Vec::new();
    {
        let mut writer = std::io::Cursor::new(&mut png);
        image::DynamicImage::ImageRgba8(buf)
            .write_to(&mut writer, image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
    }
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(&png);
    Ok(UserContent::Image {
        mime: "image/png".to_string(),
        data,
    })
}

// ── 展示函数 ──────────────────────────────────────────────────────────────────

fn print_help(ctx: &CommandContext<'_>) {
    eprintln!("{}", t!("help.title"));
    for key in HELP_KEYS {
        eprintln!("{}", agent_i18n::tr(key, &[]));
    }
    if !ctx.commands.is_empty() {
        eprintln!("{}", t!("help.custom_title"));
        for c in ctx.commands {
            if c.description.is_empty() {
                eprintln!("  /{}", c.name);
            } else {
                eprintln!(
                    "{}",
                    t!("help.custom_entry", name = c.name, desc = c.description)
                );
            }
        }
    }
    eprintln!("{}", t!("help.tip"));
}

fn print_status(ctx: &CommandContext<'_>) {
    let usage = ctx.context.token_usage();
    let pct = if usage.limit > 0 {
        #[allow(clippy::cast_precision_loss)]
        {
            usage.current as f64 / usage.limit as f64 * 100.0
        }
    } else {
        0.0
    };
    eprintln!("{}", t!("status.title"));
    eprintln!("{}", t!("status.model", model = ctx.model.id));
    eprintln!("{}", t!("status.mode", mode = mode_label(ctx.mode)));
    eprintln!("{}", t!("status.session", session = ctx.session_id));
    eprintln!("{}", t!("status.cwd", cwd = ctx.cwd.display()));
    eprintln!(
        "{}",
        t!(
            "status.context",
            current = usage.current,
            limit = usage.limit,
            pct = format!("{:.1}", pct)
        )
    );
    if usage.limit > 0 {
        eprintln!("{}", t!("status.bar", bar = render_bar(pct, 30)));
        eprintln!(
            "{}",
            t!("status.guard", pct = format!("{:.0}", ctx.guard * 100.0))
        );
    }
    eprintln!(
        "{}",
        t!(
            "status.tokens",
            input = ctx.accumulated.input_tokens,
            out = ctx.accumulated.output_tokens,
            cr = ctx.accumulated.cache_read_tokens,
            cw = ctx.accumulated.cache_write_tokens
        )
    );
    // P0-4：缓存命中率（cache_read / 提示词总量），为零时省略该行。
    let prompt_total = ctx.accumulated.input_tokens + ctx.accumulated.cache_read_tokens;
    if prompt_total > 0 {
        #[allow(clippy::cast_precision_loss)]
        let rate = ctx.accumulated.cache_read_tokens as f64 / prompt_total as f64 * 100.0;
        eprintln!(
            "{}",
            t!(
                "status.cache",
                rate = format!("{:.1}", rate),
                cr = ctx.accumulated.cache_read_tokens,
                total = prompt_total
            )
        );
    }
    eprintln!(
        "{}",
        t!(
            "status.cost",
            cost = format!("{:.6}", ctx.accumulated.cost_usd)
        )
    );
    eprintln!("{}", t!("status.footer"));
}

/// H34 `/context` 的构成统计（纯函数，便于回归测试）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ContextBreakdown {
    /// 活跃路径消息总数。
    pub total: usize,
    /// 用户消息数。
    pub user: usize,
    /// 助手消息数。
    pub assistant: usize,
    /// 工具结果数。
    pub tool: usize,
    /// 其它（状态/询问/软需求等）。
    pub other: usize,
}

/// 统计活跃路径的消息构成（`/context` 报告用）。
#[must_use]
pub fn context_breakdown(nodes: &[agent_core::SessionNode]) -> ContextBreakdown {
    let mut out = ContextBreakdown::default();
    for n in nodes {
        out.total += 1;
        match n.message {
            agent_core::AgentMessage::User(_) => out.user += 1,
            agent_core::AgentMessage::Assistant(_) => out.assistant += 1,
            agent_core::AgentMessage::ToolResult(_) => out.tool += 1,
            _ => out.other += 1,
        }
    }
    out
}

/// H34 `/retry`：取最近一条「真实用户输入」（跳过系统注入的提醒/通知）。
///
/// 注入消息（eager prelude / magic-keyword 通知 / 预算提醒 / 压缩提示）都以
/// `<`、`[system`、`⚠` 等标记开头；重试应回到用户原话，而不是重发一条提醒。
#[must_use]
pub fn last_user_prompt(nodes: &[agent_core::SessionNode]) -> Option<String> {
    let text_of = |m: &agent_core::AgentMessage| -> Option<String> {
        let agent_core::AgentMessage::User(u) = m else {
            return None;
        };
        let text = u
            .content
            .iter()
            .filter_map(|c| match c {
                UserContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        (!text.trim().is_empty()).then_some(text)
    };
    let is_injected = |t: &str| {
        let t = t.trim_start();
        t.starts_with('<') || t.starts_with("[system") || t.starts_with('⚠')
    };
    let mut fallback = None;
    for n in nodes.iter().rev() {
        let Some(text) = text_of(&n.message) else {
            continue;
        };
        if !is_injected(&text) {
            return Some(text);
        }
        fallback.get_or_insert(text);
    }
    fallback
}

/// H4 `/queue`：运行期消息队列。
///
/// - 无参 / `list`：列出排队消息（序号从 1 起）
/// - `add <文本>` / `enqueue <文本>`：入队（支持 `1. a\n2. b` 顺序列表一次入多条）
/// - `pop` / `next`：出队头部并**立即作为下一轮任务**执行（空闲时的「取回执行」）
/// - `undo` / `back`：取回最近一次入队
/// - `rm <序号>` / `drop <序号>`：按序号删除
/// - `clear`：清空
fn handle_queue(input: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let mut parts = input.splitn(3, char::is_whitespace);
    let _ = parts.next();
    let sub = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    let Ok(mut q) = ctx.queue.lock() else {
        eprintln!("队列锁中毒");
        return CommandOutcome::Handled;
    };
    match sub {
        "" | "list" => {
            let items = q.list();
            if items.is_empty() {
                eprintln!("{}", t!("queue.empty"));
            } else {
                eprintln!("{}", t!("queue.title", count = items.len()));
                for (idx, text) in items {
                    eprintln!("  {idx}. {}", text.replace('\n', " ⏎ "));
                }
            }
            CommandOutcome::Handled
        }
        "add" | "enqueue" => {
            if rest.is_empty() {
                eprintln!("{}", t!("queue.usage"));
                return CommandOutcome::Handled;
            }
            let msgs = agent_cli::queue::split_queued_messages(rest);
            let n = msgs.len();
            let len = q.enqueue_many(msgs);
            eprintln!("{}", t!("queue.added", n = n, len = len));
            CommandOutcome::Handled
        }
        "pop" | "next" => match q.pop_front() {
            Some(text) => {
                eprintln!("{}", t!("queue.popped", text = text.replace('\n', " ⏎ ")));
                CommandOutcome::Inject(text)
            }
            None => {
                eprintln!("{}", t!("queue.empty"));
                CommandOutcome::Handled
            }
        },
        "undo" | "back" => match q.pop_back() {
            Some(text) => {
                eprintln!("{}", t!("queue.undone", text = text.replace('\n', " ⏎ ")));
                CommandOutcome::Handled
            }
            None => {
                eprintln!("{}", t!("queue.empty"));
                CommandOutcome::Handled
            }
        },
        "rm" | "drop" => match rest.parse::<usize>() {
            Ok(n) => match q.remove(n) {
                Some(text) => {
                    eprintln!(
                        "{}",
                        t!("queue.removed", n = n, text = text.replace('\n', " ⏎ "))
                    );
                    CommandOutcome::Handled
                }
                None => {
                    eprintln!("{}", t!("queue.bad_index", n = n, len = q.len()));
                    CommandOutcome::Handled
                }
            },
            Err(_) => {
                eprintln!("{}", t!("queue.usage"));
                CommandOutcome::Handled
            }
        },
        "clear" => {
            let n = q.clear();
            eprintln!("{}", t!("queue.cleared", n = n));
            CommandOutcome::Handled
        }
        _ => {
            eprintln!("{}", t!("queue.usage"));
            CommandOutcome::Handled
        }
    }
}

/// H4：运行中键入的处理（`/queue` 由宿主消费；`->`/`=>` 入队；其余仍即时 steering）。
///
/// 返回 `Some(text)` 表示该消息应**立即投递给运行中的 agent**（dequeue 语义）。
pub fn handle_running_input(
    line: &str,
    queue: &Arc<std::sync::Mutex<agent_cli::queue::MessageQueue>>,
) -> RunningInput {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return RunningInput::Ignored;
    }
    // 简写：只入队，不打断当前轮次。
    if let Some(msg) = agent_cli::queue::parse_queue_shorthand(trimmed) {
        let len = lock_queue(queue).map_or(0, |mut q| q.enqueue(msg));
        return RunningInput::Queued(len);
    }
    if let Some(rest) = trimmed.strip_prefix("/queue") {
        let Ok(mut q) = queue.lock() else {
            return RunningInput::Ignored;
        };
        let args: Vec<&str> = rest.split_whitespace().collect();
        match args.first().copied() {
            None | Some("list") => {
                let items = q.list();
                if items.is_empty() {
                    eprintln!("{}", t!("queue.empty"));
                } else {
                    eprintln!("{}", t!("queue.title", count = items.len()));
                    for (idx, text) in items {
                        eprintln!("  {idx}. {}", text.replace('\n', " ⏎ "));
                    }
                }
            }
            Some("add" | "enqueue") => {
                let text = rest
                    .trim_start()
                    .strip_prefix("add")
                    .or_else(|| rest.trim_start().strip_prefix("enqueue"))
                    .unwrap_or("")
                    .trim();
                if text.is_empty() {
                    eprintln!("{}", t!("queue.usage"));
                } else {
                    let msgs = agent_cli::queue::split_queued_messages(text);
                    let n = msgs.len();
                    let len = q.enqueue_many(msgs);
                    eprintln!("{}", t!("queue.added", n = n, len = len));
                }
            }
            Some("pop" | "next") => match q.pop_front() {
                Some(text) => {
                    eprintln!("{}", t!("queue.popped", text = text.replace('\n', " ⏎ ")));
                    // 运行中取回 = 立刻投递（不等本轮结束）。
                    return RunningInput::Steer(text);
                }
                None => eprintln!("{}", t!("queue.empty")),
            },
            Some("undo" | "back") => match q.pop_back() {
                Some(text) => {
                    eprintln!("{}", t!("queue.undone", text = text.replace('\n', " ⏎ ")));
                }
                None => eprintln!("{}", t!("queue.empty")),
            },
            Some("clear") => {
                let n = q.clear();
                eprintln!("{}", t!("queue.cleared", n = n));
            }
            Some(_) => eprintln!("{}", t!("queue.usage")),
        }
        return RunningInput::Ignored;
    }
    RunningInput::Steer(trimmed.to_string())
}

/// 运行中键入的处理结果（H4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunningInput {
    /// 忽略（空行 / `/queue` 已由宿主消费）。
    Ignored,
    /// 入队成功（携带入队后长度）。
    Queued(usize),
    /// 立即投递给运行中的 agent（普通输入 / 运行中取回）。
    Steer(String),
}

/// 取队列锁（中毒时恢复内部数据，不静默丢弃队列）。
fn lock_queue(
    queue: &Arc<std::sync::Mutex<agent_cli::queue::MessageQueue>>,
) -> Option<std::sync::MutexGuard<'_, agent_cli::queue::MessageQueue>> {
    queue.lock().ok()
}

/// H34 `/usage`：会话累计用量与上下文占用（只读；数据源同 `/status`，侧重 token/成本口径）。
fn print_usage(ctx: &CommandContext<'_>) {
    let usage = ctx.context.token_usage();
    let pct = if usage.limit > 0 {
        #[allow(clippy::cast_precision_loss)]
        {
            usage.current as f64 / usage.limit as f64 * 100.0
        }
    } else {
        0.0
    };
    eprintln!("{}", t!("usage.title"));
    eprintln!(
        "{}",
        t!(
            "usage.tokens",
            input = ctx.accumulated.input_tokens,
            out = ctx.accumulated.output_tokens,
            cr = ctx.accumulated.cache_read_tokens,
            cw = ctx.accumulated.cache_write_tokens
        )
    );
    let billed = ctx.accumulated.input_tokens
        + ctx.accumulated.cache_write_tokens
        + ctx.accumulated.output_tokens;
    let prompt_total = ctx.accumulated.input_tokens + ctx.accumulated.cache_read_tokens;
    let rate = if prompt_total > 0 {
        #[allow(clippy::cast_precision_loss)]
        {
            ctx.accumulated.cache_read_tokens as f64 / prompt_total as f64 * 100.0
        }
    } else {
        0.0
    };
    eprintln!(
        "{}",
        t!(
            "usage.billed",
            billed = billed,
            rate = format!("{rate:.1}"),
            total = prompt_total
        )
    );
    eprintln!(
        "{}",
        t!(
            "usage.cost",
            cost = format!("{:.6}", ctx.accumulated.cost_usd)
        )
    );
    eprintln!(
        "{}",
        t!(
            "usage.context",
            current = usage.current,
            limit = usage.limit,
            pct = format!("{pct:.1}")
        )
    );
}

/// H34 `/jobs`：后台作业列表 / `cancel <id>`。
fn handle_jobs(input: &str, ctx: &CommandContext<'_>) {
    let Some(jm) = ctx.jobs else {
        eprintln!("{}", t!("jobs.disabled"));
        return;
    };
    let args: Vec<&str> = input.split_whitespace().collect();
    match args.get(1).copied() {
        None | Some("list") => {
            let jobs = jm.recent_jobs(20, None);
            if jobs.is_empty() {
                eprintln!("{}", t!("jobs.none"));
                return;
            }
            eprintln!("{}", t!("jobs.title", count = jobs.len()));
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            for j in jobs {
                let label = preview_text(&j.label, 60);
                eprintln!(
                    "{}",
                    t!(
                        "jobs.entry",
                        id = j.id,
                        kind = j.job_type.as_str(),
                        status = j.status.as_str(),
                        ms = j.duration_ms(now),
                        label = label
                    )
                );
            }
        }
        Some("cancel") => match args.get(2).copied() {
            Some(id) if !id.is_empty() => {
                if jm.cancel(id, None) {
                    eprintln!("{}", t!("jobs.cancelled", id = id));
                } else {
                    eprintln!("{}", t!("jobs.not_found", id = id));
                }
            }
            _ => eprintln!("{}", t!("jobs.usage")),
        },
        Some(_) => eprintln!("{}", t!("jobs.usage")),
    }
}

/// H34 `/settings`：当前生效设置摘要（配置 + 运行时开关；只读展示）。
fn print_settings(ctx: &CommandContext<'_>) {
    let cfg = ctx.config;
    let on: Vec<&str> = ctx
        .optional
        .iter()
        .filter_map(|(k, v)| v.then_some(k.as_str()))
        .collect();
    eprintln!("{}", t!("settings.title"));
    eprintln!(
        "{}",
        t!(
            "settings.model",
            id = ctx.model.id,
            count = cfg.models.len() + 1
        )
    );
    eprintln!("{}", t!("settings.mode", mode = mode_label(ctx.mode)));
    eprintln!(
        "{}",
        t!(
            "settings.approval",
            mode = format!("{:?}", cfg.agent.approval_mode)
        )
    );
    eprintln!(
        "{}",
        t!(
            "settings.guard",
            pct = format!("{:.0}", cfg.agent.context_window_guard * 100.0),
            turns = cfg.agent.max_turns
        )
    );
    eprintln!(
        "{}",
        t!(
            "settings.todo",
            eager = cfg.todo.eager.as_deref().unwrap_or("off"),
            reminders = cfg.todo.reminders,
            max = cfg.todo.reminders_max
        )
    );
    eprintln!(
        "{}",
        t!(
            "settings.goals",
            tokens = cfg.goals.token_budget,
            secs = cfg.goals.time_budget_secs,
            mode = if cfg.goals.hard_stop { "hard" } else { "soft" }
        )
    );
    eprintln!(
        "{}",
        t!(
            "settings.async",
            enabled = cfg.agent.async_enabled,
            max = cfg.agent.async_max_jobs
        )
    );
    eprintln!(
        "{}",
        t!(
            "settings.subagent",
            enabled = cfg.subagent.enabled,
            conc = cfg.subagent.max_concurrent
        )
    );
    eprintln!(
        "{}",
        t!(
            "settings.compaction",
            backend = format!("{:?}", cfg.compaction.backend)
        )
    );
    eprintln!(
        "{}",
        t!(
            "settings.tools",
            on = if on.is_empty() {
                "-".to_string()
            } else {
                on.join(",")
            },
            github = if ctx.github_enabled { "on" } else { "off" },
            write = if ctx.github_allow_write { "on" } else { "off" }
        )
    );
    eprintln!(
        "{}",
        t!(
            "settings.lang",
            lang = cfg.language.as_deref().unwrap_or("auto"),
            ua = cfg.user_agent.as_deref().unwrap_or("default")
        )
    );
    eprintln!("{}", t!("settings.footer"));
}

/// H5 `/hotkeys`：打印**生效**键位表（默认 + `[keybindings]` 覆盖），并附固定约定。
fn print_hotkeys(ctx: &CommandContext<'_>) {
    eprintln!("{}", t!("hotkeys.title"));
    for line in agent_cli::keybindings::render_table(ctx.keybindings) {
        eprintln!("{line}");
    }
    eprintln!("{}", t!("hotkeys.fixed"));
    for line in [
        "  Enter           提交输入（多行粘贴后一次性提交）",
        "  Up / Down       历史上下浏览",
        "  Esc             取消当前行 / 关闭选择器",
    ] {
        eprintln!("{line}");
    }
}

/// H34 `/todo [add|start|done|drop|rm|clear]`：查看与编辑待办清单（与 `todo` 工具同一状态）。
///
/// id 采用工具同款 `t<N>`；`replace` 会按位置重排 id，故只改阶段、不重排顺序
/// （单活跃不变量由 `TodoState::replace` 内部的 `enforce_single_active` 保证）。
fn handle_todo(input: &str, ctx: &CommandContext<'_>) {
    let mut parts = input.splitn(3, char::is_whitespace);
    let _ = parts.next();
    let sub = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    if sub.is_empty() || sub == "list" || sub == "show" {
        eprintln!("{}", ctx.todo.render_markdown());
        return;
    }
    let mut items = ctx.todo.snapshot().items;
    match sub {
        "add" => {
            if rest.is_empty() {
                eprintln!("{}", t!("todo.usage"));
                return;
            }
            items.push(TodoItem {
                id: String::new(),
                content: rest.to_string(),
                phase: TodoPhase::Pending,
                blocked_reason: None,
            });
            let list = ctx.todo.replace(items);
            eprintln!("{}", t!("todo.added", count = list.items.len()));
        }
        "start" | "done" | "drop" | "rm" => {
            if rest.is_empty() {
                eprintln!("{}", t!("todo.usage"));
                return;
            }
            let target = normalize_todo_id(rest);
            let phase = match sub {
                "start" => TodoPhase::InProgress,
                "done" => TodoPhase::Completed,
                _ => TodoPhase::Abandoned,
            };
            let Some(item) = items.iter_mut().find(|i| i.id == target) else {
                eprintln!("{}", t!("todo.unknown_id", id = target));
                return;
            };
            item.phase = phase;
            if phase != TodoPhase::Blocked {
                item.blocked_reason = None;
            }
            let list = ctx.todo.replace(items);
            eprintln!(
                "{}",
                t!(
                    "todo.updated",
                    id = target,
                    phase = phase.as_str(),
                    count = list.items.len()
                )
            );
        }
        "clear" => {
            ctx.todo.replace(Vec::new());
            eprintln!("{}", t!("todo.cleared"));
        }
        _ => eprintln!("{}", t!("todo.usage")),
    }
}

/// 把 `3` / `t3` / `#3` 归一为工具同款 id（`t3`）。
fn normalize_todo_id(raw: &str) -> String {
    let t = raw.trim().trim_start_matches(['#', 't']);
    format!("t{t}")
}

/// H34 `/resume`（无参）：带序号的会话候选列表（配合 `/resume <n>`）。
fn print_session_choices(ctx: &CommandContext<'_>) {
    let list = ctx.sessions.list();
    if list.is_empty() {
        eprintln!("{}", t!("sessions.none"));
        return;
    }
    eprintln!("{}", t!("resume.title"));
    for (idx, s) in list.iter().enumerate() {
        let cur = if s.id == ctx.session_id {
            t!("sessions.current_mark")
        } else {
            String::new()
        };
        let preview = first_user_message_text(&ctx.sessions.path_for(&s.id))
            .map(|txt| format!("「{}」", preview_text(&txt, 50)))
            .unwrap_or_else(|| t!("sessions.no_user_msg"));
        eprintln!(
            "  {:>2}. {}  {}  {}{}",
            idx + 1,
            s.id,
            format_time(s.mtime),
            preview,
            cur
        );
    }
    eprintln!("{}", t!("resume.hint"));
}

/// `/goal [set <tokens> | extend <tokens> | pause | resume | complete | drop]`：
/// 查看 / 调整目标状态与预算（H29：目标状态机）。
///
/// - 无参数：显示目标状态（objective/续跑计数）与会话预算、累计用量与是否超限。
/// - `set <tokens>` / `extend <tokens>`：调整 token 预算（0 = 不限）。
/// - `pause` / `resume` / `complete` / `drop`：目标状态机（与 `goal` 工具同一状态）。
///
/// 预算调整即时生效（共享 `Arc<Mutex<GoalState>>`，无需重建 Agent）；若调整后未超限，
/// 重置一次性提醒标记，允许后续再次超限时重新注入。
fn handle_goal(input: &str, ctx: &CommandContext<'_>) {
    let Some(goal) = ctx.goal.as_ref() else {
        eprintln!("{}", t!("goal.disabled"));
        return;
    };
    let rest = input.strip_prefix("/goal").unwrap_or("").trim();
    if rest.is_empty() {
        let g = goal.lock().expect("goal 锁中毒");
        eprintln!("{}", t!("goal.title"));
        eprintln!(
            "{}",
            t!(
                "goal.objective",
                state = g
                    .goal_summary()
                    .unwrap_or_else(|| t!("goal.none").to_string())
            )
        );
        eprintln!("{}", t!("goal.status", summary = g.summary()));
        eprintln!(
            "{}",
            t!(
                "goal.exceeded",
                state = if g.exceeded() { "yes" } else { "no" }
            )
        );
        eprintln!(
            "{}",
            t!(
                "goal.mode",
                mode = if g.budget.hard_stop { "hard" } else { "soft" }
            )
        );
        eprintln!("{}", t!("goal.usage"));
        return;
    }
    // H29：状态机子命令（与 `goal` 工具共享同一状态；错误如实回报而非静默）。
    match rest {
        "pause" | "resume" | "complete" | "drop" => {
            let outcome = {
                let mut g = goal.lock().expect("goal 锁中毒");
                match rest {
                    "pause" => g.pause().map(|()| t!("goal.paused").to_string()),
                    "resume" => g
                        .resume()
                        .map(|()| t!("goal.resumed", summary = g.summary()).to_string()),
                    "complete" => g
                        .complete()
                        .map(|()| t!("goal.completed", summary = g.summary()).to_string()),
                    _ => Ok(g.drop_goal().map_or_else(
                        || t!("goal.none").to_string(),
                        |o| t!("goal.dropped", objective = o).to_string(),
                    )),
                }
            };
            match outcome {
                Ok(msg) => eprintln!("{msg}"),
                Err(e) => eprintln!("{}", t!("goal.error", error = e)),
            }
            return;
        }
        _ => {}
    }
    let tokens: u64 = rest
        .split_whitespace()
        .last()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let mut g = goal.lock().expect("goal 锁中毒");
    if let Some(n) = rest.strip_prefix("set ") {
        g.budget.token_budget = n.trim().parse().unwrap_or(0);
    } else if let Some(n) = rest.strip_prefix("extend ") {
        g.budget.token_budget = g
            .budget
            .token_budget
            .saturating_add(n.trim().parse().unwrap_or(0));
    } else {
        eprintln!("{}", t!("goal.usage"));
        return;
    }
    // 调整后未超限则重置提醒标记（允许再次超限时重新注入）。
    if !g.exceeded() {
        g.notified = false;
    }
    drop(g);
    let g = goal.lock().expect("goal 锁中毒");
    eprintln!(
        "{}",
        t!(
            "goal.updated",
            budget = g.budget.token_budget,
            tokens = tokens
        )
    );
}

/// 打印 GitHub 工具运行时状态（启用/写权限/token）与按需切换提示。
///
/// 状态取自运行时（可被 `/github` 动态切换），而非静态配置——
/// 动态开关后此处即时反映，与下一轮 system prompt 注入保持一致。
fn print_github(ctx: &CommandContext<'_>) {
    let token_set = std::env::var("GH_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok()
        .filter(|s| !s.is_empty())
        .is_some();
    eprintln!("{}", t!("github.title"));
    eprintln!(
        "{}",
        t!(
            "github.enabled_line",
            state = if ctx.github_enabled { "on" } else { "off" }
        )
    );
    eprintln!(
        "{}",
        t!(
            "github.write_line",
            state = if ctx.github_allow_write { "on" } else { "off" }
        )
    );
    eprintln!(
        "{}",
        t!(
            "github.token_line",
            state = if token_set {
                t!("github.token_set")
            } else {
                t!("github.token_unset")
            }
        )
    );
    if !ctx.github_enabled {
        eprintln!("{}", t!("github.hint_enable"));
        eprintln!("{}", t!("github.hint_config"));
    } else if !ctx.github_allow_write {
        eprintln!("{}", t!("github.hint_write"));
    }
    eprintln!("{}", t!("tools.footer"));
}

/// `/tools`：查看 / 按需切换可选工具组（ast/lsp/image/hashline/pty/github）。
///
/// 形式：
/// - `/tools`                 列出所有可选组及启用态
/// - `/tools <key>`           查看指定组（等价于列表视图）
/// - `/tools <key> on|off`    启用/禁用指定组（动态注册工具并注入/屏蔽提示词）
fn handle_tools(input: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let mut parts = input.split_whitespace();
    let _ = parts.next(); // "/tools"
    let key = parts.next();
    let val = parts.next();
    match (key, val) {
        (None, _) => {
            print_tools(ctx);
            CommandOutcome::Handled
        }
        (Some(k), None) => {
            if !is_known_tools_key(k) {
                eprintln!("{}", t!("tools.unknown_group", key = k));
            } else {
                print_tools(ctx);
            }
            CommandOutcome::Handled
        }
        (Some(k), Some(v)) => {
            let enabled = match v.to_ascii_lowercase().as_str() {
                "on" | "enable" | "enabled" | "true" => true,
                "off" | "disable" | "disabled" | "false" => false,
                other => {
                    eprintln!("{}", t!("tools.unknown_switch", val = other, key = k));
                    return CommandOutcome::Handled;
                }
            };
            if !is_known_tools_key(k) {
                eprintln!("{}", t!("tools.unknown_group", key = k));
                return CommandOutcome::Handled;
            }
            CommandOutcome::SetTool {
                key: k.to_string(),
                enabled,
            }
        }
    }
}

/// 可选工具组展示表（key, 对应 i18n 标签 key）。
const OPTIONAL_TOOL_GROUPS: &[(&str, &str)] = &[
    ("ast", "tools.label.ast"),
    ("lsp", "tools.label.lsp"),
    ("image", "tools.label.image"),
    ("hashline", "tools.label.hashline"),
    ("pty", "tools.label.pty"),
    ("github", "tools.label.github"),
];

/// 判断 key 是否为已知可选工具组（含 github）。
#[must_use]
fn is_known_tools_key(key: &str) -> bool {
    OPTIONAL_TOOL_GROUPS.iter().any(|(k, _)| *k == key)
}

/// 打印所有可选工具组及其运行时启用态。
fn print_tools(ctx: &CommandContext<'_>) {
    eprintln!("{}", t!("tools.title"));
    eprintln!("{}", t!("tools.core_line"));
    for (key, label_key) in OPTIONAL_TOOL_GROUPS {
        let on = if *key == "github" {
            ctx.github_enabled
        } else {
            *ctx.optional.get(*key).unwrap_or(&false)
        };
        eprintln!(
            "{}",
            t!(
                "tools.entry",
                state = if on { "on" } else { "off" },
                key = key,
                label = agent_i18n::tr(label_key, &[])
            )
        );
    }
    eprintln!("{}", t!("tools.hint_toggle"));
    eprintln!("{}", t!("tools.hint_config"));
    eprintln!("{}", t!("tools.footer"));
}

/// `/tools` 参数补全：第一个参数补工具组 key，第二个补 on/off。
fn complete_tools_args(typed: &str, prefix: &str) -> Vec<String> {
    // 第一个参数 token 已完整（其后含空格）→ 进入第二个参数
    let in_second_arg = typed
        .strip_prefix("/tools")
        .unwrap_or("")
        .trim_start()
        .contains(' ');
    if !in_second_arg {
        ["ast", "lsp", "image", "hashline", "pty", "github"]
            .iter()
            .filter(|k| k.starts_with(prefix))
            .map(|s| (*s).to_string())
            .collect()
    } else {
        ["on", "off"]
            .iter()
            .filter(|a| a.starts_with(prefix))
            .map(|s| (*s).to_string())
            .collect()
    }
}

/// `/mcp [status|list|add|remove|enable|disable]`：无参 = 列出已加载工具（原行为）；
/// 其余子命令复用 [`agent_cli::manage`] 的路由与实现（CLI / REPL 同一份逻辑，不复制）。
///
/// 写操作落盘后「下次启动生效」——运行中的 `McpRegistry` 不在本模块的可变面内。
fn handle_mcp(input: &str, ctx: &CommandContext<'_>) {
    let rest = input.strip_prefix("/mcp").unwrap_or("").trim();
    if rest.is_empty() {
        print_mcp(ctx);
        return;
    }
    let mut tokens = vec!["mcp".to_owned()];
    tokens.extend(rest.split_whitespace().map(str::to_owned));
    let Some(config_dir) = agent_core::platform::config_dir() else {
        eprintln!("{}", t!("manage.mcp.no_config_dir"));
        return;
    };
    let mut out = std::io::stderr();
    match agent_cli::manage::route(&tokens) {
        agent_cli::manage::Route::Run(cmd) => {
            if let Err(e) = agent_cli::manage::run_mcp_cmd(&cmd, ctx.cwd, &config_dir, &mut out) {
                eprintln!("{e}");
            }
        }
        agent_cli::manage::Route::Usage(msg) => eprintln!("{msg}"),
        agent_cli::manage::Route::Help(text) => eprintln!("{text}"),
        agent_cli::manage::Route::Fallthrough => eprintln!("{}", t!("manage.mcp.usage")),
    }
}

fn print_mcp(ctx: &CommandContext<'_>) {
    let tools = ctx.mcp.tools();
    if tools.is_empty() {
        eprintln!("{}", t!("mcp.none"));
    } else {
        eprintln!("{}", t!("mcp.tools_count", count = tools.len()));
        for tool in tools {
            let desc = if tool.description().is_empty() {
                t!("mcp.no_desc")
            } else {
                tool.description().to_string()
            };
            // Deferred 工具（server 未连上、元信息来自缓存快照）标注，避免误以为可直接用。
            let mark = if tool.is_deferred() {
                t!("mcp.deferred_mark")
            } else {
                String::new()
            };
            eprintln!("  - {}{}  {}", tool.name(), mark, desc);
        }
    }
    // 提示词模板即斜杠命令：`/<server>:<prompt>`（内容由 server 侧模板生成）。
    let prompts = ctx.mcp.prompts();
    if !prompts.is_empty() {
        eprintln!("{}", t!("mcp.prompts_count", count = prompts.len()));
        for (server, p) in prompts {
            let desc = if p.description.is_empty() {
                t!("mcp.no_desc")
            } else {
                p.description
            };
            eprintln!("  - /{server}:{}  {}", p.name, desc);
        }
    }
}

fn print_skills(ctx: &CommandContext<'_>) {
    if ctx.skills.is_empty() {
        eprintln!("{}", t!("skill.none"));
        return;
    }
    eprintln!("{}", t!("skill.count", count = ctx.skills.skills.len()));
    for s in &ctx.skills.skills {
        let level = match s.source.level {
            SkillLevel::User => "user",
            SkillLevel::Project => "project",
        };
        let desc = if s.description.is_empty() {
            t!("skill.no_desc")
        } else {
            s.description.clone()
        };
        eprintln!("  - {}  [{level}]  {}", s.name, desc);
    }
}

fn print_models(ctx: &CommandContext<'_>) {
    eprintln!("{}", t!("model.current", id = ctx.model.id));
    eprintln!("{}", t!("model.available"));
    print_profile(&ctx.config.default_model, &ctx.model.id, true);
    for m in &ctx.config.models {
        print_profile(m, &ctx.model.id, false);
    }
    eprintln!("{}", t!("model.switch_hint"));
}

fn print_profile(p: &ModelProfile, current_id: &str, is_default: bool) {
    let mark = if p.id == current_id {
        t!("model.current_mark")
    } else {
        String::new()
    };
    let alias = p
        .alias
        .as_deref()
        .map(|a| t!("model.alias_suffix", alias = a))
        .unwrap_or_default();
    let dft = if is_default {
        t!("model.default_mark")
    } else {
        String::new()
    };
    eprintln!(
        "{}",
        t!(
            "model.profile_line",
            id = p.id,
            alias = alias,
            default = dft,
            mark = mark,
            api = p.api,
            base = p.base_url
        )
    );
}

fn print_sessions(ctx: &CommandContext<'_>) {
    let list = ctx.sessions.list();
    if list.is_empty() {
        eprintln!("{}", t!("sessions.none"));
        return;
    }
    eprintln!("{}", t!("sessions.list_title"));
    for s in list {
        let cur = if s.id == ctx.session_id {
            t!("sessions.current_mark")
        } else {
            String::new()
        };
        // 首条用户输入：读取会话 JSONL 的第一条 User 消息（遇到即停）
        let preview = first_user_message_text(&ctx.sessions.path_for(&s.id))
            .map(|txt| format!("「{}」", preview_text(&txt, 50)))
            .unwrap_or_else(|| t!("sessions.no_user_msg"));
        eprintln!("  {}  {}  {}{}", s.id, format_time(s.mtime), preview, cur);
    }
}

/// 读取会话 JSONL 的第一条用户消息文本（逐行解析，遇到即停，避免读取整个大文件）。
fn first_user_message_text(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = line.ok()?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // v1+ 会话文件首行为 header 记录：跳过（非消息）。
        if agent_context::is_session_header_line(trimmed) {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<agent_core::AgentMessage>(trimmed) else {
            continue;
        };
        if let agent_core::AgentMessage::User(u) = msg {
            let text: String = u
                .content
                .iter()
                .filter_map(|c| match c {
                    agent_core::UserContent::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// 折叠空白并截断预览，使每个会话在一行内可读展示。
fn preview_text(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let truncated: String = flat.chars().take(max).collect();
    format!("{truncated}…")
}

/// 读取会话 JSONL 并渲染为可读对话历史（仅 user/assistant 轮次）。
/// 每条折叠空白并截断，便于在终端一行回顾；tool 结果/状态/ask 等消息被跳过。
#[must_use]
pub fn session_history_lines(path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(file) = std::fs::File::open(path) else {
        return out;
    };
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<agent_core::AgentMessage>(trimmed) else {
            continue;
        };
        match msg {
            agent_core::AgentMessage::User(u) => {
                let text: String = u
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        agent_core::UserContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let t = preview_text(&text, 300);
                if !t.is_empty() {
                    out.push(format!("🧑 {t}"));
                }
            }
            agent_core::AgentMessage::Assistant(a) => {
                let mut text = String::new();
                let mut tools = Vec::new();
                for b in &a.content {
                    match b {
                        agent_core::ContentBlock::Text { text: t } => text.push_str(t),
                        agent_core::ContentBlock::ToolCall { name, .. } => tools.push(name.clone()),
                        agent_core::ContentBlock::Thinking { .. } => {}
                    }
                }
                let t = preview_text(&text, 600);
                if !t.is_empty() {
                    out.push(format!("🤖 {t}"));
                }
                if !tools.is_empty() {
                    out.push(t!("event.tool_call_history", tools = tools.join(", ")));
                }
            }
            _ => {}
        }
    }
    out
}

/// SystemTime → UTC `MM-DD HH:MM`（无外部依赖，基于 Howard Hinnant 的 civil date 算法）。
fn format_time(time: std::time::SystemTime) -> String {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let (_, m, d) = civil_from_days(day);
    format!("{m:02}-{d:02} {:02}:{:02}", sod / 3600, (sod % 3600) / 60)
}

/// 儒略日序号（自 1970-01-01 起的天数）→ (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `/collab`：生成一个端到端加密协同房间，打印密钥片段、中继地址与分享链接。
///
/// 中继本身对密钥盲视（按派生的 room_id 路由不透明密封字节）；
/// 密钥仅存于链接的 `#` 片段，需先以 `agent --serve` 启动中继。
fn print_collab(ctx: &CommandContext<'_>) {
    let key = agent_collab::generate_room_key();
    let room = agent_collab::room_id(&key);
    let fragment = agent_collab::encode_room_key(&key);
    // 本地 CLI 生成的房间不经服务端注册 write token → 中继按开放房间处理；
    // 经 `agent --serve` 的 `/api/collab/room` 创建的房间才具备写权限校验。
    // 这里仍打印 token 与两种链接形态，供用户了解权限模型。
    let token = agent_collab::generate_write_token();
    let bind = &ctx.config.server.bind;
    eprintln!("{}", t!("collab.title"));
    eprintln!("{}", t!("collab.room", room = room));
    eprintln!("{}", t!("collab.fragment", fragment = fragment));
    eprintln!("{}", t!("collab.relay", bind = bind, room = room));
    eprintln!(
        "{}",
        t!("collab.write_link", bind = bind, room = room, token = token)
    );
    eprintln!("{}", t!("collab.view_link", bind = bind, room = room));
    eprintln!("{}", t!("collab.link", bind = bind, fragment = fragment));
    eprintln!("{}", t!("collab.footer"));
    eprintln!("{}", t!("collab.tip"));
    eprintln!("{}", t!("collab.wt_tip"));
}

/// `/lang [code]`：查看 / 切换界面语言（en/zh/ru/ja）。直接作用于全局 i18n 状态，即时生效。
fn handle_lang(input: &str) -> CommandOutcome {
    let arg = input.split_whitespace().nth(1);
    match arg {
        None => {
            let cur = agent_i18n::current_locale();
            let list = agent_i18n::SUPPORTED
                .iter()
                .map(|c| format!("{c} ({})", agent_i18n::tr(&format!("lang.name.{c}"), &[])))
                .collect::<Vec<_>>()
                .join(" / ");
            eprintln!("{}", t!("lang.current", locale = cur));
            eprintln!("{}", t!("lang.available", list = list));
        }
        Some(code) => {
            if agent_i18n::SUPPORTED.contains(&code) {
                agent_i18n::init(Some(code));
                eprintln!("{}", t!("lang.switched", locale = code));
            } else {
                eprintln!("{}", t!("lang.invalid", code = code));
            }
        }
    }
    CommandOutcome::Handled
}

/// `/enhance <draft>`：解析草稿（Roo-Code 风格单一增强，无 preset / 无标志）。
///
/// 空草稿 → 打印用法并返回 [`CommandOutcome::Handled`]。
fn handle_enhance(input: &str) -> CommandOutcome {
    let draft: String = input
        .split_whitespace()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ");
    if draft.is_empty() {
        eprintln!("{}", t!("enhance.usage"));
        return CommandOutcome::Handled;
    }
    CommandOutcome::Enhance { draft }
}

/// `/suggest <query>`：解析查询草稿。空查询 → 打印用法并返回 Handled。
fn handle_suggest(input: &str) -> CommandOutcome {
    let query: String = input
        .split_whitespace()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ");
    if query.is_empty() {
        eprintln!("{}", t!("suggest.usage"));
        return CommandOutcome::Handled;
    }
    CommandOutcome::Suggest { query }
}

/// `/review [--staged] [N]`：解析评审参数。
///
/// - `--staged`：只评暂存区 diff（默认 HEAD）。
/// - `N`：评审子代理数（1-4）；省略按 diff 权重自动。
///
/// 参数非法（N 越界 / 未知参数）→ 打印用法并返回 [`CommandOutcome::Handled`]。
fn handle_review(input: &str) -> CommandOutcome {
    let mut staged = false;
    let mut reviewers: Option<usize> = None;
    for tok in input.split_whitespace().skip(1) {
        match tok {
            "--staged" | "-s" => staged = true,
            n if n.bytes().all(|b| b.is_ascii_digit()) => match n.parse::<usize>() {
                Ok(v) if (1..=4).contains(&v) => reviewers = Some(v),
                _ => {
                    eprintln!("{}", t!("review.usage"));
                    return CommandOutcome::Handled;
                }
            },
            _ => {
                eprintln!("{}", t!("review.usage"));
                return CommandOutcome::Handled;
            }
        }
    }
    CommandOutcome::Review { staged, reviewers }
}

fn inject_skill(name: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    match ctx.skills.find(name) {
        Some(skill) => match std::fs::read_to_string(&skill.file_path) {
            Ok(content) => CommandOutcome::Inject(content),
            Err(e) => {
                eprintln!("{}", t!("skill.read_failed", e = e));
                CommandOutcome::Handled
            }
        },
        None => {
            eprintln!("{}", t!("skill.unknown", name = name));
            CommandOutcome::Handled
        }
    }
}

fn try_custom_command(cmd_token: &str, full: &str, commands: &[CustomCommand]) -> Option<String> {
    let name = cmd_token.trim_start_matches('/');
    let name = name.split_whitespace().next()?;
    let c = commands.iter().find(|c| c.name == name)?;
    let args = full
        .trim_start_matches('/')
        .strip_prefix(name)
        .unwrap_or("");
    let args = args.trim();
    if args.is_empty() {
        Some(c.body.clone())
    } else {
        Some(format!("{}\n\n# 命令参数\n{args}", c.body))
    }
}

fn mode_label(m: Mode) -> String {
    match m {
        Mode::Code => t!("mode.code"),
        Mode::Architect => t!("mode.architect"),
        Mode::Ask => t!("mode.ask"),
        Mode::Debug => t!("mode.debug"),
        Mode::Plan => t!("mode.plan"),
    }
}

fn render_bar(pct: f64, width: usize) -> String {
    let pct = pct.clamp(0.0, 100.0);
    let filled = (pct / 100.0 * width as f64).round() as usize;
    let filled = filled.min(width);
    let bar: String = "█".repeat(filled);
    let empty: String = "░".repeat(width - filled);
    format!("[{bar}{empty}]")
}

// ── rustyline 补全 ────────────────────────────────────────────────────────────

/// rustyline 辅助：提供 `/` 命令名与部分参数的 Tab 补全。
pub struct ReplHelper {
    commands: Vec<String>,
    model_aliases: Vec<String>,
    skill_names: Vec<String>,
    session_ids: Vec<String>,
}

impl ReplHelper {
    /// 构造补全器。
    #[must_use]
    pub fn new(
        commands: Vec<String>,
        model_aliases: Vec<String>,
        skill_names: Vec<String>,
        session_ids: Vec<String>,
    ) -> Self {
        Self {
            commands,
            model_aliases,
            skill_names,
            session_ids,
        }
    }

    /// 纯补全逻辑（无 rustyline Context 依赖，便于测试）。
    /// 返回 `(起始位置, 候选列表)`：rustyline 会用候选替换 `line[start..pos]`。
    #[must_use]
    pub fn complete_line(&self, line: &str, pos: usize) -> (usize, Vec<String>) {
        let typed = &line[..pos];
        if !typed.starts_with('/') {
            return (0, Vec::new());
        }

        // 未输入空格：补全命令名（或 /skill:<name>）
        if !typed.contains(' ') {
            if let Some(rest) = typed.strip_prefix("/skill:") {
                let cands = self
                    .skill_names
                    .iter()
                    .filter(|s| s.starts_with(rest))
                    .map(|s| format!("/skill:{s}"))
                    .collect::<Vec<_>>();
                return (0, cands);
            }
            let mut cands = self
                .commands
                .iter()
                .filter(|c| c.starts_with(typed))
                .cloned()
                .collect::<Vec<_>>();
            cands.sort();
            cands.dedup();
            return (0, cands);
        }

        // 带空格：按命令补全参数
        let cmd = typed.split_whitespace().next().unwrap_or("");
        let arg_start = typed.rfind(' ').map(|i| i + 1).unwrap_or(0);
        let prefix = &typed[arg_start..];
        let cands: Vec<String> = match cmd {
            "/mode" => MODES
                .iter()
                .filter(|m| m.starts_with(prefix))
                .map(|s| (*s).to_string())
                .collect(),
            "/github" => ["on", "off", "write"]
                .iter()
                .filter(|a| a.starts_with(prefix))
                .map(|s| (*s).to_string())
                .collect(),
            "/tools" => complete_tools_args(typed, prefix),
            "/lang" => ["en", "zh", "ru", "ja"]
                .iter()
                .filter(|c| c.starts_with(prefix))
                .map(|s| (*s).to_string())
                .collect(),
            "/model" => self
                .model_aliases
                .iter()
                .filter(|a| a.starts_with(prefix))
                .cloned()
                .collect(),
            "/skill" | "/skills" => self
                .skill_names
                .iter()
                .filter(|s| s.starts_with(prefix))
                .cloned()
                .collect(),
            "/session" | "/sessions" | "/resume" => self
                .session_ids
                .iter()
                .filter(|s| s.starts_with(prefix))
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        (arg_start, cands)
    }
}

impl Completer for ReplHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        Ok(self.complete_line(line, pos))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;
}

impl Highlighter for ReplHelper {}

impl Validator for ReplHelper {}

impl rustyline::Helper for ReplHelper {}

#[cfg(test)]
mod tests {
    use super::*;

    fn helper() -> ReplHelper {
        ReplHelper::new(
            vec![
                "/help".into(),
                "/model".into(),
                "/mode".into(),
                "/mcp".into(),
            ],
            vec!["ds".into(), "gpt-4o".into()],
            vec!["pdf".into(), "react".into()],
            vec!["abc123".into(), "def456".into()],
        )
    }

    #[test]
    fn parses_mode_names() {
        assert_eq!(parse_mode("code"), Some(Mode::Code));
        assert_eq!(parse_mode("architect"), Some(Mode::Architect));
        assert_eq!(parse_mode("  debug "), Some(Mode::Debug));
        assert_eq!(parse_mode("plan"), Some(Mode::Plan));
        assert_eq!(parse_mode("bogus"), None);
    }

    #[test]
    fn parses_github_subcommand() {
        assert!(matches!(
            parse_github_subcommand(None),
            GithubToggle::Status
        ));
        assert!(matches!(
            parse_github_subcommand(Some("")),
            GithubToggle::Status
        ));
        assert!(matches!(
            parse_github_subcommand(Some("   ")),
            GithubToggle::Status
        ));
        assert!(matches!(
            parse_github_subcommand(Some("on")),
            GithubToggle::Enable
        ));
        assert!(matches!(
            parse_github_subcommand(Some(" Enable ")),
            GithubToggle::Enable
        ));
        assert!(matches!(
            parse_github_subcommand(Some("ENABLED")),
            GithubToggle::Enable
        ));
        assert!(matches!(
            parse_github_subcommand(Some("off")),
            GithubToggle::Disable
        ));
        assert!(matches!(
            parse_github_subcommand(Some("DISABLED")),
            GithubToggle::Disable
        ));
        assert!(matches!(
            parse_github_subcommand(Some("write")),
            GithubToggle::Write
        ));
        assert!(matches!(
            parse_github_subcommand(Some("bogus")),
            GithubToggle::Unknown(_)
        ));
    }

    #[test]
    fn completes_command_prefix() {
        let h = helper();
        let (start, cands) = h.complete_line("/mo", 3);
        assert_eq!(start, 0);
        assert!(cands.contains(&"/mode".to_string()));
        assert!(cands.contains(&"/model".to_string()));
    }

    #[test]
    fn completes_mode_args_after_space() {
        let h = helper();
        // "/mode ar" 长度为 8，光标在末尾 → prefix="ar"
        let (start, cands) = h.complete_line("/mode ar", "/mode ar".len());
        assert_eq!(start, 6);
        assert_eq!(cands, vec!["architect".to_string()]);
    }

    #[test]
    fn completes_model_alias_args() {
        let h = helper();
        let (start, cands) = h.complete_line("/model d", 8);
        assert_eq!(start, 7);
        assert_eq!(cands, vec!["ds".to_string()]);
    }

    #[test]
    fn completes_skill_colon_form() {
        let h = helper();
        let (start, cands) = h.complete_line("/skill:p", 8);
        assert_eq!(start, 0);
        assert_eq!(cands, vec!["/skill:pdf".to_string()]);
    }

    #[test]
    fn no_completion_without_slash() {
        let h = helper();
        let (_, cands) = h.complete_line("hello", 5);
        assert!(cands.is_empty());
    }

    #[test]
    fn all_command_names_includes_builtin_and_custom() {
        let custom = vec![CustomCommand {
            name: "test".into(),
            description: String::new(),
            body: String::new(),
        }];
        let names = all_command_names(&custom);
        assert!(names.contains(&"/help".to_string()));
        assert!(names.contains(&"/test".to_string()));
        assert!(names.contains(&"/status".to_string()));
    }

    fn sample_prompt() -> agent_mcp::McpPromptInfo {
        agent_mcp::McpPromptInfo {
            name: "review".into(),
            description: "代码评审".into(),
            arguments: vec![
                agent_mcp::McpPromptArg {
                    name: "path".into(),
                    required: true,
                },
                agent_mcp::McpPromptArg {
                    name: "lang".into(),
                    required: false,
                },
            ],
        }
    }

    /// `key=value` 词元解析 + 必填项缺失报告（不猜默认值）。
    #[test]
    fn prompt_args_parse_kv_and_report_missing_required() {
        let prompt = sample_prompt();
        let (args, missing) = parse_prompt_args("path=src/a.rs lang=rust 裸词", &prompt);
        assert_eq!(args["path"], serde_json::json!("src/a.rs"));
        assert_eq!(args["lang"], serde_json::json!("rust"));
        assert!(missing.is_empty(), "可选参数不影响必填校验");

        let (args, missing) = parse_prompt_args("lang=rust", &prompt);
        assert_eq!(args["lang"], serde_json::json!("rust"));
        assert_eq!(missing, vec!["path".to_string()]);

        // 空参数：必填仍缺。
        let (_, missing) = parse_prompt_args("", &prompt);
        assert_eq!(missing, vec!["path".to_string()]);

        // 用法展示区分必填/可选。
        assert_eq!(prompt_usage_args(&prompt), "path=<值> [lang]=<值>");
    }

    /// MCP 提示词以 `/<server>:<prompt>` 进入补全命令表（与内置/自定义命令并存）。
    #[test]
    fn command_names_include_mcp_prompts() {
        let custom = vec![CustomCommand {
            name: "test".into(),
            description: String::new(),
            body: String::new(),
        }];
        let prompts = vec![("github".to_string(), sample_prompt())];
        let names = all_command_names_with_prompts(&custom, &prompts);
        assert!(names.contains(&"/github:review".to_string()));
        assert!(names.contains(&"/help".to_string()));
        assert!(names.contains(&"/test".to_string()));
        // 无提示词时不改变既有命令面。
        assert_eq!(
            all_command_names_with_prompts(&custom, &[]),
            all_command_names(&custom)
        );
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // 闰年：2024-02-29 存在 → 2024-03-01 = 19723 + 31(Jan) + 29(Feb)
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
        assert_eq!(civil_from_days(19_784), (2024, 3, 2));
    }

    #[test]
    fn preview_text_collapses_and_truncates() {
        assert_eq!(preview_text("hello   world\nfoo", 50), "hello world foo");
        assert_eq!(preview_text("short", 50), "short");
        let long = "x".repeat(60);
        let p = preview_text(&long, 10);
        assert_eq!(p.chars().count(), 11); // 10 字符 + …
        assert!(p.ends_with('…'));
    }

    #[test]
    fn first_user_message_from_jsonl() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("agent-sess-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = dir.join(format!("{n}.jsonl"));
        let msg = agent_core::AgentMessage::user_text("请帮我重构这段代码");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "{}", serde_json::to_string(&msg).unwrap()).unwrap();
        }
        assert_eq!(
            first_user_message_text(&path),
            Some("请帮我重构这段代码".to_string())
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn first_user_message_skips_session_header() {
        use agent_context::{CURRENT_SESSION_VERSION, SessionHeader};
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("agent-sess-hdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = dir.join(format!("{n}.jsonl"));
        let header = SessionHeader {
            version: CURRENT_SESSION_VERSION,
            created_at_unix: 1_700_000_000,
            agent: "gyre".into(),
            ..SessionHeader::new("gyre")
        };
        let header_line =
            serde_json::to_string(&agent_context::SessionRecord::Header(header)).unwrap();
        let msg = agent_core::AgentMessage::user_text("header 之后的用户消息");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "{header_line}").unwrap();
            writeln!(f, "{}", serde_json::to_string(&msg).unwrap()).unwrap();
        }
        assert_eq!(
            first_user_message_text(&path),
            Some("header 之后的用户消息".to_string())
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn session_history_renders_user_and_assistant() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("agent-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = dir.join(format!("{n}.jsonl"));
        let assistant = agent_core::AssistantMessage {
            content: vec![agent_core::ContentBlock::Text {
                text: "好的，我来帮你".into(),
            }],
            usage: agent_core::Usage::default(),
            model: "test".into(),
            stop_reason: None,
            stop_details: None,
        };
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(
                f,
                "{}",
                serde_json::to_string(&agent_core::AgentMessage::user_text("hello")).unwrap()
            )
            .unwrap();
            writeln!(
                f,
                "{}",
                serde_json::to_string(&agent_core::AgentMessage::Assistant(assistant)).unwrap()
            )
            .unwrap();
        }
        let lines = session_history_lines(&path);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("🧑") && l.contains("hello"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("🤖") && l.contains("好的，我来帮你"))
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_image_from_file_png() {
        let dir = std::env::temp_dir().join(format!("agent-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n payload").unwrap();
        match read_image_from_file(&path) {
            Ok(UserContent::Image { mime, data }) => {
                assert_eq!(mime, "image/png");
                assert!(!data.is_empty());
            }
            other => panic!("期望 Image，得到 {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_image_from_file_rejects_unsupported() {
        let dir = std::env::temp_dir().join(format!("agent-img2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.bmp");
        std::fs::write(&path, b"BM").unwrap();
        assert!(read_image_from_file(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn completes_tools_first_arg_keys() {
        // 第一个参数：补工具组 key
        assert_eq!(
            complete_tools_args("/tools a", "a"),
            vec!["ast".to_string()]
        );
        let all = complete_tools_args("/tools ", "");
        for k in ["ast", "lsp", "image", "hashline", "pty", "github"] {
            assert!(all.contains(&k.to_string()), "缺 key 补全 {k}");
        }
    }

    #[test]
    fn completes_tools_second_arg_on_off() {
        // 第二个参数：补 on/off
        assert_eq!(
            complete_tools_args("/tools ast o", "o"),
            vec!["on".to_string(), "off".to_string()]
        );
        let sw = complete_tools_args("/tools ast ", "");
        assert!(sw.contains(&"on".to_string()));
        assert!(sw.contains(&"off".to_string()));
    }

    #[test]
    fn known_tools_key_recognizes_groups() {
        assert!(is_known_tools_key("ast"));
        assert!(is_known_tools_key("github"));
        assert!(!is_known_tools_key("bogus"));
    }

    #[test]
    fn parses_enhance_draft() {
        // 多词草稿拼接。
        let CommandOutcome::Enhance { draft } = handle_enhance("/enhance fix the login bug") else {
            panic!("期望 Enhance");
        };
        assert_eq!(draft, "fix the login bug");
        // 空草稿 → Handled（打印用法）。
        assert!(matches!(
            handle_enhance("/enhance"),
            CommandOutcome::Handled
        ));
        assert!(matches!(
            handle_enhance("/enhance   "),
            CommandOutcome::Handled
        ));
    }

    #[test]
    fn parses_suggest_query() {
        let CommandOutcome::Suggest { query } = handle_suggest("/suggest auth login") else {
            panic!("期望 Suggest");
        };
        assert_eq!(query, "auth login");
        assert!(matches!(
            handle_suggest("/suggest"),
            CommandOutcome::Handled
        ));
    }

    #[test]
    fn parses_review_args() {
        // 无参：默认 HEAD、自动子代理数。
        let CommandOutcome::Review { staged, reviewers } = handle_review("/review") else {
            panic!("期望 Review");
        };
        assert!(!staged);
        assert_eq!(reviewers, None);

        // 仅 N。
        let CommandOutcome::Review { staged, reviewers } = handle_review("/review 2") else {
            panic!("期望 Review");
        };
        assert!(!staged);
        assert_eq!(reviewers, Some(2));

        // --staged + N（任意顺序）。
        let CommandOutcome::Review { staged, reviewers } = handle_review("/review --staged 3")
        else {
            panic!("期望 Review");
        };
        assert!(staged);
        assert_eq!(reviewers, Some(3));

        let CommandOutcome::Review { staged, reviewers } = handle_review("/review 4 --staged")
        else {
            panic!("期望 Review");
        };
        assert!(staged);
        assert_eq!(reviewers, Some(4));

        // N 越界 / 未知参数 → 用法 + Handled。
        assert!(matches!(
            handle_review("/review 0"),
            CommandOutcome::Handled
        ));
        assert!(matches!(
            handle_review("/review 5"),
            CommandOutcome::Handled
        ));
        assert!(matches!(
            handle_review("/review --staged 9"),
            CommandOutcome::Handled
        ));
        assert!(matches!(
            handle_review("/review --bogus"),
            CommandOutcome::Handled
        ));
    }

    #[test]
    fn completes_enhance_and_suggest_command_names() {
        let h = ReplHelper::new(
            vec!["/enhance".into(), "/suggest".into(), "/help".into()],
            vec![],
            vec![],
            vec![],
        );
        let (_, cands) = h.complete_line("/enha", "/enha".len());
        assert!(cands.contains(&"/enhance".to_string()));
        let (_, cands) = h.complete_line("/sugg", "/sugg".len());
        assert!(cands.contains(&"/suggest".to_string()));
    }

    // ── P0-3（/goal 命令：未配置提示 / 查看 / set / extend）─────────────────

    /// 持有 CommandContext 全部引用的测试夹具（生命周期由 &self 借出）。
    struct GoalCtx {
        model: agent_core::Model,
        cfg: agent_config::Config,
        inmem: agent_context::InMemoryContext,
        mcp: agent_mcp::McpRegistry,
        skills: agent_skills::SkillCatalog,
        sessions: agent_context::SessionStore,
        optional: std::collections::HashMap<String, bool>,
        usage: agent_core::Usage,
        queue: Arc<std::sync::Mutex<agent_cli::queue::MessageQueue>>,
        keybindings: agent_cli::keybindings::Resolved,
    }

    impl GoalCtx {
        fn new() -> Self {
            let cfg = agent_config::Config {
                default_model: agent_config::ModelProfile {
                    headers: std::collections::BTreeMap::new(),
                    quirks: agent_core::ProviderQuirks::default(),
                    auth: agent_core::AuthMode::ApiKey,
                    compat: agent_config::CompatConfig::default(),
                    id: "m".into(),
                    alias: None,
                    api: agent_core::Api::OpenAiCompletions,
                    base_url: "http://localhost".into(),
                    api_key: secrecy::SecretString::new("x".into()),
                    api_keys: vec![],
                    fallbacks: vec![],
                    temperature: None,
                    max_output_tokens: None,
                    tokenizer: None,
                    max_input_tokens: None,
                    extra_body: None,
                },
                models: vec![],
                agent: agent_config::AgentConfig::default(),
                server: agent_config::ServerConfig::default(),
                skills: agent_config::SkillsConfig::default(),
                mcp: agent_config::McpConfig::default(),
                memory: agent_config::MemoryConfig::default(),
                github: agent_config::GithubConfig::default(),
                tools: agent_config::ToolsSwitchConfig::default(),
                subagent: agent_config::SubagentConfig::default(),
                language: None,
                acp: agent_config::AcpConfig::default(),
                ttsr: agent_config::TtsrConfig::default(),
                goals: agent_config::GoalsConfig::default(),
                todo: agent_config::TodoConfig::default(),
                eval: agent_config::EvalConfig::default(),
                compaction: agent_config::CompactionConfig::default(),
                socks5: agent_config::Socks5Config::default(),
                user_agent: None,
                hooks: Vec::new(),
                models_roles: None,
                keybindings: agent_config::KeybindingsConfig::default(),
            };
            Self {
                model: agent_core::Model::with_defaults(
                    "m",
                    "m",
                    agent_core::Api::OpenAiCompletions,
                ),
                cfg,
                keybindings: agent_cli::keybindings::resolve(&std::collections::BTreeMap::new()),
                inmem: agent_context::InMemoryContext::new(vec![]),
                queue: Arc::new(std::sync::Mutex::new(agent_cli::queue::MessageQueue::new())),
                mcp: agent_mcp::McpRegistry::default(),
                skills: agent_skills::SkillCatalog::default(),
                sessions: agent_context::SessionStore::default(),
                optional: std::collections::HashMap::new(),
                usage: agent_core::Usage::default(),
            }
        }

        fn ctx<'a>(
            &'a self,
            goal: Option<Arc<std::sync::Mutex<agent::GoalState>>>,
        ) -> CommandContext<'a> {
            CommandContext {
                model: &self.model,
                mode: Mode::Code,
                context: &self.inmem,
                accumulated: &self.usage,
                mcp: &self.mcp,
                skills: &self.skills,
                config: &self.cfg,
                commands: &[],
                sessions: &self.sessions,
                session_id: "s",
                guard: 0.8,
                cwd: std::path::Path::new("."),
                github_enabled: false,
                github_allow_write: false,
                optional: &self.optional,
                goal,
                todo: agent_tools::TodoState::in_memory().shared(),
                jobs: None,
                queue: &self.queue,
                keybindings: &self.keybindings,
            }
        }
    }

    #[test]
    fn goal_command_disabled_without_config() {
        let fixture = GoalCtx::new();
        let ctx = fixture.ctx(None);
        assert!(matches!(
            handle_command("/goal", &ctx),
            CommandOutcome::Handled
        ));
    }

    #[test]
    fn goal_command_set_and_extend_mutate_budget() {
        let fixture = GoalCtx::new();
        let goal = Arc::new(std::sync::Mutex::new(agent::GoalState::new(
            agent::GoalBudget::unlimited(),
        )));
        let ctx = fixture.ctx(Some(Arc::clone(&goal)));
        assert!(matches!(
            handle_command("/goal set 5000", &ctx),
            CommandOutcome::Handled
        ));
        assert_eq!(goal.lock().unwrap().budget.token_budget, 5000);
        assert!(matches!(
            handle_command("/goal extend 300", &ctx),
            CommandOutcome::Handled
        ));
        assert_eq!(goal.lock().unwrap().budget.token_budget, 5300);
        assert!(!goal.lock().unwrap().notified, "未超限时重置一次性提醒标记");
    }

    #[test]
    fn todo_command_prints_current_list() {
        let fixture = GoalCtx::new();
        let ctx = fixture.ctx(None);
        // 空清单下 /todo 正常 Handled（输出走 stderr，不 panic）。
        assert!(matches!(
            handle_command("/todo", &ctx),
            CommandOutcome::Handled
        ));
    }

    /// 边界匹配：`cmd` 出现且其后是命令列的对齐边界（空格/逗号/参数列起始），
    /// 防止 `/mode` 被 `/model` 行这类子串误判为已覆盖。
    fn help_line_mentions(line: &str, cmd: &str) -> bool {
        let Some(i) = line.find(cmd) else {
            return false;
        };
        let after = &line[i + cmd.len()..];
        after.is_empty()
            || after.starts_with(' ')
            || after.starts_with(',')
            || after.starts_with('[')
    }

    // ── H34：交互命令族（clear/new/retry/context/usage/jobs/settings/hotkeys/todo/resume）──

    #[test]
    fn h34_new_clear_retry_context_are_outcomes() {
        let fixture = GoalCtx::new();
        let ctx = fixture.ctx(None);
        assert!(matches!(
            handle_command("/clear", &ctx),
            CommandOutcome::Clear
        ));
        assert!(matches!(
            handle_command("/new", &ctx),
            CommandOutcome::Fresh
        ));
        assert!(matches!(
            handle_command("/retry", &ctx),
            CommandOutcome::Retry
        ));
        assert!(matches!(
            handle_command("/context", &ctx),
            CommandOutcome::Context
        ));
        // 只读命令自行处理，不改运行时状态。
        for cmd in ["/usage", "/settings", "/hotkeys", "/jobs"] {
            assert!(
                matches!(handle_command(cmd, &ctx), CommandOutcome::Handled),
                "{cmd} 应为 Handled"
            );
        }
    }

    #[test]
    fn h34_todo_is_writable_from_slash_command() {
        let fixture = GoalCtx::new();
        let ctx = fixture.ctx(None);
        // add → 清单出现待办（id 由 replace 重排为 t1）。
        assert!(matches!(
            handle_command("/todo add 写测试", &ctx),
            CommandOutcome::Handled
        ));
        let items = ctx.todo.snapshot().items;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "写测试");
        assert_eq!(items[0].phase, TodoPhase::Pending);
        // start t1 → in_progress。
        handle_command("/todo start t1", &ctx);
        assert_eq!(ctx.todo.snapshot().items[0].phase, TodoPhase::InProgress);
        // 序号写法 `1` 与 `#1` 等价。
        handle_command("/todo done 1", &ctx);
        assert_eq!(ctx.todo.snapshot().items[0].phase, TodoPhase::Completed);
        // 未知 id 不改状态。
        handle_command("/todo drop t9", &ctx);
        assert_eq!(ctx.todo.snapshot().items[0].phase, TodoPhase::Completed);
        // 非法子命令只打印用法。
        handle_command("/todo bogus", &ctx);
        assert_eq!(ctx.todo.snapshot().items.len(), 1);
        // clear 清空。
        handle_command("/todo clear", &ctx);
        assert!(ctx.todo.snapshot().items.is_empty());
    }

    #[test]
    fn h34_resume_numeric_selector_maps_to_session_id() {
        let fixture = GoalCtx::new();
        let ctx = fixture.ctx(None);
        // 空会话目录：任何序号都超范围 → Handled（不 panic、不产生 Resume）。
        assert!(matches!(
            handle_command("/resume 3", &ctx),
            CommandOutcome::Handled
        ));
        // 无参：列出候选（空目录 → sessions.none）。
        assert!(matches!(
            handle_command("/resume", &ctx),
            CommandOutcome::Handled
        ));
        // 非数字参数按会话 id 透传。
        assert!(matches!(
            handle_command("/resume abc123", &ctx),
            CommandOutcome::Resume(ref id) if id == "abc123"
        ));
    }

    #[test]
    fn h34_last_user_prompt_skips_injected_reminders() {
        use agent_core::{AgentMessage, NodeId, SessionNode};
        let mk = |text: &str| SessionNode {
            id: NodeId::new(),
            parent_id: None,
            message: AgentMessage::user_text(text),
        };
        // 注入提醒是最后一条 → 回退到用户原话。
        let nodes = vec![
            mk("实现 H34"),
            mk("<system-reminder>\n继续推进待办\n</system-reminder>"),
        ];
        assert_eq!(last_user_prompt(&nodes).as_deref(), Some("实现 H34"));
        // 正常情况取最后一条用户消息。
        let nodes = vec![mk("第一个问题"), mk("第二个问题")];
        assert_eq!(last_user_prompt(&nodes).as_deref(), Some("第二个问题"));
        // 空上下文 → None（/retry 报「没有可重发的输入」）。
        assert!(last_user_prompt(&[]).is_none());
    }

    #[test]
    fn h34_context_breakdown_counts_roles() {
        use agent_core::{AgentMessage, AssistantMessage, NodeId, SessionNode, StopReason, Usage};
        let node = |message: AgentMessage| SessionNode {
            id: NodeId::new(),
            parent_id: None,
            message,
        };
        let nodes = vec![
            node(AgentMessage::user_text("hi")),
            node(AgentMessage::Assistant(AssistantMessage {
                content: Vec::new(),
                usage: Usage::default(),
                model: "m".into(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            })),
            node(AgentMessage::ToolResult(agent_core::ToolResultMessage {
                tool_call_id: "c1".into(),
                result: agent_core::ToolResult::Text("x".into()),
            })),
            node(AgentMessage::Status(agent_core::StatusMessage {
                text: "note".into(),
                kind: agent_core::StatusKind::Info,
            })),
        ];
        let b = context_breakdown(&nodes);
        assert_eq!(
            (b.total, b.user, b.assistant, b.tool, b.other),
            (4, 1, 1, 1, 1)
        );
        assert_eq!(context_breakdown(&[]), ContextBreakdown::default());
    }

    // ── H4：运行期消息队列（/queue + 运行中键入路径）──────────────────────────

    #[test]
    fn h4_queue_command_enqueues_lists_pops_and_clears() {
        let fixture = GoalCtx::new();
        let ctx = fixture.ctx(None);
        // 空队列提示。
        assert!(matches!(
            handle_command("/queue", &ctx),
            CommandOutcome::Handled
        ));
        // 顺序列表一次入多条（1. / 2.）。
        handle_command("/queue add 1. 先跑测试\n2. 再写文档", &ctx);
        assert_eq!(ctx.queue.lock().unwrap().len(), 2);
        // 再入一条单消息。
        handle_command("/queue add 收尾", &ctx);
        assert_eq!(ctx.queue.lock().unwrap().len(), 3);
        // pop = 取队首并作为下一轮任务（Inject）。
        match handle_command("/queue pop", &ctx) {
            CommandOutcome::Inject(text) => assert_eq!(text, "先跑测试"),
            _ => panic!("pop 应 Inject 队首"),
        }
        assert_eq!(ctx.queue.lock().unwrap().len(), 2, "pop 后应出队");
        // undo 取回最近一次入队。
        handle_command("/queue undo", &ctx);
        assert_eq!(ctx.queue.lock().unwrap().len(), 1, "undo 去掉尾部『收尾』");
        // 按序号删除 + 越界提示。
        handle_command("/queue add A", &ctx);
        handle_command("/queue rm 1", &ctx);
        assert_eq!(ctx.queue.lock().unwrap().list(), vec![(1, "A")]);
        handle_command("/queue rm 9", &ctx);
        assert_eq!(ctx.queue.lock().unwrap().len(), 1, "越界删除不改队列");
        // clear。
        handle_command("/queue clear", &ctx);
        assert!(ctx.queue.lock().unwrap().is_empty());
        // 非法子命令 / 缺参数只打印用法。
        handle_command("/queue bogus", &ctx);
        handle_command("/queue add", &ctx);
        assert!(ctx.queue.lock().unwrap().is_empty());
    }

    #[test]
    fn h4_running_input_queues_shorthand_and_steers_plain_text() {
        let queue = Arc::new(std::sync::Mutex::new(agent_cli::queue::MessageQueue::new()));
        // 普通输入 → 立即 steering（既有语义不变）。
        assert_eq!(
            handle_running_input("看看日志", &queue),
            RunningInput::Steer("看看日志".into())
        );
        // `->` / `=>` 简写 → 只入队。
        assert_eq!(
            handle_running_input("-> 等会儿做", &queue),
            RunningInput::Queued(1)
        );
        assert_eq!(
            handle_running_input("=> 还有这条", &queue),
            RunningInput::Queued(2)
        );
        assert_eq!(queue.lock().unwrap().len(), 2);
        // 空行忽略。
        assert_eq!(handle_running_input("   ", &queue), RunningInput::Ignored);
        // 运行中 `/queue list` 由宿主消费（不投给模型、不改队列）。
        assert_eq!(
            handle_running_input("/queue list", &queue),
            RunningInput::Ignored
        );
        assert_eq!(queue.lock().unwrap().len(), 2);
        // 运行中 `/queue add` 入队且不打断。
        assert_eq!(
            handle_running_input("/queue add 第三条", &queue),
            RunningInput::Ignored
        );
        assert_eq!(queue.lock().unwrap().len(), 3);
        // 运行中 `/queue pop` = 取出并立即投递给运行中的 agent。
        assert_eq!(
            handle_running_input("/queue pop", &queue),
            RunningInput::Steer("等会儿做".into())
        );
        assert_eq!(queue.lock().unwrap().len(), 2);
    }

    // ── H5：可定制键位（/hotkeys 反映生效绑定）───────────────────────────────

    #[test]
    fn h5_hotkeys_reflects_configured_bindings() {
        let mut fixture = GoalCtx::new();
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("app.message.dequeue".to_string(), "alt-d".to_string());
        overrides.insert("app.retry".to_string(), "ctrl-t".to_string());
        overrides.insert("app.nope".to_string(), "ctrl-k".to_string());
        fixture.keybindings = agent_cli::keybindings::resolve(&overrides);
        let ctx = fixture.ctx(None);
        // 生效表里应是覆盖后的键与显式绑定的插入型动作；未知动作被忽略。
        let keys: Vec<String> = ctx
            .keybindings
            .bindings
            .iter()
            .map(|b| agent_cli::keybindings::render_key(&b.key))
            .collect();
        assert!(keys.contains(&"Alt-D".to_string()), "{keys:?}");
        assert!(keys.contains(&"Ctrl-T".to_string()), "{keys:?}");
        assert!(
            !keys.contains(&"Ctrl-Y".to_string()),
            "默认键应被覆盖: {keys:?}"
        );
        assert_eq!(
            ctx.keybindings.unknown_actions,
            vec!["app.nope".to_string()]
        );
        // `/hotkeys` 命令可正常执行（打印生效表 + 固定键位说明）。
        assert!(matches!(
            handle_command("/hotkeys", &ctx),
            CommandOutcome::Handled
        ));
    }

    #[test]
    fn help_covers_all_builtin_commands() {
        agent_i18n::init(Some("en"));
        // 纯别名不在帮助表单独出现：/help、/? 归 /h 行；/resume 归 /session 行；/skills 归 /skill 行。
        const ALIASES: [&str; 4] = ["/help", "/?", "/resume", "/skills"];
        let rendered = HELP_KEYS
            .iter()
            .map(|key| agent_i18n::tr(key, &[]))
            .collect::<Vec<_>>()
            .join("\n");
        for cmd in builtin_commands() {
            if ALIASES.contains(cmd) {
                continue;
            }
            assert!(
                rendered.lines().any(|line| help_line_mentions(line, cmd)),
                "帮助表缺少内置命令 `{cmd}` 的条目：新增命令请同步 help.* 文案与 HELP_KEYS"
            );
        }
        // 帮助文案中的模式枚举必须覆盖 parse_mode 接受的全部模式名。
        let mode_line = agent_i18n::tr("help.mode", &[]);
        for m in ["code", "architect", "ask", "debug", "plan"] {
            assert!(mode_line.contains(m), "help.mode 缺少模式 `{m}`");
        }
    }
}
