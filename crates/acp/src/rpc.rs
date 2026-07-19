//! ACP JSON-RPC 方法分发。
//!
//! 遵循 Agent Client Protocol v1（agentclientprotocol.com）标准方法名：
//! - `initialize`（请求）— 握手 + 能力协商
//! - `session/new`（请求）— 创建会话
//! - `session/prompt`（请求）— 投递用户消息（阻塞到 turn 完成返回 stopReason）
//! - `session/cancel`（通知）— 取消当前 turn
//! - `session/load`（请求）— 恢复历史会话
//! - `session/close`（请求）— 关闭会话
//!
//! `session/prompt` 需在 turn 期间持续推送 `session/update` 通知，因此由各传输层
//! 自行调用 [`start_prompt`] + 消费 broadcast 实现（HTTP 经 SSE，stdio 经主循环 select）。

use agent_server::{ClientFrame, ServerFrame, SessionManager};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::types::RpcError;

/// ACP 协议版本（客户端 `initialize` 时协商）。
///
/// ACP v1 线协议中 `protocolVersion` 为整数（非日期字符串）。
pub const ACP_PROTOCOL_VERSION: u16 = 1;

// JSON-RPC 标准错误码。
const PARSE_ERROR: i32 = -32700;
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
        "authenticate" => Ok(json!({})),
        "logout" => Ok(json!({})),
        // session/prompt 需传输层特殊处理（阻塞推送通知）。
        "session/prompt" => Err(rpc_error(
            INTERNAL_ERROR,
            "session/prompt 须由传输层处理（stdio 主循环 / HTTP SSE）",
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
    RpcError { code, message: message.into(), data: None }
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
/// - `mcp.http: false` —— 未实现 HTTP 传输的 MCP server
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
                "http": false,
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
async fn handle_session_new(state: &SessionManager, req: &crate::types::JsonRpcRequest) -> Result<Value, RpcError> {
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
async fn handle_session_cancel(state: &SessionManager, req: &crate::types::JsonRpcRequest) -> Result<Value, RpcError> {
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
async fn handle_session_load(state: &SessionManager, req: &crate::types::JsonRpcRequest) -> Result<Value, RpcError> {
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
async fn handle_session_close(state: &SessionManager, req: &crate::types::JsonRpcRequest) -> Result<Value, RpcError> {
    let sid = param_str(req.params.as_ref(), "sessionId")
        .ok_or_else(|| rpc_error(INVALID_PARAMS, "缺少必填参数 sessionId"))?;
    state.close_session(sid).await;
    Ok(json!({}))
}

/// `session/set_mode`：切换模式（通过重建会话实现，与 Web `/switchMode` 一致）。
async fn handle_set_mode(state: &SessionManager, req: &crate::types::JsonRpcRequest) -> Result<Value, RpcError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{JsonRpcId, JsonRpcRequest};
    use serde_json::json;

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
        assert!(
            result["agentCapabilities"]["promptCapabilities"]["embeddedContext"].is_boolean()
        );
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
}
