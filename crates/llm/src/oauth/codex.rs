//! OpenAI Codex（ChatGPT 计划）OAuth：回环 PKCE（固定端口 1455）+ JWT 身份 + 刷新。
//!
//! omp `oauth/openai-codex.ts` 逐常量移植。要点：
//! - redirect_uri 固定 `http://localhost:1455/auth/callback`，**禁用端口兜底**
//!   （服务端按注册回调校验，忙端口随机化会被 403 拒绝）；
//! - 授权参数带 `id_token_add_organizations` / `codex_cli_simplified_flow` / `originator`；
//! - 交换与刷新均为 form-urlencoded（15s 超时）；
//! - 身份取自 access/id_token 的 JWT claim（`https://api.openai.com/auth`），
//!   `orgId = accountId`、`orgName = planType`；expires **不减 skew**（omp 同）；
//! - 刷新结果刻意不含 org 字段（merge 保旧）。
//!
//! 偏差注明：omp originator 发送 `pi`；Gyre 发送 `gyre`（服务端为信息性字段，
//! 值不参与校验）。运行时使用该 token 需把 base_url 指向 ChatGPT 后端并补
//! `originator`/`chatgpt-account-id` 等头——属 openai-responses 适配器增强，凭证层此处完备。

use std::time::Duration;

use agent_config::OAuthCredentials;
use async_trait::async_trait;

use super::{AuthInfo, CallbackServer, LoginCtl, LoginResult, OAuthFlow, wait_code};
use crate::oauth::anthropic::urlencode;
use crate::oauth::now_ms;

const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// JWT claim 路径（omp `JWT_CLAIM_PATH` / `JWT_PROFILE_CLAIM`）。
const CLAIM_AUTH: &str = "https://api.openai.com/auth";
const CLAIM_PROFILE: &str = "https://api.openai.com/profile";

#[derive(Debug, Clone)]
pub struct CodexEndpoints {
    pub client_id: &'static str,
    pub authorize_url: String,
    pub token_url: String,
    pub callback_port: u16,
    pub callback_path: &'static str,
    pub scope: &'static str,
    pub originator: &'static str,
}

impl Default for CodexEndpoints {
    fn default() -> Self {
        Self {
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
            authorize_url: "https://auth.openai.com/oauth/authorize".into(),
            token_url: "https://auth.openai.com/oauth/token".into(),
            callback_port: 1455,
            callback_path: "/auth/callback",
            scope: "openid profile email offline_access api.connectors.read api.connectors.invoke",
            // omp 发 "pi"；Gyre 以自身客户端名标识（信息性，服务端不校验）。
            originator: "gyre",
        }
    }
}

pub struct CodexFlow {
    ep: CodexEndpoints,
}

impl Default for CodexFlow {
    fn default() -> Self {
        Self::new(CodexEndpoints::default())
    }
}

impl CodexFlow {
    pub fn new(ep: CodexEndpoints) -> Self {
        Self { ep }
    }
}

#[async_trait]
impl OAuthFlow for CodexFlow {
    fn store_key(&self) -> &'static str {
        "openai-responses"
    }

    fn ids(&self) -> &'static [&'static str] {
        &["codex", "openai-codex", "openai-responses"]
    }

    async fn login(&self, client: &reqwest::Client, ctl: &LoginCtl) -> anyhow::Result<LoginResult> {
        let pkce = agent_core::oauth_callback::generate_pkce();
        let state = agent_core::oauth_callback::generate_state();
        // 固定 redirect：端口被占直接失败（禁兜底）。
        let server =
            CallbackServer::bind(self.ep.callback_port, self.ep.callback_path, false).await?;
        let redirect_uri = server.redirect_uri();

        let url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&id_token_add_organizations=true&codex_cli_simplified_flow=true&originator={}",
            self.ep.authorize_url,
            urlencode(self.ep.client_id),
            urlencode(&redirect_uri),
            urlencode(self.ep.scope),
            urlencode(&pkce.challenge),
            urlencode(&state),
            urlencode(self.ep.originator),
        );
        ctl.auth(&AuthInfo {
            url: url.clone(),
            launch_url: None,
            instructions: Some(
                "在浏览器完成 ChatGPT 登录；若浏览器无法访问本机，把最终跳转 URL 或授权码粘贴回终端。".into(),
            ),
        });

        let code = wait_code(server, &state, ctl).await?;
        let creds = self
            .exchange(client, &code, &redirect_uri, &pkce.verifier)
            .await?;
        Ok(LoginResult::Credentials(Box::new(creds)))
    }

    async fn refresh(
        &self,
        client: &reqwest::Client,
        creds: &OAuthCredentials,
    ) -> anyhow::Result<OAuthCredentials> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", creds.refresh.as_str()),
            ("client_id", self.ep.client_id),
        ];
        let data = self.post_form(client, &form, "refresh").await?;
        // 刻意不取 org：凭据绑定的 workspace 在登录时固定（merge 保旧）。
        let profile = jwt_profile(data["access_token"].as_str().unwrap_or_default(), None);
        let fallback = creds.refresh.clone();
        Ok(OAuthCredentials {
            access: require_str(&data, "access_token", "refresh")?,
            refresh: data["refresh_token"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(&fallback)
                .to_owned(),
            expires: now_ms() + require_num(&data, "expires_in", "refresh")? * 1000,
            account_id: profile.account_id,
            email: profile.email,
            ..OAuthCredentials::default()
        })
    }
}

struct Profile {
    account_id: Option<String>,
    email: Option<String>,
    plan_type: Option<String>,
}

impl CodexFlow {
    async fn exchange(
        &self,
        client: &reqwest::Client,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> anyhow::Result<OAuthCredentials> {
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", self.ep.client_id),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ];
        let data = self.post_form(client, &form, "exchange").await?;
        let profile = jwt_profile(
            data["access_token"].as_str().unwrap_or_default(),
            data["id_token"].as_str(),
        );
        let account_id = profile
            .account_id
            .ok_or_else(|| anyhow::anyhow!("无法从 token 提取 accountId（JWT claim 缺失）"))?;
        Ok(OAuthCredentials {
            access: require_str(&data, "access_token", "exchange")?,
            refresh: require_str(&data, "refresh_token", "exchange")?,
            expires: now_ms() + require_num(&data, "expires_in", "exchange")? * 1000,
            account_id: Some(account_id.clone()),
            email: profile.email,
            org_id: Some(account_id),
            org_name: profile.plan_type,
            authorized_at: Some(now_ms()),
            ..OAuthCredentials::default()
        })
    }

    async fn post_form(
        &self,
        client: &reqwest::Client,
        form: &[(&str, &str)],
        op: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let resp = client
            .post(&self.ep.token_url)
            .timeout(TOKEN_REQUEST_TIMEOUT)
            .form(form)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("token {op} 请求失败：{e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("token {op} 失败：HTTP {status} body={text}");
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("token {op} 响应非 JSON：{e}；body={text}"))?;
        if let Some(err) = v["error"].as_str() {
            anyhow::bail!(
                "token {op} 被拒：{err} {}",
                v["error_description"].as_str().unwrap_or_default()
            );
        }
        Ok(v)
    }
}

fn require_str(v: &serde_json::Value, key: &str, op: &str) -> anyhow::Result<String> {
    v[key]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("token {op} 响应缺字段：{key}"))
}

fn require_num(v: &serde_json::Value, key: &str, op: &str) -> anyhow::Result<i64> {
    v[key]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("token {op} 响应缺字段：{key}"))
}

/// 从 access（与可选 id_token 兜底 plan_type）解 JWT claim。
fn jwt_profile(access: &str, id_token: Option<&str>) -> Profile {
    let payload = decode_jwt_payload(access);
    let id_payload = id_token.and_then(decode_jwt_payload);
    let auth = payload.as_ref().and_then(|p| p.get(CLAIM_AUTH));
    let id_auth = id_payload.as_ref().and_then(|p| p.get(CLAIM_AUTH));
    let email = payload
        .as_ref()
        .and_then(|p| p.get(CLAIM_PROFILE))
        .and_then(|p| p["email"].as_str())
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty());
    let plan = auth
        .and_then(|a| a["chatgpt_plan_type"].as_str())
        .or_else(|| id_auth.and_then(|a| a["chatgpt_plan_type"].as_str()))
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty());
    Profile {
        account_id: auth
            .and_then(|a| a["chatgpt_account_id"].as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        email,
        plan_type: plan,
    }
}

/// 解 JWT payload 段（不验签——身份提取用，与 omp decodeJwt 一致）。
#[must_use]
pub fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload_b64 = token.split('.').nth(1)?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use base64::Engine as _;

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
    }

    /// 构造假 JWT（header.payload.signature），payload 带指定 claim。
    fn fake_jwt(auth: serde_json::Value, profile: Option<serde_json::Value>) -> String {
        let mut payload = json!({ "sub": "u1" });
        payload[CLAIM_AUTH] = auth;
        if let Some(p) = profile {
            payload[CLAIM_PROFILE] = p;
        }
        format!(
            "{}.{}.sig",
            b64(br#"{"alg":"none"}"#),
            b64(payload.to_string().as_bytes())
        )
    }

    fn flow_for(server: &MockServer) -> CodexFlow {
        CodexFlow::new(CodexEndpoints {
            token_url: format!("{}/oauth/token", server.uri()),
            ..CodexEndpoints::default()
        })
    }

    #[test]
    fn jwt_profile_extracts_claims() {
        let access = fake_jwt(
            json!({"chatgpt_account_id": "acc-1", "chatgpt_plan_type": "Pro "}),
            None,
        );
        let p = jwt_profile(&access, None);
        assert_eq!(p.account_id.as_deref(), Some("acc-1"));
        assert_eq!(p.plan_type.as_deref(), Some("pro"), "plan 取小写去空白");
    }

    #[tokio::test]
    async fn exchange_posts_form_and_builds_creds() {
        let server = MockServer::start().await;
        let id_token = fake_jwt(json!({"chatgpt_account_id": "x"}), None);
        let access = fake_jwt(
            json!({"chatgpt_account_id": "acc-7", "chatgpt_plan_type": "free"}),
            Some(json!({"email": " U@Ex.com "})),
        );
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": access,
                "refresh_token": "r1",
                "id_token": id_token,
                "expires_in": 600
            })))
            .mount(&server)
            .await;
        let f = flow_for(&server);
        let creds = f
            .exchange(
                &reqwest::Client::new(),
                "c",
                "http://localhost:1455/auth/callback",
                "v",
            )
            .await
            .unwrap();
        assert_eq!(creds.account_id.as_deref(), Some("acc-7"));
        assert_eq!(creds.org_id.as_deref(), Some("acc-7"), "orgId=accountId");
        assert_eq!(creds.org_name.as_deref(), Some("free"));
        assert_eq!(creds.email.as_deref(), Some("u@ex.com"), "email 归一小写");
        // expires 不减 skew：now + 600s（误差 5s 容忍）。
        let now = now_ms();
        assert!(
            (creds.expires - (now + 600_000)).abs() < 5_000,
            "{}",
            creds.expires - now
        );
        // 请求体为 form。
        let body =
            String::from_utf8(server.received_requests().await.unwrap().remove(0).body).unwrap();
        assert!(
            body.contains("grant_type=authorization_code") && body.contains("code_verifier=v"),
            "{body}"
        );
        let ct = server.received_requests().await.unwrap()[0]
            .headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(ct.starts_with("application/x-www-form-urlencoded"), "{ct}");
    }

    #[tokio::test]
    async fn refresh_omits_org_and_falls_back_refresh_token() {
        let server = MockServer::start().await;
        // 刷新响应不带 id_token、access 无 account claim → 身份缺省（merge 保旧）。
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": fake_jwt(json!({}), None),
                "refresh_token": "r2",
                "expires_in": 600
            })))
            .mount(&server)
            .await;
        let f = flow_for(&server);
        let old = OAuthCredentials {
            refresh: "r-old".into(),
            access: "a".into(),
            expires: now_ms() - 1,
            org_id: Some("keep-org".into()),
            email: Some("old@x".into()),
            ..Default::default()
        };
        let fresh = f.refresh(&reqwest::Client::new(), &old).await.unwrap();
        assert!(fresh.org_id.is_none());
        assert!(fresh.email.is_none(), "access 无 profile claim 时不虚造");
        let merged = old.merge_refreshed(fresh);
        assert_eq!(merged.org_id.as_deref(), Some("keep-org"));
        assert_eq!(merged.refresh, "r2");
    }

    #[tokio::test]
    async fn token_error_surfaces_provider_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_grant",
                "error_description": "bad code"
            })))
            .mount(&server)
            .await;
        let f = flow_for(&server);
        let err = f
            .exchange(
                &reqwest::Client::new(),
                "bad",
                "http://localhost:1455/auth/callback",
                "v",
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid_grant") && err.to_string().contains("bad code"),
            "{err}"
        );
    }

    #[test]
    fn decode_jwt_payload_rejects_garbage() {
        assert!(decode_jwt_payload("not-a-jwt").is_none());
        assert!(decode_jwt_payload("a.!!!.c").is_none());
    }
}
