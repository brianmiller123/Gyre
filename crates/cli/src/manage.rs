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
    /// `config path`：打印配置文件路径。
    ConfigPath,
    /// `config check`：加载 + 校验配置并打印摘要。
    ConfigCheck,
}

/// 路由结果三态（对齐 omp `resolveCliArgv`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// 非管理命令语法 → 回落原路径（reserved 提示 / 真实 prompt，行为不变）。
    Fallthrough,
    /// 动词/子动作识别成功但参数不合法 → 打印用法消息，退出码 2。
    Usage(String),
    /// 识别成功 → 执行。
    Run(ManageCmd),
}

/// `list` 子动作别名（`models list` / `auth ls` 共用）。
const LIST_ACTIONS: &[&str] = &["list", "ls"];

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
    if first.starts_with('-') || first.starts_with('@') {
        return Route::Fallthrough;
    }
    // 转 &str 便于字面量切片模式匹配。
    let rest: Vec<&str> = tokens[1..].iter().map(String::as_str).collect();
    match first.as_str() {
        "models" => match rest.as_slice() {
            // 裸保留词：回落（reserved 提示照旧）。
            [] => Route::Fallthrough,
            [action] if LIST_ACTIONS.contains(action) => Route::Run(ManageCmd::ModelsList),
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
            _ => Route::Fallthrough,
        },
        "config" => match rest.as_slice() {
            [] => Route::Fallthrough,
            ["path"] => Route::Run(ManageCmd::ConfigPath),
            ["path", ..] => Route::Usage(t!("manage.usage.config")),
            ["check"] => Route::Run(ManageCmd::ConfigCheck),
            ["check", ..] => Route::Usage(t!("manage.usage.config")),
            _ => Route::Fallthrough,
        },
        _ => Route::Fallthrough,
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
            Route::Usage(_)
        ));
        // 裸保留词 / 未知子动作 → 回落。
        assert_eq!(route(&["mcp".into()]), Route::Fallthrough);
        assert_eq!(
            route(&["mcp".into(), "frobnicate".into()]),
            Route::Fallthrough
        );
    }

    #[test]
    fn route_config_shapes() {
        assert_eq!(
            route(&["config".into(), "path".into()]),
            Route::Run(ManageCmd::ConfigPath)
        );
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
}
