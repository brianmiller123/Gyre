//! MCP Streamable HTTP 传输集成测试：真实 axum server 覆盖 JSON / SSE / 202 / 会话头路径。

use std::collections::HashMap;
use std::sync::Arc;

use agent_config::{McpHttpConfig, McpServerConfig};
use agent_mcp::McpClient;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::{Value, json};

/// server 记录到的每个请求（供断言头部行为）。
#[derive(Debug, Clone)]
struct RecordedReq {
    method: String,
    /// `Mcp-Session-Id` 请求头。
    session: Option<String>,
    /// `MCP-Protocol-Version` 请求头。
    protocol: Option<String>,
    /// 自定义头 `X-Gyre-Mcp-Test`（验证配置头透传 + `${ENV}` 展开）。
    custom: Option<String>,
    /// initialize 请求体出价的 `protocolVersion`（仅 initialize 非空）。
    bid: Option<String>,
    /// 请求体是否含 id（false = 通知）。
    has_id: bool,
}

type SharedState = Arc<Mutex<Vec<RecordedReq>>>;
fn recorded(state: &SharedState, method: &str) -> RecordedReq {
    state
        .lock()
        .iter()
        .find(|r| r.method == method)
        .cloned()
        .unwrap_or_else(|| panic!("server 未收到 {method} 请求"))
}

/// POST 处理：按 JSON-RPC method 分发各测试场景。
async fn handle(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    let id = body.get("id").cloned();
    let bid = body
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str)
        .map(str::to_string);
    record(&state, &headers, method, id.is_some(), bid);

    // 通知（无 id）→ 202 Accepted。
    let Some(id) = id else {
        return StatusCode::ACCEPTED.into_response();
    };

    let response = |result: Value| json!({"jsonrpc":"2.0","id":id,"result":result});
    let error = |code: i64, message: &str| json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}});

    match method {
        "initialize" => {
            let mut resp = Json(response(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": {"name": "test", "version": "0"}
            })))
            .into_response();
            resp.headers_mut()
                .insert("Mcp-Session-Id", HeaderValue::from_static("sess-42"));
            resp
        }
        "tools/list" => Json(response(json!({
            "tools": [
                {"name": "echo", "description": "回声", "inputSchema": {"type": "object"}}
            ]
        })))
        .into_response(),
        "tools/call" => {
            let name = body
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            // SSE 流：先夹带一条 server 通知，再给匹配 id 的响应（或 error）。
            let resp = if name == "boom" {
                error(-32000, "boom failed")
            } else {
                response(json!({
                    "content": [
                        {"type": "text", "text": "line1"},
                        {"type": "text", "text": "line2"}
                    ]
                }))
            };
            let stream =
                futures::stream::iter(vec![
                    Ok::<Event, std::convert::Infallible>(Event::default().event("message").data(
                        json!({"jsonrpc":"2.0","method":"notifications/changed"}).to_string(),
                    )),
                    Ok::<Event, std::convert::Infallible>(
                        Event::default().event("message").data(resp.to_string()),
                    ),
                ]);
            Sse::new(stream).into_response()
        }
        // JSON 响应体里的 JSON-RPC error（区别于 SSE error 路径）。
        "resources/list" => Json(error(-32603, "json failure")).into_response(),
        _ => Json(error(-32601, "Method not found")).into_response(),
    }
}

/// DELETE 处理：close() 的会话终止请求。
async fn handle_delete(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    record(&state, &headers, "__delete__", false, None);
    StatusCode::OK.into_response()
}

fn record(
    state: &SharedState,
    headers: &HeaderMap,
    method: &str,
    has_id: bool,
    bid: Option<String>,
) {
    state.lock().push(RecordedReq {
        method: method.to_string(),
        session: headers
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        protocol: headers
            .get("MCP-Protocol-Version")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        custom: headers
            .get("X-Gyre-Mcp-Test")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        bid,
        has_id,
    });
}

/// 启动测试 MCP server（127.0.0.1 随机端口），返回 (base_url, 记录器)。
async fn spawn_server() -> (String, SharedState) {
    let state: SharedState = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route(
            "/mcp",
            post(handle)
                .delete(handle_delete)
                .with_state(Arc::clone(&state)),
        )
        .into_make_service();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{addr}/mcp"), state)
}

/// 全场景 client：配置含自定义头（透传 + ENV 展开）与伪造会话头（必须被剥除）。
async fn connect_test_client(url: &str) -> McpClient {
    // SAFETY: 测试专用环境变量，本测试二进制独占，无并发竞争。
    unsafe { std::env::set_var("AGENT_MCP_TEST_TOK", "tok-123") };
    let mut headers = HashMap::new();
    headers.insert(
        "X-Gyre-Mcp-Test".to_string(),
        "Bearer ${AGENT_MCP_TEST_TOK}".to_string(),
    );
    // 传输层独占头：用户配置的同名头必须被剥除（防伪造会话）。
    headers.insert("Mcp-Session-Id".to_string(), "forged".to_string());
    let cfg = McpServerConfig::Http(McpHttpConfig {
        url: url.to_string(),
        headers,
        timeout_ms: None,
    });
    McpClient::connect(&cfg).await.expect("connect")
}

#[tokio::test]
async fn http_json_roundtrip_session_and_protocol_version() {
    let (url, state) = spawn_server().await;
    let client = connect_test_client(&url).await;

    // initialize：请求不得携带会话头/协议版本头（均为握手后才发送）。
    client.initialize().await.expect("initialize");
    let init = recorded(&state, "initialize");
    assert_eq!(init.session.as_deref(), None, "initialize 不得带会话头");
    assert_eq!(init.protocol.as_deref(), None, "握手前不得带协议版本头");
    assert_eq!(
        init.bid.as_deref(),
        Some("2025-11-25"),
        "initialize 应出价 2025-11-25（Bedrock AgentCore 门槛）；server 可降级"
    );
    assert_eq!(
        init.custom.as_deref(),
        Some("Bearer tok-123"),
        "自定义头应透传且 $ENV 展开"
    );

    // notifications/initialized：无 id 的 POST → 202 视为成功。
    let notif = recorded(&state, "notifications/initialized");
    assert!(!notif.has_id, "通知不得带 id");

    // tools/list：携带 server 下发的会话头 + 协议版本头；伪造头不得透传。
    let tools = client.list_tools().await.expect("list_tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    let listed = recorded(&state, "tools/list");
    assert_eq!(listed.session.as_deref(), Some("sess-42"), "应回传会话 id");
    assert_eq!(
        listed.protocol.as_deref(),
        Some("2024-11-05"),
        "握手后应带协议版本头"
    );

    // close：DELETE + Mcp-Session-Id 终止会话。
    client.close().await;
    let del = recorded(&state, "__delete__");
    assert_eq!(del.session.as_deref(), Some("sess-42"), "DELETE 应带会话头");
}

#[tokio::test]
async fn http_sse_response_extracts_matching_id() {
    let (url, _state) = spawn_server().await;
    let client = connect_test_client(&url).await;
    client.initialize().await.expect("initialize");

    // SSE 响应流：夹带通知 + 匹配 id 的 result → 文本拼接。
    let out = client
        .call_tool("echo", json!({}))
        .await
        .expect("call_tool");
    assert_eq!(out, "line1\nline2");
}

#[tokio::test]
async fn http_sse_jsonrpc_error_maps_to_server_error() {
    let (url, _state) = spawn_server().await;
    let client = connect_test_client(&url).await;
    client.initialize().await.expect("initialize");

    let err = client
        .call_tool("boom", json!({}))
        .await
        .expect_err("应报错");
    assert!(err.to_string().contains("boom failed"), "实际: {err}");
}

#[tokio::test]
async fn http_json_rpc_error_maps_to_server_error() {
    let (url, _state) = spawn_server().await;
    let client = connect_test_client(&url).await;
    client.initialize().await.expect("initialize");

    // resources/list → JSON 响应体携带 JSON-RPC error。
    let err = client.list_resources().await.expect_err("应报错");
    assert!(err.to_string().contains("json failure"), "实际: {err}");
}

#[tokio::test]
async fn http_non_2xx_maps_to_http_error() {
    let (url, _state) = spawn_server().await;

    // 指向未注册路径 → 404 → McpError::Http。
    let cfg = McpServerConfig::Http(McpHttpConfig {
        url: format!("{url}nope"),
        headers: HashMap::new(),
        timeout_ms: None,
    });
    let client = McpClient::connect(&cfg).await.expect("connect");
    let err = client.list_tools().await.expect_err("应 404");
    assert!(err.to_string().contains("404"), "实际: {err}");
}
