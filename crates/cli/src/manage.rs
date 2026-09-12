//! # 管理子命令面
//!
//! 顶层管理命令路由与执行（`agent models list` / `agent auth save …` /
//! `agent config check`），对齐 oh-my-pi `cli-commands.ts` 的显式注册表语义：
//! 命令动词 + 子动作集中登记，识别成功即执行并退出；语法不匹配（裸保留词 /
//! 未知子动作 / 真实多词 prompt）一律回落原路径——既有 reserved 顶层词提示
//! （oh-my-pi #4845 防误注入）与 prompt 行为完全不变。
//!
//! 结构约定：执行函数显式收 `&Path`（config_dir）或 `&Config` 参数，不读全局
//! 状态，便于离线测试；main.rs 只做路由调用与退出码映射（成功 0 / 用法错 2 /
//! 运行错 1）。凭据操作复用 `agent_config` 的 auth 链（`save` / `load` /
//! `env_var_name`），密钥只写不读不回显。
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::Context as _;

use agent_i18n::t;
use secrecy::ExposeSecret;

/// 管理命令执行载荷（路由识别成功后的目标动作）。
///
/// 对齐 omp `cli-commands.ts` 的 `commands` 显式注册表：动词 + 子动作在此集中
/// 登记（见 [`route`]），新增管理命令时同步扩展本枚举与 `route` 的分发表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManageCmd {
    /// `models list|ls`：列出 config 的模型 profile（无 omp 的 catalog 概念）。
    ModelsList,
    /// `agent models catalog [filter]`：内置模型目录（H25；离线，无需 provider）。
    ModelsCatalog(String),
    /// `auth list|ls`：列出各模型 profile 的 api_key 来源判定（不显示值）。
    AuthList,
    /// `auth save|remove|rm <provider>`：写入 / 删除 auth.toml 凭据条目。
    AuthSave { provider: String },
    /// `auth remove|rm <provider>`。
    AuthRemove { provider: String },
    /// `auth login <id>`：交互式 OAuth 登录（omp auth-broker login 语义）。
    AuthLogin { id: String },
    /// `auth logout <provider>`：本地移除 OAuth/API 凭据（omp 不做远端撤销）。
    AuthLogout { provider: String },
    /// `mcp login <server>`：Remote MCP server 的 OAuth 登录（授权码 + PKCE +
    /// 自动发现 + DCR），凭据落 oauth.toml 的 `mcp_oauth:<url>` 行。
    McpLogin { server: String },
    /// `mcp logout <server>`：移除该 server 的 OAuth 凭据行。
    McpLogout { server: String },
    /// `mcp add <name> [flags]`：写入 user（默认）或项目级 `mcp.json`。
    McpAdd(McpAddArgs),
    /// `mcp remove <name> [--project]`：从对应来源删除该 server。
    McpRemove {
        /// server 名（`mcpServers` 键）。
        name: String,
        /// `--project`：写 `<cwd>/.agent/mcp.json`（默认 user `<config_dir>/mcp.json`）。
        project: bool,
    },
    /// `mcp enable <name>`：从 user 级 `disabledServers` 移除。
    McpEnable {
        /// server 名。
        name: String,
    },
    /// `mcp disable <name>`：加入 user 级 `disabledServers`（压过一切来源）。
    McpDisable {
        /// server 名。
        name: String,
    },
    /// `mcp list`：按来源列出 `mcp.json` 内的 server（含被禁用者）。
    McpList,
    /// `mcp status`：合并 TOML + JSON 后的生效 server 与合并说明。
    McpStatus,
    /// `config path`：打印配置文件路径。
    ConfigPath,
    /// `config check`：加载 + 校验配置并打印摘要。
    ConfigCheck,
    /// `config get <key>`：打印生效值（合并后），密钥类键掩码（H23）。
    ConfigGet {
        /// 点分键路径（如 `agent.mode` / `default_model.max_output_tokens`）。
        key: String,
    },
    /// `config show`：打印合并后的生效配置（密钥类键掩码；H23）。
    ConfigShow,
    /// `config keys [<prefix>]`：列出已知配置键（点分路径 + 形态）——逐键内省（H23）。
    ConfigKeys {
        /// 可选前缀（如 `agent.commands`）。
        prefix: Option<String>,
    },
    /// `config set <key> <value>`：写入用户 config.toml（保留注释 + 校验 + 失败回滚；H23）。
    ConfigSet {
        /// 点分键路径。
        key: String,
        /// 原始值文本（按 TOML 字面量解析）。
        value: String,
    },
}

/// 路由结果（对齐 omp `resolveCliArgv`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// 非管理命令语法 → 回落原路径（reserved 提示 / 真实 prompt，行为不变）。
    Fallthrough,
    /// 动词/子动作识别成功但参数不合法 → 打印用法消息，退出码 2。
    Usage(String),
    /// 识别成功 → 执行。
    Run(ManageCmd),
    /// 子命令帮助（H2）：打印帮助并退出 0（`agent models --help` / `agent help models`）。
    /// 载荷是已渲染的帮助文本（含主题）。
    Help(String),
}

/// `list` 子动作别名（`models list` / `auth ls` 共用）。
const LIST_ACTIONS: &[&str] = &["list", "ls"];

/// `mcp add` 解析结果（路由层完成互斥 / 缺失校验，执行层只构造配置）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpAddArgs {
    /// server 名（`mcpServers` 键）。
    pub name: String,
    /// stdio 可执行命令；与 [`Self::url`] 互斥。
    pub command: Option<String>,
    /// stdio 命令参数。
    pub args: Vec<String>,
    /// HTTP 端点 URL；与 [`Self::command`] 互斥。
    pub url: Option<String>,
    /// HTTP 传输模式（`--type`；仅 `--url` 型 server 可给）。
    pub transport: Option<agent_config::McpHttpTransport>,
    /// stdio 额外环境变量（`--env K=V`，按给定顺序）。
    pub env: Vec<(String, String)>,
    /// `--project`：写 `<cwd>/.agent/mcp.json`（默认 user 级）。
    pub project: bool,
}

impl McpAddArgs {
    /// 构造 server 配置（路由层已校验，此处仅兜底并收敛）。
    fn to_server(&self) -> anyhow::Result<agent_config::McpServerConfig> {
        match (&self.command, &self.url) {
            (Some(command), None) => Ok(agent_config::McpServerConfig::Stdio(
                agent_config::McpStdioConfig {
                    command: command.clone(),
                    args: self.args.clone(),
                    env: self.env.iter().cloned().collect(),
                    timeout_ms: None,
                },
            )),
            (None, Some(url)) => Ok(agent_config::McpServerConfig::Http(
                agent_config::McpHttpConfig {
                    url: url.clone(),
                    headers: std::collections::HashMap::new(),
                    timeout_ms: None,
                    oauth: None,
                    transport: self.transport.unwrap_or_default(),
                },
            )),
            _ => anyhow::bail!("{}", t!("manage.mcp.add.conflict")),
        }
    }
}

/// 管理命令路由：纯语法识别（不读文件 / 不触 IO），返回 [`Route`]。
///
/// - 裸保留词（`agent models`）→ [`Route::Fallthrough`]：交由既有 reserved
///   提示拦截（main.rs `reserved_top_level_word_hint`），行为完全不变。
/// - 已知动词 + 未知子动作（`agent models frobnicate`）→ 回落原路径（历史上
///   该形态作为 prompt 发送，保持不变）。
/// - 已知动词 + 已知子动作 + 多余参数 → [`Route::Usage`]（退出码 2）。
pub fn route(tokens: &[String]) -> Route {
    let Some(first) = tokens.first() else {
        return Route::Fallthrough;
    };
    // flag / @file 语法永不构成管理命令（对齐 omp isSubcommand 与既有放行规则）。
    // 例外：`help` 动词本身是管理命令（H2）。
    if first.starts_with('@') {
        return Route::Fallthrough;
    }
    if first.starts_with('-') {
        return Route::Fallthrough;
    }
    // H2：子命令帮助优先于一切动作（`agent models --help` / `agent mcp add -h` /
    // `agent models help` / `agent help [verb]`）。未知主题 → Usage（退出码 2），
    // 避免 `help me fix this` 这类真实 prompt 被误吞（仅 `help <已知动词>` 才拦截）。
    if let Some(topic) = help_topic(tokens) {
        return Route::Help(help_text(&topic));
    }
    // 转 &str 便于字面量切片模式匹配。
    let rest: Vec<&str> = tokens[1..].iter().map(String::as_str).collect();
    match first.as_str() {
        "models" => match rest.as_slice() {
            // 裸保留词：回落（reserved 提示照旧）。
            [] => Route::Fallthrough,
            [action] if LIST_ACTIONS.contains(action) => Route::Run(ManageCmd::ModelsList),
            // H25：内置目录（可带过滤词）；与需要联网的 `models list` 区分。
            ["catalog", filter @ ..] => Route::Run(ManageCmd::ModelsCatalog(filter.join(" "))),
            ["cat", filter @ ..] => Route::Run(ManageCmd::ModelsCatalog(filter.join(" "))),
            [action, ..] if LIST_ACTIONS.contains(action) => {
                Route::Usage(t!("manage.usage.models"))
            }
            // 未知子动作：回落为 prompt（历史行为）。
            _ => Route::Fallthrough,
        },
        "auth" => match rest.as_slice() {
            [] => Route::Fallthrough,
            [action] if LIST_ACTIONS.contains(action) => Route::Run(ManageCmd::AuthList),
            [action, ..] if LIST_ACTIONS.contains(action) => Route::Usage(t!("manage.usage.auth")),
            ["save", args @ ..] => route_auth_save(args),
            ["remove", args @ ..] | ["rm", args @ ..] => route_auth_remove(args),
            ["login", args @ ..] => route_auth_login(args),
            ["logout", args @ ..] => route_auth_logout(args),
            _ => Route::Fallthrough,
        },
        "mcp" => match rest.as_slice() {
            [] => Route::Fallthrough,
            ["login", args @ ..] => route_mcp_login(args),
            ["logout", args @ ..] => route_mcp_logout(args),
            ["add", args @ ..] => route_mcp_add(args),
            ["remove", args @ ..] | ["rm", args @ ..] => route_mcp_remove(args),
            ["enable", args @ ..] => route_mcp_toggle(args, false),
            ["disable", args @ ..] => route_mcp_toggle(args, true),
            [action] if LIST_ACTIONS.contains(action) => Route::Run(ManageCmd::McpList),
            [action, ..] if LIST_ACTIONS.contains(action) => Route::Usage(t!("manage.mcp.usage")),
            ["status"] => Route::Run(ManageCmd::McpStatus),
            ["status", ..] => Route::Usage(t!("manage.mcp.usage")),
            _ => Route::Fallthrough,
        },
        "config" => match rest.as_slice() {
            [] => Route::Fallthrough,
            ["path"] => Route::Run(ManageCmd::ConfigPath),
            ["path", ..] => Route::Usage(t!("manage.usage.config")),
            ["check"] => Route::Run(ManageCmd::ConfigCheck),
            ["check", ..] => Route::Usage(t!("manage.usage.config")),
            ["keys"] => Route::Run(ManageCmd::ConfigKeys { prefix: None }),
            ["keys", prefix] => Route::Run(ManageCmd::ConfigKeys {
                prefix: Some((*prefix).to_string()),
            }),
            ["keys", ..] => Route::Usage(t!("manage.usage.config")),
            ["show"] => Route::Run(ManageCmd::ConfigShow),
            ["show", ..] => Route::Usage(t!("manage.usage.config")),
            ["get", args @ ..] => match args {
                [key] => Route::Run(ManageCmd::ConfigGet {
                    key: (*key).to_string(),
                }),
                _ => Route::Usage(t!("manage.usage.config")),
            },
            ["set", args @ ..] => match args {
                [key, value] => Route::Run(ManageCmd::ConfigSet {
                    key: (*key).to_string(),
                    value: (*value).to_string(),
                }),
                _ => Route::Usage(t!("manage.usage.config")),
            },
            _ => Route::Fallthrough,
        },
        _ => Route::Fallthrough,
    }
}

/// 已知管理动词（`help_topic` / `help_text` 的单一事实来源）。
const KNOWN_VERBS: &[&str] = &["models", "auth", "mcp", "config"];

/// `mcp` 的两级主题子动词（`agent mcp add --help` 显示 add 的用法）。
const MCP_SUBVERBS: &[&str] = &[
    "add", "remove", "rm", "enable", "disable", "list", "ls", "status", "login", "logout",
];

/// 从 token 序列识别帮助请求主题（H2）；非帮助请求返回 `None`。
///
/// 识别形态：
/// - `agent help` → 主题 `""`（全量管理命令列表）；
/// - `agent help <verb>` → 仅当 `<verb>` 是已知动词；
/// - `agent <verb> --help` / `agent <verb> -h` / `agent <verb> help` → 该动词；
/// - `agent mcp <sub> --help` → 主题 `"mcp <sub>"`。
fn help_topic(tokens: &[String]) -> Option<String> {
    let first = tokens.first()?.as_str();
    if first == "help" {
        return match tokens.len() {
            1 => Some(String::new()),
            2 if KNOWN_VERBS.contains(&tokens[1].as_str()) => Some(tokens[1].clone()),
            _ => None,
        };
    }
    if !KNOWN_VERBS.contains(&first) {
        return None;
    }
    let second = tokens.get(1).map(String::as_str);
    let wants_help =
        second == Some("help") || tokens.iter().skip(1).any(|t| t == "--help" || t == "-h");
    if !wants_help {
        return None;
    }
    if first == "mcp" {
        if let Some(sub) = second {
            if MCP_SUBVERBS.contains(&sub) {
                return Some(format!("mcp {sub}"));
            }
        }
    }
    Some(first.to_string())
}

/// 渲染帮助文本（`topic` 由 [`help_topic`] 保证为已知主题）。
fn help_text(topic: &str) -> String {
    let hint = || t!("manage.help.hint");
    match topic {
        "" => format!(
            "{}\n{}\n{}\n{}\n{}\n\n{}",
            t!("manage.help.title"),
            t!("manage.help.row.models"),
            t!("manage.help.row.auth"),
            t!("manage.help.row.mcp"),
            t!("manage.help.row.config"),
            hint()
        ),
        "models" => format!(
            "{}\n{}\n\n{}",
            t!("manage.help.title"),
            t!("manage.help.row.models"),
            hint()
        ),
        "auth" => format!(
            "{}\n{}\n\n{}",
            t!("manage.help.title"),
            t!("manage.help.row.auth"),
            hint()
        ),
        "mcp" => format!(
            "{}\n{}\n\n{}",
            t!("manage.help.title"),
            t!("manage.help.row.mcp"),
            hint()
        ),
        "mcp add" => t!("manage.mcp.add.usage"),
        // 其余 mcp 子动词共用 mcp 级用法（`agent mcp --help` 列出全部子动词）。
        t if t.starts_with("mcp ") => t!("manage.mcp.usage"),
        "config" => format!(
            "{}\n{}\n\n{}",
            t!("manage.help.title"),
            t!("manage.help.row.config"),
            hint()
        ),
        // help_topic 只产出已知主题；兜底给全量列表而非 panic。
        _ => format!(
            "{}\n{}\n{}\n{}\n{}\n\n{}",
            t!("manage.help.title"),
            t!("manage.help.row.models"),
            t!("manage.help.row.auth"),
            t!("manage.help.row.mcp"),
            t!("manage.help.row.config"),
            hint()
        ),
    }
}

/// `auth save` 参数解析：恰好一个 provider + 可选 `--stdin`。
///
/// 密钥只接受 stdin 读入（cli 未依赖 rpassword，不做 TTY 隐藏输入）；未给
/// `--stdin` 直接给出用法提示。
fn route_auth_save(args: &[&str]) -> Route {
    let mut stdin = false;
    let mut positional: Vec<&str> = Vec::new();
    for arg in args {
        if *arg == "--stdin" {
            stdin = true;
        } else {
            positional.push(arg);
        }
    }
    let Some(provider) = positional.first() else {
        return Route::Usage(t!("manage.auth.stdin_required"));
    };
    if positional.len() > 1 {
        return Route::Usage(t!("manage.usage.auth"));
    }
    match validate_provider(provider) {
        Ok(norm) if stdin => Route::Run(ManageCmd::AuthSave { provider: norm }),
        Ok(_) => Route::Usage(t!("manage.auth.stdin_required")),
        Err(msg) => Route::Usage(msg),
    }
}

/// `auth remove|rm` 参数解析：恰好一个 provider。
fn route_auth_remove(args: &[&str]) -> Route {
    match args {
        [provider] => match validate_provider(provider) {
            Ok(norm) => Route::Run(ManageCmd::AuthRemove { provider: norm }),
            Err(msg) => Route::Usage(msg),
        },
        _ => Route::Usage(t!("manage.usage.auth")),
    }
}

/// `auth login` 参数解析：恰好一个登录 id（omp 风格 id 或 Gyre 线协议名；
/// 流匹配大小写不敏感，此处仅 trim）。
fn route_auth_login(args: &[&str]) -> Route {
    match args {
        [id] if !id.trim().is_empty() && !id.starts_with('-') => Route::Run(ManageCmd::AuthLogin {
            id: id.trim().to_owned(),
        }),
        _ => Route::Usage(t!("manage.auth.login.usage")),
    }
}

/// `auth logout` 参数解析：恰好一个 provider（与 remove 同规）。
fn route_auth_logout(args: &[&str]) -> Route {
    match args {
        [provider] => match validate_provider(provider) {
            Ok(norm) => Route::Run(ManageCmd::AuthLogout { provider: norm }),
            Err(msg) => Route::Usage(msg),
        },
        _ => Route::Usage(t!("manage.usage.auth")),
    }
}

/// `mcp login` 参数解析：恰好一个 server 名（配置键，大小写敏感；trim + 拒 flag）。
fn route_mcp_login(args: &[&str]) -> Route {
    match args {
        [server] if !server.trim().is_empty() && !server.starts_with('-') => {
            Route::Run(ManageCmd::McpLogin {
                server: server.trim().to_owned(),
            })
        }
        _ => Route::Usage(t!("manage.mcp.login.usage")),
    }
}

/// `mcp logout` 参数解析：与 login 同规。
fn route_mcp_logout(args: &[&str]) -> Route {
    match args {
        [server] if !server.trim().is_empty() && !server.starts_with('-') => {
            Route::Run(ManageCmd::McpLogout {
                server: server.trim().to_owned(),
            })
        }
        _ => Route::Usage(t!("manage.mcp.usage")),
    }
}

/// `mcp add <name>` 参数解析：`--command` 与 `--url` 互斥且至少给一个；
/// `--arg` / `--env` 仅 stdio 型，`--type` 仅 url 型；`--project` 选项目级写回。
/// flag 缺值的用法错误（`manage.mcp.add.missing_value`）。
fn missing_flag_value(flag: &str) -> Route {
    Route::Usage(t!("manage.mcp.add.missing_value", flag = flag))
}

fn route_mcp_add(args: &[&str]) -> Route {
    let mut name: Option<String> = None;
    let mut command: Option<String> = None;
    let mut positional_args: Vec<String> = Vec::new();
    let mut url: Option<String> = None;
    let mut transport: Option<agent_config::McpHttpTransport> = None;
    let mut env: Vec<(String, String)> = Vec::new();
    let mut project = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        // flag 取值：缺值即用法错误（`None` 由调用点构造错误，避免在闭包里塞大
        // `Route` 值——clippy `result_large_err`）。
        let mut value = || it.next().map(|v| (*v).to_owned());
        match *arg {
            "--command" => match value() {
                Some(v) => command = Some(v),
                None => return missing_flag_value("--command"),
            },
            "--arg" => match value() {
                Some(v) => positional_args.push(v),
                None => return missing_flag_value("--arg"),
            },
            "--url" => match value() {
                Some(v) => url = Some(v),
                None => return missing_flag_value("--url"),
            },
            "--type" => match value() {
                Some(v) => match v.as_str() {
                    "http" => transport = Some(agent_config::McpHttpTransport::Streamable),
                    "sse" => transport = Some(agent_config::McpHttpTransport::Sse),
                    _ => {
                        return Route::Usage(t!("manage.mcp.add.bad_type", value = v));
                    }
                },
                None => return missing_flag_value("--type"),
            },
            "--env" => match value() {
                Some(v) => match v.split_once('=') {
                    Some((k, val)) if !k.trim().is_empty() => {
                        env.push((k.trim().to_owned(), val.to_owned()));
                    }
                    _ => return Route::Usage(t!("manage.mcp.add.bad_env", value = v)),
                },
                None => return missing_flag_value("--env"),
            },
            "--project" => project = true,
            other if other.starts_with('-') => return Route::Usage(t!("manage.mcp.usage")),
            other => {
                if name.is_some() {
                    return Route::Usage(t!("manage.mcp.usage"));
                }
                name = Some(other.trim().to_owned());
            }
        }
    }

    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Route::Usage(t!("manage.mcp.add.name_required"));
    };
    match (&command, &url) {
        (Some(_), Some(_)) => Route::Usage(t!("manage.mcp.add.conflict")),
        (None, None) => Route::Usage(t!("manage.mcp.add.need_target")),
        (Some(_), None) if transport.is_some() => Route::Usage(t!("manage.mcp.add.type_url_only")),
        (None, Some(_)) if !positional_args.is_empty() || !env.is_empty() => {
            Route::Usage(t!("manage.mcp.add.arg_command_only"))
        }
        _ => Route::Run(ManageCmd::McpAdd(McpAddArgs {
            name,
            command,
            args: positional_args,
            url,
            transport,
            env,
            project,
        })),
    }
}

/// `mcp remove <name> [--project]` 参数解析：恰好一个非 flag 名。
fn route_mcp_remove(args: &[&str]) -> Route {
    let mut name: Option<String> = None;
    let mut project = false;
    for arg in args {
        match *arg {
            "--project" => project = true,
            other if other.starts_with('-') || name.is_some() => {
                return Route::Usage(t!("manage.mcp.usage"));
            }
            other => name = Some(other.trim().to_owned()),
        }
    }
    match name.filter(|n| !n.is_empty()) {
        Some(name) => Route::Run(ManageCmd::McpRemove { name, project }),
        None => Route::Usage(t!("manage.mcp.usage")),
    }
}

/// `mcp enable|disable <name>` 参数解析：恰好一个非 flag 名。
fn route_mcp_toggle(args: &[&str], disabled: bool) -> Route {
    match args {
        [name] if !name.trim().is_empty() && !name.starts_with('-') => {
            let name = name.trim().to_owned();
            Route::Run(if disabled {
                ManageCmd::McpDisable { name }
            } else {
                ManageCmd::McpEnable { name }
            })
        }
        _ => Route::Usage(t!("manage.mcp.usage")),
    }
}

/// provider 名合法性校验：trim + 小写归一（与 `agent_config` 的 auth.toml key
/// 归一化一致），仅允许 ASCII 字母 / 数字 / 连字符且不以连字符开头——保证与
/// `GYRE_<PROVIDER>_API_KEY` 环境变量映射可逆（连字符 ↔ 下划线一一对应；空白 /
/// 下划线 / 非 ASCII 会破坏可逆性，一律拒绝）。
pub fn validate_provider(provider: &str) -> Result<String, String> {
    let lowered = provider.trim().to_lowercase();
    let ok = !lowered.is_empty()
        && !lowered.starts_with('-')
        && lowered
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        && lowered.chars().any(|c| c != '-');
    if ok {
        Ok(lowered)
    } else {
        Err(t!("manage.auth.invalid_provider", provider = provider))
    }
}

/// `models list`：列出 default_model + `[[models]]`（id / alias / api /
/// base_url，默认 profile 加标注）。
///
/// 与 omp `models ls` 的偏差：无 catalog 概念（输出尾部固定注明）。
pub fn models_list(
    cfg: &agent_config::Config,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let count = 1 + cfg.models.len();
    writeln!(out, "{}", t!("manage.models.header", count = count))?;
    let d = &cfg.default_model;
    writeln!(
        out,
        "{}",
        t!(
            "manage.models.entry_default",
            id = d.id,
            alias = d.alias.as_deref().unwrap_or("-"),
            api = d.api.as_str(),
            base_url = d.base_url
        )
    )?;
    if cfg.models.is_empty() {
        writeln!(out, "{}", t!("manage.models.no_extra"))?;
    } else {
        for m in &cfg.models {
            writeln!(
                out,
                "{}",
                t!(
                    "manage.models.entry",
                    id = m.id,
                    alias = m.alias.as_deref().unwrap_or("-"),
                    api = m.api.as_str(),
                    base_url = m.base_url
                )
            )?;
        }
    }
    writeln!(out, "{}", t!("manage.models.no_catalog"))
}

/// H25：打印内置模型目录（离线近似值；`filter` 为空则全量）。
///
/// # Errors
/// 写输出失败。
pub fn models_catalog(filter: &str, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    let entries = agent_config::model_catalog::search(filter);
    if entries.is_empty() {
        writeln!(
            out,
            "内置目录中没有匹配 `{filter}` 的模型（可用 `agent models list` 查看当前 provider 的真实清单）"
        )?;
        return Ok(());
    }
    writeln!(
        out,
        "内置模型目录（离线近似；未匹配的模型沿用配置/默认值）：{} 条",
        entries.len()
    )?;
    let header = format!(
        "  {:<22} {:<24} {:>10} {:>9}  {}",
        "id 模式", "建议 api", "上下文", "最大输出", "思考"
    );
    writeln!(out, "{header}")?;
    for e in entries {
        writeln!(
            out,
            "  {:<22} {:<24} {:>10} {:>9}  {}",
            e.pattern,
            e.api.as_str(),
            e.max_input_tokens,
            e.max_output_tokens,
            if e.supports_thinking { "yes" } else { "no" }
        )?;
    }
    writeln!(
        out,
        "\n提示：目录只补缺（显式 max_input_tokens / max_output_tokens 优先）；运行时可用模型请用 `agent models list`。"
    )?;
    Ok(())
}

/// api_key 来源判定结果（只判来源，绝不携带值本身）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeySource {
    /// config `api_key` 非空且 `${ENV}` 展开后非空。
    Config,
    /// `<config_dir>/auth.toml` 有该 provider 条目。
    AuthToml,
    /// 环境变量存在（携带变量名，供展示）。
    Env(String),
    /// 三段皆缺（携带期望的环境变量名）。
    Missing(String),
}

/// 单个 profile 的 api_key 来源判定（含 config 值展开）。
fn key_source(
    profile: &agent_config::ModelProfile,
    store: &agent_config::AuthStore,
    provider: &str,
) -> KeySource {
    let config_key_empty = profile.api_key.expose_secret().is_empty();
    key_source_core(
        config_key_empty,
        profile.resolve_api_key().expose_secret(),
        store,
        provider,
    )
}

/// 来源判定核心（与 `agent_config::resolve` 的三段顺序一一对应：config 值 →
/// auth.toml → 环境变量；此处仅判定来源，不返回密钥）。
fn key_source_core(
    config_key_empty: bool,
    expanded_config_value: &str,
    store: &agent_config::AuthStore,
    provider: &str,
) -> KeySource {
    // 1) config 值（raw 非空且展开后非空；展开为空视同未配置，与 resolve 一致）。
    if !config_key_empty && !expanded_config_value.is_empty() {
        return KeySource::Config;
    }
    // 2) auth.toml（空串条目视同未配置）。
    if store.get(provider).is_some_and(|k| !k.is_empty()) {
        return KeySource::AuthToml;
    }
    // 3) 环境变量（存在非空值即视为来源；只展示变量名）。
    let name = agent_config::env_var_name(provider);
    if std::env::var(&name).is_ok_and(|v| !v.is_empty()) {
        KeySource::Env(name)
    } else {
        KeySource::Missing(name)
    }
}

/// `auth list`：对 config 内每个模型 profile（default_model + `[[models]]`）
/// 给出 api_key 来源判定（config 值 / auth.toml / 环境变量名 / 缺失）。
/// 密钥值本身绝不显示。
pub fn auth_list(
    cfg: &agent_config::Config,
    config_dir: &Path,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let store = agent_config::load(config_dir);
    let count = 1 + cfg.models.len();
    writeln!(out, "{}", t!("manage.auth.header", count = count))?;
    for p in std::iter::once(&cfg.default_model).chain(&cfg.models) {
        let provider = p.api.as_str();
        let source = match key_source(p, &store, provider) {
            KeySource::Config => t!("manage.auth.source_config"),
            KeySource::AuthToml => t!("manage.auth.source_auth_toml"),
            KeySource::Env(name) => t!("manage.auth.source_env", name = name),
            KeySource::Missing(name) => {
                t!(
                    "manage.auth.source_missing",
                    name = name,
                    provider = provider
                )
            }
        };
        let model = p.alias.clone().unwrap_or_else(|| p.id.clone());
        writeln!(
            out,
            "{}",
            t!(
                "manage.auth.row",
                provider = provider,
                model = model,
                source = source
            )
        )?;
    }
    // OAuth 凭据面（oauth.toml）：有入口就展示（过期态明示），无则整段省略。
    // OAuth 条目优先于 auth.toml（与解析序一致），其 provider 视为已被「认领」。
    let oauth_store = agent_config::load_oauth(config_dir);
    let now = now_ms();
    for (provider, creds) in &oauth_store.0 {
        let expiry = if creds.expires >= agent_config::OAuthCredentials::NEVER_EXPIRES - 86_400_000
        {
            t!("manage.auth.oauth_longlived").to_string()
        } else if creds.is_expired(now) {
            t!("manage.auth.oauth_expired").to_string()
        } else {
            t!(
                "manage.auth.oauth_valid",
                remaining = humanize_ms(creds.expires - now)
            )
            .to_string()
        };
        let who = creds
            .email
            .as_deref()
            .map(|e| format!(" <{e}>"))
            .unwrap_or_default();
        writeln!(
            out,
            "{}",
            t!(
                "manage.auth.oauth_row",
                provider = provider,
                who = who,
                expiry = expiry
            )
        )?;
        // anthropic 30 天绝对授权寿命：到期只能重新交互登录（rotation 不续期）。
        if agent_llm::oauth::grant_expired_warning(provider, creds) {
            writeln!(out, "{}", t!("manage.auth.oauth_grant_expired"))?;
        }
    }
    let oauth_claimed: std::collections::BTreeSet<&str> =
        oauth_store.0.keys().map(String::as_str).collect();
    // 孤儿条目：auth.toml 里有、但没有任何模型 profile 的 api 名引用的键。
    // 不提示的话，`auth save <错名>` 的凭据会在 list 里隐形，用户无从发现拼写漂移。
    let claimed: std::collections::BTreeSet<&str> = std::iter::once(cfg.default_model.api.as_str())
        .chain(cfg.models.iter().map(|m| m.api.as_str()))
        .chain(oauth_claimed.iter().copied())
        .collect();
    let orphans: Vec<&String> = store
        .0
        .iter()
        .filter(|(k, v)| !v.is_empty() && !claimed.contains(k.as_str()))
        .map(|(k, _)| k)
        .collect();
    if !orphans.is_empty() {
        writeln!(
            out,
            "{}",
            t!(
                "manage.auth.orphan",
                keys = orphans
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        )?;
    }
    Ok(())
}

/// `auth save <provider>`：把密钥写入 `<config_dir>/auth.toml`（复用
/// `agent_config::save` 的原子写 / 0600 / 排序语义）。密钥 trim 后为空报错。
///
/// 返回写入的 auth.toml 路径（供成功消息展示）。
pub fn auth_save(config_dir: &Path, provider: &str, key: &str) -> anyhow::Result<PathBuf> {
    let key = key.trim();
    if key.is_empty() {
        anyhow::bail!("{}", t!("manage.auth.empty_key"));
    }
    agent_config::save(config_dir, provider, key).with_context(|| {
        t!(
            "manage.auth.save_failed",
            path = agent_config::auth_path(config_dir).display().to_string()
        )
    })?;
    Ok(agent_config::auth_path(config_dir))
}

/// `auth login <id>`：交互式 OAuth 登录（omp `auth-broker login` 语义）。
///
/// - URL 与指引打 stdout（SSH 安全，浏览器可在任意机器打开）；
/// - stdin 为 TTY 时提供「粘贴最终跳转 URL/授权码」赛跑（headless 兜底）；
/// - API key 产物（zai 铸 key）落 auth.toml；OAuth 凭据落 oauth.toml；
/// - HTTP 客户端为直连（SOCKS5 控制器属会话装配面，登录命令独立进程拿不到
///   config 时的兜底要求登录「永不被 config 损坏阻断」——与 omp 全局 fetch 对齐）。
pub async fn auth_login(id: &str, config_dir: &Path) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;

    let flow = agent_llm::oauth::flow_for(id).ok_or_else(|| {
        anyhow::anyhow!(
            "{}",
            t!(
                "manage.auth.login.not_found",
                id = id,
                ids = agent_llm::oauth::supported_ids().join(", ")
            )
        )
    })?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .context("构建 HTTP 客户端失败")?;

    let mut ctl = agent_llm::oauth::LoginCtl::new()
        .on_auth(|info| {
            println!("{}", t!("manage.auth.login.url", url = info.url));
            if let Some(instr) = &info.instructions {
                println!("{instr}");
            }
        })
        .on_progress(|msg| eprintln!("{msg}"));
    if std::io::stdin().is_terminal() {
        ctl = ctl.manual_code(|| {
            Box::pin(async move {
                tokio::task::spawn_blocking(|| {
                    eprintln!("{}", t!("manage.auth.login.paste_prompt"));
                    let mut line = String::new();
                    let _ = std::io::stdin().read_line(&mut line);
                    line
                })
                .await
                .unwrap_or_default()
            })
        });
    }

    match flow.login(&client, &ctl).await? {
        agent_llm::oauth::LoginResult::ApiKey(key) => {
            let path = auth_save(config_dir, flow.store_key(), &key)?;
            println!(
                "{}",
                t!(
                    "manage.auth.login.saved_key",
                    provider = flow.store_key(),
                    path = path.display().to_string()
                )
            );
        }
        agent_llm::oauth::LoginResult::Credentials(creds) => {
            let path =
                agent_config::save_oauth(config_dir, flow.store_key(), &creds).map_err(|e| {
                    anyhow::anyhow!(
                        "{}",
                        t!("manage.auth.login.save_failed", err = e.to_string())
                    )
                })?;
            println!(
                "{}",
                t!(
                    "manage.auth.login.saved_oauth",
                    provider = flow.store_key(),
                    path = path.display().to_string()
                )
            );
            if let Some(email) = &creds.email {
                println!("{}", t!("manage.auth.login.account", email = email));
            }
            if agent_llm::oauth::grant_expired_warning(flow.store_key(), &creds) {
                eprintln!("{}", t!("manage.auth.oauth_grant_expired"));
            }
        }
    }
    Ok(())
}

/// `auth logout` 的产物：哪些凭据面被清理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogoutOutcome {
    pub oauth_removed: bool,
    pub api_key_removed: bool,
}

/// `auth logout <provider>`：本地移除 OAuth 条目与 auth.toml API key（omp 不做
/// 远端撤销——token 有效性以服务端为准，此处仅清理本机存储）。
///
/// 两者都不存在时报错（not_found）；返回清理面供提示。
pub fn auth_logout(config_dir: &Path, provider: &str) -> anyhow::Result<LogoutOutcome> {
    let norm = validate_provider(provider).map_err(anyhow::Error::msg)?;
    let oauth_removed = agent_config::remove_oauth(config_dir, &norm)?;
    let api_key_removed = if agent_config::load(config_dir).get(&norm).is_some() {
        auth_remove(config_dir, &norm)?;
        true
    } else {
        false
    };
    if !oauth_removed && !api_key_removed {
        anyhow::bail!("{}", t!("manage.auth.not_found", provider = norm));
    }
    Ok(LogoutOutcome {
        oauth_removed,
        api_key_removed,
    })
}

/// 定位 MCP server 配置：存在且为 HTTP 传输时返回 (URL, oauth 子配置)。
fn resolve_mcp_http_server<'a>(
    cfg: &'a agent_config::Config,
    server: &str,
) -> anyhow::Result<(String, Option<&'a agent_config::McpOAuthConfig>)> {
    let entry = cfg.mcp.servers.get(server).ok_or_else(|| {
        let names: Vec<&String> = cfg.mcp.servers.keys().collect();
        anyhow::anyhow!(
            "{}",
            t!(
                "manage.mcp.not_found",
                server = server,
                names = names
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        )
    })?;
    match entry {
        agent_config::McpServerConfig::Http(http) => Ok((http.url.clone(), http.oauth.as_ref())),
        agent_config::McpServerConfig::Stdio(_) => {
            anyhow::bail!("{}", t!("manage.mcp.stdio_only", server = server))
        }
    }
}

/// `mcp login <server>`：Remote MCP server OAuth 登录（omp `mcp` OAuth 流语义）。
///
/// - 自动发现（RFC 9728/8414/OIDC）→ 需要 client_id 时尝试 DCR；
/// - 回环回调 + TTY 粘贴赛跑（与 `auth login` 同 UX）；
/// - 凭据（含 token_url/client_id/resource 刷新物）落 oauth.toml 的
///   `mcp_oauth:<url>` 行，HTTP 传输连接时自动装载。
pub async fn mcp_login(server: &str, config_dir: &Path) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;

    let cfg = agent_config::Config::load(Path::new(".")).context(t!("error.load_config"))?;
    let (url, oauth_cfg) = resolve_mcp_http_server(&cfg, server)?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .context("构建 HTTP 客户端失败")?;

    let flow = agent_mcp::oauth::LoginFlow::discover(&client, &url, oauth_cfg, None).await?;
    let manual = if std::io::stdin().is_terminal() {
        Some(Box::pin(async move {
            tokio::task::spawn_blocking(|| {
                eprintln!("{}", t!("manage.mcp.login.paste_prompt"));
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                line
            })
            .await
            .unwrap_or_default()
        }) as Pin<Box<dyn Future<Output = String> + Send>>)
    } else {
        None
    };

    let creds = flow
        .run(config_dir, manual, |url| {
            println!("{}", t!("manage.mcp.login.url", server = server, url = url))
        })
        .await?;
    println!(
        "{}",
        t!(
            "manage.mcp.login.saved",
            server = server,
            path = agent_config::oauth_path(config_dir).display().to_string()
        )
    );
    let _ = creds;
    Ok(())
}

/// `mcp logout <server>`：移除该 server 的 `mcp_oauth:<url>` 凭据行。
///
/// server 必须存在于 config 且为 HTTP 传输（URL 即凭据键）；行不存在时报 not_found。
pub fn mcp_logout(server: &str, config_dir: &Path) -> anyhow::Result<()> {
    let cfg = agent_config::Config::load(Path::new(".")).context(t!("error.load_config"))?;
    let (url, _) = resolve_mcp_http_server(&cfg, server)?;
    if !agent_config::remove_oauth(config_dir, &agent_mcp::oauth::credential_key(&url))? {
        anyhow::bail!("{}", t!("manage.mcp.logout.not_found", server = server));
    }
    eprintln!(
        "{}",
        t!(
            "manage.mcp.logout.removed",
            server = server,
            path = agent_config::oauth_path(config_dir).display().to_string()
        )
    );
    Ok(())
}

/// MCP 写回目标路径：默认 user `<config_dir>/mcp.json`；`--project` 用 `<cwd>/.agent/mcp.json`。
#[must_use]
pub fn mcp_write_path(cwd: &Path, config_dir: &Path, project: bool) -> PathBuf {
    if project {
        cwd.join(agent_core::platform::project_config_dir_name())
            .join("mcp.json")
    } else {
        config_dir.join("mcp.json")
    }
}

/// server 传输摘要（`stdio: cmd args` / `http: url` / `sse: url`），供 list / status 展示。
fn mcp_detail(server: &agent_config::McpServerConfig) -> String {
    match server {
        agent_config::McpServerConfig::Stdio(c) => {
            if c.args.is_empty() {
                format!("stdio: {}", c.command)
            } else {
                format!("stdio: {} {}", c.command, c.args.join(" "))
            }
        }
        agent_config::McpServerConfig::Http(h) => match h.transport {
            agent_config::McpHttpTransport::Streamable => format!("http: {}", h.url),
            agent_config::McpHttpTransport::Sse => format!("sse: {}", h.url),
        },
    }
}

/// 该名字是否出现在任一来源（各 `mcp.json` 来源，或 TOML `[mcp.servers]`）。
///
/// 被禁用（拒绝名单命中）的 server 不在合并后的 config 里，故必须查原始来源。
fn mcp_server_known(cwd: &Path, name: &str) -> bool {
    if agent_config::load_mcp_json_sources(cwd)
        .iter()
        .any(|load| load.servers.contains_key(name))
    {
        return true;
    }
    agent_config::Config::load(cwd).is_ok_and(|cfg| cfg.mcp.servers.contains_key(name))
}

/// `mcp add <name> …`：写入 user（默认）或项目级 `mcp.json`（保留文件内其他键与 server）。
pub fn mcp_add(
    cwd: &Path,
    config_dir: &Path,
    spec: &McpAddArgs,
    out: &mut dyn std::io::Write,
) -> anyhow::Result<PathBuf> {
    let server = spec.to_server()?;
    let path = mcp_write_path(cwd, config_dir, spec.project);
    agent_config::write_mcp_server(&path, &spec.name, &server)?;
    writeln!(
        out,
        "{}",
        t!(
            "manage.mcp.add.saved",
            name = spec.name,
            path = path.display().to_string()
        )
    )?;
    Ok(path)
}

/// `mcp remove <name> [--project]`：从对应来源删除该 server（不存在报错）。
pub fn mcp_remove(
    cwd: &Path,
    config_dir: &Path,
    name: &str,
    project: bool,
    out: &mut dyn std::io::Write,
) -> anyhow::Result<PathBuf> {
    let path = mcp_write_path(cwd, config_dir, project);
    if !agent_config::remove_mcp_server(&path, name)? {
        anyhow::bail!(
            "{}",
            t!(
                "manage.mcp.remove.not_found",
                name = name,
                path = path.display().to_string()
            )
        );
    }
    writeln!(
        out,
        "{}",
        t!(
            "manage.mcp.remove.removed",
            name = name,
            path = path.display().to_string()
        )
    )?;
    Ok(path)
}

/// `mcp enable|disable <name>`：改 **user 级** `disabledServers`（拒绝名单压过一切来源）。
///
/// `disable` 前校验该名确在某个来源中（防手误写入孤儿名单）；`enable` 只做名单清理，
/// 不要求存在（被删掉的 server 也应能清掉残留名单）。
pub fn mcp_set_disabled(
    cwd: &Path,
    config_dir: &Path,
    name: &str,
    disabled: bool,
    out: &mut dyn std::io::Write,
) -> anyhow::Result<PathBuf> {
    if disabled && !mcp_server_known(cwd, name) {
        anyhow::bail!("{}", t!("manage.mcp.toggle.not_found", name = name));
    }
    let path = config_dir.join("mcp.json");
    agent_config::set_mcp_server_disabled(&path, name, disabled)?;
    let message = if disabled {
        t!(
            "manage.mcp.disable.done",
            name = name,
            path = path.display().to_string()
        )
    } else {
        t!(
            "manage.mcp.enable.done",
            name = name,
            path = path.display().to_string()
        )
    };
    writeln!(out, "{message}")?;
    Ok(path)
}

/// `mcp list`：按来源（user → 项目三级）列出 `mcp.json` 内声明的 server，含被禁用者。
///
/// 只读原始来源文件（不要求 config.toml 存在/有效），与 `status` 的生效视图互补。
pub fn mcp_list(cwd: &Path, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    let loads = agent_config::load_mcp_json_sources(cwd);
    let existing: Vec<&agent_config::McpJsonLoad> =
        loads.iter().filter(|load| load.path.exists()).collect();
    writeln!(
        out,
        "{}",
        t!("manage.mcp.list.header", count = existing.len())
    )?;
    if existing.is_empty() {
        if let Some(first) = loads.first() {
            writeln!(
                out,
                "{}",
                t!(
                    "manage.mcp.list.none",
                    path = first.path.display().to_string()
                )
            )?;
        }
        return Ok(());
    }
    for load in existing {
        writeln!(
            out,
            "{}",
            t!(
                "manage.mcp.list.source",
                level = load.level.as_str(),
                path = load.path.display().to_string()
            )
        )?;
        if load.servers.is_empty() {
            writeln!(out, "{}", t!("manage.mcp.list.empty"))?;
            continue;
        }
        for (name, server) in &load.servers {
            let tag = if load.disabled.iter().any(|d| d == name) {
                t!("manage.mcp.list.disabled_tag")
            } else {
                String::new()
            };
            writeln!(
                out,
                "{}",
                t!(
                    "manage.mcp.list.entry",
                    name = name,
                    detail = mcp_detail(server),
                    tag = tag
                )
            )?;
        }
    }
    Ok(())
}

/// `mcp status`：合并 TOML + `mcp.json` 后的生效 server，附合并说明与来源告警。
///
/// 说明由「重放一次合并」得出：先从已合并配置里剥出非 JSON 来源条目（即 TOML），
/// 再对原始来源跑 [`agent_config::merge_mcp_json_sources`]——这样 `/mcp status`
/// 展示的说明与 `Config::load` 的日志逐字一致，且不必重复实现冲突判定。
pub fn mcp_status(
    cwd: &Path,
    cfg: &agent_config::Config,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let loads = agent_config::load_mcp_json_sources(cwd);
    let mut effective = agent_config::McpConfig::default();
    for (name, server) in &cfg.mcp.servers {
        let from_json = loads
            .iter()
            .any(|load| load.servers.get(name) == Some(server));
        if !from_json {
            effective.servers.insert(name.clone(), server.clone());
        }
    }
    let notes = agent_config::merge_mcp_json_sources(&mut effective, &loads);

    let mut names: Vec<&String> = effective.servers.keys().collect();
    names.sort();
    writeln!(
        out,
        "{}",
        t!("manage.mcp.status.header", count = names.len())
    )?;
    if names.is_empty() {
        writeln!(out, "{}", t!("manage.mcp.status.none"))?;
    }
    for name in names {
        writeln!(
            out,
            "{}",
            t!(
                "manage.mcp.status.entry",
                name = name,
                detail = mcp_detail(&effective.servers[name])
            )
        )?;
    }
    for note in &notes {
        writeln!(out, "{}", t!("manage.mcp.status.note", note = note))?;
    }
    for load in &loads {
        for warning in &load.warnings {
            writeln!(
                out,
                "{}",
                t!("manage.mcp.status.warning", warning = warning)
            )?;
        }
    }
    Ok(())
}

/// 执行 `/mcp` 管理子命令（CLI 与 REPL 共用同一实现）。
///
/// 返回 `Ok(false)` 表示该命令不属于 MCP 子命令族（调用方自行回落）。
pub fn run_mcp_cmd(
    cmd: &ManageCmd,
    cwd: &Path,
    config_dir: &Path,
    out: &mut dyn std::io::Write,
) -> anyhow::Result<bool> {
    match cmd {
        ManageCmd::McpAdd(spec) => {
            mcp_add(cwd, config_dir, spec, out)?;
        }
        ManageCmd::McpRemove { name, project } => {
            mcp_remove(cwd, config_dir, name, *project, out)?;
        }
        ManageCmd::McpEnable { name } => {
            mcp_set_disabled(cwd, config_dir, name, false, out)?;
        }
        ManageCmd::McpDisable { name } => {
            mcp_set_disabled(cwd, config_dir, name, true, out)?;
        }
        ManageCmd::McpList => {
            mcp_list(cwd, out)?;
        }
        ManageCmd::McpStatus => {
            let cfg = agent_config::Config::load(cwd).context(t!("error.load_config"))?;
            mcp_status(cwd, &cfg, out)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn humanize_ms(ms: i64) -> String {
    let mins = (ms / 60_000).max(1);
    if mins < 60 {
        t!("manage.auth.dur_min", n = mins).to_string()
    } else if mins < 48 * 60 {
        t!("manage.auth.dur_hour", n = mins / 60).to_string()
    } else {
        t!("manage.auth.dur_day", n = mins / (24 * 60)).to_string()
    }
}

/// epoch 毫秒（OAuth 行过期态展示用）。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// `auth remove|rm <provider>`：从 auth.toml 删除该条目；不存在报错。
///
/// `agent_config::save` 是「读旧文件 + 合并写入」，无删除语义；此处以「删文件 +
/// 逐条 `save` 剩余项」重建，完整复用其原子写 / 0600 / 排序语义（逐条重建的
/// 瞬间窗口内其余条目短暂缺失，对 CLI 凭据存储可接受）。
pub fn auth_remove(config_dir: &Path, provider: &str) -> anyhow::Result<()> {
    let norm = validate_provider(provider).map_err(anyhow::Error::msg)?;
    let store = agent_config::load(config_dir);
    if store.get(&norm).is_none() {
        anyhow::bail!("{}", t!("manage.auth.not_found", provider = norm));
    }
    let path = agent_config::auth_path(config_dir);
    let remaining: Vec<(&String, &String)> = store.0.iter().filter(|(k, _)| *k != &norm).collect();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    for (k, v) in remaining {
        agent_config::save(config_dir, k, v)?;
    }
    Ok(())
}

/// `config path`：打印用户级配置文件路径（`platform::config_dir()` 之下），
/// 不存在时注明。
pub fn config_path(config_dir: &Path, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    let path = config_dir.join("config.toml");
    if path.exists() {
        writeln!(
            out,
            "{}",
            t!("manage.config.path", path = path.display().to_string())
        )
    } else {
        writeln!(
            out,
            "{}",
            t!(
                "manage.config.path_missing",
                path = path.display().to_string()
            )
        )
    }
}

/// `config check` 成功侧：打印配置摘要（模型数 / 默认模型 / mcp servers 数 /
/// tools 覆盖数）。加载 + 校验由 `agent_config::Config::load` 全管线完成（含
/// 深度合并、`[models.roles]` 提升与语义校验），失败侧错误经
/// [`config_check_error`] 文案化。
pub fn config_check_summary(
    cfg: &agent_config::Config,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let models = 1 + cfg.models.len();
    let default = cfg
        .default_model
        .alias
        .as_deref()
        .unwrap_or(&cfg.default_model.id);
    writeln!(
        out,
        "{}",
        t!(
            "manage.config.check_ok",
            models = models,
            default = default,
            mcp = cfg.mcp.servers.len(),
            tools = cfg.tools.enabled.len()
        )
    )
}

/// `config keys [<prefix>]`：逐键内省（H23）——列出 schema 中的已知键与形态。
///
/// 参数：`prefix` 为空列出全部；否则列出该前缀本身与全部后代（点分层级）。
pub fn config_keys(prefix: Option<&str>, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    let prefix = prefix.unwrap_or("").trim().trim_end_matches('.');
    let all = agent_config::known_keys();
    let keys: Vec<agent_config::KeyInfo> = if prefix.is_empty() {
        all
    } else {
        let dotted = format!("{prefix}.");
        all.into_iter()
            .filter(|k| k.path == prefix || k.path.starts_with(&dotted))
            .collect()
    };
    if keys.is_empty() {
        writeln!(
            out,
            "{}",
            t!("manage.config.keys_none", prefix = prefix.to_string())
        )?;
        return Ok(());
    }
    for key in keys {
        writeln!(out, "{}\t{}", key.path, key.kind)?;
    }
    Ok(())
}

/// `config check` 的未知键告警段（H23）：逐条打印（带同层最接近键的建议）。
pub fn config_check_warnings(
    warnings: &[agent_config::ConfigWarning],
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    for warning in warnings {
        let suggestion = warning
            .suggestion
            .as_deref()
            .map(|s| {
                t!(
                    "manage.config.check_warning_suggest",
                    suggestion = s.to_string()
                )
            })
            .unwrap_or_default();
        writeln!(
            out,
            "{}",
            t!(
                "manage.config.check_warning",
                key = warning.path.clone(),
                suggestion = suggestion
            )
        )?;
    }
    Ok(())
}

/// 启动期未知键提示（H23）：一行摘要 + 逐条明细（写 stderr，避免污染 stdout 管道）。
pub fn report_config_warnings(warnings: &[agent_config::ConfigWarning]) {
    if warnings.is_empty() {
        return;
    }
    use std::io::Write as _;
    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "{}",
        t!("manage.config.load_warning", count = warnings.len())
    );
    let _ = config_check_warnings(warnings, &mut err);
}

/// 密钥类键名（`get`/`show` 掩码用；H23）：命中即打码，**绝不**回显明文。
fn is_secret_key(key: &str) -> bool {
    let last = key.rsplit('.').next().unwrap_or(key).to_ascii_lowercase();
    // 精确 + 后缀匹配：api_key / api_keys / password / token / secret / *_key（access_key 等）。
    matches!(
        last.as_str(),
        "api_key" | "api_keys" | "password" | "token" | "secret" | "access_token" | "refresh_token"
    ) || last.ends_with("_key")
        || last.ends_with("_secret")
        || last.ends_with("_token")
        || last.ends_with("_password")
}

/// 递归掩码：把密钥类键的值替换为 `"***"`（非空时）；返回是否改动过。
fn mask_secrets(value: &mut toml::Value, path: &str) -> bool {
    let mut changed = false;
    match value {
        toml::Value::Table(table) => {
            for (k, v) in table.iter_mut() {
                let child = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                if is_secret_key(&child) {
                    let empty = matches!(v, toml::Value::String(s) if s.is_empty());
                    if !empty {
                        *v = toml::Value::String("***".into());
                        changed = true;
                    }
                    continue;
                }
                changed |= mask_secrets(v, &child);
            }
        }
        toml::Value::Array(items) => {
            for (i, v) in items.iter_mut().enumerate() {
                changed |= mask_secrets(v, &format!("{path}[{i}]"));
            }
        }
        _ => {}
    }
    changed
}

/// `config show`：打印合并后的生效配置（TOML），密钥掩码。
///
/// # Errors
/// 无配置 / 解析失败时返回 IO 错误（文案由 `ConfigError` 提供）。
pub fn config_show(cwd: &std::path::Path, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    match agent_config::Config::load_raw(cwd) {
        Ok(mut value) => {
            let masked = mask_secrets(&mut value, "");
            if masked {
                writeln!(out, "{}", t!("manage.config.masked"))?;
            }
            let text =
                toml::to_string_pretty(&value).unwrap_or_else(|_| "（配置序列化失败）".to_string());
            write!(out, "{text}")
        }
        Err(e) => {
            writeln!(
                out,
                "{}",
                t!("manage.config.check_failed", error = e.to_string())
            )
        }
    }
}

/// `config get <key>`：按点分路径打印生效值（密钥掩码）。
///
/// # Errors
/// IO 错误。
pub fn config_get(
    cwd: &std::path::Path,
    key: &str,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let value = match agent_config::Config::load_raw(cwd) {
        Ok(v) => v,
        Err(e) => {
            writeln!(
                out,
                "{}",
                t!("manage.config.check_failed", error = e.to_string())
            )?;
            return Ok(());
        }
    };
    let mut cursor = &value;
    for seg in key.split('.').filter(|s| !s.is_empty()) {
        match cursor.get(seg) {
            Some(next) => cursor = next,
            None => {
                writeln!(out, "{}", t!("manage.config.get_missing", key = key))?;
                return Ok(());
            }
        }
    }
    let mut found = cursor.clone();
    if is_secret_key(key) {
        let empty = matches!(&found, toml::Value::String(s) if s.is_empty());
        if !empty {
            found = toml::Value::String("***".into());
        }
    } else {
        mask_secrets(&mut found, key);
    }
    let text = match &found {
        toml::Value::String(s) => s.clone(),
        other => toml::to_string(other).unwrap_or_else(|_| other.to_string()),
    };
    write!(out, "{text}")?;
    if !text.ends_with('\n') {
        writeln!(out)?;
    }
    Ok(())
}

/// `config set <key> <value>`：写入用户 config.toml 并报告结果。
///
/// # Errors
/// IO 错误。
pub fn config_set(
    cwd: &std::path::Path,
    key: &str,
    value: &str,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    match agent_config::Config::set_key(cwd, key, value) {
        Ok(path) => writeln!(
            out,
            "{}",
            t!(
                "manage.config.set_ok",
                key = key,
                path = path.display().to_string()
            )
        ),
        Err(e) => writeln!(
            out,
            "{}",
            t!("manage.config.set_failed", error = e.to_string())
        ),
    }
}

/// `config check` 失败侧：把 `ConfigError` 文案化（含「校验失败」前缀）。
pub fn config_check_error(err: &agent_core::ConfigError) -> String {
    t!("manage.config.check_failed", error = err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试统一激活英文词表：断言基于 en 文案，跨机器 locale 无关。
    fn init_en() {
        agent_i18n::init(Some("en"));
    }

    /// 构造 `route` 所需的 argv token 列表。
    fn toks(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    /// H23：`config get` / `show` 掩码密钥；`get` 未知键给出可操作提示。
    #[test]
    fn config_get_and_show_mask_secrets() {
        init_en();
        let dir = std::env::temp_dir().join(format!("gyre-cfgget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"https://x\"\napi_key = \"sk-super-secret\"\n[agent]\nmode = \"code\"\n",
        )
        .unwrap();

        let mut out = Vec::new();
        config_get(&dir, "agent.mode", &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().trim(), "code");

        // 密钥类键 → 掩码，绝不回显明文。
        let mut out = Vec::new();
        config_get(&dir, "default_model.api_key", &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.trim(), "***");
        assert!(!text.contains("sk-super-secret"));

        // 未知键 → 明确提示。
        let mut out = Vec::new();
        config_get(&dir, "nope.nothing", &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("nope.nothing"));

        // show：整体掩码 + 提示行。
        let mut out = Vec::new();
        config_show(&dir, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("***"), "{text}");
        assert!(!text.contains("sk-super-secret"), "show 不得泄漏密钥");
        assert!(text.contains("mode = \"code\""), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H23：`config set` 走真实用户配置路径（以临时目录为 cwd 的降级路径不可控，
    /// 故只验证路由与错误文案；写入语义由 `agent-config` 的 `set_key_at` 用例覆盖）。
    #[test]
    fn config_set_reports_failure_without_panic() {
        init_en();
        let mut out = Vec::new();
        // 空键 → 失败文案（不 panic、不写盘）。
        config_set(std::path::Path::new("."), "", "x", &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("failed"));
    }
    /// H2 回归：子命令帮助必须可发现（此前 `agent models --help` 会落 prompt 路径）。
    #[test]
    fn subcommand_help_is_routed() {
        init_en();
        // `agent <verb> --help` / `-h` / `help`。
        for verb in ["models", "auth", "mcp", "config"] {
            for form in [vec![verb, "--help"], vec![verb, "-h"], vec![verb, "help"]] {
                match route(&toks(&form)) {
                    Route::Help(text) => assert!(
                        text.contains(verb),
                        "{form:?} 帮助文本应含动词名，实得 {text:?}"
                    ),
                    other => panic!("{form:?} 应为 Help，实得 {other:?}"),
                }
            }
        }
        // `agent help` 全量 + `agent help <verb>` 单动词。
        match route(&toks(&["help"])) {
            Route::Help(text) => {
                for verb in ["models", "auth", "mcp", "config"] {
                    assert!(text.contains(verb), "全量帮助应含 {verb}：{text:?}");
                }
            }
            other => panic!("`agent help` 应为 Help，实得 {other:?}"),
        }
        assert!(matches!(
            route(&toks(&["help", "mcp"])),
            Route::Help(text) if text.contains("mcp")
        ));
        // 两级主题：`agent mcp add --help` 显示 add 用法。
        assert!(matches!(
            route(&toks(&["mcp", "add", "--help"])),
            Route::Help(text) if text.contains("--command")
        ));
        // 未知帮助主题（`agent help <非动词>`）不拦截：`help me fix this` 仍是真实 prompt。
        assert_eq!(route(&toks(&["help", "frobnicate"])), Route::Fallthrough);
        // 真实 prompt 不被误吞：`help me fix this` 既非已知动词主题，也回落原路径。
        assert_eq!(
            route(&toks(&["help", "me", "fix", "this"])),
            Route::Fallthrough
        );
    }

    fn toml_config(src: &str) -> agent_config::Config {
        toml::from_str(src).expect("解析测试配置")
    }

    /// 三 provider / 三 profile 的最小配置（gpt 的 api_key 直接写 config 值）。
    const CFG_TOML: &str = r#"
[default_model]
id = "gpt-test"
alias = "gpt"
api = "openai-completions"
base_url = "https://api.example.com/v1"
api_key = "sk-from-config"

[[models]]
id = "claude-test"
alias = "claude"
api = "anthropic-messages"
base_url = "https://anthropic.example.com"

[[models]]
id = "ds-test"
alias = "ds"
api = "deepseek"
base_url = "https://api.deepseek.com"
"#;

    // ── 路由识别 ─────────────────────────────────────────────────────────────

    #[test]
    fn route_models_shapes() {
        assert_eq!(
            route(&["models".into(), "list".into()]),
            Route::Run(ManageCmd::ModelsList)
        );
        assert_eq!(
            route(&["models".into(), "ls".into()]),
            Route::Run(ManageCmd::ModelsList)
        );
        // 多余参数 → 用法错误。
        assert!(matches!(
            route(&["models".into(), "list".into(), "extra".into()]),
            Route::Usage(_)
        ));
        // 裸保留词 / 未知子动作 → 回落原路径（提示 / prompt 行为不变）。
        assert_eq!(route(&["models".into()]), Route::Fallthrough);
        assert_eq!(
            route(&["models".into(), "frobnicate".into()]),
            Route::Fallthrough
        );
    }

    #[test]
    fn route_auth_shapes() {
        assert_eq!(
            route(&["auth".into(), "list".into()]),
            Route::Run(ManageCmd::AuthList)
        );
        assert_eq!(
            route(&["auth".into(), "ls".into()]),
            Route::Run(ManageCmd::AuthList)
        );
        assert!(matches!(
            route(&["auth".into(), "list".into(), "x".into()]),
            Route::Usage(_)
        ));
        // save：provider + --stdin；provider 名归一化（大小写不敏感）。
        assert_eq!(
            route(&[
                "auth".into(),
                "save".into(),
                "DeepSeek".into(),
                "--stdin".into()
            ]),
            Route::Run(ManageCmd::AuthSave {
                provider: "deepseek".into()
            })
        );
        // 缺 --stdin / 缺 provider / 多余参数 → 用法错误。
        assert!(matches!(
            route(&["auth".into(), "save".into(), "deepseek".into()]),
            Route::Usage(_)
        ));
        assert!(matches!(
            route(&["auth".into(), "save".into()]),
            Route::Usage(_)
        ));
        assert!(matches!(
            route(&[
                "auth".into(),
                "save".into(),
                "a".into(),
                "b".into(),
                "--stdin".into()
            ]),
            Route::Usage(_)
        ));
        // login：omp 风格 id / 线协议名；缺参 / 多参 / flag → 用法错误。
        assert_eq!(
            route(&["auth".into(), "login".into(), "Anthropic".into()]),
            Route::Run(ManageCmd::AuthLogin {
                id: "Anthropic".into()
            })
        );
        assert!(matches!(
            route(&["auth".into(), "login".into()]),
            Route::Usage(_)
        ));
        assert!(matches!(
            route(&["auth".into(), "login".into(), "a".into(), "b".into()]),
            Route::Usage(_)
        ));
        // logout：与 remove 同规（归一化 + 恰一参）。
        assert_eq!(
            route(&["auth".into(), "logout".into(), "ZAI".into()]),
            Route::Run(ManageCmd::AuthLogout {
                provider: "zai".into()
            })
        );
        assert!(matches!(
            route(&["auth".into(), "logout".into()]),
            Route::Usage(_)
        ));
        // 非法 provider 名（破坏 GYRE_<P>_API_KEY 可逆映射）→ 用法错误。
        assert!(matches!(
            route(&[
                "auth".into(),
                "save".into(),
                "openai_completions".into(),
                "--stdin".into()
            ]),
            Route::Usage(_)
        ));
        // remove / rm：恰好一个 provider。
        assert_eq!(
            route(&["auth".into(), "rm".into(), "zai".into()]),
            Route::Run(ManageCmd::AuthRemove {
                provider: "zai".into()
            })
        );
        assert_eq!(
            route(&["auth".into(), "remove".into(), "zai".into()]),
            Route::Run(ManageCmd::AuthRemove {
                provider: "zai".into()
            })
        );
        assert!(matches!(
            route(&["auth".into(), "rm".into()]),
            Route::Usage(_)
        ));
        assert!(matches!(
            route(&["auth".into(), "rm".into(), "a".into(), "b".into()]),
            Route::Usage(_)
        ));
        // 裸保留词 / 未知子动作 → 回落。
        assert_eq!(route(&["auth".into()]), Route::Fallthrough);
        assert_eq!(
            route(&["auth".into(), "frobnicate".into()]),
            Route::Fallthrough
        );
    }

    #[test]
    fn route_mcp_shapes() {
        // login/logout：恰好一个非 flag server 名。
        assert_eq!(
            route(&["mcp".into(), "login".into(), "remote".into()]),
            Route::Run(ManageCmd::McpLogin {
                server: "remote".into()
            })
        );
        assert_eq!(
            route(&["mcp".into(), "logout".into(), "remote".into()]),
            Route::Run(ManageCmd::McpLogout {
                server: "remote".into()
            })
        );
        // 缺参 / 多参 / flag → 用法错误。
        assert!(matches!(
            route(&["mcp".into(), "login".into()]),
            Route::Usage(_)
        ));
        assert!(matches!(
            route(&["mcp".into(), "login".into(), "a".into(), "b".into()]),
            Route::Usage(_)
        ));
        assert!(matches!(
            route(&["mcp".into(), "logout".into(), "--help".into()]),
            Route::Help(_)
        ));
        // 裸保留词 / 未知子动作 → 回落。
        assert_eq!(route(&["mcp".into()]), Route::Fallthrough);
        assert_eq!(
            route(&["mcp".into(), "frobnicate".into()]),
            Route::Fallthrough
        );

        // add：stdio 路径（--command/--arg/--env/--project）。
        assert_eq!(
            route(&toks(&[
                "mcp",
                "add",
                "fs",
                "--command",
                "npx",
                "--arg",
                "-y",
                "--env",
                "TOKEN=t",
                "--project"
            ])),
            Route::Run(ManageCmd::McpAdd(McpAddArgs {
                name: "fs".into(),
                command: Some("npx".into()),
                args: vec!["-y".into()],
                url: None,
                transport: None,
                env: vec![("TOKEN".into(), "t".into())],
                project: true,
            }))
        );
        // add：url 路径（--url/--type；默认 user 级写回）。
        assert_eq!(
            route(&toks(&[
                "mcp",
                "add",
                "remote",
                "--url",
                "https://x/mcp",
                "--type",
                "sse"
            ])),
            Route::Run(ManageCmd::McpAdd(McpAddArgs {
                name: "remote".into(),
                command: None,
                args: vec![],
                url: Some("https://x/mcp".into()),
                transport: Some(agent_config::McpHttpTransport::Sse),
                env: vec![],
                project: false,
            }))
        );
        // 互斥 / 缺目标 / 旗标误用 / 非法取值 / 名缺失 → 用法错误。
        for bad in [
            vec!["mcp", "add", "x", "--command", "a", "--url", "u"],
            vec!["mcp", "add", "x"],
            vec!["mcp", "add"],
            vec!["mcp", "add", "x", "--url", "u", "--arg", "a"],
            vec!["mcp", "add", "x", "--url", "u", "--env", "K=V"],
            vec!["mcp", "add", "x", "--command", "a", "--type", "sse"],
            vec!["mcp", "add", "x", "--url", "u", "--type", "ws"],
            vec!["mcp", "add", "x", "--url", "u", "--env", "novalue"],
            vec!["mcp", "add", "x", "--command"],
            vec!["mcp", "add", "x", "y", "--command", "a"],
            vec!["mcp", "add", "x", "--url", "u", "--bogus"],
        ] {
            assert!(
                matches!(route(&toks(&bad)), Route::Usage(_)),
                "应报用法错误: {bad:?}"
            );
        }

        // remove / enable / disable / list / status。
        assert_eq!(
            route(&toks(&["mcp", "remove", "fs"])),
            Route::Run(ManageCmd::McpRemove {
                name: "fs".into(),
                project: false
            })
        );
        assert_eq!(
            route(&toks(&["mcp", "rm", "fs", "--project"])),
            Route::Run(ManageCmd::McpRemove {
                name: "fs".into(),
                project: true
            })
        );
        assert_eq!(
            route(&toks(&["mcp", "enable", "fs"])),
            Route::Run(ManageCmd::McpEnable { name: "fs".into() })
        );
        assert_eq!(
            route(&toks(&["mcp", "disable", "fs"])),
            Route::Run(ManageCmd::McpDisable { name: "fs".into() })
        );
        assert_eq!(
            route(&toks(&["mcp", "list"])),
            Route::Run(ManageCmd::McpList)
        );
        assert_eq!(route(&toks(&["mcp", "ls"])), Route::Run(ManageCmd::McpList));
        assert_eq!(
            route(&toks(&["mcp", "status"])),
            Route::Run(ManageCmd::McpStatus)
        );
        assert!(matches!(route(&toks(&["mcp", "remove"])), Route::Usage(_)));
        assert!(matches!(route(&toks(&["mcp", "enable"])), Route::Usage(_)));
        assert!(matches!(
            route(&toks(&["mcp", "disable", "a", "b"])),
            Route::Usage(_)
        ));
        assert!(matches!(
            route(&toks(&["mcp", "list", "x"])),
            Route::Usage(_)
        ));
        assert!(matches!(
            route(&toks(&["mcp", "status", "x"])),
            Route::Usage(_)
        ));
    }

    #[test]
    fn route_config_shapes() {
        assert_eq!(
            route(&["config".into(), "path".into()]),
            Route::Run(ManageCmd::ConfigPath)
        );
        assert_eq!(
            route(&["config".into(), "keys".into()]),
            Route::Run(ManageCmd::ConfigKeys { prefix: None })
        );
        assert_eq!(
            route(&["config".into(), "keys".into(), "agent".into()]),
            Route::Run(ManageCmd::ConfigKeys {
                prefix: Some("agent".into())
            })
        );
        assert!(matches!(
            route(&["config".into(), "keys".into(), "a".into(), "b".into()]),
            Route::Usage(_)
        ));
        assert_eq!(
            route(&["config".into(), "check".into()]),
            Route::Run(ManageCmd::ConfigCheck)
        );
        assert!(matches!(
            route(&["config".into(), "path".into(), "x".into()]),
            Route::Usage(_)
        ));
        assert!(matches!(
            route(&["config".into(), "check".into(), "-y".into()]),
            Route::Usage(_)
        ));
        assert_eq!(route(&["config".into()]), Route::Fallthrough);
        assert_eq!(
            route(&["config".into(), "frobnicate".into()]),
            Route::Fallthrough
        );
    }

    #[test]
    fn auth_logout_clears_both_credential_stores() {
        let dir = std::env::temp_dir().join(format!("gyre-logout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let creds = agent_config::OAuthCredentials {
            access: "a".into(),
            refresh: "r".into(),
            expires: agent_config::OAuthCredentials::NEVER_EXPIRES,
            ..Default::default()
        };
        agent_config::save_oauth(&dir, "anthropic-messages", &creds).unwrap();
        agent_config::save(&dir, "anthropic-messages", "sk-key").unwrap();

        // 双面清理。
        let out = auth_logout(&dir, "anthropic-messages").unwrap();
        assert!(out.oauth_removed && out.api_key_removed);
        assert!(
            agent_config::load_oauth(&dir)
                .get("anthropic-messages")
                .is_none()
        );
        assert!(agent_config::load(&dir).get("anthropic-messages").is_none());

        // 全无 → not_found。
        assert!(auth_logout(&dir, "anthropic-messages").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_list_shows_oauth_rows_and_expiry() {
        init_en();
        let cfg = toml_config(
            r#"
[default_model]
id = "gpt-test"
api = "openai-completions"
base_url = "https://api.example.com/v1"
"#,
        );
        let dir = std::env::temp_dir().join(format!("gyre-oauth-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let now: i64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        agent_config::save_oauth(
            &dir,
            "anthropic-messages",
            &agent_config::OAuthCredentials {
                access: "a".into(),
                refresh: "r".into(),
                expires: now + 3600 * 1000,
                email: Some("u@x.io".into()),
                ..Default::default()
            },
        )
        .unwrap();
        agent_config::save_oauth(
            &dir,
            "zai",
            &agent_config::OAuthCredentials {
                access: "long".into(),
                refresh: String::new(),
                expires: agent_config::OAuthCredentials::NEVER_EXPIRES,
                ..Default::default()
            },
        )
        .unwrap();
        let mut out: Vec<u8> = Vec::new();
        auth_list(&cfg, &dir, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("[OAuth"), "OAuth 行展示：{text}");
        assert!(text.contains("<u@x.io>"), "邮箱展示：{text}");
        assert!(text.contains("expires in"), "时效展示：{text}");
        assert!(text.contains("long-lived"), "长效展示：{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn route_fallthrough_prompts_and_flags() {
        // 多词真实 prompt 放行不变。
        assert_eq!(
            route(&["explain".into(), "models".into()]),
            Route::Fallthrough
        );
        assert_eq!(
            route(&["list".into(), "all".into(), "my".into(), "files".into()]),
            Route::Fallthrough
        );
        // flag / @file 前缀永不构成管理命令；空参数同样回落。
        assert_eq!(route(&["-models".into()]), Route::Fallthrough);
        assert_eq!(
            route(&["@file".into(), "models".into()]),
            Route::Fallthrough
        );
        assert_eq!(route(&[]), Route::Fallthrough);
        // 非管理动词首词放行。
        assert_eq!(
            route(&["upgrade".into(), "the".into(), "deps".into()]),
            Route::Fallthrough
        );
    }

    #[test]
    fn validate_provider_rules() {
        assert_eq!(validate_provider("deepseek").as_deref(), Ok("deepseek"));
        assert_eq!(validate_provider("  OpenAI ").as_deref(), Ok("openai"));
        assert_eq!(
            validate_provider("openai-completions").as_deref(),
            Ok("openai-completions")
        );
        // 破坏可逆映射 / 环境变量命名的形态一律拒绝。
        assert!(validate_provider("").is_err());
        assert!(validate_provider("   ").is_err());
        assert!(validate_provider("openai_completions").is_err());
        assert!(validate_provider("a b").is_err());
        assert!(validate_provider("a@b").is_err());
        assert!(validate_provider("深度").is_err());
        assert!(validate_provider("-x").is_err());
        assert!(validate_provider("---").is_err());
    }

    // ── models list ─────────────────────────────────────────────────────────

    /// H25：内置目录命令——全量列出、按 api/名称过滤、无匹配给提示。
    #[test]
    fn models_catalog_lists_and_filters() {
        let mut out = Vec::new();
        models_catalog("", &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("内置模型目录"), "{text}");
        assert!(text.contains("gpt-4o*"), "{text}");
        assert!(text.contains("claude-sonnet-4*"), "{text}");
        assert!(
            text.contains("yes") && text.contains("no"),
            "思考列应有 yes/no: {text}"
        );

        // 过滤：只出现命中的家族。
        let mut out = Vec::new();
        models_catalog("deepseek", &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("deepseek-reasoner*") && text.contains("deepseek*"),
            "{text}"
        );
        assert!(!text.contains("gpt-4o*"), "过滤后不应出现其它家族: {text}");

        // 无匹配：给可执行的下一步提示而非报错。
        let mut out = Vec::new();
        models_catalog("no-such-family", &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("没有匹配"), "{text}");
        assert!(text.contains("agent models list"), "{text}");
    }

    #[test]
    fn models_list_marks_default_and_alias() {
        init_en();
        let cfg = toml_config(CFG_TOML);
        let mut out: Vec<u8> = Vec::new();
        models_list(&cfg, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        // 默认 profile 标注 + 别名 / id / api / base_url 齐备。
        assert!(text.contains("[default]"), "应标注默认：{text}");
        assert!(text.contains("gpt-test") && text.contains("gpt"), "{text}");
        assert!(text.contains("openai-completions"), "{text}");
        assert!(text.contains("https://api.example.com/v1"), "{text}");
        assert!(
            text.contains("claude") && text.contains("anthropic-messages"),
            "{text}"
        );
        assert!(text.contains("ds") && text.contains("deepseek"), "{text}");
        // 无 catalog 概念的偏差注明。
        assert!(text.contains("catalog"), "应注明无 catalog：{text}");
    }

    // ── auth save → list → remove 闭环 ──────────────────────────────────────

    #[test]
    fn auth_save_list_remove_roundtrip() {
        init_en();
        let dir = std::env::temp_dir().join(format!("gyre-manage-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let cfg = toml_config(CFG_TOML);

        // 初始：无 auth.toml → 任何 profile 都不应显示 auth.toml 来源。
        let mut out: Vec<u8> = Vec::new();
        auth_list(&cfg, &dir, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("[gpt]"), "应含 gpt 行：{text}");
        assert!(!text.contains("sk-from-config"), "绝不能显示密钥值：{text}");

        // save（含首尾空白，内部 trim）→ auth.toml 来源；密钥不回显。
        let path = auth_save(&dir, "deepseek", "  sk-secret-123  ").expect("save");
        assert_eq!(path, agent_config::auth_path(&dir));
        assert!(path.exists());
        let mut out: Vec<u8> = Vec::new();
        auth_list(&cfg, &dir, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("auth.toml"),
            "deepseek 行应显示 auth.toml 来源：{text}"
        );
        assert!(!text.contains("sk-secret-123"), "绝不能回显密钥：{text}");

        // 大小写归一 remove（auth.toml key 不分大小写）。
        auth_remove(&dir, "DeepSeek").expect("remove");
        let mut out: Vec<u8> = Vec::new();
        auth_list(&cfg, &dir, &mut out).unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("auth.toml"));

        // 再删 → 不存在报错。
        assert!(auth_remove(&dir, "deepseek").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_save_rejects_empty_key() {
        init_en();
        let dir = std::env::temp_dir().join(format!("gyre-manage-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        assert!(auth_save(&dir, "deepseek", "").is_err());
        assert!(auth_save(&dir, "deepseek", "   \n  ").is_err());
        assert!(!agent_config::auth_path(&dir).exists(), "空密钥不应落盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 来源判定 ────────────────────────────────────────────────────────────

    #[test]
    fn key_source_precedence() {
        let mut store = agent_config::AuthStore::default();
        store.0.insert("p".into(), "sk-toml".into());
        // config 值优先于 auth.toml。
        assert_eq!(
            key_source_core(false, "sk-config", &store, "p"),
            KeySource::Config
        );
        // config raw 非空但展开为空 → 视同未配置，回落 auth.toml。
        assert_eq!(key_source_core(false, "", &store, "p"), KeySource::AuthToml);
        // config 空缺 + auth.toml 空缺 → 环境变量（此处未设 → 缺失）。
        assert_eq!(
            key_source_core(true, "", &store, "gyre-manage-noenv"),
            KeySource::Missing("GYRE_GYRE_MANAGE_NOENV_API_KEY".into())
        );
    }

    #[test]
    fn key_source_env_branch() {
        // SAFETY: 测试专用环境变量，本测试独占（仓库既有 env 测试惯例，见
        // agent_config auth 测试）。
        const VAR: &str = "GYRE_MANAGE_TEST_ENV_ONLY_API_KEY";
        unsafe { std::env::set_var(VAR, "sk-env") };
        let store = agent_config::AuthStore::default();
        assert!(matches!(
            key_source_core(true, "", &store, "manage-test-env-only"),
            KeySource::Env(_)
        ));
        unsafe { std::env::remove_var(VAR) };
        assert_eq!(
            key_source_core(true, "", &store, "manage-test-env-only"),
            KeySource::Missing(VAR.into())
        );
    }

    #[test]
    fn auth_list_flags_orphan_entries() {
        init_en();
        let cfg = toml_config(
            r#"
[default_model]
id = "gpt-test"
api = "openai-completions"
base_url = "https://api.example.com/v1"
"#,
        );
        let dir = std::env::temp_dir().join(format!("gyre-manage-orphan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        // 阶段一：拼写漂移的 provider（不在任何 profile 的 api 名里）→ 孤儿提示。
        auth_save(&dir, "anthropic", "sk-orphan").expect("save");
        let mut out: Vec<u8> = Vec::new();
        auth_list(&cfg, &dir, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("anthropic"), "孤儿条目必须可见：{text}");
        assert!(text.contains("not referenced"), "带清理指引：{text}");
        // 阶段二：正名入库并清掉漂移条目后，孤儿提示消失。
        auth_save(&dir, "openai-completions", "sk-right").expect("save");
        auth_remove(&dir, "anthropic").expect("remove orphan");
        let mut out: Vec<u8> = Vec::new();
        auth_list(&cfg, &dir, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("not referenced"), "无孤儿不提示：{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── config path / config check ──────────────────────────────────────────

    #[test]
    fn config_keys_lists_known_keys_and_filters_by_prefix() {
        init_en();
        let mut out = Vec::new();
        config_keys(None, &mut out).unwrap();
        let all = String::from_utf8(out).unwrap();
        assert!(all.lines().any(|l| l == "agent.max_turns\tscalar"), "{all}");
        assert!(all.lines().any(|l| l == "mcp.servers\topen-map"), "{all}");

        let mut out = Vec::new();
        config_keys(Some("agent.commands"), &mut out).unwrap();
        let filtered = String::from_utf8(out).unwrap();
        assert!(
            filtered
                .lines()
                .any(|l| l == "agent.commands.minimizer\ttable"),
            "{filtered}"
        );
        assert!(
            filtered
                .lines()
                .any(|l| l == "agent.commands.allow\tarray-of-tables")
        );
        assert!(!filtered.contains("agent.max_turns"), "{filtered}");

        // 未知前缀 → 明确提示（不是空输出）。
        let mut out = Vec::new();
        config_keys(Some("nope"), &mut out).unwrap();
        let none = String::from_utf8(out).unwrap();
        assert!(none.contains("nope"), "{none}");
    }

    /// H23：未知键告警的人读渲染（带建议）。
    #[test]
    fn config_check_warnings_render_suggestion() {
        init_en();
        let warnings = vec![
            agent_config::ConfigWarning {
                path: "agent.max_turn".into(),
                suggestion: Some("max_turns".into()),
            },
            agent_config::ConfigWarning {
                path: "agnet".into(),
                suggestion: None,
            },
        ];
        let mut out = Vec::new();
        config_check_warnings(&warnings, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("agent.max_turn"), "{text}");
        assert!(text.contains("max_turns"), "{text}");
        assert!(text.contains("agnet"), "{text}");
        assert_eq!(text.lines().count(), 2, "{text}");
    }

    #[test]
    fn config_check_summary_and_error() {
        init_en();
        let cfg = toml_config(
            r#"
[default_model]
id = "gpt-test"
alias = "gpt"
api = "openai-completions"
base_url = "https://api.example.com/v1"

[[models]]
id = "claude-test"
api = "anthropic-messages"
base_url = "https://anthropic.example.com"

[mcp.servers.ctx]
command = "uvx"
args = ["mcp-server-ctx"]

[tools.enabled]
ast = true
lsp = true
"#,
        );
        let mut out: Vec<u8> = Vec::new();
        config_check_summary(&cfg, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("2 model profile(s)"), "模型数：{text}");
        assert!(
            text.contains("default model gpt"),
            "默认模型取 alias：{text}"
        );
        assert!(text.contains("1 MCP server(s)"), "mcp 数：{text}");
        assert!(text.contains("2 tools override(s)"), "tools 覆盖数：{text}");

        // 坏态：校验错误文案化（含原始错误信息）。
        let err = agent_core::ConfigError::Invalid("context_window_guard 越界".into());
        let msg = config_check_error(&err);
        assert!(msg.contains("Config check failed"), "{msg}");
        assert!(msg.contains("context_window_guard 越界"), "{msg}");
    }

    #[test]
    fn config_path_notes_missing() {
        init_en();
        let dir = std::env::temp_dir().join(format!("gyre-manage-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let mut out: Vec<u8> = Vec::new();
        config_path(&dir, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("not found"), "缺失应注明：{text}");
        assert!(text.contains("config.toml"), "{text}");
        std::fs::write(dir.join("config.toml"), "[default_model]\n").unwrap();
        let mut out: Vec<u8> = Vec::new();
        config_path(&dir, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("not found"), "存在时不注明：{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── mcp add / list / disable / remove ───────────────────────────────────

    #[test]
    fn mcp_add_list_disable_remove_roundtrip() {
        init_en();
        let cwd = std::env::temp_dir().join(format!(
            "gyre-manage-mcp-cwd-{}-{:#x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config_dir = cwd.join("user-config");
        std::fs::create_dir_all(&cwd).expect("建临时目录");
        std::fs::create_dir_all(&config_dir).expect("建临时配置目录");

        // 项目级写回：`<cwd>/.agent/mcp.json`。
        let mut out: Vec<u8> = Vec::new();
        let spec = McpAddArgs {
            name: "fs".into(),
            command: Some("npx".into()),
            args: vec!["-y".into(), "server-filesystem".into()],
            url: None,
            transport: None,
            env: vec![("TOKEN".into(), "t".into())],
            project: true,
        };
        let path = mcp_add(&cwd, &config_dir, &spec, &mut out).unwrap();
        assert_eq!(path, cwd.join(".agent").join("mcp.json"));
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            String::from_utf8_lossy(&out).contains("written to"),
            "成功消息：{}",
            String::from_utf8_lossy(&out)
        );

        // 再写一个 url 型（http 默认传输）+ 一个 user 级 server。
        out.clear();
        let remote = McpAddArgs {
            name: "remote".into(),
            command: None,
            args: vec![],
            url: Some("https://x/mcp".into()),
            transport: Some(agent_config::McpHttpTransport::Sse),
            env: vec![],
            project: true,
        };
        mcp_add(&cwd, &config_dir, &remote, &mut out).unwrap();
        let user_spec = McpAddArgs {
            name: "user-stdio".into(),
            command: Some("uvx".into()),
            args: vec![],
            url: None,
            transport: None,
            env: vec![],
            project: false,
        };
        let user_path = mcp_add(&cwd, &config_dir, &user_spec, &mut out).unwrap();
        assert_eq!(user_path, config_dir.join("mcp.json"));
        // 注意：`mcp_list` 的 user 级来源取 `agent_core::platform::config_dir()`（真实
        // 配置目录），故临时 config_dir 的写入只在文件层断言。
        let user_raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&user_path).unwrap()).unwrap();
        assert_eq!(user_raw["mcpServers"]["user-stdio"]["command"], "uvx");
        let written: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            written["mcpServers"]["fs"],
            serde_json::json!({
                "command": "npx",
                "args": ["-y", "server-filesystem"],
                "env": {"TOKEN": "t"}
            })
        );

        // list：项目来源列出全部 server，含传输摘要。
        out.clear();
        mcp_list(&cwd, &mut out).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("[project]"), "{text}");
        assert!(text.contains("stdio: npx -y server-filesystem"), "{text}");
        assert!(text.contains("sse: https://x/mcp"), "{text}");

        // disable：user 级拒绝名单（写 `config_dir/mcp.json`）。
        out.clear();
        let deny_path = mcp_set_disabled(&cwd, &config_dir, "fs", true, &mut out).unwrap();
        assert_eq!(deny_path, config_dir.join("mcp.json"));
        let denied: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&deny_path).unwrap()).unwrap();
        assert_eq!(denied["disabledServers"], serde_json::json!(["fs"]));
        assert!(
            String::from_utf8_lossy(&out).contains("disabled"),
            "禁用消息：{}",
            String::from_utf8_lossy(&out)
        );
        // list 标注禁用态（改项目来源文件——user 级路径是平台配置目录，测试环境不可见）。
        agent_config::set_mcp_server_disabled(&path, "fs", true).unwrap();
        out.clear();
        mcp_list(&cwd, &mut out).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("[disabled]"), "禁用标注：{text}");
        agent_config::set_mcp_server_disabled(&path, "fs", false).unwrap();

        // enable：名单清空后删除键。
        out.clear();
        mcp_set_disabled(&cwd, &config_dir, "fs", false, &mut out).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&deny_path).unwrap()).unwrap();
        assert!(raw.get("disabledServers").is_none(), "{raw}");

        // 未知 server 禁用报错（防手误写孤儿名单）。
        out.clear();
        assert!(mcp_set_disabled(&cwd, &config_dir, "nope", true, &mut out).is_err());

        // remove：项目级删除；二次删除报不存在。
        out.clear();
        assert_eq!(
            mcp_remove(&cwd, &config_dir, "fs", true, &mut out).unwrap(),
            path
        );
        assert!(mcp_remove(&cwd, &config_dir, "fs", true, &mut out).is_err());
        let remaining: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(remaining["mcpServers"]["fs"].is_null());
        assert!(remaining["mcpServers"]["remote"].is_object());

        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn mcp_status_reports_effective_servers_and_toml_win() {
        init_en();
        let cwd = std::env::temp_dir().join(format!(
            "gyre-manage-mcp-status-{}-{:#x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(cwd.join(".agent")).expect("建临时目录");
        std::fs::write(
            cwd.join(".agent").join("config.toml"),
            "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"\n\
             [mcp.servers.toml-dup]\ncommand = \"toml-cmd\"\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join("mcp.json"),
            r#"{"mcpServers":{"toml-dup":{"command":"json-cmd"},"json-only":{"command":"ok"},
                "denied":{"url":"https://x/mcp","type":"sse"}},"disabledServers":["denied"]}"#,
        )
        .unwrap();
        let cfg = agent_config::Config::load(&cwd).unwrap();
        let mut out: Vec<u8> = Vec::new();
        mcp_status(&cwd, &cfg, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        // 生效集合：TOML 定义胜出 + JSON 独有条目；拒绝名单条目缺席（按 endpoint 判定，
        // 说明行本身会提到被剔除的名字）。
        assert!(text.contains("toml-dup"), "{text}");
        assert!(text.contains("stdio: toml-cmd"), "TOML 胜出：{text}");
        assert!(text.contains("json-only"), "{text}");
        assert!(
            !text.contains("https://x/mcp"),
            "被拒绝的 server 不应进入生效集合：{text}"
        );
        assert!(
            text.contains("note:") && text.contains("toml-dup"),
            "合并说明：{text}"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
