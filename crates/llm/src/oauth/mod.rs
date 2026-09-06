//! 认证链 OAuth：回环 PKCE / 授权码 / 设备码流引擎 + provider 流注册表。
//!
//! 移植 oh-my-pi `packages/ai/src/registry/oauth/` 的核心机制（截枝版）：
//! - PKCE S256（96 字节 verifier，与 omp `pkce.ts` 一致）；
//! - 回环回调服务器（state 校验 / 成功-错误页 / 手动粘贴赛跑 / 5 分钟超时；
//!   简化：仅绑 127.0.0.1，无 IPv6 双栈与 `/launch` 短链——头lessness 环境仍可用粘贴流）；
//! - RFC 8628 设备码轮询（1.2x/1.4x 倍率、slow_down +5s、deadline 超时）；
//! - 统一凭据 [`agent_config::OAuthCredentials`] + 刷新合并语义（org 字段保旧）。
//!
//! 存储在 `agent_config`（`oauth.toml`，0600）；本模块只管「拿到/刷新」凭据。
//! 运行时注入：[`ensure_fresh`] 在解析凭据时先刷后用（60s 提前量 + 10s 刷新超时）。

mod anthropic;
mod codex;
mod device;
mod zai;

pub use agent_core::oauth_callback::{CallbackServer, parse_callback_input};
pub use device::{DevicePollConfig, Poll, poll_device_flow};

use std::sync::Arc;
use std::time::Duration;

use agent_config::OAuthCredentials;
use async_trait::async_trait;

/// 刷新请求超时（omp `DEFAULT_OAUTH_REFRESH_TIMEOUT_MS`）。
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);
/// 回调等待超时（omp `DEFAULT_TIMEOUT`）。
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// 交给用户打开的授权入口。
#[derive(Debug, Clone)]
pub struct AuthInfo {
    pub url: String,
    /// 本机快捷方式（回环 `/launch` 302）；Gyre 截枝版恒为 `None`，字段保留对齐 omp。
    pub launch_url: Option<String>,
    /// 附加指引（如「输入设备码」）。
    pub instructions: Option<String>,
}

/// 授权入口 / 进度回调类型。
pub type AuthCallback = Box<dyn Fn(&AuthInfo) + Send + Sync>;
pub type ProgressCallback = Box<dyn Fn(&str) + Send + Sync>;

/// 登录过程 UI 回调（CLI/TUI 注入；测试注入 no-op）。
#[derive(Default)]
pub struct LoginCtl {
    on_auth: Option<AuthCallback>,
    on_progress: Option<ProgressCallback>,
    /// 手动粘贴授权码入口（CLI 经 stdin 读行；None = 无粘贴赛跑）。
    manual_code: Option<ManualCodeInput>,
}

pub type ManualCodeInput =
    Box<dyn Fn() -> futures::future::BoxFuture<'static, String> + Send + Sync>;

impl LoginCtl {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn on_auth(mut self, f: impl Fn(&AuthInfo) + Send + Sync + 'static) -> Self {
        self.on_auth = Some(Box::new(f));
        self
    }

    #[must_use]
    pub fn on_progress(mut self, f: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_progress = Some(Box::new(f));
        self
    }

    #[must_use]
    pub fn manual_code(
        mut self,
        f: impl Fn() -> futures::future::BoxFuture<'static, String> + Send + Sync + 'static,
    ) -> Self {
        self.manual_code = Some(Box::new(f));
        self
    }

    pub fn auth(&self, info: &AuthInfo) {
        if let Some(f) = &self.on_auth {
            f(info);
        }
    }

    pub fn progress(&self, msg: &str) {
        if let Some(f) = &self.on_progress {
            f(msg);
        }
    }

    fn manual(&self) -> Option<futures::future::BoxFuture<'static, String>> {
        self.manual_code.as_ref().map(|f| f())
    }
}

/// 登录产物：有的流铸出长效 API key（写入 auth.toml），有的产 OAuth 凭据（写入 oauth.toml）。
/// 对齐 omp「login 返回 string ⇒ api_key 登录」语义。
#[derive(Debug, Clone)]
pub enum LoginResult {
    ApiKey(String),
    /// 装箱：OAuthCredentials 字段多（变体尺寸差越 clippy 线），LoginResult
    /// 本身按值传递、凭据体大——堆置更合适。
    Credentials(Box<OAuthCredentials>),
}

/// 一个 provider 的 OAuth 流。
#[async_trait]
pub trait OAuthFlow: Send + Sync {
    /// 凭据落库键（Gyre 线协议族名；ApiKey 产物落到 auth.toml 同名键）。
    fn store_key(&self) -> &'static str;
    /// 接受的 CLI 登录 id（omp 风格 id + Gyre 线协议名）。
    fn ids(&self) -> &'static [&'static str];

    fn matches(&self, id: &str) -> bool {
        let id = id.trim().to_lowercase();
        self.ids().iter().any(|k| *k == id)
    }

    async fn login(&self, client: &reqwest::Client, ctl: &LoginCtl) -> anyhow::Result<LoginResult>;
    async fn refresh(
        &self,
        client: &reqwest::Client,
        creds: &OAuthCredentials,
    ) -> anyhow::Result<OAuthCredentials>;
}

/// 全部内置流。新增流：实现 [`OAuthFlow`] 后在此注册（对齐 omp registry 声明式接入）。
pub fn flows() -> Vec<Arc<dyn OAuthFlow>> {
    vec![
        Arc::new(anthropic::AnthropicFlow::default()),
        Arc::new(codex::CodexFlow::default()),
        Arc::new(zai::ZaiFlow::default()),
    ]
}

/// 按 CLI 登录 id 找流。
#[must_use]
pub fn flow_for(id: &str) -> Option<Arc<dyn OAuthFlow>> {
    flows().into_iter().find(|f| f.matches(id))
}

/// 登录 id 候选清单（错误提示用）。
#[must_use]
pub fn supported_ids() -> Vec<&'static str> {
    flows()
        .iter()
        .flat_map(|f| f.ids().iter().copied())
        .collect()
}

/// epoch 毫秒（测试外统一时钟入口）。
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// 回调等待 + 手动粘贴赛跑：`ctl.manual_code` 存在时两路取先；
/// 粘贴自带的 state 若存在且与预期不符 → 拒绝（防粘贴错窗口的授权码）。
pub(crate) async fn wait_code(
    server: CallbackServer,
    state: &str,
    ctl: &LoginCtl,
) -> anyhow::Result<String> {
    let expected = state.to_owned();
    let server_wait = server.wait_for_code(&expected, CALLBACK_TIMEOUT);
    match ctl.manual() {
        Some(manual) => {
            tokio::select! {
                r = server_wait => r,
                pasted = manual => {
                    let (code, pasted_state) = agent_core::oauth_callback::parse_callback_input(&pasted)
                        .ok_or_else(|| anyhow::anyhow!("无法从粘贴内容解析授权码：{pasted}"))?;
                    if let Some(s) = pasted_state.filter(|s| *s != expected) {
                        anyhow::bail!("粘贴的授权码 state 不匹配（期望 {expected}，得到 {s}）——可能复制自另一次登录");
                    }
                    Ok(code)
                }
            }
        }
        None => server_wait.await,
    }
}

/// 「绝对授权寿命已过，需重新登录」提示（仅 anthropic 家族有 30 天硬寿命）。
#[must_use]
pub fn grant_expired_warning(store_key: &str, creds: &OAuthCredentials) -> bool {
    if store_key == "anthropic-messages" {
        anthropic::grant_expired(creds, now_ms())
    } else {
        false
    }
}

/// 请求路径上的凭据保鲜：未过期直接返回 access；过期则分派流刷新（merge 保 org、
/// 落盘回写、10s 超时），失败向上抛（调用方决定降级/报错——不做静默降级）。
///
/// # Errors
/// 找不到已存 provider 对应的流（手工放置的凭据），或刷新请求失败时返回错误。
pub async fn ensure_fresh(
    client: &reqwest::Client,
    config_dir: &std::path::Path,
    provider: &str,
    creds: &OAuthCredentials,
) -> anyhow::Result<String> {
    let now = now_ms();
    if !creds.is_expired(now) {
        return Ok(creds.access.clone());
    }
    let flow = flow_for(provider)
        .ok_or_else(|| anyhow::anyhow!("provider「{provider}」没有内置 OAuth 流，无法自动刷新"))?;
    let fresh = tokio::time::timeout(REFRESH_TIMEOUT, flow.refresh(client, creds))
        .await
        .map_err(|_| anyhow::anyhow!("OAuth 刷新超时（{provider}，10s）"))??;
    let merged = creds.merge_refreshed(fresh);
    agent_config::save_oauth(config_dir, provider, &merged)
        .map_err(|e| anyhow::anyhow!("刷新后的凭据落盘失败：{e}"))?;
    Ok(merged.access)
}

/// 运行时凭据解析（会话启动 / 模型热切换共用）：OAuth 条目（有效→access；将
/// 过期→先刷后用）优先于 auth.toml/env，与 omp「oauth 覆写 api_key 行」语义对齐；
/// config `api_key` 仍为最高优先（项目级显式配置）。无 OAuth 条目时退化为
/// [`agent_config::resolve`] 三段回退。
///
/// 同步封装：内部 `block_in_place`（须在 multi-thread runtime 上下文调用——
/// `#[tokio::main]` / server / rpc 装配点均满足）。刷新失败不阻断启动：WARN +
/// 用旧 token 尝试请求（omp 刷新失败 backoff 后同语义，不做静默降级之外的分叉）。
///
/// 返回空串 = 无凭据（调用方原样传入 ProviderCallContext）。
#[must_use]
pub fn resolve_runtime_api_key(
    client: &reqwest::Client,
    config_dir: &std::path::Path,
    provider: &str,
    config_key_empty: bool,
    config_key: &str,
) -> String {
    // config 显式值最高优先（与 auth::resolve 第一段一致）。
    if !config_key_empty && !config_key.is_empty() {
        return config_key.to_owned();
    }
    let Some(creds) = agent_config::load_oauth(config_dir).get(provider).cloned() else {
        return agent_config::resolve(
            config_key_empty,
            config_key,
            &agent_config::load(config_dir),
            provider,
        )
        .unwrap_or_default();
    };
    if !creds.is_expired(now_ms()) {
        return creds.access.clone();
    }
    // 将过期/已过期：先刷后用。
    let attempt = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::try_current()
            .ok()
            .map(|h| h.block_on(ensure_fresh(client, config_dir, provider, &creds)))
    });
    match attempt {
        Some(Ok(access)) => access,
        other => {
            let err = match other {
                Some(Err(e)) => e.to_string(),
                Some(Ok(_)) => unreachable!(),
                None => {
                    "无 tokio runtime 上下文（current-thread flavor 不支持运行中刷新）".to_owned()
                }
            };
            tracing::warn!(provider, error = %err, "OAuth 刷新失败，先用现有 token 尝试请求");
            creds.access.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_rfc7636_shape() {
        // RFC 7636 附录 B 向量（verifier 为 43 字符 ASCII；此处借 96 字节源验证
        // 编码链正确性：SHA-256 → base64url 无填充）。
        use sha2::Digest as _;
        let verifier = b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let digest = sha2::Sha256::digest(verifier);
        assert_eq!(
            agent_core::oauth_callback::b64url(&digest),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn pkce_verifier_is_96_bytes_b64url() {
        let pkce = agent_core::oauth_callback::generate_pkce();
        // 96 字节 → base64url 128 字符（无填充）。
        assert_eq!(pkce.verifier.len(), 128);
        assert!(!pkce.verifier.contains('+') && !pkce.verifier.contains('/'));
        assert_eq!(pkce.challenge.len(), 43);
    }

    #[test]
    fn resolve_runtime_precedence_config_oauth_authstore() {
        let dir = std::env::temp_dir().join(format!("gyre-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let client = reqwest::Client::new();

        // 无任何凭据 → 空。
        assert_eq!(resolve_runtime_api_key(&client, &dir, "zai", true, ""), "");
        // auth.toml 兜底。
        agent_config::save(&dir, "zai", "sk-auth").unwrap();
        assert_eq!(
            resolve_runtime_api_key(&client, &dir, "zai", true, ""),
            "sk-auth"
        );
        // oauth.toml 有效条目压过 auth.toml（omp：oauth 覆写 api_key 行）。
        agent_config::save_oauth(
            &dir,
            "zai",
            &OAuthCredentials {
                access: "sk-oauth-fresh".into(),
                refresh: String::new(),
                expires: now_ms() + 3600 * 1000,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            resolve_runtime_api_key(&client, &dir, "zai", true, ""),
            "sk-oauth-fresh"
        );
        // config 显式值最高优先（项目级配置压过一切用户级凭据）。
        assert_eq!(
            resolve_runtime_api_key(&client, &dir, "zai", false, "sk-config"),
            "sk-config"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_is_32_hex() {
        let s = agent_core::oauth_callback::generate_state();
        assert_eq!(s.len(), 32);
        assert!(s.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn supported_ids_cover_registered_flows() {
        let ids = supported_ids();
        assert!(ids.contains(&"anthropic") && ids.contains(&"anthropic-messages"));
        assert!(ids.contains(&"codex") && ids.contains(&"openai-responses"));
        assert!(ids.contains(&"zai") && ids.contains(&"zai-coding-plan"));
    }
}
