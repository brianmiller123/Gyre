//! MCP legacy HTTP+SSE 传输集成测试：`type = "sse"` 配置分派到 [`SseTransport`]，
//! 经真实 axum server 验证 endpoint 握手 + POST 受理 + SSE 流内回包的完整回路。

use std::collections::HashMap;
use std::sync::Arc;

use agent_config::{McpHttpConfig, McpHttpTransport, McpServerConfig};
use agent_mcp::McpClient;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};

/// POST /messages 解析出的 JSON-RPC 响应经此通道推给常驻 SSE 流（legacy 语义：
/// 响应不走 POST 响应体，POST 仅 202 受理）。
type ReplyTx = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>>;

/// GET /sse：首帧发 `endpoint` 事件指定 message 端点（相对路径，客户端 base.join 解析），
/// 之后转发回复通道上的 JSON-RPC 响应为 `message` 事件。
async fn handle_sse(
    State(replies): State<ReplyTx>,
) -> Sse<futures::stream::BoxStream<'static, Result<Event, std::convert::Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    *replies.lock() = Some(tx);
    let endpoint_frame = futures::stream::iter([Ok(Event::default()
        .event("endpoint")
        .data("/messages?session_id=t1"))]);
    let message_frames = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|msg| (Ok(Event::default().event("message").data(msg)), rx))
    });
    Sse::new(endpoint_frame.chain(message_frames).boxed())
}

/// POST /messages：通知（无 id）→ 202 无回包；请求解析后按 method 生成 result，
/// 经 SSE 流回包（错误路径同样走流内 JSON-RPC error）。
async fn handle_messages(State(replies): State<ReplyTx>, Json(body): Json<Value>) -> Response {
    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    let Some(id) = body.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let payload = match method {
        "initialize" => json!({
            "jsonrpc": "2.0", "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": {"name": "legacy-sse-test", "version": "0"}
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0", "id": id,
            "result": {"tools": [
                {"name": "echo", "description": "回声", "inputSchema": {"type": "object"}}
            ]}
        }),
        other => json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": format!("method not found: {other}")}
        }),
    };
    replies
        .lock()
        .as_ref()
        .expect("SSE 流应已建立")
        .send(payload.to_string())
        .ok();
    StatusCode::ACCEPTED.into_response()
}

#[tokio::test]
async fn legacy_sse_transport_connects_via_type_sse_config() {
    let replies: ReplyTx = Arc::new(Mutex::new(None));
    let app = Router::new()
        .route("/sse", get(handle_sse).with_state(Arc::clone(&replies)))
        .route("/messages", post(handle_messages).with_state(replies))
        .into_make_service();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    // `transport: Sse` 等价 TOML `type = "sse"`：此前该传输体因配置面无判别键而不可达。
    let cfg = McpServerConfig::Http(McpHttpConfig {
        url: format!("http://{addr}/sse"),
        headers: HashMap::new(),
        timeout_ms: Some(5_000),
        oauth: None,
        transport: McpHttpTransport::Sse,
    });
    let client = McpClient::connect(&cfg).await.expect("connect");
    client.initialize().await.expect("initialize");
    let tools = client.list_tools().await.expect("list_tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    client.close().await;
}
