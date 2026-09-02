//! # agent-proxy
//!
//! SOCKS5 出站代理运行时控制器：进程级共享单例（[`Socks5Controller`]）+ 统一客户端
//! 工厂（[`build_http_client`]），使后端出站 HTTP/HTTPS 请求可经 SOCKS5 代理转发，
//! 且启用与否可在**运行时实时切换**（无需重建 client / Provider / 会话）。
//!
//! 核心机制：`reqwest::Proxy::custom` 闭包按请求求值代理 URL——读共享
//! [`AtomicBool`] 开关，启用则返回预构造的 `socks5://…` 代理 URL，禁用则返回 `None`
//! 直连。reqwest 连接池按「实际连接目标（代理地址 vs 直连源站）」区分建池，切换后
//! 新请求走正确路径，不存在「启用代理后复用直连连接」的问题。
//!
//! 用户选择（Web 开关）持久化到 `.gyre/socks5.state` sidecar（JSON
//! `{"enabled": bool}`），下次启动自动恢复；优先级：CLI 显式参数 ＞ sidecar ＞
//! 配置 `[socks5].enabled`。
//!
//! 仅影响后端出站请求；前端（浏览器）自身访问不经此代理。密码一律经
//! [`Socks5Config`] 的 `SecretString` 存储与 `${ENV}` 展开，日志/API 只出现
//! [`Socks5Controller::redacted`] 脱敏描述，**绝不打印明文密码**。

#![deny(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_config::Socks5Config;
use url::Url;

/// 无控制器时的默认连接超时（与既有 `reqwest::Client::builder()` 行为一致）。
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;

/// 进程级 SOCKS5 运行时控制器（共享单例，经 `Arc` 分发）。
///
/// 持有已校验的配置快照 + 原子开关 + 可选持久化 sidecar 路径。由 [`Self::new`] 构造；
/// 配置不完整（host/port 缺失）时返回 `None`——此时装配层不装代理、开关不可用。
pub struct Socks5Controller {
    /// 已校验配置快照（密码仍为 `SecretString`，仅在构造时展开一次）。
    cfg: Socks5Config,
    /// 预构造的代理 URL（含 percent-encode 后的认证信息）；`proxy_url()` 直接克隆返回。
    proxy: Option<Url>,
    /// 运行时开关（`Proxy::custom` 闭包按请求读取）。
    enabled: AtomicBool,
    /// 持久化 sidecar（`None` = 不落盘，如无写权限的只读装配）。
    state_path: Option<PathBuf>,
}

impl Socks5Controller {
    /// 从配置构造控制器。
    ///
    /// - 配置不完整（host 空或 port 缺失/为 0）→ `warn!` 明确提示并返回 `None`（代理自动禁用）。
    /// - `cli_override`：CLI `--socks` 给出的显式值（最高优先；给出 `--socks` 即 `Some(true)`）。
    /// - 无 CLI 覆盖时读 sidecar 持久化态；都没有则用配置 `[socks5].enabled`。
    ///
    /// # Panics
    /// 无 panic。
    #[must_use]
    pub fn new(
        cfg: &Socks5Config,
        state_path: Option<PathBuf>,
        cli_override: Option<bool>,
    ) -> Option<Arc<Self>> {
        if !cfg.is_configured() {
            tracing::warn!(
                "SOCKS5 代理配置不完整（host 或 port 缺失），代理已禁用；如需启用请在 \
                 config.toml 的 [socks5] 段或 CLI --socks host:port 补齐"
            );
            return None;
        }
        let proxy = build_proxy_url(cfg);
        let persisted = state_path.as_deref().and_then(read_state);
        let enabled = cli_override.or(persisted).unwrap_or(cfg.enabled);
        Some(Arc::new(Self {
            cfg: cfg.clone(),
            proxy,
            enabled: AtomicBool::new(enabled),
            state_path,
        }))
    }

    /// 当前是否启用代理（原子读；`Proxy::custom` 闭包按请求读取）。
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// 运行时切换开关：原子写 + 尽力持久化到 sidecar（失败仅告警，不影响本次生效）。
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        if let Some(path) = &self.state_path {
            persist_state(path, on);
        }
    }

    /// 配置是否完整（host 非空且 port > 0）。
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.cfg.is_configured()
    }

    /// 预构造的代理 URL（含认证信息）；未配置时返回 `None`。
    #[must_use]
    pub fn proxy_url(&self) -> Option<Url> {
        self.proxy.clone()
    }

    /// 代理主机。
    #[must_use]
    pub fn host(&self) -> &str {
        &self.cfg.host
    }

    /// 代理端口（已配置时恒为 `Some(>0)`）。
    #[must_use]
    pub const fn port(&self) -> Option<u16> {
        self.cfg.port
    }

    /// 代理用户名（未配置认证时为 `None`）。
    #[must_use]
    pub fn username(&self) -> Option<&str> {
        self.cfg.username.as_deref()
    }

    /// 代理连接（含 SOCKS5 握手）超时。
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.cfg.connect_timeout_secs.max(1))
    }

    /// 脱敏描述（日志/API 展示）：`socks5://user:***@host:port`，**绝不出现明文密码**。
    #[must_use]
    pub fn redacted(&self) -> String {
        self.cfg.redacted()
    }
}

/// 统一出站客户端工厂：注入按请求求值的动态代理（`Proxy::custom`）。
///
/// - 有控制器：启用时走 SOCKS5 代理、禁用时直连（同一 client 实时切换）；连接超时用
///   控制器配置（覆盖 SOCKS5 握手阶段）。
/// - 无控制器：行为与现状完全一致（直连 + 默认 10s 连接超时），不装代理。
///
/// 所有经此工厂构建的客户端都携带 User-Agent（LLM 请求默认与 OMP 对齐的
/// `pi/<version> (<platform> <release>; <arch>)`，见
/// [`agent_core::platform::default_llm_user_agent`]）；`user_agent` 传入配置覆盖值
/// （`None` 或空字符串 = 默认），空字符串不会产生空 UA 头。
///
/// 既有各 client 的 `tcp_keepalive` / `pool_idle_timeout` 等定制经 `configure` 闭包
/// 原样保留。
///
/// # Errors
/// 底层 `reqwest::ClientBuilder::build` 失败（如 TLS 后端初始化失败）。
pub fn build_http_client(
    socks5: Option<Arc<Socks5Controller>>,
    user_agent: Option<&str>,
    configure: impl FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
) -> reqwest::Result<reqwest::Client> {
    let mut b = reqwest::Client::builder();
    match socks5 {
        Some(ctrl) => {
            let for_closure = Arc::clone(&ctrl);
            let proxy = reqwest::Proxy::custom(move |_url: &reqwest::Url| {
                if for_closure.enabled() {
                    for_closure.proxy_url()
                } else {
                    None // 直连
                }
            });
            b = b.proxy(proxy).connect_timeout(ctrl.connect_timeout());
        }
        None => {
            b = b.connect_timeout(Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS));
        }
    }
    let ua = user_agent
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(agent_core::platform::default_llm_user_agent);
    b = b.user_agent(ua);
    configure(b).build()
}

/// 构造代理 URL：`socks5://[user:pass@]host:port`，认证信息经 `url` crate
/// percent-encode（`@` / `:` / 非 ASCII 等转义）。仅密码无用户名 → 忽略密码并告警
/// （见 [`Socks5Config::auth`]）。
fn build_proxy_url(cfg: &Socks5Config) -> Option<Url> {
    if !cfg.is_configured() {
        return None;
    }
    let host = cfg.host.trim();
    let port = cfg.port?;
    // IPv6 字面量需方括号包裹。
    let host_part = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let mut u = Url::parse(&format!("socks5://{host_part}:{port}")).ok()?;
    if let Some((user, pass)) = cfg.auth() {
        let _ = u.set_username(&user);
        let _ = u.set_password(Some(&pass));
    }
    Some(u)
}

/// 读取 sidecar 持久化开关（`{"enabled": bool}`）；缺失/损坏返回 `None`。
fn read_state(path: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("enabled").and_then(serde_json::Value::as_bool)
}

/// 尽力持久化开关到 sidecar（创建父目录）；失败仅告警，不影响本次生效。
fn persist_state(path: &Path, enabled: bool) {
    let Ok(text) = serde_json::to_string(&serde_json::json!({ "enabled": enabled })) else {
        return;
    };
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!("SOCKS5: 无法创建状态目录 {}: {e}", dir.display());
            return;
        }
    }
    if let Err(e) = std::fs::write(path, text) {
        tracing::warn!("SOCKS5: 无法持久化开关状态 {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(host: &str, port: Option<u16>, username: Option<&str>, password: &str) -> Socks5Config {
        Socks5Config {
            enabled: false,
            host: host.into(),
            port,
            username: username.map(str::to_string),
            password: secrecy::SecretString::from(password),
            connect_timeout_secs: 10,
        }
    }

    #[test]
    fn proxy_url_no_auth() {
        let u = build_proxy_url(&cfg("127.0.0.1", Some(1080), None, "")).expect("应构造 URL");
        assert_eq!(u.as_str(), "socks5://127.0.0.1:1080");
    }

    #[test]
    fn proxy_url_with_auth_percent_encodes() {
        let u = build_proxy_url(&cfg(
            "proxy.example.com",
            Some(1080),
            Some("us@er"),
            "p@ss:w/rd",
        ))
        .expect("应构造 URL");
        assert_eq!(
            u.as_str(),
            "socks5://us%40er:p%40ss%3Aw%2Frd@proxy.example.com:1080",
            "认证信息应 percent-encode（@ : /）"
        );
    }

    #[test]
    fn proxy_url_ipv6_brackets() {
        let u = build_proxy_url(&cfg("::1", Some(1080), None, "")).expect("应构造 URL");
        assert_eq!(u.as_str(), "socks5://[::1]:1080");
    }

    #[test]
    fn proxy_url_unconfigured_is_none() {
        assert!(build_proxy_url(&cfg("", Some(1080), None, "")).is_none());
        assert!(build_proxy_url(&cfg("h", None, None, "")).is_none());
        assert!(build_proxy_url(&cfg("h", Some(0), None, "")).is_none());
    }

    #[test]
    fn redacted_never_leaks_password() {
        let c = cfg("h", Some(1080), Some("u"), "s3cr3t@pass");
        let red = c.redacted();
        assert_eq!(red, "socks5://u:***@h:1080");
        assert!(!red.contains("s3cr3t"));
    }

    #[test]
    fn new_returns_none_when_unconfigured() {
        assert!(Socks5Controller::new(&cfg("", Some(1080), None, ""), None, None).is_none());
        assert!(Socks5Controller::new(&cfg("h", None, None, ""), None, None).is_none());
    }

    #[test]
    fn toggle_and_sidecar_roundtrip() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state_path = dir.path().join("socks5.state");
        let ctrl = Socks5Controller::new(
            &cfg("127.0.0.1", Some(1080), None, ""),
            Some(state_path.clone()),
            None,
        )
        .expect("已配置应构造成功");
        assert!(!ctrl.enabled(), "配置默认关");
        ctrl.set_enabled(true);
        assert!(ctrl.enabled(), "切换后应立即可见");
        assert_eq!(read_state(&state_path), Some(true), "sidecar 应落盘");
        ctrl.set_enabled(false);
        assert_eq!(read_state(&state_path), Some(false));
    }

    #[test]
    fn priority_cli_over_sidecar_over_config() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state_path = dir.path().join("socks5.state");
        persist_state(&state_path, true);
        // CLI 覆盖 > sidecar。
        let ctrl = Socks5Controller::new(
            &cfg("h", Some(1080), None, ""),
            Some(state_path.clone()),
            Some(false),
        )
        .expect("已配置");
        assert!(!ctrl.enabled(), "CLI 显式 false 应压过 sidecar true");
        // 无 CLI → sidecar。
        let ctrl = Socks5Controller::new(&cfg("h", Some(1080), None, ""), Some(state_path), None)
            .expect("已配置");
        assert!(ctrl.enabled(), "sidecar true 应压过配置默认 false");
        // 无 CLI 无 sidecar → 配置。
        let ctrl =
            Socks5Controller::new(&cfg("h", Some(1080), None, ""), None, None).expect("已配置");
        assert!(!ctrl.enabled());
    }

    #[test]
    fn build_http_client_without_controller_matches_plain_builder() {
        let client = build_http_client(None, None, |b| b).expect("构建");
        // 直连行为由 reqwest 保证；此处仅验证无控制器时工厂不装代理（成功构建即足够）。
        let _ = client;
    }

    #[test]
    fn controller_accessors() {
        let ctrl = Socks5Controller::new(&cfg("127.0.0.1", Some(1080), Some("u"), ""), None, None)
            .expect("已配置");
        assert_eq!(ctrl.host(), "127.0.0.1");
        assert_eq!(ctrl.port(), Some(1080));
        assert_eq!(ctrl.username(), Some("u"));
        assert_eq!(ctrl.connect_timeout(), Duration::from_secs(10));
        assert!(ctrl.is_configured());
    }
}
