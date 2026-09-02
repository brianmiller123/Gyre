//! HTTP + SSE 传输层：JSON-RPC 请求入口（`/acp/rpc`）+ SSE 事件流（`/acp/sse/{id}`）。
//!
//! 鉴权复用 `SessionManager::expected_token()`（与 Web 前端同一份 `config.server.auth_token`），
//! 支持 `Authorization: Bearer <token>` 头或 `?token=` 查询参数。
//!
//! `session/prompt` 在 HTTP 模式中阻塞等待 turn 完成后返回 `PromptResponse`；
//! 期间的 `session/update` 通知由 SSE 流（`/acp/sse/{id}`）异步推送。
//!
//! ## 权限交互（`session/request_permission`）
//!
//! turn 中的 [`ServerFrame::Ask`]（工具审批 / 追问）由 SSE 流转为
//! `session/request_permission` JSON-RPC **请求**推送（SSE 事件名即方法名，data 为
//! 完整请求 JSON，params 按 ACP v1），并登记到进程级注册表；客户端经
//! `POST /acp/rpc` 以同 id 的 JSON-RPC **响应**回答，outcome 映射为
//! [`AskResponse`] 后经 `ClientFrame::Respond` 投递到会话 inbound 通道
//! （与 Web 前端同一 pending 通道），被阻塞的工具审批 / 询问继续执行。
//! 等待上限 [`crate::rpc::PERMISSION_TIMEOUT_SECS`]（10 分钟）：超时按拒绝处理
//! （tracing 记录，工具层错误会经 update 流回客户端）。
//!
//! 注意：若客户端未订阅 SSE 流，权限请求无法送达（无服务端→客户端请求通道），
//! 此时审批依赖服务端自身的审批超时兜底。

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{LazyLock, Mutex};

use agent_server::{ClientFrame, ServerFrame, SessionManager};
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
use crate::rpc::{
    PERMISSION_TIMEOUT_SECS, PendingPermissions, dispatch_rpc, extract_prompt_text,
    is_rpc_response, permission_request, permission_timeout_resolution, register_permission,
    resolve_permission_response, start_prompt,
};
use crate::types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, SessionNotification};

/// 构建 ACP HTTP 路由（已注入 state，返回 `Router<()>`）。
///
/// 由于 `agent-acp` 依赖 `agent-server`（复用 `SessionManager`），为避免循环依赖，
/// 路由合并在 CLI 组装层完成：`agent_server::app(state.clone()).merge(acp_routes(state))`。
pub fn acp_routes(state: SessionManager) -> Router {
    Router::new()
        // JSON-RPC 请求入口（亦接收客户端对 session/request_permission 的响应）。
        .route("/acp/rpc", post(handle_rpc))
        // SSE 事件流：客户端订阅指定 session 的事件。
        .route("/acp/sse/{session_id}", get(handle_sse))
        .with_state(state)
}

/// 处理 JSON-RPC 请求；若载荷是 JSON-RPC 响应（客户端回答权限请求）则走回执路由。
async fn handle_rpc(
    State(state): State<SessionManager>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(code) = check_auth(&state, extract_token(&headers).as_deref()) {
        return (code, "无效或缺失 token").into_response();
    }

    // JSON-RPC 响应：客户端回答服务端发起的 session/request_permission。
    if is_rpc_response(&body) {
        return handle_permission_answer(&state, &body).await;
    }

    let req: JsonRpcRequest = match serde_json::from_value(body) {
        Ok(req) => req,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("无效的 JSON-RPC 请求: {e}"),
            )
                .into_response();
        }
    };
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

/// HTTP 传输待回执权限请求注册表（进程级，rpc id → 待回执项）。
///
/// SSE 流发出 `session/request_permission` 时登记；客户端经 `POST /acp/rpc`
/// 回答（JSON-RPC 响应）或超时任务到期时移除。
static HTTP_PERMISSIONS: LazyLock<Mutex<PendingPermissions>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 锁定互斥量（中毒时恢复：持锁 panic 不会破坏注册表不变量）。
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// 为一条 HTTP 权限请求派生超时任务：到期仍未回执则按拒绝投递（fail closed）。
fn spawn_permission_timeout(state: SessionManager, rpc_id: String) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(PERMISSION_TIMEOUT_SECS)).await;
        let entry = lock(&HTTP_PERMISSIONS).remove(&rpc_id);
        if let Some(entry) = entry {
            let resolved = permission_timeout_resolution(entry);
            tracing::warn!(
                rpc_id = %rpc_id,
                note = resolved.note.as_deref().unwrap_or_default(),
                "ACP HTTP 权限请求等待超时，已按拒绝处理"
            );
            if let Some(session) = state.get(&resolved.session_id).await {
                let _ = session.inbound.send(ClientFrame::Respond {
                    ask_id: resolved.ask_id,
                    response: resolved.response,
                });
            }
        }
    });
}

/// 处理客户端对 `session/request_permission` 的 JSON-RPC 响应（`POST /acp/rpc`）。
///
/// 经进程级注册表配对待回执请求，outcome 映射为 [`agent_core::AskResponse`] 后经
/// `ClientFrame::Respond` 投递（与 Web 前端同一 pending 通道），被阻塞的工具/询问继续执行。
async fn handle_permission_answer(state: &SessionManager, body: &serde_json::Value) -> Response {
    // 登记表操作不跨 await：先完成配对再投递。
    let resolved = {
        let mut registry = lock(&HTTP_PERMISSIONS);
        resolve_permission_response(&mut registry, body)
    };
    match resolved {
        Some(resolved) => {
            if let Some(note) = &resolved.note {
                tracing::warn!(%note, "ACP 权限回执按拒绝处理");
            }
            if let Some(session) = state.get(&resolved.session_id).await {
                let _ = session.inbound.send(ClientFrame::Respond {
                    ask_id: resolved.ask_id,
                    response: resolved.response,
                });
            }
            StatusCode::OK.into_response()
        }
        // 未知 / 已回执的 id：幂等忽略，返回 404 供客户端诊断。
        None => (StatusCode::NOT_FOUND, "未知或已处理的权限请求 id").into_response(),
    }
}

/// HTTP 模式 `session/prompt`：投递消息后阻塞等待 turn 完成。
///
/// session/update 通知由 SSE 流异步推送（客户端须已订阅 `/acp/sse/{id}`）；
/// 此 handler 仅消费 broadcast 等待终止帧，然后返回 `PromptResponse`。
/// 权限请求（`ServerFrame::Ask`）同样由 SSE 流负责转为 `session/request_permission`。
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

    let stop_reason = tokio::time::timeout(std::time::Duration::from_secs(600), async {
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
    .await
    .unwrap_or("timeout");

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

/// SSE 事件流：订阅指定 session 的 `ServerFrame` broadcast，转为 `session/update` SSE；
/// `ServerFrame::Ask`（审批/追问）转为 `session/request_permission` JSON-RPC 请求推送。
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
                Ok(frame) => match frame {
                    // 审批/追问：转为 session/request_permission 请求（SSE 事件名即方法名，
                    // data 为完整 JSON-RPC 请求），登记待回执并派生超时任务；
                    // 客户端经 POST /acp/rpc 以同 id 的 JSON-RPC 响应回答。
                    ServerFrame::Ask { ask } => {
                        let rpc_id =
                            register_permission(&mut lock(&HTTP_PERMISSIONS), &sid, &ask);
                        let request = permission_request(rpc_id.clone(), &sid, &ask);
                        spawn_permission_timeout(state.clone(), rpc_id);
                        match serde_json::to_string(&request) {
                            Ok(json) => yield Ok::<_, Infallible>(
                                Event::default()
                                    .event("session/request_permission")
                                    .data(json),
                            ),
                            Err(e) => tracing::warn!(error = %e, "session/request_permission 序列化失败"),
                        }
                    }
                    frame => {
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
                },
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
