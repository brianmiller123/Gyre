//! 127.0.0.1 环回桥：Python 内核经 `tool.<name>(**kwargs)` 回调宿主工具。
//!
//! 桥仅监听 127.0.0.1 随机端口；POST /call 请求体 `{"token":"…","tool":"…","args":{…}}`，
//! token 无效返回 HTTP 401；有效则在 run 的 `cancel.child_token()` 下构造
//! [`ToolContext`]（workspace/approval/cancel 必填，其余 None）执行工具，返回
//! `{"ok":true,"output":"…"}` 或 `{"ok":false,"error":…}`。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use agent_core::{ApprovalPolicy, ToolResult, Workspace};
use agent_tools::{ToolContext, ToolRegistry};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::EvalError;

/// 一个已注册的 run：持取消令牌（桥侧工具执行在其 child token 下进行）。
#[derive(Clone)]
struct RunEntry {
    cancel: CancellationToken,
}

/// 桥共享状态（axum handler 与 [`BridgeServer`] 共用）。
struct BridgeState {
    registry: Arc<dyn ToolRegistry>,
    workspace: Arc<Workspace>,
    approval: Arc<dyn ApprovalPolicy>,
    runs: Mutex<HashMap<String, RunEntry>>,
}

/// 环回桥：仅监听 127.0.0.1，随机端口，serve 任务随进程存活。
pub struct BridgeServer {
    addr: SocketAddr,
    state: Arc<BridgeState>,
}

/// run 注册守卫：drop 时注销 token（此后该 token 的 /call 一律 401）。
pub struct RunGuard {
    state: Arc<BridgeState>,
    token: String,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.state
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.token);
    }
}

/// POST /call 请求体。
#[derive(Debug, Deserialize)]
struct CallRequest {
    /// run bearer token。
    token: String,
    /// 工具名。
    tool: String,
    /// 工具参数（缺省为空对象）。
    #[serde(default)]
    args: Value,
}

/// 构造环回桥 URL（内核侧 `tool.*` 回调目标）。
fn bridge_url(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

impl BridgeServer {
    /// 启动环回桥：绑定 127.0.0.1:0 取随机端口，serve 任务后台运行。
    ///
    /// # Errors
    /// 绑定或取地址失败时返回 [`EvalError::BridgeError`]。
    pub async fn spawn(
        registry: Arc<dyn ToolRegistry>,
        workspace: Arc<Workspace>,
        approval: Arc<dyn ApprovalPolicy>,
    ) -> Result<Arc<Self>, EvalError> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| EvalError::BridgeError(format!("绑定 127.0.0.1:0 失败：{e}")))?;
        let addr = listener
            .local_addr()
            .map_err(|e| EvalError::BridgeError(format!("获取监听地址失败：{e}")))?;
        let state = Arc::new(BridgeState {
            registry,
            workspace,
            approval,
            runs: Mutex::new(HashMap::new()),
        });
        let app = Router::new()
            .route("/call", post(call_handler))
            .with_state(Arc::clone(&state));
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::warn!("eval 环回桥退出：{e}");
            }
        });
        Ok(Arc::new(Self { addr, state }))
    }

    /// 监听地址（127.0.0.1 + 随机端口）。
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 注册一个 run token；返回的 [`RunGuard`] drop 时注销。
    pub async fn register_run(
        &self,
        token: String,
        cancel: CancellationToken,
    ) -> RunGuard {
        self.state
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token.clone(), RunEntry { cancel });
        RunGuard {
            state: Arc::clone(&self.state),
            token,
        }
    }
}

/// POST /call 处理：token 校验 → 查工具 → 在 run 的 child cancel 下执行。
async fn call_handler(
    State(state): State<Arc<BridgeState>>,
    Json(req): Json<CallRequest>,
) -> (StatusCode, Json<Value>) {
    let run = state
        .runs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&req.token)
        .cloned();
    let Some(run) = run else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok": false, "error": "invalid token：eval run 已结束或 token 无效"})),
        );
    };
    let Some(tool) = state.registry.get(&req.tool) else {
        return (
            StatusCode::OK,
            Json(json!({"ok": false, "error": json!({"message": format!("未知工具：{}", req.tool)})})),
        );
    };
    let child = run.cancel.child_token();
    let ctx = ToolContext {
        workspace: &state.workspace,
        approval: &*state.approval,
        cancel: &child,
        skills: None,
        memory: None,
        resources: None,
        write_effect: None,
        update_tx: None,
        conflicts: None,
        pending_rewrites: None,
    };
    match tool.execute(req.args, &ctx).await {
        Ok(ToolResult::Text(text)) => {
            (StatusCode::OK, Json(json!({"ok": true, "output": text})))
        }
        Ok(ToolResult::Image { mime, .. }) => (
            StatusCode::OK,
            Json(json!({"ok": true, "output": format!("[image/{mime}]")})),
        ),
        Ok(ToolResult::Error {
            message,
            recoverable,
        }) => (
            StatusCode::OK,
            Json(json!({"ok": false, "error": json!({"message": message, "recoverable": recoverable})})),
        ),
        Err(e) => (StatusCode::OK, Json(json!({"ok": false, "error": e.to_string()}))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_tools::DefaultToolRegistry;
    use crate::test_support::NoopApproval;

    /// 环回桥 URL 拼接。
    #[test]
    fn proxy_url_format() {
        let addr: SocketAddr = "127.0.0.1:45678".parse().expect("地址解析失败");
        assert_eq!(bridge_url(addr), "http://127.0.0.1:45678");
    }

    /// RunGuard drop 注销 token；期间 token 有效。
    #[tokio::test]
    async fn run_guard_registers_and_deregisters_token() {
        let bridge = BridgeServer::spawn(
            Arc::new(DefaultToolRegistry::new()),
            Arc::new(Workspace::current_dir()),
            Arc::new(NoopApproval),
        )
        .await
        .expect("桥启动失败");
        let token = "unit-token".to_string();
        let guard = bridge.register_run(token.clone(), CancellationToken::new()).await;
        assert!(
            bridge
                .state
                .runs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&token)
        );
        drop(guard);
        assert!(
            !bridge
                .state
                .runs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&token)
        );
    }
}
