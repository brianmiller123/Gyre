//! MCP OAuth 端到端：axum 仿真授权服务器 + 受保护资源 + MCP server。
//!
//! 覆盖三条链路：
//! 1. 发现链：RFC 9728 受保护资源元数据 → authorization_servers → RFC 8414
//!    AS 元数据（issuer 校验）→ DCR → 授权码 + PKCE 交换（资源指示随行）；
//! 2. 传输装载：过期凭据连接期先刷后用（refresh 授权），Bearer 自动携带；
//! 3. 401 处置：server 拒绝旧 token → 单飞刷新 → 一次性重试成功 → 新 token 落盘；
//!    刷新被确定性拒绝（invalid_grant）→ 凭据清除 + 错误带登录指引。

use std::sync::Arc;

use agent_config::{OAuthCredentials, save_oauth};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use parking_lot::Mutex;
use serde_json::{Value, json};

/// 仿真服务器共享态。
#[derive(Clone)]
struct StubState {
    /// 已签发 access token 序列（刷新轮换；`tokens[n]` = access-{n+1}）。
    tokens: Arc<Mutex<Vec<String>>>,
    /// MCP 端点当前接受的 token。
    accepted: Arc<Mutex<String>>,
    /// token 端点请求摘要（form 解析）。
    token_calls: Arc<Mutex<Vec<Value>>>,
    /// DCR 注册体。
    dcr_body: Arc<Mutex<Option<Value>>>,
    /// 监听端口（redirect_uri 回调构造用）。
    port: u16,
}

fn form(body: &str) -> Value {
    let mut m = serde_json::Map::new();
    for pair in body.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        m.insert(k.to_string(), Value::String(percent_decode(v)));
    }
    Value::Object(m)
}

/// form 值百分号解码（`+` → 空格；`%XX` 十六进制）。
fn percent_decode(v: &str) -> String {
    let v = v.replace('+', " ");
    let bytes = v.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16)),
                bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16)),
            ) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn well_known_resource(State(st): State<StubState>) -> Response {
    (
        StatusCode::OK,
        axum::Json(json!({
            "resource": "https://mcp.example.test/mcp",
            "authorization_servers": [format!("http://127.0.0.1:{}", st.port)],
            "scopes_supported": ["mcp:tools"],
        })),
    )
        .into_response()
}

async fn well_known_as(State(st): State<StubState>) -> Response {
    let base = format!("http://127.0.0.1:{}", st.port);
    (
        StatusCode::OK,
        axum::Json(json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "registration_endpoint": format!("{base}/register"),
        })),
    )
        .into_response()
}

async fn register(State(st): State<StubState>, body: String) -> Response {
    *st.dcr_body.lock() = serde_json::from_str(&body).ok();
    (
        StatusCode::OK,
        axum::Json(json!({"client_id": "dcr-client-1", "client_secret": "dcr-secret-1"})),
    )
        .into_response()
}

async fn authorize(State(_st): State<StubState>, req: Request) -> Response {
    // 模拟浏览器：302 回 redirect_uri，原样带回 state + 授权码。
    let query = req.uri().query().unwrap_or_default();
    let get = |k: &str| {
        query.split('&').find_map(|p| {
            let (key, val) = p.split_once('=')?;
            (key == k).then(|| val.to_string())
        })
    };
    // redirect_uri 值经 URL 编码（:// → %3A%2F%2F 等），需解码。
    let redirect = get("redirect_uri")
        .map(|v| v.replace("%3A", ":").replace("%2F", "/"))
        .filter(|v| !v.is_empty())
        .expect("authorize 请求应带 redirect_uri");
    let state = get("state").unwrap_or_default();
    Redirect::to(&format!("{redirect}?code=auth-code-1&state={state}")).into_response()
}

async fn token(State(st): State<StubState>, body: String) -> Response {
    let f = form(&body);
    st.token_calls.lock().push(f.clone());
    match f.get("grant_type").and_then(Value::as_str) {
        Some("authorization_code") => {
            let tok = format!("access-{}", st.tokens.lock().len() + 1);
            st.tokens.lock().push(tok.clone());
            *st.accepted.lock() = tok.clone();
            (
                StatusCode::OK,
                axum::Json(json!({
                    "access_token": tok,
                    "refresh_token": "refresh-1",
                    "expires_in": 3600,
                    "token_type": "Bearer",
                })),
            )
                .into_response()
        }
        Some("refresh_token") => {
            if f.get("refresh_token").and_then(Value::as_str) != Some("refresh-1") {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": "invalid_grant"})),
                )
                    .into_response();
            }
            let tok = format!("access-{}", st.tokens.lock().len() + 1);
            st.tokens.lock().push(tok.clone());
            *st.accepted.lock() = tok.clone();
            (
                StatusCode::OK,
                axum::Json(json!({
                    "access_token": tok,
                    "expires_in": 3600,
                    "token_type": "Bearer",
                })),
            )
                .into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "unsupported_grant_type"})),
        )
            .into_response(),
    }
}

async fn mcp_endpoint(State(st): State<StubState>, headers: HeaderMap) -> Response {
    let authz = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let expected = format!("Bearer {}", st.accepted.lock());
    if authz != expected {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(
                "WWW-Authenticate",
                format!(
                    "Bearer error=\"invalid_token\", resource_metadata=\"http://127.0.0.1:{}/.well-known/oauth-protected-resource\"",
                    st.port
                ),
            )
            .body(axum::body::Body::from("unauthorized"))
            .unwrap();
    }
    (
        StatusCode::OK,
        axum::Json(json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"protocolVersion": "2025-06-18", "serverInfo": {"name": "stub"}}
        })),
    )
        .into_response()
}

struct StubServer {
    url: String,
    state: StubState,
}

async fn spawn_stub() -> StubServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = StubState {
        tokens: Arc::default(),
        accepted: Arc::new(Mutex::new(String::new())),
        token_calls: Arc::default(),
        dcr_body: Arc::default(),
        port,
    };
    let app = axum::Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(well_known_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(well_known_as),
        )
        .route("/register", post(register))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .route("/mcp", post(mcp_endpoint))
        .with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    StubServer {
        url: format!("http://127.0.0.1:{port}/mcp"),
        state,
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("gyre-mcp-oauth-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_flow_end_to_end_with_dcr_and_resource() {
    let stub = spawn_stub().await;
    let config_dir = temp_dir("flow");
    let http = reqwest::Client::new();

    // 发现（9728 → authorization_servers → 8414）→ DCR → 登录流。
    let flow = agent_mcp::oauth::LoginFlow::discover(&http, &stub.url, None, None)
        .await
        .expect("发现应成功");
    let auth_url: Arc<Mutex<String>> = Arc::default();
    let auth_url_sink = auth_url.clone();
    let creds = flow
        .run(&config_dir, None, move |url| {
            *auth_url_sink.lock() = url.to_string();
            // 模拟浏览器：跟随授权链接（302 打回本进程的 CallbackServer）。
            let url = url.to_string();
            tokio::spawn(async move {
                let resp = reqwest::get(&url).await.expect("authorize 应可达");
                assert_eq!(resp.status(), 200, "回调成功页应 200");
            });
        })
        .await
        .expect("登录流应成功");
    assert_eq!(creds.access, "access-1");
    assert_eq!(creds.refresh, "refresh-1");
    let expected_token_url = format!("http://127.0.0.1:{}/token", stub.state.port);
    assert_eq!(
        creds.token_url.as_deref(),
        Some(expected_token_url.as_str())
    );
    assert_eq!(creds.client_id.as_deref(), Some("dcr-client-1"));
    assert_eq!(creds.client_secret.as_deref(), Some("dcr-secret-1"));
    // 资源指示 = 受保护资源元数据公告值（非 server URL 兜底合成）。
    assert_eq!(
        creds.resource.as_deref(),
        Some("https://mcp.example.test/mcp")
    );

    // 授权 URL 携带 DCR client_id 与公告资源。
    let au = auth_url.lock().clone();
    assert!(au.contains("client_id=dcr-client-1"), "授权 URL: {au}");
    assert!(
        au.contains("resource=https%3A%2F%2Fmcp.example.test%2Fmcp"),
        "授权 URL: {au}"
    );
    assert!(au.contains("code_challenge_method=S256"), "授权 URL: {au}");

    // DCR 注册体：公开客户端 / native / 双 grant（omp 语义）。
    let dcr = stub.state.dcr_body.lock().clone().expect("应发起 DCR");
    assert_eq!(dcr["token_endpoint_auth_method"], "none");
    assert_eq!(dcr["application_type"], "native");
    assert_eq!(dcr["grant_types"][0], "authorization_code");
    assert_eq!(dcr["grant_types"][1], "refresh_token");

    // 交换请求携带资源指示与 client_id。
    let calls = stub.state.token_calls.lock().clone();
    let exchange = calls
        .iter()
        .find(|c| c["grant_type"] == "authorization_code")
        .expect("应有授权码交换");
    assert_eq!(exchange["resource"], "https://mcp.example.test/mcp");
    assert_eq!(exchange["client_id"], "dcr-client-1");

    // 落盘：`mcp_oauth:<url>` 行 + 刷新物随行。
    let store = agent_config::load_oauth(&config_dir);
    let row = store
        .0
        .get(&agent_mcp::oauth::credential_key(&stub.url))
        .expect("凭据应落盘");
    assert_eq!(row.access, "access-1");
    let expected_token_url = format!("http://127.0.0.1:{}/token", stub.state.port);
    assert_eq!(row.token_url.as_deref(), Some(expected_token_url.as_str()));
}

#[tokio::test(flavor = "multi_thread")]
async fn transport_proactive_refresh_and_401_retry() {
    let stub = spawn_stub().await;
    let config_dir = temp_dir("transport");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let key = agent_mcp::oauth::credential_key(&stub.url);

    // 场景 1：过期凭据 → connect 期先刷后用，Bearer 自动携带。
    save_oauth(
        &config_dir,
        &key,
        &OAuthCredentials {
            access: "stale-0".into(),
            refresh: "refresh-1".into(),
            expires: now - 60_000,
            token_url: Some(format!("http://127.0.0.1:{}/token", stub.state.port)),
            client_id: Some("dcr-client-1".into()),
            client_secret: Some("dcr-secret-1".into()),
            resource: Some("https://mcp.example.test/mcp".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let client = agent_mcp::McpClient::connect_in_cfg(
        &agent_config::McpServerConfig::Http(agent_config::McpHttpConfig {
            url: stub.url.clone(),
            headers: Default::default(),
            timeout_ms: Some(5_000),
            oauth: None,
            transport: agent_config::McpHttpTransport::default(),
        }),
        &config_dir,
    )
    .await
    .expect("连接应成功");
    client
        .initialize()
        .await
        .expect("预刷新后 initialize 应成功");
    // 刷新已发生：token 端点收到 refresh 授权，且 server 现在接受新 token。
    assert!(
        stub.state
            .token_calls
            .lock()
            .iter()
            .any(|c| c["grant_type"] == "refresh_token" && c["refresh_token"] == "refresh-1"),
        "应发起 refresh 授权"
    );

    // 场景 2：server 换接受值 → 401 → 单飞刷新 → 重试成功 → 新 token 落盘。
    *stub.state.accepted.lock() = "future-token".into();
    client.initialize().await.expect("401 刷新重试后应成功");
    let row = agent_config::load_oauth(&config_dir)
        .0
        .get(&key)
        .cloned()
        .expect("行应仍在");
    assert_eq!(
        row.access,
        *stub.state.accepted.lock(),
        "落盘 token 应与 server 接受值一致"
    );

    // 场景 3：刷新被确定性拒绝（refresh token 篡改 → 400 invalid_grant）→
    // 凭据清除 + 错误带登录指引。篡改落盘行后**重连**（内存凭据仍是新鲜的，
    // 须让装载路径读回被篡改的行）。
    *stub.state.accepted.lock() = "future-token-2".into();
    save_oauth(
        &config_dir,
        &key,
        &OAuthCredentials {
            refresh: "broken".into(),
            ..row.clone()
        },
    )
    .unwrap();
    let client2 = agent_mcp::McpClient::connect_in_cfg(
        &agent_config::McpServerConfig::Http(agent_config::McpHttpConfig {
            url: stub.url.clone(),
            headers: Default::default(),
            timeout_ms: Some(5_000),
            oauth: None,
            transport: agent_config::McpHttpTransport::default(),
        }),
        &config_dir,
    )
    .await
    .expect("重连应成功（行未过期，不触发预刷新）");
    let err = client2.initialize().await.expect_err("应失败");
    let msg = err.to_string();
    assert!(msg.contains("agent mcp login"), "错误应指引用户重登：{msg}");
    assert!(
        !agent_config::load_oauth(&config_dir).0.contains_key(&key),
        "确定性失败后凭据行应被清除"
    );
}
