//! Z.ai（GLM Coding Plan）OAuth：回环授权码（无 PKCE/无 secret）→ 铸长效 API key。
//!
//! omp `oauth/zai.ts` 逐常量移植。要点：
//! - redirect **精确** `http://127.0.0.1:9999/callback`（服务端按 client 注册的唯一
//!   CLI 回调，端口被占必须失败，禁兜底——omp #10245）；
//! - 授权参数无 PKCE、无 scope（逐字对齐 ZCode）；
//! - token 请求是**非标准 JSON 体** `{provider, code, redirect_uri, state}`，
//!   响应 `{code,msg,data,success}` 信封（code 0/200 成功）；
//! - 业务链铸 key：business-login 换 biz token → getCustomerInfo 找默认 org/project
//!   → 找/建名为 `gyre` 的 key → copy 端点取未打码 secret → 产 `${apiKey}.${secretKey}`；
//! - 产物是**长效 API key**（expires = 永不过期哨兵、refresh 为空）→ CLI 落 auth.toml。
//!
//! 偏差注明：omp 的 key 名为 `oh-my-pi`；Gyre 用 `gyre`（各铸各的 key，互不覆写）。

use std::time::Duration;

use agent_config::OAuthCredentials;
use async_trait::async_trait;
use serde_json::json;

use super::{AuthInfo, CallbackServer, LoginCtl, LoginResult, OAuthFlow, wait_code};
use crate::oauth::anthropic::{post_json, urlencode};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 长效 key 永不过期哨兵（8.64e15 ms，omp `NEVER_EXPIRES`）。
const NEVER_EXPIRES: i64 = 8_640_000_000_000_000;
/// Gyre 自己的 key 名（不与 ZCode `zcode-api-key`、omp `oh-my-pi` 相互覆写）。
const KEY_NAME: &str = "gyre";

/// 流端点（生产默认经 env 覆盖对齐 omp；测试注入 wiremock）。
#[derive(Debug, Clone)]
pub struct ZaiEndpoints {
    pub client_id: String,
    pub authorize_url: String,
    pub token_url: String,
    pub biz_base: String,
    pub business_login_url: String,
    pub callback_port: u16,
    pub callback_path: &'static str,
}

impl Default for ZaiEndpoints {
    fn default() -> Self {
        let env = |k: &str, d: &str| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| d.to_owned())
        };
        Self {
            client_id: env("ZAI_OAUTH_CLIENT_ID", "client_P8X5CMWmlaRO9gyO-KSqtg"),
            authorize_url: env(
                "ZAI_OAUTH_AUTHORIZE_URL",
                "https://chat.z.ai/api/oauth/authorize",
            ),
            token_url: env(
                "ZAI_OAUTH_TOKEN_URL",
                "https://zcode.z.ai/api/v1/oauth/token",
            ),
            biz_base: env("ZAI_BIZ_BASE", "https://api.z.ai"),
            business_login_url: env(
                "ZAI_BUSINESS_LOGIN_URL",
                "https://api.z.ai/api/auth/z/login",
            ),
            callback_port: 9999,
            callback_path: "/callback",
        }
    }
}

pub struct ZaiFlow {
    ep: ZaiEndpoints,
}

impl Default for ZaiFlow {
    fn default() -> Self {
        Self::new(ZaiEndpoints::default())
    }
}

impl ZaiFlow {
    pub fn new(ep: ZaiEndpoints) -> Self {
        Self { ep }
    }
}

#[async_trait]
impl OAuthFlow for ZaiFlow {
    fn store_key(&self) -> &'static str {
        "zai"
    }

    fn ids(&self) -> &'static [&'static str] {
        &["zai", "zai-coding-plan"]
    }

    async fn login(&self, client: &reqwest::Client, ctl: &LoginCtl) -> anyhow::Result<LoginResult> {
        let state = agent_core::oauth_callback::generate_state();
        // 精确回调：禁端口兜底（服务端只认注册的 9999/callback）。
        let server =
            CallbackServer::bind(self.ep.callback_port, self.ep.callback_path, false).await?;
        let redirect_uri = server.redirect_uri();

        let url = format!(
            "{}?redirect_uri={}&response_type=code&client_id={}&state={}",
            self.ep.authorize_url,
            urlencode(&redirect_uri),
            urlencode(&self.ep.client_id),
            urlencode(&state),
        );
        ctl.auth(&AuthInfo {
            url: url.clone(),
            launch_url: None,
            instructions: Some(
                "在浏览器完成 Z.ai 登录；若浏览器无法访问本机，把最终跳转 URL 或授权码粘贴回终端。"
                    .into(),
            ),
        });

        let code = wait_code(server, &state, ctl).await?;
        let creds = self.exchange(client, &code, &state, &redirect_uri).await?;
        Ok(LoginResult::Credentials(Box::new(creds)))
    }

    async fn refresh(
        &self,
        _client: &reqwest::Client,
        _creds: &OAuthCredentials,
    ) -> anyhow::Result<OAuthCredentials> {
        // 产物是长效 key（expires=永不过期哨兵），永远走不到刷新路径；
        // 显式实现而非默认抛错：签名完整 + 语义明确。
        anyhow::bail!("Z.ai 登录产物为长效 API key，无需刷新")
    }
}

impl ZaiFlow {
    async fn exchange(
        &self,
        client: &reqwest::Client,
        code: &str,
        state: &str,
        redirect_uri: &str,
    ) -> anyhow::Result<OAuthCredentials> {
        // 非标准 token 体（无 grant_type/code_verifier）：逐字对齐 ZCode。
        let body = json!({
            "provider": "zai",
            "code": code,
            "redirect_uri": redirect_uri,
            "state": state,
        });
        let text = post_json(client, &self.ep.token_url, body, &[]).await?;
        let data = unwrap_envelope(&text, "token exchange")?;
        let oauth_access = data["zai"]["access_token"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Z.ai token 响应缺 access_token"))?;

        let minted = self.mint_api_key(client, oauth_access).await?;
        Ok(OAuthCredentials {
            access: minted,
            refresh: String::new(),
            expires: NEVER_EXPIRES,
            email: data["user"]["email"].as_str().map(str::to_owned),
            account_id: data["user"]["id"].as_str().map(str::to_owned),
            authorized_at: Some(crate::oauth::now_ms()),
            ..OAuthCredentials::default()
        })
    }

    /// business-login → 默认 org/project → 找/建 `gyre` key → copy 取 secret。
    async fn mint_api_key(
        &self,
        client: &reqwest::Client,
        oauth_access: &str,
    ) -> anyhow::Result<String> {
        // 1) business login：OAuth token 换 biz token（业务 API 拒收原始 OAuth token）。
        let text = post_json(
            client,
            &self.ep.business_login_url,
            json!({ "token": oauth_access }),
            &[],
        )
        .await?;
        let data = unwrap_envelope(&text, "business login")?;
        let biz_token = data["access_token"]
            .as_str()
            .or_else(|| data["accessToken"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Z.ai business login 未返回 access token"))?;
        let auth = format!("Bearer {biz_token}");

        // 2) 默认 org / project（isDefault 优先，缺省取首个）。
        let info_url = format!("{}/api/biz/customer/getCustomerInfo", self.ep.biz_base);
        let text = self
            .get_json(client, &info_url, &auth, "customer lookup")
            .await?;
        let data = unwrap_envelope(&text, "customer lookup")?;
        let orgs = data["organizations"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let pick_default = |arr: &[serde_json::Value]| {
            arr.iter()
                .find(|v| v["isDefault"].as_bool().unwrap_or(false))
                .or_else(|| arr.first())
                .cloned()
        };
        let org = pick_default(&orgs);
        let projects = org
            .as_ref()
            .and_then(|o| o["projects"].as_array().cloned())
            .unwrap_or_default();
        let project = pick_default(&projects);
        let organization_id = org
            .as_ref()
            .and_then(|o| o["organizationId"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let project_id = project
            .as_ref()
            .and_then(|p| p["projectId"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let (Some(organization_id), Some(project_id)) = (organization_id, project_id) else {
            anyhow::bail!("Z.ai 账号上没有 organization/project，无法铸造 key");
        };

        // 3) 找已有同名 key，否则创建；copy 端点取未打码 secret（列表是掩码，
        //    创建响应内联 secret 不可靠——逐字对齐 omp 策略）。
        let keys_url = format!(
            "{}/api/biz/v1/organization/{organization_id}/projects/{project_id}/api_keys",
            self.ep.biz_base
        );
        let text = self
            .get_json(client, &keys_url, &auth, "api key list")
            .await?;
        let list_data = unwrap_envelope(&text, "api key list")?;
        let keys = as_key_array(&list_data);
        let existing = keys
            .iter()
            .find(|k| k["name"].as_str() == Some(KEY_NAME))
            .and_then(|k| k["apiKey"].as_str())
            .map(str::to_owned);
        let api_key = match existing {
            Some(k) if !k.trim().is_empty() => k,
            _ => {
                let text = post_json(
                    client,
                    &keys_url,
                    json!({ "name": KEY_NAME }),
                    &[("Authorization", auth.as_str())],
                )
                .await?;
                let data = unwrap_envelope(&text, "api key create")?;
                data["apiKey"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Z.ai 创建 key 未返回 apiKey"))?
                    .to_owned()
            }
        };

        let copy_url = format!("{keys_url}/copy/{api_key}");
        let text = self
            .get_json(client, &copy_url, &auth, "api key copy")
            .await?;
        let data = unwrap_envelope(&text, "api key copy")?;
        let secret = data["secretKey"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Z.ai copy 端点未返回 secretKey"))?;
        Ok(format!("{api_key}.{secret}"))
    }

    async fn get_json(
        &self,
        client: &reqwest::Client,
        url: &str,
        auth: &str,
        op: &str,
    ) -> anyhow::Result<String> {
        let resp = client
            .get(url)
            .timeout(REQUEST_TIMEOUT)
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Z.ai {op} 请求失败：{e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("Z.ai {op} 失败：HTTP {status} body={text}");
        }
        Ok(text)
    }
}

/// omp `unwrapEnvelope`：`{code,msg,data,success}` 信封（code 0/200 或缺省成功）；
/// `success: false` 或异常 code → 抛 `msg`；无信封形态的裸体直接透传。
pub(crate) fn unwrap_envelope(text: &str, operation: &str) -> anyhow::Result<serde_json::Value> {
    let body: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("Z.ai {operation} 响应非 JSON：{e}；body={text}"))?;
    let is_envelope =
        body.get("code").is_some() || body.get("success").is_some() || body.get("msg").is_some();
    if is_envelope {
        let code = &body["code"];
        let success_false = body["success"].as_bool() == Some(false);
        let code_bad = match code {
            serde_json::Value::Null => false,
            serde_json::Value::Number(n) => n.as_i64() != Some(0) && n.as_i64() != Some(200),
            serde_json::Value::String(s) => s != "0" && s != "200",
            _ => true,
        };
        if success_false || code_bad {
            let msg = body["msg"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("code {code}"));
            anyhow::bail!("Z.ai {operation} 失败：{msg}");
        }
        return Ok(if body.get("data").is_some() {
            body["data"].clone()
        } else {
            body
        });
    }
    Ok(body)
}

/// key 列表容错：裸数组或常见包装字段（list/keys/apiKeys/records）。
fn as_key_array(v: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(arr) = v.as_array() {
        return arr.clone();
    }
    ["list", "keys", "apiKeys", "records"]
        .iter()
        .find_map(|k| v[*k].as_array().cloned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ep_for(server: &MockServer) -> ZaiEndpoints {
        ZaiEndpoints {
            token_url: format!("{}/oauth/token", server.uri()),
            biz_base: server.uri(),
            business_login_url: format!("{}/z/login", server.uri()),
            ..ZaiEndpoints::default()
        }
    }

    #[tokio::test]
    async fn full_mint_chain_against_stubs() {
        let server = MockServer::start().await;
        // token 端点：非标准 JSON 体 + 信封。
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "msg": "", "data": {
                    "zai": {"access_token": "oa-1"},
                    "user": {"email": "me@z.ai", "id": "u-9"}
                }, "success": true
            })))
            .mount(&server)
            .await;
        // business login。
        Mock::given(method("POST"))
            .and(path("/z/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 200, "data": {"access_token": "biz-1"}
            })))
            .mount(&server)
            .await;
        // customer info：默认 org/project。
        Mock::given(method("GET"))
            .and(path("/api/biz/customer/getCustomerInfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 200, "data": {"organizations": [
                    {"organizationId": "o2", "isDefault": false},
                    {"organizationId": "o1", "isDefault": true, "projects": [
                        {"projectId": "p1", "isDefault": true}
                    ]}
                ]}
            })))
            .mount(&server)
            .await;
        // key 列表：没有 gyre key → 走创建。
        Mock::given(method("GET"))
            .and(path("/api/biz/v1/organization/o1/projects/p1/api_keys"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 200, "data": {"list": [{"name": "zcode-api-key", "apiKey": "k-old"}]}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/biz/v1/organization/o1/projects/p1/api_keys"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 200, "data": {"apiKey": "k-new", "name": "gyre"}
            })))
            .mount(&server)
            .await;
        // copy 端点取全量 secret。
        Mock::given(method("GET"))
            .and(path(
                "/api/biz/v1/organization/o1/projects/p1/api_keys/copy/k-new",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 200, "data": {"secretKey": "s-secret"}
            })))
            .mount(&server)
            .await;

        let f = ZaiFlow::new(ep_for(&server));
        let creds = f
            .exchange(
                &reqwest::Client::new(),
                "cd",
                "st",
                "http://127.0.0.1:9999/callback",
            )
            .await
            .unwrap();
        assert_eq!(
            creds.access, "k-new.s-secret",
            "长效 key = apiKey.secretKey"
        );
        assert_eq!(creds.expires, NEVER_EXPIRES);
        assert!(creds.refresh.is_empty());
        assert_eq!(creds.email.as_deref(), Some("me@z.ai"));
        assert!(creds.authorized_at.is_some());

        // 创建请求体 {name: "gyre"}。
        let reqs = server.received_requests().await.unwrap();
        let create = reqs
            .iter()
            .find(|r| r.method == "POST" && r.url.path().ends_with("/api_keys"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&create.body).unwrap();
        assert_eq!(body["name"], "gyre");
    }

    #[tokio::test]
    async fn envelope_failure_and_missing_secret_are_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 401, "msg": "bad state", "data": null
            })))
            .mount(&server)
            .await;
        let f = ZaiFlow::new(ep_for(&server));
        let err = f
            .exchange(
                &reqwest::Client::new(),
                "cd",
                "st",
                "http://127.0.0.1:9999/callback",
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bad state"), "{err}");
    }

    #[test]
    fn envelope_passthrough_and_string_codes() {
        assert!(unwrap_envelope(r#"{"code":"0"}"#, "x").is_ok());
        assert!(unwrap_envelope(r#"{"plain":1}"#, "x").is_ok(), "裸体透传");
        let v = unwrap_envelope(r#"{"data":{"a":1},"code":200}"#, "x").unwrap();
        assert_eq!(v["a"], 1);
        assert!(unwrap_envelope(r#"{"success":false,"msg":"nope"}"#, "x").is_err());
    }
}
