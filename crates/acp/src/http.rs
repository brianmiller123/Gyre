//! HTTP + SSE 传输层：JSON-RPC 请求入口（`/acp/rpc`）+ SSE 事件流（`/acp/sse/{id}`）。
//!
//! 鉴权复用 `SessionManager::expected_token()`（与 Web 前端同一份 `config.server.auth_token`），
//! 支持 `Authorization: Bearer <token>` 头或 `?token=` 查询参数。
//!
//! `session/prompt` 在 HTTP 模式中阻塞等待 turn 完成后返回 `PromptResponse`；
//! 期间的 `session/update` 通知由 SSE 流（`/acp/sse/{id}`）异步推送。

use std::convert::Infallible;

use agent_server::SessionManager;
use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Json, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::adapter::{is_terminal_frame, server_frame_to_acp};
use crate::rpc::{dispatch_rpc, extract_prompt_text, start_prompt};
use crate::types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, SessionNotification};

/// 构建 ACP HTTP 路由（已注入 state，返回 `Router<()>`）。
///
/// 由于 `agent-acp` 依赖 `agent-server`（复用 `SessionManager`），为避免循环依赖，
/// 路由合并在 CLI 组装层完成：`agent_server::app(state.clone()).merge(acp_routes(state))`。
pub fn acp_routes(state: SessionManager) -> Router {
    Router::new()
        // JSON-RPC 请求入口。
        .route("/acp/rpc", post(handle_rpc))
        // SSE 事件流：客户端订阅指定 session 的事件。
        .route("/acp/sse/{session_id}", get(handle_sse))
        .with_state(state)
}

/// 处理 JSON-RPC 请求。
async fn handle_rpc(
    State(state): State<SessionManager>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> Response {
    if let Err(code) = check_auth(&state, extract_token(&headers).as_deref()) {
        return (code, "无效或缺失 token").into_response();
    }
    let id = req.id.clone();
    let method = req.method.clone();

    // session/prompt：阻塞等待 turn 完成（session/update 由 SSE 流推送）。
    if method == "session/prompt" {
        return handle_http_prompt(&state, req, id).await;
    }

    match dispatch_rpc(&state, &req).await {
        Ok(result) => id.map_or_else(
            || StatusCode::ACCEPTED.into_response(),
            |id| {
                Json(JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: Some(id),
                    result,
                })
                .into_response()
            },
        ),
        Err(err) => Json(JsonRpcError {
            jsonrpc: "2.0".into(),
            id,
            error: err,
        })
        .into_response(),
    }
}

/// HTTP 模式 `session/prompt`：投递消息后阻塞等待 turn 完成。
///
/// session/update 通知由 SSE 流异步推送（客户端须已订阅 `/acp/sse/{id}`）；
/// 此 handler 仅消费 broadcast 等待终止帧，然后返回 `PromptResponse`。
async fn handle_http_prompt(
    state: &SessionManager,
    req: JsonRpcRequest,
    id: Option<crate::types::JsonRpcId>,
) -> Response {
    use tokio::sync::broadcast::error::RecvError;

    let Some(sid) = req
        .params
        .as_ref()
        .and_then(|p| p.get("sessionId"))
        .and_then(|v| v.as_str())
    else {
        return Json(JsonRpcError {
            jsonrpc: "2.0".into(),
            id,
            error: crate::rpc::rpc_error(-32602, "缺少必填参数 sessionId"),
        })
        .into_response();
    };

    let prompt_text = extract_prompt_text(req.params.as_ref());
    let prompt_text = if prompt_text.is_empty() {
        req.params
            .as_ref()
            .and_then(|p| p.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    } else {
        prompt_text
    };

    let rx_result = start_prompt(state, sid, &prompt_text).await;
    let mut rx = match rx_result {
        Ok(rx) => rx,
        Err(err) => {
            return Json(JsonRpcError {
                jsonrpc: "2.0".into(),
                id,
                error: err,
            })
            .into_response();
        }
    };

    let stop_reason;
    let result = tokio::time::timeout(std::time::Duration::from_secs(600), async {
        loop {
            match rx.recv().await {
                Ok(frame) if is_terminal_frame(&frame) => {
                    return crate::rpc::stop_reason(&frame);
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return "end_turn",
            }
        }
    })
    .await;
    match result {
        Ok(reason) => stop_reason = reason,
        Err(_) => stop_reason = "timeout",
    }

    Json(JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: serde_json::json!({ "stopReason": stop_reason }),
    })
    .into_response()
}

/// SSE 查询鉴权参数。
#[derive(Debug, Deserialize)]
struct SseAuth {
    #[serde(default)]
    token: Option<String>,
}

/// SSE 事件流：订阅指定 session 的 `ServerFrame` broadcast，转为 `session/update` SSE。
async fn handle_sse(
    Path(session_id): Path<String>,
    State(state): State<SessionManager>,
    Query(auth): Query<SseAuth>,
) -> Response {
    if let Err(code) = check_auth(&state, auth.token.as_deref()) {
        return (code, "无效或缺失 token").into_response();
    }
    let Some(session) = state.get(&session_id).await else {
        return (StatusCode::NOT_FOUND, "会话不存在").into_response();
    };
    let rx = session.broadcast.subscribe();
    let sid = session_id;

    let stream = async_stream::stream! {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if let Some(update) = server_frame_to_acp(frame) {
                        let notif = SessionNotification::new(&sid, update);
                        match serde_json::to_string(&notif) {
                            Ok(json) => yield Ok::<_, Infallible>(
                                Event::default().event("session/update").data(json),
                            ),
                            Err(e) => tracing::warn!(error = %e, "session/update 序列化失败"),
                        }
                    }
                }
                // 滞后（订阅慢于生产）：跳过，继续接收最新。
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                // 通道关闭（会话结束）：结束流。
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// 从 `Authorization: Bearer <token>` 头提取 token。
fn extract_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").map(|t| t.trim().to_string()))
}

/// 校验 token：配置了 `auth_token` 则必须匹配（近似常量时间比较）。
fn check_auth(state: &SessionManager, token: Option<&str>) -> Result<(), StatusCode> {
    let Some(expected) = state.expected_token() else {
        return Ok(());
    };
    let ok = token.is_some_and(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()));
    if ok {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// 近似常量时间的字节比较（与 `agent_server` 一致，避免时序侧信道）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
