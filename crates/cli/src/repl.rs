//! REPL 辅助：rustyline 行编辑 + Tab 补全，slash 命令分发与展示。
//!
//! 本模块把「命令如何展示/派发」与「命令如何改变运行时状态」解耦：
//! - [`handle_command`] 是纯函数：读取 [`CommandContext`] 快照后返回 [`CommandOutcome`]，
//!   状态变更（切换模型/模式、压缩、退出）交回 `main` 执行。
//! - [`ReplHelper`] 实现 rustyline 的补全：对 `/` 命令名及部分参数（模式、模型、skill）补全。

use std::io::BufRead;
use std::path::Path;

use agent_config::{Config, CustomCommand, ModelProfile};
use agent_context::SessionStore;
use agent_core::{ContextManager, Mode, Model, SkillLevel, Usage, UserContent};
use agent_mcp::McpRegistry;
use agent_skills::SkillCatalog;
use agent_tools::Tool;
/// i18n 取词宏。
use agent_i18n::t;
use rustyline::completion::Completer;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;

const MODES: [&str; 4] = ["code", "architect", "ask", "debug"];

/// 命令派发结果：`main` 据此决定是否改变运行时状态。
pub enum CommandOutcome {
    /// 命令已自行处理（已打印），继续等待输入。
    Handled,
    /// 注入文本作为下一轮任务。
    Inject(String),
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
    /// 粘贴图像（剪贴板或本地文件）作为多模态用户消息发送。
    Paste {
        /// 附带的文本提示（可为空）。
        prompt: String,
        /// 图像内容块（mime + base64 数据）。
        image: UserContent,
    },
    /// 请求退出。
    Quit,
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
}

/// 内置命令名（带 `/`），用于补全与帮助。
#[must_use]
pub const fn builtin_commands() -> &'static [&'static str] {
    &[
        "/h",
        "/help",
        "/status",
        "/model",
        "/mode",
        "/paste",
        "/compact",
        "/mcp",
        "/skill",
        "/skills",
        "/sessions",
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

/// 解析模式名 → [`Mode`]。
#[must_use]
pub fn parse_mode(arg: &str) -> Option<Mode> {
    match arg.trim() {
        "code" => Some(Mode::Code),
        "architect" => Some(Mode::Architect),
        "ask" => Some(Mode::Ask),
        "debug" => Some(Mode::Debug),
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
        "/status" => {
            print_status(ctx);
            CommandOutcome::Handled
        }
        "/agents" => CommandOutcome::Agents,
        "/mcp" => {
            print_mcp(ctx);
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
            Some(id) if !id.is_empty() => CommandOutcome::Resume(id.to_string()),
            _ => {
                eprintln!("{}", t!("resume.usage"));
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
                    eprintln!(
                        "{}",
                        t!("mode.usage", current = mode_label(ctx.mode))
                    );
                    CommandOutcome::Handled
                }
            }
        }
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
        "/exit" | "/quit" => CommandOutcome::Quit,
        other if other.starts_with("/skill:") => {
            let skill_name = &other["/skill:".len()..];
            inject_skill(skill_name, ctx)
        }
        other => match try_custom_command(other, input, ctx.commands) {
            Some(text) => CommandOutcome::Inject(text),
            None => {
                eprintln!("{}", t!("common.unknown_command", cmd = other));
                CommandOutcome::Handled
            }
        },
    }
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
    eprintln!("{}", t!("help.h"));
    eprintln!("{}", t!("help.status"));
    eprintln!("{}", t!("help.model"));
    eprintln!("{}", t!("help.mode"));
    eprintln!("{}", t!("help.compact"));
    eprintln!("{}", t!("help.mcp"));
    eprintln!("{}", t!("help.skill"));
    eprintln!("{}", t!("help.skill_colon"));
    eprintln!("{}", t!("help.sessions"));
    eprintln!("{}", t!("help.session"));
    eprintln!("{}", t!("help.swarm"));
    eprintln!("{}", t!("help.collab"));
    eprintln!("{}", t!("help.github"));
    eprintln!("{}", t!("help.tools"));
    eprintln!("{}", t!("help.lang"));
    eprintln!("{}", t!("help.exit"));
    if !ctx.commands.is_empty() {
        eprintln!("{}", t!("help.custom_title"));
        for c in ctx.commands {
            if c.description.is_empty() {
                eprintln!("  /{}", c.name);
            } else {
                eprintln!("{}", t!("help.custom_entry", name = c.name, desc = c.description));
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
    eprintln!(
        "{}",
        t!("status.cost", cost = format!("{:.6}", ctx.accumulated.cost_usd))
    );
    eprintln!("{}", t!("status.footer"));
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
        t!("github.enabled_line", state = if ctx.github_enabled { "on" } else { "off" })
    );
    eprintln!(
        "{}",
        t!("github.write_line", state = if ctx.github_allow_write { "on" } else { "off" })
    );
    eprintln!(
        "{}",
        t!(
            "github.token_line",
            state = if token_set { t!("github.token_set") } else { t!("github.token_unset") }
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

fn print_mcp(ctx: &CommandContext<'_>) {
    let tools = ctx.mcp.tools();
    if tools.is_empty() {
        eprintln!("{}", t!("mcp.none"));
        return;
    }
    eprintln!("{}", t!("mcp.tools_count", count = tools.len()));
    for tool in tools {
        let desc = if tool.description().is_empty() {
            t!("mcp.no_desc")
        } else {
            tool.description().to_string()
        };
        eprintln!("  - {}  {}", tool.name(), desc);
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
    let dft = if is_default { t!("model.default_mark") } else { String::new() };
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
    let bind = &ctx.config.server.bind;
    eprintln!("{}", t!("collab.title"));
    eprintln!("{}", t!("collab.room", room = room));
    eprintln!("{}", t!("collab.fragment", fragment = fragment));
    eprintln!("{}", t!("collab.relay", bind = bind, room = room));
    eprintln!("{}", t!("collab.link", bind = bind, fragment = fragment));
    eprintln!("{}", t!("collab.footer"));
    eprintln!("{}", t!("collab.tip"));
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
        assert_eq!(parse_mode("bogus"), None);
    }

    #[test]
    fn parses_github_subcommand() {
        assert!(matches!(parse_github_subcommand(None), GithubToggle::Status));
        assert!(matches!(parse_github_subcommand(Some("")), GithubToggle::Status));
        assert!(matches!(parse_github_subcommand(Some("   ")), GithubToggle::Status));
        assert!(matches!(parse_github_subcommand(Some("on")), GithubToggle::Enable));
        assert!(matches!(
            parse_github_subcommand(Some(" Enable ")),
            GithubToggle::Enable
        ));
        assert!(matches!(
            parse_github_subcommand(Some("ENABLED")),
            GithubToggle::Enable
        ));
        assert!(matches!(parse_github_subcommand(Some("off")), GithubToggle::Disable));
        assert!(matches!(
            parse_github_subcommand(Some("DISABLED")),
            GithubToggle::Disable
        ));
        assert!(matches!(parse_github_subcommand(Some("write")), GithubToggle::Write));
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
}
