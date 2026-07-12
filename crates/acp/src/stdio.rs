//! stdio 传输层：读取 stdin 的换行分隔 JSON-RPC，向 stdout 写响应与 `session/update` 通知。
//!
//! 供本地编辑器（如 Zed）作为子进程调用。
//!
//! `session/prompt` 是请求（有 id），需阻塞到 prompt turn 完成后才返回 `PromptResponse`。
//! 在此期间，`session/update` 通知持续写入 stdout；同时监听 stdin 以处理
//! `session/cancel` 通知（用户取消）。turn 完成后写入最终响应。
//!
//! stdout 写入采用同步方式（`std::io::stdout` lock + flush），避免异步 writer task
//! 在进程退出时被 runtime 中断导致输出丢失。stdout 写极快，阻塞可忽略。

use std::io::Write;

use agent_server::{ClientFrame, SessionManager};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::adapter::{is_terminal_frame, server_frame_to_acp};
use crate::rpc::{dispatch_rpc, extract_prompt_text, parse_error_line, start_prompt};
use crate::types::{
    AcpError, JsonRpcError, JsonRpcRequest, JsonRpcResponse, SessionNotification, SessionUpdate,
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
                    let resp = JsonRpcResponse { jsonrpc: "2.0".into(), id: Some(id), result };
                    write_line(&serde_json::to_string(&resp).unwrap_or_default());
                }
                // 通知（无 id）只处理副作用，不返回响应。
            }
            Err(err) => {
                if let Some(id) = id {
                    let resp = JsonRpcError { jsonrpc: "2.0".into(), id: Some(id), error: err };
                    write_line(&serde_json::to_string(&resp).unwrap_or_default());
                }
            }
        }
    }
    Ok(())
}

/// 处理一次 `session/prompt` turn：投递消息 → 持续推送 session/update → 返回 PromptResponse。
///
/// 使用 `select!` 同时监听 broadcast（agent 事件）和 stdin（cancel 通知），
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
            write_line(&serde_json::to_string(&JsonRpcError {
                jsonrpc: "2.0".into(),
                id: Some(id),
                error: err,
            }).unwrap_or_default());
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
                let resp = JsonRpcError { jsonrpc: "2.0".into(), id: Some(id), error: err };
                write_line(&serde_json::to_string(&resp).unwrap_or_default());
            }
        }
    }
}

/// prompt turn 主循环：消费 broadcast 事件推送通知，同时监听 stdin cancel。
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

    loop {
        tokio::select! {
            biased;
            // 优先消费 agent 事件。
            frame_result = rx.recv() => match frame_result {
                Ok(frame) => {
                    if is_terminal_frame(&frame) {
                        return Ok(crate::rpc::stop_reason(&frame));
                    }
                    if let Some(update) = server_frame_to_acp(frame) {
                        push_update(session_id, update);
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return Ok("end_turn"),
            },
            // 同时监听 stdin：处理 session/cancel 通知。
            n = reader.read_line(&mut cancel_buf) => {
                let n = n.unwrap_or(0);
                if n == 0 {
                    // stdin EOF：客户端断开，终止 turn。
                    return Ok("end_turn");
                }
                let trimmed = cancel_buf.trim();
                if !trimmed.is_empty() {
                    if let Ok(req) = serde_json::from_str::<JsonRpcRequest>(trimmed) {
                        if req.method == "session/cancel" {
                            if let Some(session) = state.get(session_id).await {
                                let _ = session.inbound.send(ClientFrame::Cancel);
                            }
                        } else {
                            // 非 cancel 请求：立即 dispatch 并写响应，避免消息丢失。
                            let id = req.id.clone();
                            match dispatch_rpc(state, &req).await {
                                Ok(result) => {
                                    if let Some(id) = id {
                                        let resp = JsonRpcResponse { jsonrpc: "2.0".into(), id: Some(id), result };
                                        write_line(&serde_json::to_string(&resp).unwrap_or_default());
                                    }
                                }
                                Err(err) => {
                                    if let Some(id) = id {
                                        let resp = JsonRpcError { jsonrpc: "2.0".into(), id: Some(id), error: err };
                                        write_line(&serde_json::to_string(&resp).unwrap_or_default());
                                    }
                                }
                            }
                        }
                    }
                }
                cancel_buf.clear();
            }
        }
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
