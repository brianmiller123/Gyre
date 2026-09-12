//! stdio 传输层：读取 stdin 的换行分隔 JSON-RPC，向 stdout 写响应与 `session/update` 通知。
//!
//! 供本地编辑器（如 Zed）作为子进程调用。
//!
//! `session/prompt` 是请求（有 id），需阻塞到 prompt turn 完成后才返回 `PromptResponse`。
//! 在此期间，`session/update` 通知持续写入 stdout；同时监听 stdin 以处理
//! `session/cancel` 通知（用户取消）与 JSON-RPC 响应（权限回执）。turn 完成后写入最终响应。
//!
//! ## 权限交互（`session/request_permission`）
//!
//! turn 进行中收到 [`ServerFrame::Ask`]（工具审批 / 追问）时不再丢弃：转为 JSON-RPC
//! **请求** `session/request_permission` 写入 stdout（params 按 ACP v1），并登记待回执。
//! 客户端对同一 id 写回 JSON-RPC 响应，outcome 映射为 [`AskResponse`] 后经
//! `ClientFrame::Respond` 投递到会话 inbound 通道（与 Web 前端同一 pending 通道），
//! 被阻塞的工具审批 / 询问继续执行。等待上限 [`crate::rpc::PERMISSION_TIMEOUT_SECS`]
//! （10 分钟）：超时按拒绝处理并以 `session/update` 通知注明。
//!
//! stdout 写入采用同步方式（`std::io::stdout` lock + flush），避免异步 writer task
//! 在进程退出时被 runtime 中断导致输出丢失。stdout 写极快，阻塞可忽略。

use std::io::Write;

use agent_server::{ClientFrame, ServerFrame, SessionManager};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::adapter::{is_terminal_frame, server_frame_to_acp};
use crate::rpc::{
    PendingPermissions, ResolvedPermission, dispatch_rpc, expired_permissions, extract_prompt_text,
    is_rpc_response, parse_error_line, permission_request, permission_timeout_resolution,
    post_dispatch_notifications, register_permission, resolve_permission_response, start_prompt,
};
use crate::types::{
    AcpError, JsonRpcError, JsonRpcRequest, JsonRpcResponse, SessionNotification, SessionUpdate,
    TextContent,
};

/// stdio 模式：阻塞读取 stdin 直到 EOF，处理 JSON-RPC 请求并向 stdout 写响应与事件。
///
/// # Errors
/// stdin I/O 错误时返回 [`AcpError`]。
pub async fn run_stdio(state: SessionManager) -> Result<(), AcpError> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line).await?.eq(&0) {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                write_line(&parse_error_line(e.to_string()));
                continue;
            }
        };
        let id = req.id.clone();
        let method = req.method.clone();

        // session/prompt 特殊处理：阻塞到 turn 完成，期间推送 session/update。
        if method == "session/prompt" {
            handle_prompt_turn(&state, &mut reader, &req, id).await;
            continue;
        }

        // 其他方法走标准分发。
        match dispatch_rpc(&state, &req).await {
            Ok(result) => {
                if let Some(id) = id {
                    let resp = JsonRpcResponse {
                        jsonrpc: "2.0".into(),
                        id: Some(id),
                        result: result.clone(),
                    };
                    write_line(&serde_json::to_string(&resp).unwrap_or_default());
                }
                // H12/H13：`session/new|load|resume|fork` 之后推 bootstrap（+ load 回放）
                // 通知。**响应先写**，再推通知——omp 明确记录过「通知抢在响应之前会被
                // 客户端当 unknown session 丢弃」（acp-agent.ts:2053-2064）。
                for note in post_dispatch_notifications(&state, &method, &result).await {
                    match serde_json::to_string(&note) {
                        Ok(json) => write_line(&json),
                        Err(e) => tracing::warn!(error = %e, "session/update 序列化失败"),
                    }
                }
            }
            Err(err) => {
                if let Some(id) = id {
                    let resp = JsonRpcError {
                        jsonrpc: "2.0".into(),
                        id: Some(id),
                        error: err,
                    };
                    write_line(&serde_json::to_string(&resp).unwrap_or_default());
                }
            }
        }
    }
    Ok(())
}

/// 处理一次 `session/prompt` turn：投递消息 → 持续推送 session/update → 返回 PromptResponse。
///
/// 使用 `select!` 同时监听 broadcast（agent 事件）和 stdin（cancel 通知 / 权限回执），
/// 直到收到终止帧（Done/Error）。
async fn handle_prompt_turn<R>(
    state: &SessionManager,
    reader: &mut BufReader<R>,
    req: &JsonRpcRequest,
    id: Option<crate::types::JsonRpcId>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let session_id = req
        .params
        .as_ref()
        .and_then(|p| p.get("sessionId"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            // 兼容旧格式 session_id
            req.params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
        });

    let Some(sid) = session_id else {
        if let Some(id) = id {
            let err = crate::rpc::rpc_error(-32602, "缺少必填参数 sessionId");
            write_line(
                &serde_json::to_string(&JsonRpcError {
                    jsonrpc: "2.0".into(),
                    id: Some(id),
                    error: err,
                })
                .unwrap_or_default(),
            );
        }
        return;
    };

    let prompt_text = extract_prompt_text(req.params.as_ref());
    // 兼容：若 prompt 数组为空，尝试直接取 text 字段（旧格式）。
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

    let result = run_prompt_loop(state, reader, sid, &prompt_text).await;

    match result {
        Ok(stop_reason) => {
            if let Some(id) = id {
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: Some(id),
                    result: serde_json::json!({ "stopReason": stop_reason }),
                };
                write_line(&serde_json::to_string(&resp).unwrap_or_default());
            }
        }
        Err(err) => {
            if let Some(id) = id {
                let resp = JsonRpcError {
                    jsonrpc: "2.0".into(),
                    id: Some(id),
                    error: err,
                };
                write_line(&serde_json::to_string(&resp).unwrap_or_default());
            }
        }
    }
}

/// prompt turn 主循环：消费 broadcast 事件推送通知，同时监听 stdin 与权限超时。
///
/// 返回 `stopReason` 字符串或错误。
async fn run_prompt_loop<R>(
    state: &SessionManager,
    reader: &mut BufReader<R>,
    session_id: &str,
    prompt_text: &str,
) -> Result<&'static str, crate::types::RpcError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::sync::broadcast::error::RecvError;

    let mut rx = start_prompt(state, session_id, prompt_text).await?;
    let mut cancel_buf = String::new();
    // 待回执权限请求（session/request_permission → 客户端 JSON-RPC 响应）。
    let mut pending = PendingPermissions::new();
    // H11：终止时用于推断 stopReason 的上下文（最后一个 assistant 消息 + 是否收到 cancel）。
    let mut cancelled = false;
    let mut last_assistant: Option<agent_core::AssistantMessage> = None;

    loop {
        // 最早到期的权限截止时刻；无待回执时超时分支永久挂起。
        let next_deadline = pending.values().map(|p| p.deadline).min();
        tokio::select! {
            biased;
            // 优先消费 agent 事件。
            frame_result = rx.recv() => match frame_result {
                // 审批/追问：转为 session/request_permission 请求发给客户端，不阻塞事件循环。
                Ok(ServerFrame::Ask { ask }) => {
                    let rpc_id = register_permission(&mut pending, session_id, &ask);
                    let request = permission_request(rpc_id, session_id, &ask);
                    write_line(&serde_json::to_string(&request).unwrap_or_default());
                }
                // 轮次结束帧携带最终 assistant 消息 → 记录供 stopReason 映射（Length/Aborted/refusal）。
                Ok(ServerFrame::TurnEnd { message, .. }) => {
                    last_assistant = Some(message);
                }
                Ok(frame) => {
                    if is_terminal_frame(&frame) {
                        return Ok(crate::rpc::stop_reason_with(
                            &frame,
                            last_assistant.as_ref(),
                            cancelled,
                        ));
                    }
                    if let Some(update) = server_frame_to_acp(frame) {
                        push_update(session_id, update);
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => {
                    return Ok(if cancelled { "cancelled" } else { "end_turn" });
                }
            },
            // 同时监听 stdin：处理 session/cancel 通知与权限回执。
            n = reader.read_line(&mut cancel_buf) => {
                let n = n.unwrap_or(0);
                if n == 0 {
                    // stdin EOF：客户端断开，终止 turn。
                    return Ok(if cancelled { "cancelled" } else { "end_turn" });
                }
                let trimmed = cancel_buf.trim();
                if !trimmed.is_empty() {
                    cancelled |= handle_stdio_line(state, session_id, &mut pending, trimmed).await;
                }
                cancel_buf.clear();
            }
            // 权限等待超时：按拒绝处理并以 session/update 注明。
            _ = permission_deadline(next_deadline) => {
                for entry in expired_permissions(&mut pending) {
                    deliver_resolution(state, permission_timeout_resolution(entry)).await;
                }
            }
        }
    }
}

/// 等待到最早的权限回执截止时刻；无待回执项时永久挂起。
async fn permission_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

/// 处理 prompt turn 期间 stdin 的一行输入。
///
/// 先识别 JSON-RPC 响应（客户端回答 `session/request_permission`），
/// 再按请求/通知走既有分发（`session/cancel` 作用于当前 turn 的会话）。
///
/// 返回 `true` 表示本行是 `session/cancel`（H11：调用方据此把 stopReason 映射为
/// `cancelled`，而不是恒 `end_turn`）。
async fn handle_stdio_line(
    state: &SessionManager,
    session_id: &str,
    pending: &mut PendingPermissions,
    line: &str,
) -> bool {
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            write_line(&parse_error_line(e.to_string()));
            return false;
        }
    };

    if is_rpc_response(&value) {
        match resolve_permission_response(pending, &value) {
            Some(resolved) => deliver_resolution(state, resolved).await,
            None => tracing::debug!("收到未知 id 的 JSON-RPC 响应，忽略"),
        }
        return false;
    }

    let Ok(req) = serde_json::from_value::<JsonRpcRequest>(value) else {
        return false;
    };
    if req.method == "session/cancel" {
        if let Some(session) = state.get(session_id).await {
            let _ = session.inbound.send(ClientFrame::Cancel);
        }
        return true;
    }
    // 其他请求：立即 dispatch 并写响应，避免消息丢失。
    let id = req.id.clone();
    match dispatch_rpc(state, &req).await {
        Ok(result) => {
            if let Some(id) = id {
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: Some(id),
                    result,
                };
                write_line(&serde_json::to_string(&resp).unwrap_or_default());
            }
        }
        Err(err) => {
            if let Some(id) = id {
                let resp = JsonRpcError {
                    jsonrpc: "2.0".into(),
                    id: Some(id),
                    error: err,
                };
                write_line(&serde_json::to_string(&resp).unwrap_or_default());
            }
        }
    }
    false
}

/// 将已解析的权限回执经 `ClientFrame::Respond` 投递到会话 inbound 通道
/// （驱动任务据此解析 pending oneshot，与 Web 前端回执同一路径），
/// 存在说明文案（超时 / 错误 / 无法解析）时以 `session/update` 告知客户端。
async fn deliver_resolution(state: &SessionManager, resolved: ResolvedPermission) {
    if let Some(session) = state.get(&resolved.session_id).await {
        let _ = session.inbound.send(ClientFrame::Respond {
            ask_id: resolved.ask_id,
            response: resolved.response,
        });
    }
    if let Some(note) = resolved.note {
        push_update(
            &resolved.session_id,
            SessionUpdate::AgentMessageChunk {
                content: TextContent::new(note),
            },
        );
    }
}

/// 构造并写入一条 `session/update` 通知到 stdout。
fn push_update(session_id: &str, update: SessionUpdate) {
    let notif = SessionNotification::new(session_id, update);
    match serde_json::to_string(&notif) {
        Ok(json) => write_line(&json),
        Err(e) => tracing::warn!(error = %e, "session/update 序列化失败"),
    }
}

/// 同步写一行到 stdout（含换行 + flush）。
fn write_line(line: &str) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(line.as_bytes());
    let _ = lock.write_all(b"\n");
    let _ = lock.flush();
}
