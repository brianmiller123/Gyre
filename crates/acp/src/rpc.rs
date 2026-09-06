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

use crate::types::{CommandInfo, RpcError};

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
        "session/close" => handle_session_close(state, req).await,
        "session/set_mode" => handle_set_mode(state, req).await,
        "session/available_commands" => handle_available_commands(state, req).await,
        "authenticate" => Ok(json!({})),
        "logout" => Ok(json!({})),
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

/// 从终止帧推断 ACP `stopReason`。
#[must_use]
pub fn stop_reason(frame: &ServerFrame) -> &'static str {
    match frame {
        ServerFrame::Done { success: false, .. } => "end_turn",
        ServerFrame::Error { .. } => "end_turn",
        _ => "end_turn",
    }
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
/// 工具/命令审批提供 ACP v1 四种许可选项；追问（`Followup`）与完成结果
/// （`CompletionResult`）只需自由文本回答（`{kind:"text"}`）。
#[must_use]
pub fn permission_options(kind: &AskKind) -> Vec<Value> {
    match kind {
        AskKind::Tool { .. } | AskKind::Command { .. } => vec![
            json!({ "kind": "allow_once" }),
            json!({ "kind": "allow_always" }),
            json!({ "kind": "reject_once" }),
            json!({ "kind": "reject_always" }),
        ],
        AskKind::Followup | AskKind::CompletionResult => vec![json!({ "kind": "text" })],
    }
}

/// 构造 `session/request_permission` JSON-RPC 请求（服务端 → 客户端方向）。
///
/// params 按 ACP v1：`sessionId` + `prompt`（文本内容块）+ `options`。
#[must_use]
pub fn permission_request(
    id: String,
    session_id: &str,
    ask: &AskMessage,
) -> crate::types::JsonRpcRequest {
    crate::types::JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(crate::types::JsonRpcId::Str(id)),
        method: "session/request_permission".into(),
        params: Some(json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": ask.prompt }],
            "options": permission_options(&ask.kind),
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
        }
    }

    #[test]
    fn permission_options_followup_and_completion_are_text_only() {
        for kind in [AskKind::Followup, AskKind::CompletionResult] {
            let options = permission_options(&kind);
            assert_eq!(options.len(), 1, "{kind:?} 应只有一个 text 选项");
            assert_eq!(options[0]["kind"], "text");
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
        assert_eq!(v["params"]["prompt"][0]["type"], "text");
        assert_eq!(
            v["params"]["prompt"][0]["text"],
            "允许执行 rm -rf /tmp/demo？"
        );
        assert_eq!(v["params"]["options"].as_array().map(Vec::len), Some(4));
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
