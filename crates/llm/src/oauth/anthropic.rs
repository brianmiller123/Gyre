//! Anthropic（Claude Console）OAuth：回环 PKCE + rotation 刷新。
//!
//! omp `oauth/anthropic.ts` 逐常量移植。要点：
//! - 必须用 `claude.ai` 授权端点（`platform.claude.com` 只发 console token，
//!   不含 `user:inference` scope，无法直连推理）；
//! - token 交换为 **JSON 体**（非 form），无 Accept 头（模仿 Claude Code）；
//! - 刷新**额外**带 `anthropic-beta: oauth-2025-04-20` 与 SDK UA；返回刻意不含
//!   org 字段（merge 保旧）；
//! - 绝对授权寿命 30 天：rotation 不续期，到期只能重新交互登录（CLI 据此提示）。

use std::time::Duration;

use agent_config::OAuthCredentials;
use async_trait::async_trait;
use serde_json::json;

use super::{AuthInfo, CallbackServer, LoginCtl, LoginResult, OAuthFlow, wait_code};
use crate::oauth::now_ms;

/// 30 天授权绝对寿命（omp `ANTHROPIC_OAUTH_GRANT_TTL_MS`）。
pub const GRANT_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// access token 过期前预留（omp：`expires_in - 5min`）。
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 流端点（生产默认；测试注入 wiremock 地址）。
#[derive(Debug, Clone)]
pub struct AnthropicEndpoints {
    pub client_id: &'static str,
    pub authorize_url: String,
    pub token_url: String,
    pub bootstrap_url: String,
    pub callback_port: u16,
    pub callback_path: &'static str,
    pub scopes: &'static str,
}

impl Default for AnthropicEndpoints {
    fn default() -> Self {
        Self {
            // omp：base64 存储的 OAuth app client_id（atob 即得）。
            client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
            authorize_url: "https://claude.ai/oauth/authorize".into(),
            token_url: "https://api.anthropic.com/v1/oauth/token".into(),
            bootstrap_url: "https://api.anthropic.com/api/claude_cli/bootstrap".into(),
            callback_port: 54_545,
            callback_path: "/callback",
            scopes: "org:create_api_key user:profile user:inference user:sessions:claude_code \
                     user:mcp_servers user:file_upload",
        }
    }
}

pub struct AnthropicFlow {
    ep: AnthropicEndpoints,
}

impl Default for AnthropicFlow {
    fn default() -> Self {
        Self::new(AnthropicEndpoints::default())
    }
}

impl AnthropicFlow {
    pub fn new(ep: AnthropicEndpoints) -> Self {
        Self { ep }
    }
}

#[async_trait]
impl OAuthFlow for AnthropicFlow {
    fn store_key(&self) -> &'static str {
        "anthropic-messages"
    }

    fn ids(&self) -> &'static [&'static str] {
        &["anthropic", "anthropic-messages", "claude"]
    }

    async fn login(&self, client: &reqwest::Client, ctl: &LoginCtl) -> anyhow::Result<LoginResult> {
        let pkce = agent_core::oauth_callback::generate_pkce();
        let state = agent_core::oauth_callback::generate_state();
        let server =
            CallbackServer::bind(self.ep.callback_port, self.ep.callback_path, true).await?;
        let redirect_uri = server.redirect_uri();

        let url = format!(
            "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
            self.ep.authorize_url,
            urlencode(self.ep.client_id),
            urlencode(&redirect_uri),
            urlencode(self.ep.scopes),
            urlencode(&pkce.challenge),
            urlencode(&state),
        );
        ctl.auth(&AuthInfo {
            url: url.clone(),
            launch_url: None,
            instructions: Some(
                "在浏览器完成登录；若浏览器无法访问本机，把最终跳转 URL 或授权码粘贴回终端。"
                    .into(),
            ),
        });

        let code = wait_code(server, &state, ctl).await?;
        let creds = self
            .exchange(client, &code, &state, &redirect_uri, &pkce.verifier)
            .await?;
        Ok(LoginResult::Credentials(Box::new(creds)))
    }

    async fn refresh(
        &self,
        client: &reqwest::Client,
        creds: &OAuthCredentials,
    ) -> anyhow::Result<OAuthCredentials> {
        // CC 在刷新时带这两个头，初次交换不带（omp 注释）。
        let body = json!({
            "grant_type": "refresh_token",
            "client_id": self.ep.client_id,
            "refresh_token": creds.refresh,
        });
        let text = post_json(
            client,
            &self.ep.token_url,
            body,
            &[
                ("anthropic-beta", "oauth-2025-04-20"),
                // omp 对齐 CC 的 SDK UA（bot 侧识别）；版本取保守占位。
                (
                    "User-Agent",
                    "anthropic-sdk-typescript/1.0.0 userOAuthProvider",
                ),
            ],
        )
        .await?;
        let data: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("刷新响应非 JSON：{e}；body={text}"))?;
        require_token_fields(&data, "refresh")?;

        // 刻意不取 org 字段：凭据绑定的 org 在登录时固定（merge 保旧）。
        let identity = resolve_identity(client, self, &data, false).await;
        let fallback_refresh = creds.refresh.clone();
        Ok(OAuthCredentials {
            refresh: data["refresh_token"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(&fallback_refresh)
                .to_owned(),
            access: data["access_token"].as_str().unwrap_or_default().to_owned(),
            expires: now_ms() + data["expires_in"].as_i64().unwrap_or_default() * 1000
                - EXPIRY_SKEW_MS,
            account_id: identity.account_id,
            email: identity.email,
            ..OAuthCredentials::default()
        })
    }
}

/// 身份字段（bootstrap 兜底后的产出）。
struct Identity {
    account_id: Option<String>,
    email: Option<String>,
    org_id: Option<String>,
    org_name: Option<String>,
}

impl AnthropicFlow {
    async fn exchange(
        &self,
        client: &reqwest::Client,
        code: &str,
        state: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> anyhow::Result<OAuthCredentials> {
        let body = json!({
            "grant_type": "authorization_code",
            "client_id": self.ep.client_id,
            "code": code,
            "state": state,
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
        });
        let text = post_json(client, &self.ep.token_url, body, &[]).await?;
        let data: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("交换响应非 JSON：{e}；body={text}"))?;
        require_token_fields(&data, "exchange")?;

        let identity = resolve_identity(client, self, &data, true).await;
        Ok(OAuthCredentials {
            refresh: data["refresh_token"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            access: data["access_token"].as_str().unwrap_or_default().to_owned(),
            expires: now_ms() + data["expires_in"].as_i64().unwrap_or_default() * 1000
                - EXPIRY_SKEW_MS,
            account_id: identity.account_id,
            email: identity.email,
            org_id: identity.org_id,
            org_name: identity.org_name,
            authorized_at: Some(now_ms()),
            ..OAuthCredentials::default()
        })
    }
}

fn require_token_fields(data: &serde_json::Value, op: &str) -> anyhow::Result<()> {
    if let Some(err) = data["error"].as_str() {
        anyhow::bail!(
            "Anthropic token {op} 被拒：{err} {}",
            data["error_description"].as_str().unwrap_or_default()
        );
    }
    let missing: Vec<&str> = ["access_token", "refresh_token", "expires_in"]
        .iter()
        .copied()
        .filter(|k| data[*k].is_null())
        .collect();
    if !missing.is_empty() {
        anyhow::bail!("Anthropic token {op} 响应缺字段：{}", missing.join(", "));
    }
    Ok(())
}

/// 从 token 响应取身份；缺失且 `include_org` 时回退 bootstrap 端点（omp 同语义，
/// 兜底失败容忍——身份仅用于展示）。
async fn resolve_identity(
    client: &reqwest::Client,
    flow: &AnthropicFlow,
    data: &serde_json::Value,
    include_org: bool,
) -> Identity {
    let account = &data["account"];
    let org = &data["organization"];
    let mut id = Identity {
        account_id: account["uuid"].as_str().map(str::to_owned),
        email: account["email_address"].as_str().map(str::to_owned),
        org_id: org["uuid"].as_str().map(str::to_owned),
        org_name: org["name"].as_str().map(str::to_owned),
    };
    let satisfied =
        id.email.is_some() && id.account_id.is_some() && (!include_org || id.org_id.is_some());
    if satisfied {
        return id;
    }
    // bootstrap 兜底（omp fetchBootstrapIdentity）。
    let access = data["access_token"].as_str().unwrap_or_default();
    if access.is_empty() {
        return id;
    }
    let url = format!(
        "{}?entrypoint=cli&model=claude-opus-4-8",
        flow.ep.bootstrap_url
    );
    let resp = client
        .get(&url)
        .timeout(REQUEST_TIMEOUT)
        .header("Authorization", format!("Bearer {access}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", "claude-code/1.0.0")
        .send()
        .await;
    if let Ok(resp) = resp {
        if let Ok(v) = resp.json::<serde_json::Value>().await {
            let acct = &v["account"];
            let orgv = &v["organization"];
            id.account_id = id
                .account_id
                .or_else(|| acct["uuid"].as_str().map(str::to_owned));
            id.email = id
                .email
                .or_else(|| acct["email_address"].as_str().map(str::to_owned));
            id.org_id = id
                .org_id
                .or_else(|| orgv["uuid"].as_str().map(str::to_owned));
            id.org_name = id
                .org_name
                .or_else(|| orgv["name"].as_str().map(str::to_owned));
        }
    }
    id
}

/// 授权寿命是否已过 30 天绝对期（CLI 重登提醒；展示启发，非线上契约）。
#[must_use]
pub fn grant_expired(creds: &OAuthCredentials, now_ms: i64) -> bool {
    match creds.authorized_at {
        Some(t) => now_ms - t > GRANT_TTL_MS,
        None => false,
    }
}

pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub(crate) async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: serde_json::Value,
    headers: &[(&str, &str)],
) -> anyhow::Result<String> {
    let mut req = client.post(url).timeout(REQUEST_TIMEOUT).json(&body);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("请求 {url} 失败：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("HTTP {status} url={url} body={text}");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn flow_for(server: &MockServer) -> AnthropicFlow {
        let ep = AnthropicEndpoints {
            token_url: format!("{}/v1/oauth/token", server.uri()),
            bootstrap_url: format!("{}/bootstrap", server.uri()),
            ..AnthropicEndpoints::default()
        };
        AnthropicFlow::new(ep)
    }

    #[tokio::test]
    async fn exchange_sends_json_body_and_parses_identity() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "sk-ant-oat01-x",
                "refresh_token": "rt1",
                "expires_in": 3600,
                "account": {"uuid": "acct-1", "email_address": "a@b.c"},
                "organization": {"uuid": "org-9", "name": "Acme"}
            })))
            .mount(&server)
            .await;
        let f = flow_for(&server);
        let creds = f
            .exchange(
                &reqwest::Client::new(),
                "code1",
                "st",
                "http://127.0.0.1:1/cb",
                "ver",
            )
            .await
            .unwrap();
        assert_eq!(creds.access, "sk-ant-oat01-x");
        assert_eq!(creds.org_id.as_deref(), Some("org-9"));
        assert_eq!(creds.email.as_deref(), Some("a@b.c"));
        assert!(creds.authorized_at.is_some());
        // expires = now + 3600s - 300s（5 分钟 skew）。
        assert!(creds.expires <= now_ms() + 3600 * 1000 - 290_000);
        // 请求体是 JSON（非 form）且含 state/code_verifier。
        let body: serde_json::Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["state"], "st");
        assert_eq!(body["code_verifier"], "ver");
        let ct = server.received_requests().await.unwrap()[0]
            .headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(ct.starts_with("application/json"), "{ct}");
    }

    #[tokio::test]
    async fn refresh_sends_beta_header_and_omits_org() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "sk-ant-oat01-y",
                "refresh_token": "rt2",
                "expires_in": 3600,
                "account": {"uuid": "acct-1", "email_address": "a@b.c"}
            })))
            .mount(&server)
            .await;
        let f = flow_for(&server);
        let old = OAuthCredentials {
            refresh: "rt-old".into(),
            access: "old".into(),
            expires: now_ms() - 1,
            org_id: Some("org-keep".into()),
            org_name: Some("Keep".into()),
            authorized_at: Some(42),
            ..Default::default()
        };
        let fresh = f.refresh(&reqwest::Client::new(), &old).await.unwrap();
        // merge 保 org（引擎 merge_refreshed 在 ensure_fresh 侧做；此处断言流输出不含 org）。
        assert!(fresh.org_id.is_none(), "刷新结果刻意不含 org");
        let merged = old.merge_refreshed(fresh);
        assert_eq!(merged.org_id.as_deref(), Some("org-keep"));
        assert_eq!(merged.authorized_at, Some(42));
        // 头：anthropic-beta + SDK UA。
        let reqs = server.received_requests().await.unwrap();
        let headers = &reqs[0].headers;
        assert_eq!(
            headers.get("anthropic-beta").unwrap().to_str().unwrap(),
            "oauth-2025-04-20"
        );
        assert!(
            headers
                .get("user-agent")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("userOAuthProvider")
        );
        // 刷新轮换：用新 refresh_token。
        assert_eq!(merged.refresh, "rt2");
    }

    #[test]
    fn grant_ttl_warning() {
        let now = now_ms();
        let c = OAuthCredentials {
            authorized_at: Some(now - GRANT_TTL_MS - 1),
            ..Default::default()
        };
        assert!(grant_expired(&c, now));
        let c = OAuthCredentials {
            authorized_at: Some(now),
            ..Default::default()
        };
        assert!(!grant_expired(&c, now));
        let c = OAuthCredentials::default();
        assert!(!grant_expired(&c, now), "无 authorizedAt 不误报");
    }

    #[test]
    fn urlencode_matches_pct_shape() {
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
        assert_eq!(urlencode("A-z.~"), "A-z.~");
    }
}
