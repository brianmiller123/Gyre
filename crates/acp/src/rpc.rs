//! ACP JSON-RPC 方法分发。
//!
//! 遵循 Agent Client Protocol v1（agentclientprotocol.com）标准方法名：
//! - `initialize`（请求）— 握手 + 能力协商
//! - `session/new`（请求）— 创建会话
//! - `session/prompt`（请求）— 投递用户消息（阻塞到 turn 完成返回 stopReason）
//! - `session/cancel`（通知）— 取消当前 turn
//! - `session/load`（请求）— 恢复历史会话
//! - `session/close`（请求）— 关闭会话
//! - `session/available_commands`（请求）— 返回注入的可用命令清单（斜杠命令菜单）
//! - `session/request_permission`（服务端→客户端请求）— turn 中的审批/追问经此发给
//!   ACP 客户端，客户端以同 id 的 JSON-RPC 响应回答（构造/登记/回执解析见下方模块）
//!
//! `session/prompt` 需在 turn 期间持续推送 `session/update` 通知，因此由各传输层
//! 自行调用 [`start_prompt`] + 消费 broadcast 实现（HTTP 经 SSE，stdio 经主循环 select）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_core::{AskKind, AskMessage, AskResponse};
use agent_server::{ClientFrame, ServerFrame, SessionManager};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::types::{
    AvailableCommand, CommandInfo, RpcError, SessionNotification, SessionNotificationParams,
    SessionUpdate, TextContent,
};

/// ACP 协议版本（客户端 `initialize` 时协商）。
///
/// ACP v1 线协议中 `protocolVersion` 为整数（非日期字符串）。
pub const ACP_PROTOCOL_VERSION: u16 = 1;

// JSON-RPC 标准错误码。
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

/// 分发 JSON-RPC 请求到对应 handler，返回 `result` 或 [`RpcError`]。
///
/// **注意**：`session/prompt` 不在此分发（需阻塞推送通知），由传输层自行处理。
/// 若通过此函数收到 `session/prompt`，返回错误提示传输层需特殊处理。
pub async fn dispatch_rpc(
    state: &SessionManager,
    req: &crate::types::JsonRpcRequest,
) -> Result<Value, RpcError> {
    match req.method.as_str() {
        "initialize" => Ok(handle_initialize(req)),
        "session/new" => handle_session_new(state, req).await,
        "session/cancel" => handle_session_cancel(state, req).await,
        "session/load" => handle_session_load(state, req).await,
        // H12：ACP 会话列表 / 恢复 / fork（omp `connection.ts:203-212` 的方法名）。
        "session/list" => handle_session_list(state, req).await,
        "session/resume" => handle_session_resume(state, req).await,
        "session/fork" => handle_session_fork(state, req).await,
        "session/close" => handle_session_close(state, req).await,
        "session/set_mode" => handle_set_mode(state, req).await,
        "session/available_commands" => handle_available_commands(state, req).await,
        // authenticate/logout：Gyre 的 ACP initialize 未声明任何 auth 方法，无认证语义
        // 可执行。此前返回空 {} 会让客户端误以为认证就绪、把下游模型失败误归因于会话；
        // 显式报错促使客户端快速失败（对齐 omp acp-agent.ts:676-684 的 fail-fast 语义）。
        "authenticate" | "logout" => Err(rpc_error(
            INTERNAL_ERROR,
            "本 agent 未声明认证方法（auth_methods 为空），authenticate/logout 不可用",
        )),
        // session/prompt 需传输层特殊处理（阻塞推送通知）。
        "session/prompt" => Err(rpc_error(
            INTERNAL_ERROR,
            "session/prompt 须由传输层处理（stdio 主循环 / HTTP SSE）",
        )),
        // session/request_permission 为服务端→客户端方向请求：客户端主动调用属协议误用，
        // 返回 Invalid Request（方法本身有效，只是方向错误）。
        "session/request_permission" => Err(rpc_error(
            INVALID_REQUEST,
            "session/request_permission 是服务端发起的请求，客户端不应调用",
        )),
        // ── 向后兼容：旧自定义方法名映射 ──
        "newTask" => handle_session_new(state, req).await,
        "cancel" => handle_session_cancel(state, req).await,
        other => Err(rpc_error(METHOD_NOT_FOUND, format!("方法未找到: {other}"))),
    }
}

/// 构造 JSON-RPC parse error（id 未知时传 `None`）并序列化为单行字符串。
pub fn parse_error_line(message: impl Into<String>) -> String {
    let err = crate::types::JsonRpcError {
        jsonrpc: "2.0".into(),
        id: None,
        error: rpc_error(PARSE_ERROR, message),
    };
    serde_json::to_string(&err).unwrap_or_else(|_| "{}".into())
}

/// 构造 [`RpcError`]。
pub fn rpc_error(code: i32, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: None,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 参数提取辅助
// ──────────────────────────────────────────────────────────────────────────────

/// 从参数对象取字符串字段。
fn param_str<'a>(params: Option<&'a Value>, key: &str) -> Option<&'a str> {
    params.and_then(|p| p.get(key)).and_then(|v| v.as_str())
}

/// 从参数对象取路径字段（如 ACP `session/new` 的 `NewSessionRequest.cwd`）。
///
/// ACP 客户端（编辑器）在创建会话时携带其打开的项目根目录；此前该字段被忽略，导致 Agent
/// 始终在服务端进程 cwd（容器内常为 `/workspace`）而非用户当前目录工作。
fn param_path<'a>(params: Option<&'a Value>, key: &str) -> Option<&'a std::path::Path> {
    params
        .and_then(|p| p.get(key))
        .and_then(|v| v.as_str())
        .map(std::path::Path::new)
}

/// 从 `session/prompt` 的 `prompt` 数组中提取纯文本。
///
/// 标准 ACP 的 `prompt` 是 `ContentBlock[]`，此处拼接所有 `type: "text"` 块的文本。
pub fn extract_prompt_text(params: Option<&Value>) -> String {
    params
        .and_then(|p| p.get("prompt"))
        .and_then(|p| p.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| {
                    (b.get("type").and_then(|t| t.as_str()) == Some("text"))
                        .then(|| b.get("text").and_then(|t| t.as_str()).map(String::from))
                        .flatten()
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

// ──────────────────────────────────────────────────────────────────────────────
// 方法 handler
// ──────────────────────────────────────────────────────────────────────────────

/// `initialize`：返回协议版本 + 能力声明。
///
/// `agentCapabilities.promptCapabilities` 与 `mcpCapabilities` 为 ACP v1 规范围必填字段
/// （严格客户端如 Zed 会做 schema 校验，缺失即拒绝握手）。此处如实声明本项目能力：
/// - `image: true` —— 支持 `/paste` 粘贴图像（`UserContent::Image`）
/// - `embeddedContext: false` —— 暂未实现 ACP 嵌入式上下文块
/// - `mcp.stdio: true` —— `McpRegistry` 通过子进程 stdio 加载 MCP server
/// - `mcp.http: true` —— `McpRegistry` 亦支持 Streamable HTTP 端点（`[mcp.servers.<name>].url`）
fn handle_initialize(req: &crate::types::JsonRpcRequest) -> Value {
    // 记录客户端信息供诊断
    if let Some(params) = &req.params {
        if let Some(info) = params.get("clientInfo") {
            let name = info.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let version = info.get("version").and_then(|v| v.as_str()).unwrap_or("?");
            tracing::info!(client = %name, version = %version, "ACP initialize");
        }
        // 记录客户端能力（debug 级别）
        if let Some(caps) = params.get("clientCapabilities") {
            tracing::debug!(capabilities = %caps, "客户端能力声明");
        }
        // 协议版本协商警告
        if let Some(client_ver) = params.get("protocolVersion").and_then(|v| v.as_u64()) {
            if (client_ver as u16) > ACP_PROTOCOL_VERSION {
                tracing::warn!(
                    client = client_ver,
                    server = ACP_PROTOCOL_VERSION,
                    "客户端协议版本高于服务端，可能缺失部分能力"
                );
            }
        }
    }

    json!({
        "protocolVersion": ACP_PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": {
                "image": true,
                "embeddedContext": false,
            },
            "mcpCapabilities": {
                "http": true,
                "stdio": true,
                // Gyre 另支持 legacy HTTP+SSE MCP 传输（`[mcp.servers.*] type = "sse"`）。
                "sse": true,
            },
            // H12：会话管理能力（list/resume/fork/close 均已实现；omp 同样声明这四项）。
            "sessionCapabilities": {
                "list": {},
                "resume": {},
                "fork": {},
                "close": {},
            },
        },
        "agentInfo": {
            "name": "agent-project",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "authMethods": [],
    })
}

/// `session/new`：创建新会话，返回 `{ sessionId }`。
///
/// 标准 ACP 的 `NewSessionRequest` 含 `cwd` + `mcpServers`；本项目额外通过 `_meta` 或扩展
/// 字段传递 `model` / `mode`（非标准但向前兼容）。`cwd` 取客户端打开的项目根，使 Agent 工具
/// 在用户当前目录而非服务端进程 cwd 工作。
async fn handle_session_new(
    state: &SessionManager,
    req: &crate::types::JsonRpcRequest,
) -> Result<Value, RpcError> {
    let model = param_str(req.params.as_ref(), "model");
    let mode = param_str(req.params.as_ref(), "mode");
    let cwd = param_path(req.params.as_ref(), "cwd");
    // 兼容：部分客户端通过 _meta 传递 model/mode。
    let model = model.or_else(|| {
        req.params
            .as_ref()
            .and_then(|p| p.get("_meta"))
            .and_then(|m| m.get("model"))
            .and_then(|v| v.as_str())
    });
    let mode = mode.or_else(|| {
        req.params
            .as_ref()
            .and_then(|p| p.get("_meta"))
            .and_then(|m| m.get("mode"))
            .and_then(|v| v.as_str())
    });

    let sid = state
        .create_session(model, None, None, mode, cwd)
        .await
        .map_err(|e| rpc_error(INTERNAL_ERROR, e))?;

    Ok(json!({ "sessionId": sid }))
}

/// `session/cancel`（通知）：投递 [`ClientFrame::Cancel`]，触发当前 turn 的取消令牌。
async fn handle_session_cancel(
    state: &SessionManager,
    req: &crate::types::JsonRpcRequest,
) -> Result<Value, RpcError> {
    let sid = param_str(req.params.as_ref(), "sessionId")
        .or_else(|| param_str(req.params.as_ref(), "session_id")) // 兼容旧格式
        .ok_or_else(|| rpc_error(INVALID_PARAMS, "缺少必填参数 sessionId"))?;
    let session = state
        .get(sid)
        .await
        .ok_or_else(|| rpc_error(INVALID_PARAMS, format!("会话不存在: {sid}")))?;
    let _ = session.inbound.send(ClientFrame::Cancel);
    Ok(json!({}))
}

/// `session/load`：恢复历史会话。
async fn handle_session_load(
    state: &SessionManager,
    req: &crate::types::JsonRpcRequest,
) -> Result<Value, RpcError> {
    let sid = param_str(req.params.as_ref(), "sessionId")
        .ok_or_else(|| rpc_error(INVALID_PARAMS, "缺少必填参数 sessionId"))?;
    let model = param_str(req.params.as_ref(), "model");
    let mode = param_str(req.params.as_ref(), "mode");
    // 与创建时一致：恢复也采用客户端 cwd，确保会话/记忆定位到同一项目目录。
    let cwd = param_path(req.params.as_ref(), "cwd");
    let new_sid = state
        .create_session(model, Some(sid), None, mode, cwd)
        .await
        .map_err(|e| rpc_error(INTERNAL_ERROR, e))?;
    Ok(json!({ "sessionId": new_sid }))
}

/// UNIX 纪元秒 → ISO-8601 UTC（`YYYY-MM-DDTHH:MM:SSZ`）。
///
/// 自实现（避免为 ACP 列表引入 chrono 依赖）：`civil_from_days` 取日期部分，
/// 余秒取时分秒。算法见 <https://howardhinnant.github.io/date_algorithms.html>。
fn unix_to_iso8601(secs: u64) -> String {
    let secs = secs as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// ACP `session/list` 分页大小（对齐 omp `SESSION_PAGE_SIZE`）。
pub const SESSION_PAGE_SIZE: usize = 20;

/// `session/list`：列出历史会话（`{sessions:[{sessionId,cwd,title?,updatedAt?}], nextCursor?}`，H12）。
///
/// 数据源为 [`agent_context::SessionStore`]（按 mtime 倒序），`cwd` 参数决定项目目录
/// （缺省用服务端 cwd）；`cursor` 为上一次响应回带的 `nextCursor`（偏移量）。
async fn handle_session_list(
    state: &SessionManager,
    req: &crate::types::JsonRpcRequest,
) -> Result<Value, RpcError> {
    let cwd = param_path(req.params.as_ref(), "cwd")
        .map_or_else(|| state.cwd().to_path_buf(), std::path::Path::to_path_buf);
    let cursor: usize = match param_str(req.params.as_ref(), "cursor") {
        None => 0,
        Some(raw) => raw
            .parse()
            .map_err(|_| rpc_error(INVALID_PARAMS, format!("cursor 非法: {raw:?}")))?,
    };
    let store = agent_context::SessionStore::for_cwd(&cwd);
    let all = store.list();
    let page: Vec<Value> = all
        .iter()
        .skip(cursor)
        .take(SESSION_PAGE_SIZE)
        .map(|info| {
            let title = store.title_for(&info.id);
            let updated = info
                .mtime
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| unix_to_iso8601(d.as_secs()));
            let mut v = json!({
                "sessionId": info.id,
                "cwd": cwd.display().to_string(),
            });
            if let Some(t) = title {
                v["title"] = json!(t);
            }
            if let Some(u) = updated {
                v["updatedAt"] = json!(u);
            }
            v
        })
        .collect();
    let next = cursor + page.len();
    let mut result = json!({ "sessions": page });
    if next < all.len() {
        result["nextCursor"] = json!(next.to_string());
    }
    Ok(result)
}

/// `session/resume`：恢复历史会话，**不**回放历史（与 `session/load` 的唯一区别，H12）。
async fn handle_session_resume(
    state: &SessionManager,
    req: &crate::types::JsonRpcRequest,
) -> Result<Value, RpcError> {
    let sid = param_str(req.params.as_ref(), "sessionId")
        .ok_or_else(|| rpc_error(INVALID_PARAMS, "缺少必填参数 sessionId"))?;
    let model = param_str(req.params.as_ref(), "model");
    let mode = param_str(req.params.as_ref(), "mode");
    let cwd = param_path(req.params.as_ref(), "cwd");
    let new_sid = state
        .create_session(model, Some(sid), None, mode, cwd)
        .await
        .map_err(|e| rpc_error(INTERNAL_ERROR, e))?;
    Ok(json!({ "sessionId": new_sid }))
}

/// 分发后的**通知**（H12/H13）：需要传输层以 `session/update` 推送的帧。
///
/// - `session/load` → 先**回放历史**（用户/助手/工具调用与结果），再发 bootstrap；
/// - `session/new` / `session/resume` / `session/fork` → 仅 bootstrap；
/// - 其余方法 → 空。
///
/// bootstrap = `available_commands_update`（H13：omp 只在 bootstrap 推，不设请求方法）
/// + `session_info_update`（标题 / 更新时间）。
pub async fn post_dispatch_notifications(
    state: &SessionManager,
    method: &str,
    result: &Value,
) -> Vec<SessionNotification> {
    let session_id = result
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if session_id.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if method == "session/load" {
        out.extend(replay_history(state, &session_id).await);
    }
    if matches!(
        method,
        "session/new" | "session/load" | "session/resume" | "session/fork"
    ) {
        out.push(available_commands_notification(state, &session_id).await);
        if let Some(note) = session_info_notification(state, &session_id).await {
            out.push(note);
        }
    }
    out
}

/// 构造 `available_commands_update` 通知（H13）。
pub async fn available_commands_notification(
    state: &SessionManager,
    session_id: &str,
) -> SessionNotification {
    let commands: Vec<AvailableCommand> = state
        .available_commands()
        .await
        .into_iter()
        .map(|(name, description)| AvailableCommand {
            // ACP/omp 的 `name` 不带前导 `/`（客户端自行补）。
            name: name.trim_start_matches('/').to_string(),
            description,
            input: None,
        })
        .collect();
    SessionNotification {
        jsonrpc: "2.0".into(),
        method: "session/update".into(),
        params: SessionNotificationParams {
            session_id: session_id.to_string(),
            update: SessionUpdate::AvailableCommandsUpdate {
                available_commands: commands,
            },
        },
    }
}

/// 构造 `session_info_update` 通知；无标题与时间时返回 `None`。
async fn session_info_notification(
    state: &SessionManager,
    session_id: &str,
) -> Option<SessionNotification> {
    // 会话存在性校验（不存在则不发 bootstrap 通知）。
    state.get(session_id).await?;
    // 会话落盘目录：ACP 会话创建时的 cwd 与 data 目录一一对应；此处用服务端 cwd
    // （与会话默认目录一致；客户端 cwd 场景下 `title_for` 可能查不到，退化为仅时间）。
    let store = agent_context::SessionStore::for_cwd(state.cwd());
    let title = store.title_for(session_id);
    let updated_at = store
        .list()
        .into_iter()
        .find(|i| i.id == session_id)
        .and_then(|i| i.mtime.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| unix_to_iso8601(d.as_secs()));
    if title.is_none() && updated_at.is_none() {
        return None;
    }
    Some(SessionNotification {
        jsonrpc: "2.0".into(),
        method: "session/update".into(),
        params: SessionNotificationParams {
            session_id: session_id.to_string(),
            update: SessionUpdate::SessionInfoUpdate { title, updated_at },
        },
    })
}

/// 回放会话历史为 `session/update` 通知（H12；对齐 omp `#replaySessionHistory`）。
///
/// 映射（沿用 ACP 既有 `ToolCall`/`ToolCallUpdate` 形状）：
/// - `User` → `user_message_chunk`
/// - `Assistant` 文本 → `agent_message_chunk`；thinking → `agent_thought_chunk`；
///   工具调用 → `tool_call`（`status:"in_progress"`）
/// - `ToolResult` → `tool_call_update`（`status: completed|failed` + `rawOutput`）
async fn replay_history(state: &SessionManager, session_id: &str) -> Vec<SessionNotification> {
    let Some(session) = state.get(session_id).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for message in active_path_messages(session.context.as_ref()).await {
        let mut push = |update: SessionUpdate| {
            out.push(SessionNotification {
                jsonrpc: "2.0".into(),
                method: "session/update".into(),
                params: SessionNotificationParams {
                    session_id: session_id.to_string(),
                    update,
                },
            });
        };
        match message {
            agent_core::AgentMessage::User(u) => {
                let text = u.content.iter().fold(String::new(), |mut acc, c| {
                    match c {
                        agent_core::UserContent::Text { text } => acc.push_str(text),
                        agent_core::UserContent::Image { .. } => acc.push_str("[image]"),
                    }
                    acc
                });
                if !text.is_empty() {
                    push(SessionUpdate::UserMessageChunk {
                        content: TextContent::new(text),
                    });
                }
            }
            agent_core::AgentMessage::Assistant(a) => {
                for block in &a.content {
                    match block {
                        agent_core::ContentBlock::Text { text } if !text.is_empty() => {
                            push(SessionUpdate::AgentMessageChunk {
                                content: TextContent::new(text.clone()),
                            });
                        }
                        agent_core::ContentBlock::Thinking { text, .. } if !text.is_empty() => {
                            push(SessionUpdate::AgentThoughtChunk {
                                content: TextContent::new(text.clone()),
                            });
                        }
                        agent_core::ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            push(SessionUpdate::ToolCall {
                                tool_call_id: id.clone(),
                                title: crate::adapter::tool_title(name, arguments),
                                kind: crate::adapter::tool_kind(name).to_string(),
                                status: "in_progress".to_string(),
                                raw_output: None,
                            });
                        }
                        _ => {}
                    }
                }
            }
            agent_core::AgentMessage::ToolResult(r) => {
                push(SessionUpdate::ToolCallUpdate {
                    tool_call_id: r.tool_call_id.clone(),
                    status: Some(
                        if matches!(r.result, agent_core::ToolResult::Error { .. }) {
                            "failed"
                        } else {
                            "completed"
                        }
                        .to_string(),
                    ),
                    raw_output: Some(r.result.to_llm_text()),
                });
            }
            _ => {}
        }
    }
    out
}

/// 会话树活跃路径消息（与 CLI RPC / server 同一算法：有活跃叶子则回溯到根）。
async fn active_path_messages(
    context: &dyn agent_core::ContextManager,
) -> Vec<agent_core::AgentMessage> {
    let nodes = context.snapshot_nodes().await;
    if nodes.is_empty() {
        return Vec::new();
    }
    let Some(leaf) = context.active_leaf().await else {
        return nodes.into_iter().map(|n| n.message).collect();
    };
    let by_id: std::collections::HashMap<&str, &agent_core::SessionNode> =
        nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut chain: Vec<agent_core::AgentMessage> = Vec::new();
    let mut cursor = Some(leaf);
    while let Some(id) = cursor {
        let Some(node) = by_id.get(id.as_str()) else {
            break;
        };
        chain.push(node.message.clone());
        cursor = node.parent_id.clone();
        if chain.len() > nodes.len() {
            break; // 防御持久化数据成环
        }
    }
    chain.reverse();
    chain
}

/// `session/fork`：把源会话复制为新会话（H12）。
async fn handle_session_fork(
    state: &SessionManager,
    req: &crate::types::JsonRpcRequest,
) -> Result<Value, RpcError> {
    let sid = param_str(req.params.as_ref(), "sessionId")
        .ok_or_else(|| rpc_error(INVALID_PARAMS, "缺少必填参数 sessionId"))?;
    let cwd = param_path(req.params.as_ref(), "cwd");
    let new_sid = state
        .create_session(None, None, Some(sid), None, cwd)
        .await
        .map_err(|e| rpc_error(INTERNAL_ERROR, e))?;
    Ok(json!({ "sessionId": new_sid }))
}

/// `session/close`：关闭并释放会话资源。
async fn handle_session_close(
    state: &SessionManager,
    req: &crate::types::JsonRpcRequest,
) -> Result<Value, RpcError> {
    let sid = param_str(req.params.as_ref(), "sessionId")
        .ok_or_else(|| rpc_error(INVALID_PARAMS, "缺少必填参数 sessionId"))?;
    state.close_session(sid).await;
    Ok(json!({}))
}

/// `session/set_mode`：切换模式（通过重建会话实现，与 Web `/switchMode` 一致）。
async fn handle_set_mode(
    state: &SessionManager,
    req: &crate::types::JsonRpcRequest,
) -> Result<Value, RpcError> {
    let sid = param_str(req.params.as_ref(), "sessionId")
        .or_else(|| param_str(req.params.as_ref(), "session_id"))
        .ok_or_else(|| rpc_error(INVALID_PARAMS, "缺少必填参数 sessionId"))?;
    let mode = param_str(req.params.as_ref(), "modeId")
        .or_else(|| param_str(req.params.as_ref(), "mode"))
        .ok_or_else(|| rpc_error(INVALID_PARAMS, "缺少必填参数 modeId"))?;
    // 重建会话须沿用客户端 cwd，否则会回落到服务端进程 cwd（如 /workspace），导致
    // 模式切换后丢失项目上下文、且会话 id 在错误目录下查不到历史。
    let cwd = param_path(req.params.as_ref(), "cwd");
    // 模式切换 = resume 当前会话 id + 新模式覆盖（create_session 内部处理重建）。
    let new_sid = state
        .create_session(None, Some(sid), None, Some(mode), cwd)
        .await
        .map_err(|e| rpc_error(INTERNAL_ERROR, e))?;
    Ok(json!({ "sessionId": new_sid }))
}

/// `session/available_commands`：返回可用命令清单（`{"commands":[...]}`）。
///
/// 清单来源为 [`SessionManager`] 的共享可写存储（装配层经
/// `set_available_commands` 注入）；未注入时返回空列表。与同族方法一致：
/// 要求 `sessionId` 且会话必须存在（未知会话按 Invalid params 拒绝）。
async fn handle_available_commands(
    state: &SessionManager,
    req: &crate::types::JsonRpcRequest,
) -> Result<Value, RpcError> {
    let sid = param_str(req.params.as_ref(), "sessionId")
        .or_else(|| param_str(req.params.as_ref(), "session_id")) // 兼容旧格式
        .ok_or_else(|| rpc_error(INVALID_PARAMS, "缺少必填参数 sessionId"))?;
    state
        .get(sid)
        .await
        .ok_or_else(|| rpc_error(INVALID_PARAMS, format!("会话不存在: {sid}")))?;
    let commands: Vec<CommandInfo> = state
        .available_commands()
        .await
        .into_iter()
        .map(|(name, description)| CommandInfo { name, description })
        .collect();
    Ok(json!({ "commands": commands }))
}

// ──────────────────────────────────────────────────────────────────────────────
// session/prompt 支持
// ──────────────────────────────────────────────────────────────────────────────

/// 投递 prompt 到指定会话，返回 broadcast receiver 供调用方消费事件。
///
/// 调用方负责消费 receiver 直到收到终止帧（`Done` / `Error`），然后返回
/// `PromptResponse { stopReason }`。
///
/// # Errors
/// 会话不存在或通道关闭时返回 [`RpcError`]。
pub async fn start_prompt(
    state: &SessionManager,
    session_id: &str,
    prompt_text: &str,
) -> Result<broadcast::Receiver<ServerFrame>, RpcError> {
    let session = state
        .get(session_id)
        .await
        .ok_or_else(|| rpc_error(INVALID_PARAMS, format!("会话不存在: {session_id}")))?;
    // 先订阅再投递，确保不遗漏首帧。
    let rx = session.broadcast.subscribe();
    session
        .inbound
        .send(ClientFrame::NewTask {
            text: prompt_text.to_string(),
            mode: None,
            content: None,
        })
        .map_err(|_| rpc_error(INTERNAL_ERROR, "会话驱动通道已关闭"))?;
    Ok(rx)
}

/// 从终止帧推断 ACP `stopReason`（无 assistant 消息上下文的简化入口）。
///
/// 完整映射见 [`stop_reason_with`]；ACP v1 合法取值为
/// `end_turn` / `max_tokens` / `max_turn_requests` / `refusal` / `cancelled`
/// ——**绝不再返回非法值**（此前恒 `end_turn`，HTTP 路径还会返回 `"timeout"`）。
#[must_use]
#[cfg_attr(not(test), allow(dead_code))]
pub fn stop_reason(frame: &ServerFrame) -> &'static str {
    stop_reason_with(frame, None, false)
}

/// 完整 `stopReason` 映射（H11，对齐 omp `acp-agent.ts:1637-1661`）：
///
/// | 输入 | ACP stopReason |
/// |---|---|
/// | 收到 `session/cancel` | `cancelled` |
/// | assistant `stop_reason = Aborted` | `cancelled` |
/// | assistant `stop_reason = Length` | `max_tokens` |
/// | assistant `stop_reason = Error` 且为 provider refusal/sensitive | `refusal` |
/// | 其它 | `end_turn` |
///
/// `max_turn_requests` 暂不可达：会话帧未携带「轮次预算耗尽」标记（见差距报告不确定项）。
#[must_use]
pub fn stop_reason_with(
    frame: &ServerFrame,
    last_assistant: Option<&agent_core::AssistantMessage>,
    cancelled: bool,
) -> &'static str {
    if cancelled {
        return "cancelled";
    }
    if let Some(msg) = last_assistant {
        match msg.stop_reason {
            Some(agent_core::StopReason::Aborted) => return "cancelled",
            Some(agent_core::StopReason::Length) => return "max_tokens",
            // provider refusal / sensitive：ACP 有对应取值，直接上报。
            Some(agent_core::StopReason::Error) if msg.is_provider_refusal() => return "refusal",
            _ => {}
        }
    }
    // 错误帧但没有 provider refusal 证据：ACP 无通用 error 取值，按正常结束上报，
    // 具体错误已通过 session/update 与日志外发。
    let _ = frame;
    "end_turn"
}

// ──────────────────────────────────────────────────────────────────────────────
// session/request_permission（服务端 → 客户端方向）
// ──────────────────────────────────────────────────────────────────────────────

/// 权限请求等待上限（秒）：超时按拒绝处理（fail closed）。
pub const PERMISSION_TIMEOUT_SECS: u64 = 600;

/// 服务端发起请求的 id 前缀（与客户端请求 id 空间隔离）。
const PERMISSION_ID_PREFIX: &str = "gyre-perm-";

/// 服务端发起请求的 id 计数器。
static PERMISSION_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 生成一个新的权限请求 id。
#[must_use]
pub fn next_permission_id() -> String {
    format!(
        "{PERMISSION_ID_PREFIX}{}",
        PERMISSION_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// 按 [`AskKind`] 选择 `session/request_permission` 的 options。
///
/// ACP v1 的 `PermissionOption` 是 `{optionId, name, kind}`（三字段全部必填，见
/// `protocol.ts:122-126`）；`optionId` 取值与 `kind` 同值，便于回执直接按 `optionId`
/// 判定（[`outcome_to_ask_response`] 即按此解析）。
///
/// 追问/完成结果在 ACP 中没有「自由文本回答」原语（`PermissionOption` 无文本载荷），
/// 因此这两种情形额外给一个 `optionId: "text"` 选项 + 请求里的 `prompt` 扩展字段，
/// 文本应答仍走 Gyre 的 `{type:"text",text}` 扩展 outcome（见 [`permission_request`] 注释）。
#[must_use]
pub fn permission_options(kind: &AskKind) -> Vec<Value> {
    match kind {
        AskKind::Tool { .. } | AskKind::Command { .. } => vec![
            json!({ "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" }),
            json!({ "optionId": "allow_always", "name": "Allow always", "kind": "allow_always" }),
            json!({ "optionId": "reject_once", "name": "Reject once", "kind": "reject_once" }),
            json!({ "optionId": "reject_always", "name": "Reject always", "kind": "reject_always" }),
        ],
        AskKind::Followup | AskKind::CompletionResult => vec![
            json!({ "optionId": "text", "name": "Reply with text", "kind": "allow_once" }),
            json!({ "optionId": "reject_once", "name": "Skip", "kind": "reject_once" }),
        ],
    }
}

/// [`AskKind`] → ACP `ToolKind`（v1：`read`/`edit`/`delete`/`move`/`search`/`execute`/`think`/`fetch`/`other`）。
fn acp_tool_kind(kind: &AskKind) -> &'static str {
    match kind {
        AskKind::Command { .. } => "execute",
        AskKind::Tool { .. } | AskKind::Followup | AskKind::CompletionResult => "other",
    }
}

/// 构造 `session/request_permission` JSON-RPC 请求（服务端 → 客户端方向）。
///
/// params 按 ACP v1（`protocol.ts:130-134`）：
/// ```json
/// { "sessionId": "…",
///   "toolCall": { "toolCallId": "…", "title": "…", "kind": "execute",
///                 "status": "pending", "rawInput": { … } },
///   "options": [ { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" }, … ] }
/// ```
/// **H10 修正**：此前发的是 `{sessionId, prompt:[…], options}`——缺 ACP 必需的 `toolCall`
/// 对象，严格客户端（Zed 等做 schema 校验）直接拒收，审批面不可用。
///
/// `prompt` 作为**附加扩展字段**保留：ACP 无自由文本回答原语，追问/完成结果需要它。
#[must_use]
pub fn permission_request(
    id: String,
    session_id: &str,
    ask: &AskMessage,
) -> crate::types::JsonRpcRequest {
    // toolCallId 用内部 AskMessage id：与回执配对稳定，且不额外占用 id 空间。
    let tool_call_id = ask.id.clone();
    let raw_input = match &ask.kind {
        AskKind::Tool { tool } => json!({ "tool": tool }),
        AskKind::Command { command } => json!({ "command": command }),
        AskKind::Followup | AskKind::CompletionResult => json!({}),
    };
    crate::types::JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(crate::types::JsonRpcId::Str(id)),
        method: "session/request_permission".into(),
        params: Some(json!({
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": tool_call_id,
                "title": ask.prompt,
                "kind": acp_tool_kind(&ask.kind),
                "status": "pending",
                "rawInput": raw_input,
            },
            "options": permission_options(&ask.kind),
            // 扩展（非 ACP 标准）：文本追问内容，供支持自由文本的客户端直接渲染。
            "prompt": [{ "type": "text", "text": ask.prompt }],
        })),
    }
}

/// 一次待回执的权限请求。
#[derive(Debug, Clone)]
pub struct PendingPermission {
    /// 所属会话 id。
    pub session_id: String,
    /// 内部 [`AskMessage`] 的 id（回执经 `ClientFrame::Respond` 投递）。
    pub ask_id: String,
    /// 回执截止时刻；过期按拒绝处理。
    pub deadline: tokio::time::Instant,
}

/// 权限请求登记表（rpc id → 待回执项）。
pub type PendingPermissions = HashMap<String, PendingPermission>;

/// 已解析的权限回执：投递目标 + 映射后的应答 + 可选说明文案。
#[derive(Debug, Clone)]
pub struct ResolvedPermission {
    /// 所属会话 id。
    pub session_id: String,
    /// 内部询问 id。
    pub ask_id: String,
    /// 映射后的应答。
    pub response: AskResponse,
    /// 拒绝原因说明（错误响应 / 无法解析 / 超时时附带）。
    pub note: Option<String>,
}

/// 登记一条新的待回执权限请求，返回其 rpc id。
pub fn register_permission(
    pending: &mut PendingPermissions,
    session_id: &str,
    ask: &AskMessage,
) -> String {
    let rpc_id = next_permission_id();
    pending.insert(
        rpc_id.clone(),
        PendingPermission {
            session_id: session_id.to_string(),
            ask_id: ask.id.clone(),
            deadline: tokio::time::Instant::now()
                + std::time::Duration::from_secs(PERMISSION_TIMEOUT_SECS),
        },
    );
    rpc_id
}

/// 解析客户端对权限请求的 JSON-RPC 响应，并从登记表移除对应项。
///
/// 返回 `None` 表示该响应不属于任何待回执请求（未知 / 重复 id）。
/// 错误响应与无法解析的 outcome 均按拒绝处理并附说明（fail closed）。
pub fn resolve_permission_response(
    pending: &mut PendingPermissions,
    response: &Value,
) -> Option<ResolvedPermission> {
    let rpc_id = id_to_string(response.get("id"))?;
    let entry = pending.remove(&rpc_id)?;
    // 客户端以 JSON-RPC 错误回应：视为拒绝。
    if let Some(err) = response.get("error").filter(|e| e.is_object()) {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Some(ResolvedPermission {
            session_id: entry.session_id,
            ask_id: entry.ask_id,
            response: AskResponse::No,
            note: Some(format!("客户端返回错误（{msg}），已按拒绝处理")),
        });
    }
    let (response, note) = match outcome_to_ask_response(response.get("result")) {
        Some(response) => (response, None),
        None => (
            AskResponse::No,
            Some("权限响应无法解析，已按拒绝处理（fail closed）".into()),
        ),
    };
    Some(ResolvedPermission {
        session_id: entry.session_id,
        ask_id: entry.ask_id,
        response,
        note,
    })
}

/// 客户端响应 outcome → [`AskResponse`]。
///
/// ACP v1 outcome 形态：`{type:"selected", optionId}` 或 `{type:"text", text}`；
/// 兼容 omp 变体（鉴别字段为 `outcome`）。`optionId` 与 option 的 `kind` 同值：
/// `allow_once`/`allow_always` → 批准，`reject_once`/`reject_always` → 拒绝，
/// `text` → 文本回答，`cancelled`（客户端关闭询问 UI）→ 拒绝。
/// 未知选项 / 缺字段返回 `None`，由调用方按拒绝处理。
#[must_use]
pub fn outcome_to_ask_response(result: Option<&Value>) -> Option<AskResponse> {
    let outcome = result?.get("outcome")?;
    let kind = outcome
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| outcome.get("outcome").and_then(Value::as_str))?;
    match kind {
        "selected" => match outcome.get("optionId").and_then(Value::as_str)? {
            "allow_once" | "allow_always" => Some(AskResponse::Yes),
            "reject_once" | "reject_always" => Some(AskResponse::No),
            _ => None,
        },
        "cancelled" => Some(AskResponse::No),
        "text" => outcome
            .get("text")
            .and_then(Value::as_str)
            .map(|text| AskResponse::Text(text.to_string())),
        _ => None,
    }
}

/// 权限等待超时的回退应答：拒绝 + 注明文案。
#[must_use]
pub fn permission_timeout_resolution(entry: PendingPermission) -> ResolvedPermission {
    ResolvedPermission {
        session_id: entry.session_id,
        ask_id: entry.ask_id,
        response: AskResponse::No,
        note: Some(format!(
            "权限请求等待超过 {PERMISSION_TIMEOUT_SECS} 秒未收到回执，已按拒绝处理"
        )),
    }
}

/// 取出全部已到期的待回执项（按 rpc id 排序，保证确定性）。
pub fn expired_permissions(pending: &mut PendingPermissions) -> Vec<PendingPermission> {
    let now = tokio::time::Instant::now();
    let mut expired_ids: Vec<String> = pending
        .iter()
        .filter(|(_, p)| p.deadline <= now)
        .map(|(id, _)| id.clone())
        .collect();
    expired_ids.sort();
    expired_ids
        .into_iter()
        .filter_map(|id| pending.remove(&id))
        .collect()
}

/// 判断 JSON 值是否为 JSON-RPC 响应（含 `id` 与 `result`/`error`、无 `method`）。
///
/// 用于在双向通道上区分客户端的「响应」与「请求/通知」。
#[must_use]
pub fn is_rpc_response(value: &Value) -> bool {
    value.as_object().is_some_and(|obj| {
        !obj.contains_key("method")
            && obj.contains_key("id")
            && (obj.contains_key("result") || obj.contains_key("error"))
    })
}

/// JSON-RPC id（数字或字符串）归一化为字符串形式。
#[must_use]
pub fn id_to_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{JsonRpcId, JsonRpcRequest};
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn initialize_returns_standard_acp_envelope() {
        // 构造 Zed 风格 initialize 请求（带 clientInfo + protocolVersion），覆盖诊断路径。
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(JsonRpcId::Str("test-init".into())),
            method: "initialize".into(),
            params: Some(json!({
                "clientInfo": { "name": "zed", "version": "1.10.2" },
                "protocolVersion": 1,
            })),
        };
        let result = handle_initialize(&req);
        // protocolVersion 必须是整数（非日期字符串）。
        assert_eq!(result["protocolVersion"], ACP_PROTOCOL_VERSION);
        assert!(result["protocolVersion"].is_number());
        // agentCapabilities（非 capabilities）。
        assert!(result["agentCapabilities"].is_object());
        // promptCapabilities 与 mcpCapabilities 为规范围必填（严格客户端 schema 校验）。
        assert!(result["agentCapabilities"]["promptCapabilities"].is_object());
        assert!(result["agentCapabilities"]["promptCapabilities"]["image"].is_boolean());
        assert!(result["agentCapabilities"]["promptCapabilities"]["embeddedContext"].is_boolean());
        assert!(result["agentCapabilities"]["mcpCapabilities"].is_object());
        assert!(result["agentCapabilities"]["mcpCapabilities"]["stdio"].is_boolean());
        assert!(result["agentCapabilities"]["mcpCapabilities"]["http"].is_boolean());
        // H12/H13：会话管理能力与 SSE MCP 传输能力均需声明。
        assert!(result["agentCapabilities"]["mcpCapabilities"]["sse"].is_boolean());
        for cap in ["list", "resume", "fork", "close"] {
            assert!(
                result["agentCapabilities"]["sessionCapabilities"][cap].is_object(),
                "sessionCapabilities.{cap} 应声明"
            );
        }
        // agentInfo 含 name + version。
        assert_eq!(result["agentInfo"]["name"], "agent-project");
        assert!(result["agentInfo"]["version"].is_string());
        // authMethods 是数组。
        assert!(result["authMethods"].is_array());
    }

    #[test]
    fn extract_prompt_text_from_content_blocks() {
        let params = json!({
            "sessionId": "s1",
            "prompt": [
                { "type": "text", "text": "hello" },
                { "type": "text", "text": "world" },
            ]
        });
        assert_eq!(extract_prompt_text(Some(&params)), "hello\nworld");
    }

    #[test]
    fn extract_prompt_text_empty_when_no_text_blocks() {
        let params = json!({ "prompt": [{ "type": "image", "data": "..." }] });
        assert_eq!(extract_prompt_text(Some(&params)), "");
    }

    #[test]
    fn extract_prompt_text_none_params() {
        assert_eq!(extract_prompt_text(None), "");
    }

    #[test]
    fn param_path_extracts_cwd() {
        // 标准 ACP NewSessionRequest 携带 cwd：编辑器实际打开的项目根。
        let params = json!({ "cwd": "/home/user/project" });
        assert_eq!(
            param_path(Some(&params), "cwd"),
            Some(std::path::Path::new("/home/user/project"))
        );
    }

    #[test]
    fn param_path_missing_returns_none() {
        // 缺失 cwd 字段时返回 None（调用方据此回退到服务端 cwd）。
        let params = json!({ "model": "default" });
        assert_eq!(param_path(Some(&params), "cwd"), None);
        assert_eq!(param_path(None, "cwd"), None);
    }

    #[test]
    fn param_path_non_string_returns_none() {
        // 非字符串的 cwd（协议误用）不得 panic，返回 None 安全回退。
        let params = json!({ "cwd": 42 });
        assert_eq!(param_path(Some(&params), "cwd"), None);
    }

    // ──────────────────────────────────────────────────────────────────────────
    // session/request_permission
    // ──────────────────────────────────────────────────────────────────────────

    /// 测试辅助：构造 AskMessage。
    fn ask(kind: AskKind) -> AskMessage {
        AskMessage {
            id: "ask-1".into(),
            kind,
            prompt: "允许执行 rm -rf /tmp/demo？".into(),
        }
    }

    /// H11 测试用：构造最小 assistant 消息（只关心 stop_reason / stop_details）。
    fn assistant(
        stop_reason: Option<agent_core::StopReason>,
        stop_details: Option<agent_core::StopDetails>,
    ) -> agent_core::AssistantMessage {
        agent_core::AssistantMessage {
            content: Vec::new(),
            usage: agent_core::Usage::default(),
            model: "test".into(),
            stop_reason,
            stop_details,
        }
    }

    #[test]
    fn permission_options_tool_and_command_offer_four_kinds() {
        for kind in [
            AskKind::Tool {
                tool: "bash".into(),
            },
            AskKind::Command {
                command: "cargo test".into(),
            },
        ] {
            let options = permission_options(&kind);
            let kinds: Vec<&str> = options
                .iter()
                .map(|o| o["kind"].as_str().unwrap_or_default())
                .collect();
            assert_eq!(
                kinds,
                ["allow_once", "allow_always", "reject_once", "reject_always"]
            );
            // H10：ACP v1 的 PermissionOption 三字段（optionId/name/kind）全部必填。
            for opt in &options {
                assert!(opt["optionId"].as_str().is_some_and(|s| !s.is_empty()));
                assert!(opt["name"].as_str().is_some_and(|s| !s.is_empty()));
                assert_eq!(opt["optionId"], opt["kind"], "optionId 与 kind 同值");
            }
        }
    }

    #[test]
    fn permission_options_followup_offer_text_and_skip() {
        for kind in [AskKind::Followup, AskKind::CompletionResult] {
            let options = permission_options(&kind);
            let ids: Vec<&str> = options
                .iter()
                .map(|o| o["optionId"].as_str().unwrap_or_default())
                .collect();
            assert_eq!(ids, ["text", "reject_once"], "{kind:?} 选项面");
            for opt in &options {
                assert!(matches!(
                    opt["kind"].as_str(),
                    Some("allow_once" | "reject_once")
                ));
            }
        }
    }

    #[test]
    fn permission_request_serializes_acp_v1_envelope() {
        let request = permission_request(
            next_permission_id(),
            "sess-9",
            &ask(AskKind::Tool {
                tool: "bash".into(),
            }),
        );
        let v: Value = serde_json::to_value(&request).unwrap_or_default();
        assert_eq!(v["method"], "session/request_permission");
        // id 为字符串形式（与客户端请求 id 空间隔离）。
        assert!(
            v["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("gyre-perm-"))
        );
        assert_eq!(v["params"]["sessionId"], "sess-9");
        // H10：ACP v1 必填 `toolCall`（ToolCallUpdate），严格客户端（Zed）依赖它做校验。
        assert_eq!(v["params"]["toolCall"]["status"], "pending");
        assert_eq!(v["params"]["toolCall"]["kind"], "other");
        assert_eq!(
            v["params"]["toolCall"]["title"],
            "允许执行 rm -rf /tmp/demo？"
        );
        assert!(
            v["params"]["toolCall"]["toolCallId"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "toolCallId 必填"
        );
        assert_eq!(v["params"]["toolCall"]["rawInput"]["tool"], "bash");
        assert_eq!(v["params"]["options"].as_array().map(Vec::len), Some(4));
        // `prompt` 为附加扩展（非 ACP 标准），保留供文本型客户端渲染。
        assert_eq!(v["params"]["prompt"][0]["type"], "text");
        assert_eq!(
            v["params"]["prompt"][0]["text"],
            "允许执行 rm -rf /tmp/demo？"
        );
    }

    #[test]
    fn permission_request_maps_command_kind_to_execute() {
        let request = permission_request(
            next_permission_id(),
            "s",
            &ask(AskKind::Command {
                command: "cargo test".into(),
            }),
        );
        let v: Value = serde_json::to_value(&request).unwrap_or_default();
        assert_eq!(v["params"]["toolCall"]["kind"], "execute");
        assert_eq!(v["params"]["toolCall"]["rawInput"]["command"], "cargo test");
    }

    /// H11：stopReason 必须是 ACP 合法枚举值，并按真实终止原因映射。
    #[test]
    fn stop_reason_maps_acp_vocabulary() {
        // 无 assistant 上下文 → end_turn（合法值）。
        let done = ServerFrame::Done {
            turns: 1,
            tool_calls: 0,
            success: false,
        };
        for frame in [
            done.clone(),
            ServerFrame::Error {
                message: "boom".into(),
            },
        ] {
            assert_eq!(stop_reason(&frame), "end_turn");
        }
        // 收到 cancel → cancelled。
        assert_eq!(stop_reason_with(&done, None, true), "cancelled");
        // assistant 终止原因映射。
        assert_eq!(
            stop_reason_with(
                &done,
                Some(&assistant(Some(agent_core::StopReason::Length), None)),
                false
            ),
            "max_tokens"
        );
        assert_eq!(
            stop_reason_with(
                &done,
                Some(&assistant(Some(agent_core::StopReason::Aborted), None)),
                false
            ),
            "cancelled"
        );
        assert_eq!(
            stop_reason_with(
                &done,
                Some(&assistant(
                    Some(agent_core::StopReason::Error),
                    Some(agent_core::StopDetails {
                        kind: "refusal".into(),
                    })
                )),
                false
            ),
            "refusal"
        );
        assert_eq!(
            stop_reason_with(
                &done,
                Some(&assistant(
                    Some(agent_core::StopReason::Error),
                    Some(agent_core::StopDetails {
                        kind: "server_error".into(),
                    })
                )),
                false
            ),
            "end_turn"
        );
        // 全量取值都在 ACP v1 词表内。
        for reason in ["end_turn", "max_tokens", "refusal", "cancelled"] {
            assert!(matches!(
                reason,
                "end_turn" | "max_tokens" | "max_turn_requests" | "refusal" | "cancelled"
            ));
        }
    }

    #[test]
    fn outcome_selected_maps_allow_to_yes() {
        for option in ["allow_once", "allow_always"] {
            let result = json!({ "outcome": { "type": "selected", "optionId": option } });
            assert!(
                matches!(
                    outcome_to_ask_response(Some(&result)),
                    Some(AskResponse::Yes)
                ),
                "{option} 应映射为 Yes"
            );
        }
    }

    #[test]
    fn outcome_selected_maps_reject_to_no() {
        for option in ["reject_once", "reject_always"] {
            let result = json!({ "outcome": { "type": "selected", "optionId": option } });
            assert!(
                matches!(
                    outcome_to_ask_response(Some(&result)),
                    Some(AskResponse::No)
                ),
                "{option} 应映射为 No"
            );
        }
    }

    #[test]
    fn outcome_text_maps_to_text_response() {
        let result = json!({ "outcome": { "type": "text", "text": "改用 cargo build" } });
        assert!(
            matches!(&outcome_to_ask_response(Some(&result)), Some(AskResponse::Text(t)) if t == "改用 cargo build")
        );
    }

    #[test]
    fn outcome_accepts_omp_variant_and_cancelled() {
        // omp 变体：鉴别字段为 outcome 而非 type。
        let omp = json!({ "outcome": { "outcome": "selected", "optionId": "allow_always" } });
        assert!(matches!(
            outcome_to_ask_response(Some(&omp)),
            Some(AskResponse::Yes)
        ));
        // cancelled（客户端关闭询问 UI）：语义为拒绝。
        let cancelled = json!({ "outcome": { "type": "cancelled" } });
        assert!(matches!(
            outcome_to_ask_response(Some(&cancelled)),
            Some(AskResponse::No)
        ));
    }

    #[test]
    fn outcome_unknown_or_malformed_fails_closed() {
        // 未知选项、缺失 outcome、空 result 均返回 None（调用方按拒绝处理）。
        let unknown = json!({ "outcome": { "type": "selected", "optionId": "maybe" } });
        assert!(outcome_to_ask_response(Some(&unknown)).is_none());
        let no_outcome = json!({});
        assert!(outcome_to_ask_response(Some(&no_outcome)).is_none());
        assert!(outcome_to_ask_response(None).is_none());
        let missing_option = json!({ "outcome": { "type": "selected" } });
        assert!(outcome_to_ask_response(Some(&missing_option)).is_none());
    }

    #[test]
    fn resolve_permission_response_delivers_registered_ask() {
        let mut pending = PendingPermissions::new();
        let rpc_id = register_permission(&mut pending, "sess-9", &ask(AskKind::Followup));

        let response = json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "result": { "outcome": { "type": "text", "text": "继续" } },
        });
        let resolved = resolve_permission_response(&mut pending, &response).expect("应解析成功");
        assert_eq!(resolved.session_id, "sess-9");
        assert_eq!(resolved.ask_id, "ask-1");
        assert!(
            matches!(&resolved.response, AskResponse::Text(t) if t == "继续"),
            "text outcome 应映射为文本回答"
        );
        assert!(resolved.note.is_none());
        // 已回执的 id 再次响应：返回 None（不重复投递）。
        assert!(resolve_permission_response(&mut pending, &response).is_none());
        assert!(pending.is_empty());
    }

    #[test]
    fn resolve_permission_response_error_rejects_with_note() {
        let mut pending = PendingPermissions::new();
        let rpc_id = register_permission(
            &mut pending,
            "s",
            &ask(AskKind::Tool {
                tool: "bash".into(),
            }),
        );
        let response = json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "error": { "code": -32603, "message": "internal" },
        });
        let resolved = resolve_permission_response(&mut pending, &response).expect("应解析成功");
        assert!(matches!(resolved.response, AskResponse::No));
        let note = resolved.note.expect("错误响应应附说明");
        assert!(note.contains("internal"), "说明应包含错误信息: {note}");
    }

    #[test]
    fn resolve_permission_response_unknown_id_is_ignored() {
        let mut pending = PendingPermissions::new();
        let response = json!({ "jsonrpc": "2.0", "id": "gyre-perm-999", "result": {} });
        assert!(resolve_permission_response(&mut pending, &response).is_none());
    }

    #[test]
    fn permission_timeout_maps_to_rejected_with_note() {
        // 登记后人为把截止时刻改为过去：到期项被取出并映射为拒绝 + 注明。
        let mut pending = PendingPermissions::new();
        register_permission(
            &mut pending,
            "sess-9",
            &ask(AskKind::Command {
                command: "make".into(),
            }),
        );
        let id = pending.keys().next().cloned().unwrap_or_default();
        if let Some(entry) = pending.get_mut(&id) {
            entry.deadline = tokio::time::Instant::now() - std::time::Duration::from_secs(1);
        }

        let expired = expired_permissions(&mut pending);
        assert_eq!(expired.len(), 1, "到期项应被取出");
        assert!(pending.is_empty(), "取出后登记表应清空");

        let resolved = permission_timeout_resolution(
            expired
                .into_iter()
                .next()
                .unwrap_or_else(|| panic!("expired_permissions 应返回到期项")),
        );
        assert!(
            matches!(resolved.response, AskResponse::No),
            "超时应映射为拒绝"
        );
        assert_eq!(resolved.ask_id, "ask-1");
        let note = resolved.note.expect("超时应附说明");
        assert!(
            note.contains(&PERMISSION_TIMEOUT_SECS.to_string()),
            "说明应含超时秒数: {note}"
        );
        assert!(note.contains("拒绝"), "说明应注明按拒绝处理: {note}");
    }

    #[test]
    fn expired_permissions_skips_unexpired_entries() {
        let mut pending = PendingPermissions::new();
        register_permission(&mut pending, "s", &ask(AskKind::Followup));
        assert!(
            expired_permissions(&mut pending).is_empty(),
            "未到期项不应被取出"
        );
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn is_rpc_response_distinguishes_response_from_request() {
        let response =
            json!({ "jsonrpc": "2.0", "id": "gyre-perm-1", "result": { "outcome": {} } });
        assert!(is_rpc_response(&response));
        let error_response =
            json!({ "jsonrpc": "2.0", "id": 7, "error": { "code": -1, "message": "x" } });
        assert!(is_rpc_response(&error_response));
        let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "session/cancel" });
        assert!(!is_rpc_response(&request));
        let notification = json!({ "jsonrpc": "2.0", "method": "session/cancel" });
        assert!(!is_rpc_response(&notification));
    }

    #[test]
    fn id_to_string_normalizes_numbers_and_strings() {
        assert_eq!(id_to_string(Some(&json!(42))), Some("42".into()));
        assert_eq!(id_to_string(Some(&json!("abc"))), Some("abc".into()));
        assert_eq!(id_to_string(None), None);
        assert_eq!(id_to_string(Some(&json!(null))), None);
    }

    // ──────────────────────────────────────────────────────────────────────────
    // session/available_commands
    // ──────────────────────────────────────────────────────────────────────────

    /// 测试辅助：以示例配置构造 SessionManager（cwd 指定，便于临时目录隔离）。
    fn test_manager(cwd: &std::path::Path) -> SessionManager {
        let cfg: agent_config::Config =
            toml::from_str(include_str!("../../../config.example.toml"))
                .expect("示例配置应可解析为 Config");
        SessionManager::new(Arc::new(cfg), reqwest::Client::new(), Arc::from(cwd), None)
    }

    /// 测试辅助：构造管理器 + 一个活跃会话（`create_session` 仅本地装配，无网络请求）。
    async fn manager_with_session(tag: &str) -> (SessionManager, String) {
        let cwd = std::env::temp_dir().join(format!("gyre-acp-cmds-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).expect("临时 cwd 应可创建");
        let mgr = test_manager(&cwd);
        let sid = mgr
            .create_session(None, None, None, None, None)
            .await
            .expect("测试会话应可创建");
        (mgr, sid)
    }

    /// 测试辅助：构造 `session/available_commands` 请求。
    fn available_commands_request(session_id: Option<&str>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(JsonRpcId::Str("cmd-1".into())),
            method: "session/available_commands".into(),
            params: session_id.map(|sid| json!({ "sessionId": sid })),
        }
    }

    // ── H37：ACP 协议一致性（dispatch 错误码 / 能力声明 ↔ 实现一致 / 旧方法别名）──

    /// 测试辅助：构造任意方法的请求。
    fn rpc_req(method: &str, params: Option<Value>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(JsonRpcId::Str("conf-1".into())),
            method: method.into(),
            params,
        }
    }

    /// 未知方法必须是 JSON-RPC `method not found`（-32601）且回显方法名。
    #[tokio::test]
    async fn unknown_method_maps_to_method_not_found() {
        let (mgr, _sid) = manager_with_session("unknown-method").await;
        let err = dispatch_rpc(&mgr, &rpc_req("session/does_not_exist", None))
            .await
            .unwrap_err();
        assert_eq!(err.code, METHOD_NOT_FOUND);
        assert!(err.message.contains("session/does_not_exist"), "{err:?}");
    }

    /// 能力声明 ↔ 实现一致：`initialize` 声明的每个会话能力都必须在 dispatch 中可达
    /// （缺失参数应得 `-32602` 而非 `-32601`），防「声明了但没实现」的协议漂移。
    #[tokio::test]
    async fn advertised_capabilities_are_dispatchable() {
        let (mgr, _sid) = manager_with_session("caps-dispatchable").await;
        let caps = handle_initialize(&rpc_req("initialize", None));
        // loadSession 布尔对应 session/load；sessionCapabilities 的键对应 session/<key>。
        assert_eq!(caps["agentCapabilities"]["loadSession"], true);
        for cap in ["list", "resume", "fork", "close"] {
            assert!(
                caps["agentCapabilities"]["sessionCapabilities"][cap].is_object(),
                "sessionCapabilities.{cap} 应声明"
            );
        }
        let methods = [
            "session/load",
            "session/list",
            "session/resume",
            "session/fork",
            "session/close",
        ];
        for m in methods {
            match dispatch_rpc(&mgr, &rpc_req(m, Some(json!({})))).await {
                Ok(_) => {}
                Err(e) => assert_ne!(
                    e.code, METHOD_NOT_FOUND,
                    "声明的能力 {m} 必须可 dispatch（得到 method not found）"
                ),
            }
        }
    }

    /// `authenticate`/`logout` 在 `authMethods: []` 下必须快速失败（内部错误），
    /// 而不是谎报成功让客户端把下游失败误归因于会话。
    #[tokio::test]
    async fn authenticate_fails_fast_while_auth_methods_empty() {
        let (mgr, _sid) = manager_with_session("auth-failfast").await;
        let caps = handle_initialize(&rpc_req("initialize", None));
        assert_eq!(
            caps["authMethods"].as_array().map(Vec::len),
            Some(0),
            "不得声明认证方法"
        );
        for m in ["authenticate", "logout"] {
            let err = dispatch_rpc(&mgr, &rpc_req(m, Some(json!({"methodId": "x"}))))
                .await
                .unwrap_err();
            assert_eq!(err.code, INTERNAL_ERROR, "{m} 应内部错误: {err:?}");
        }
    }

    /// 方向误用：`session/request_permission` 是服务端→客户端请求，客户端调用得
    /// `invalid request`（-32600），而不是 `method not found`（方法本身有效）。
    #[tokio::test]
    async fn client_initiated_permission_request_is_invalid_request() {
        let (mgr, _sid) = manager_with_session("perm-direction").await;
        let err = dispatch_rpc(
            &mgr,
            &rpc_req("session/request_permission", Some(json!({}))),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, INVALID_REQUEST);
    }

    /// 旧自定义方法名（`newTask` / `cancel`）仍路由到标准会话处理器（向后兼容契约）。
    #[tokio::test]
    async fn legacy_method_aliases_route_to_session_handlers() {
        let (mgr, _sid) = manager_with_session("legacy-alias").await;
        // newTask ≡ session/new：返回 sessionId。
        let out = dispatch_rpc(&mgr, &rpc_req("newTask", Some(json!({}))))
            .await
            .unwrap();
        assert!(
            out.get("sessionId").and_then(Value::as_str).is_some(),
            "newTask 应返回 sessionId: {out}"
        );
        // cancel ≡ session/cancel：缺 sessionId 时是参数错误（而非方法未找到）。
        let err = dispatch_rpc(&mgr, &rpc_req("cancel", Some(json!({}))))
            .await
            .unwrap_err();
        assert_eq!(err.code, INVALID_PARAMS, "{err:?}");
    }

    // ── H12 / H13：会话列表、恢复、fork 与 bootstrap/回放通知 ──────────────────

    #[test]
    fn unix_to_iso8601_matches_known_instants() {
        assert_eq!(unix_to_iso8601(0), "1970-01-01T00:00:00Z");
        // 2024-01-01T00:00:00Z = 1704067200
        assert_eq!(unix_to_iso8601(1_704_067_200), "2024-01-01T00:00:00Z");
        // 含时分秒与闰年：2024-02-29T12:34:56Z = 1709210096
        assert_eq!(unix_to_iso8601(1_709_210_096), "2024-02-29T12:34:56Z");
    }

    #[tokio::test]
    async fn session_new_pushes_bootstrap_notifications() {
        let (mgr, sid) = manager_with_session("bootstrap").await;
        mgr.set_available_commands(vec![("/review".into(), "代码评审".into())])
            .await;
        // 模拟分发结果（session/new 的返回形状）。
        let notes =
            post_dispatch_notifications(&mgr, "session/new", &json!({"sessionId": sid})).await;
        assert!(!notes.is_empty(), "session/new 应有 bootstrap 通知");
        let first = serde_json::to_value(&notes[0]).unwrap();
        assert_eq!(first["method"], "session/update");
        assert_eq!(
            first["params"]["update"]["sessionUpdate"],
            "available_commands_update"
        );
        // H13：命令名不带前导 `/`（ACP/omp 约定，客户端自行补）。
        assert_eq!(
            first["params"]["update"]["availableCommands"][0]["name"],
            "review"
        );
    }

    #[tokio::test]
    async fn session_load_replays_history_before_bootstrap() {
        let (mgr, sid) = manager_with_session("replay").await;
        // 造历史：用户消息 + 助手（文本 + 工具调用）+ 工具结果。
        let session = mgr.get(&sid).await.expect("会话存在");
        session
            .context
            .append(agent_core::AgentMessage::user_text("你好"))
            .await;
        session
            .context
            .append(agent_core::AgentMessage::Assistant(
                agent_core::AssistantMessage {
                    content: vec![
                        agent_core::ContentBlock::Text {
                            text: "我来看看".into(),
                        },
                        agent_core::ContentBlock::ToolCall {
                            id: "call_1".into(),
                            name: "read_file".into(),
                            arguments: json!({"path": "src/main.rs"}),
                            signature: None,
                        },
                    ],
                    usage: agent_core::Usage::default(),
                    model: "m".into(),
                    stop_reason: None,
                    stop_details: None,
                },
            ))
            .await;
        session
            .context
            .append(agent_core::AgentMessage::ToolResult(
                agent_core::ToolResultMessage {
                    tool_call_id: "call_1".into(),
                    result: agent_core::ToolResult::Text("file body".into()),
                },
            ))
            .await;

        let notes =
            post_dispatch_notifications(&mgr, "session/load", &json!({"sessionId": sid})).await;
        let kinds: Vec<String> = notes
            .iter()
            .map(|n| {
                serde_json::to_value(n).unwrap()["params"]["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        // 回放顺序：用户 → 助手文本 → 工具调用 → 工具结果；随后 bootstrap
        // （available_commands_update + session_info_update）。
        assert_eq!(
            kinds,
            vec![
                "user_message_chunk",
                "agent_message_chunk",
                "tool_call",
                "tool_call_update",
                "available_commands_update",
                "session_info_update",
            ],
            "{kinds:?}"
        );
        let replay = serde_json::to_value(&notes[2]).unwrap();
        assert_eq!(replay["params"]["update"]["toolCallId"], "call_1");
        assert_eq!(replay["params"]["update"]["kind"], "read");
        let result = serde_json::to_value(&notes[3]).unwrap();
        assert_eq!(result["params"]["update"]["status"], "completed");
        assert_eq!(result["params"]["update"]["rawOutput"], "file body");
    }

    #[tokio::test]
    async fn session_list_pages_and_resume_skips_replay() {
        let cwd = std::env::temp_dir().join(format!("gyre-acp-list-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let mgr = test_manager(&cwd);
        let sid = mgr
            .create_session(None, None, None, None, Some(&cwd))
            .await
            .unwrap();
        // 落盘会话文件（会话创建即写 header）。
        let store = agent_context::SessionStore::for_cwd(&cwd);
        store.set_title(&sid, "标题甲").unwrap();

        let req = |params: Value| JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(JsonRpcId::Str("l".into())),
            method: "session/list".into(),
            params: Some(params),
        };
        let listed = handle_session_list(&mgr, &req(json!({"cwd": cwd.display().to_string()})))
            .await
            .unwrap();
        let sessions = listed["sessions"].as_array().unwrap();
        assert!(
            sessions.iter().any(|s| s["sessionId"] == sid.as_str()),
            "列表应含刚创建的会话: {listed}"
        );
        let row = sessions
            .iter()
            .find(|s| s["sessionId"] == sid.as_str())
            .unwrap();
        assert_eq!(row["title"], "标题甲");
        assert_eq!(row["cwd"], cwd.display().to_string());
        assert!(row["updatedAt"].as_str().unwrap().contains('T'));

        // cursor 越界 → 空页且无 nextCursor。
        let listed = handle_session_list(
            &mgr,
            &req(json!({"cwd": cwd.display().to_string(), "cursor": "999"})),
        )
        .await
        .unwrap();
        assert!(listed["sessions"].as_array().unwrap().is_empty());
        assert!(listed.get("nextCursor").is_none());
        // 非法 cursor → Invalid params。
        let err = handle_session_list(
            &mgr,
            &req(json!({"cwd": cwd.display().to_string(), "cursor": "abc"})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, INVALID_PARAMS);

        // resume：返回同一 sessionId，**不**回放历史（通知面为空）。
        let resume = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(JsonRpcId::Str("r".into())),
            method: "session/resume".into(),
            params: Some(json!({"sessionId": sid, "cwd": cwd.display().to_string()})),
        };
        let resolved = handle_session_resume(&mgr, &resume).await.unwrap();
        assert_eq!(resolved["sessionId"], sid.as_str());
        let notes = post_dispatch_notifications(&mgr, "session/resume", &resolved).await;
        assert!(
            notes.iter().all(|n| {
                serde_json::to_value(n).unwrap()["params"]["update"]["sessionUpdate"]
                    != "user_message_chunk"
            }),
            "resume 不得回放历史"
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn session_fork_creates_distinct_session() {
        let cwd = std::env::temp_dir().join(format!("gyre-acp-fork-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let mgr = test_manager(&cwd);
        let sid = mgr
            .create_session(None, None, None, None, Some(&cwd))
            .await
            .unwrap();
        let fork = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(JsonRpcId::Str("f".into())),
            method: "session/fork".into(),
            params: Some(json!({"sessionId": sid, "cwd": cwd.display().to_string()})),
        };
        let forked = handle_session_fork(&mgr, &fork).await.unwrap();
        let new_sid = forked["sessionId"].as_str().unwrap();
        assert_ne!(new_sid, sid, "fork 必须产生新 id");
        assert!(
            agent_context::SessionStore::for_cwd(&cwd)
                .path_for(new_sid)
                .exists()
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }
    #[tokio::test]
    async fn available_commands_returns_injected_list() {
        let (mgr, sid) = manager_with_session("injected").await;
        mgr.set_available_commands(vec![
            ("review".into(), "代码评审".into()),
            ("compact".into(), "压缩会话上下文".into()),
        ])
        .await;

        let result = dispatch_rpc(&mgr, &available_commands_request(Some(&sid)))
            .await
            .expect("已注入时应成功");
        // 顶层仅 commands 键；条目为 name/description 形状（camelCase，单词形锁定键名）。
        assert_eq!(result.as_object().map(|o| o.len()), Some(1));
        let commands = result["commands"].as_array().expect("commands 应为数组");
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0]["name"], "review");
        assert_eq!(commands[0]["description"], "代码评审");
        assert_eq!(commands[1]["name"], "compact");
        assert_eq!(commands[1]["description"], "压缩会话上下文");
    }

    #[tokio::test]
    async fn available_commands_empty_when_not_injected() {
        let (mgr, sid) = manager_with_session("empty").await;
        let result = dispatch_rpc(&mgr, &available_commands_request(Some(&sid)))
            .await
            .expect("未注入也应成功（空列表）");
        assert_eq!(result["commands"], json!([]));
    }

    #[tokio::test]
    async fn available_commands_unknown_session_is_invalid_params() {
        let mgr = test_manager(std::path::Path::new("."));
        let err = dispatch_rpc(&mgr, &available_commands_request(Some("no-such-session")))
            .await
            .expect_err("未知会话应报错");
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(err.message.contains("会话不存在"), "{err:?}");
    }

    #[tokio::test]
    async fn available_commands_missing_session_id_is_invalid_params() {
        let mgr = test_manager(std::path::Path::new("."));
        let err = dispatch_rpc(&mgr, &available_commands_request(None))
            .await
            .expect_err("缺少 sessionId 应报错");
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(err.message.contains("sessionId"), "{err:?}");
    }
}
