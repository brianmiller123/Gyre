//! MCP stdio 传输：JSON-RPC 2.0 over 子进程 stdin/stdout（行分隔）。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_config::McpStdioConfig;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, oneshot};

use crate::client::{
    McpError, McpTransport, NotificationHandler, ServerRequestHandler, resolve_timeout,
    server_request_error, server_request_result,
};

/// MCP stdout 单行最大字节数：超过即丢弃缓冲（防无换行的超长行 OOM）。
const MAX_MCP_LINE_BYTES: usize = 4 * 1024 * 1024;

/// MCP stdio 传输：持有子进程，后台读 task 按 id 分发响应、把 server→client
pub(crate) struct StdioTransport {
    /// server→client 通知处理器槽（读 task 与 `set_on_notification` 共享）。
    notifications: Arc<parking_lot::Mutex<Option<NotificationHandler>>>,
    /// server→client **请求**处理器槽（H16；有 `method` 且有 `id` 的帧必须应答）。
    server_requests: Arc<parking_lot::Mutex<Option<ServerRequestHandler>>>,
    /// 异常断连处理器槽（读 task 退出时触发；主动 `close` 前由 [`McpClient`] 摘除）。
    on_close: Arc<parking_lot::Mutex<Option<crate::client::CloseHandler>>>,
    /// 写端共享句柄：请求路径与读 task（应答 server→client 请求）共用，避免交错误帧。
    write: Arc<Mutex<ChildStdin>>,
    child: Mutex<Child>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    /// 单请求超时（`None` = 不限制）。
    timeout: Option<Duration>,
}

impl StdioTransport {
    /// 启动 MCP server 子进程并建立传输。
    ///
    /// # Errors
    /// 启动失败或 stdin/stdout 不可用时返回 [`McpError`]。
    pub async fn spawn(cfg: &McpStdioConfig) -> Result<Self, McpError> {
        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or(McpError::Closed)?;
        let stdout = child.stdout.take().ok_or(McpError::Closed)?;
        // 后台排空 MCP server 的 stderr（不再静默丢弃），便于排查子进程报错。
        if let Some(stderr) = child.stderr.take() {
            let name = cfg.command.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!(target: "mcp::stderr", server = %name, "{line}");
                }
            });
        }

        let notifications: Arc<parking_lot::Mutex<Option<NotificationHandler>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let server_requests: Arc<parking_lot::Mutex<Option<ServerRequestHandler>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let write: Arc<Mutex<ChildStdin>> = Arc::new(Mutex::new(stdin));
        let server_requests_clone = Arc::clone(&server_requests);
        let write_clone = Arc::clone(&write);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // 后台读 task：逐行解析 JSON-RPC，按 id 分发响应；stdout 关闭时清空 pending（rx 报错）。
        let pending_clone = Arc::clone(&pending);
        let notifications_clone = Arc::clone(&notifications);
        let on_close: Arc<parking_lot::Mutex<Option<crate::client::CloseHandler>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let on_close_clone = Arc::clone(&on_close);
        tokio::spawn(async move {
            // 手动有界行读：按 `\n` 切分并对单行设 [`MAX_MCP_LINE_BYTES`] 上限，
            // 杜绝 server 发送无换行的超长行导致内存无界增长。
            let mut reader = stdout;
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                match reader.read(&mut tmp).await {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                            let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
                            let Ok(s) = std::str::from_utf8(&line_bytes) else {
                                continue;
                            };
                            let trimmed = s.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                                continue;
                            };
                            // H16：有 `method` **且**有 `id` → server→client 请求，必须应答
                            // （此前与响应同路径处理：pending 查不到即静默丢弃 → server 永久等待）。
                            if let (Some(id), Some(method)) =
                                (val.get("id"), val.get("method").and_then(Value::as_str))
                            {
                                let params = val.get("params").cloned().unwrap_or(Value::Null);
                                let answer = match server_requests_clone.lock().clone() {
                                    Some(h) => h(id, method, &params),
                                    None => Err((
                                        crate::client::JSONRPC_METHOD_NOT_FOUND,
                                        format!("Method not found: {method}"),
                                    )),
                                };
                                let frame = match answer {
                                    Ok(result) => server_request_result(id, result),
                                    Err((code, message)) => {
                                        server_request_error(id, code, &message)
                                    }
                                };
                                let mut w = write_clone.lock().await;
                                if let Ok(s) = serde_json::to_string(&frame) {
                                    if w.write_all(s.as_bytes()).await.is_ok()
                                        && w.write_all(b"\n").await.is_ok()
                                    {
                                        let _ = w.flush().await;
                                    }
                                }
                            } else if let Some(id) = val.get("id").and_then(Value::as_u64) {
                                let mut p = pending_clone.lock().await;
                                if let Some(tx) = p.remove(&id) {
                                    let _ = tx.send(val);
                                }
                            } else if val.get("id").is_none() {
                                // server→client 通知：转发给注册的处理器；未注册仅记 debug。
                                if let Some(method) = val.get("method").and_then(Value::as_str) {
                                    let params = val.get("params").cloned().unwrap_or(Value::Null);
                                    match notifications_clone.lock().clone() {
                                        Some(h) => h(method, &params),
                                        None => tracing::debug!(
                                            target: "mcp::client",
                                            method,
                                            "忽略 server 通知（未注册处理器）"
                                        ),
                                    }
                                }
                            }
                        }
                        if buf.len() > MAX_MCP_LINE_BYTES {
                            tracing::warn!(
                                target: "mcp::client",
                                "MCP 行超过 {MAX_MCP_LINE_BYTES} 字节上限，丢弃到下一行边界"
                            );
                            // 丢弃到下一个换行（含），保留换行后的完整行数据，避免协议失步。
                            if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                                buf.drain(..=nl);
                            } else {
                                buf.clear();
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        tracing::warn!(target: "mcp::client", "MCP stdout 读取错误: {e}");
                        break;
                    }
                }
            }
            pending_clone.lock().await.clear();
            // 读循环退出即视为异常断连（EOF / IO 错误）：触发重连监督。主动 close
            // 的回收路径会先经 [`McpClient::install_transport`] 摘除处理器，不自触发。
            if let Some(h) = on_close_clone.lock().clone() {
                h();
            }
        });

        Ok(Self {
            server_requests,
            write,
            child: Mutex::new(child),
            next_id: AtomicU64::new(1),
            pending,
            notifications,
            on_close,
            timeout: resolve_timeout(cfg.timeout_ms),
        })
    }
}

#[async_trait::async_trait]
impl McpTransport for StdioTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        // 序列化或写入失败时必须清理 pending，否则对应 id 永久泄漏，rx 也无法被响应。
        let send_result: Result<(), McpError> = async {
            let serialized = serde_json::to_string(&req)?;
            let mut w = self.write.lock().await;
            w.write_all(serialized.as_bytes()).await?;
            w.write_all(b"\n").await?;
            w.flush().await?;
            Ok(())
        }
        .await;
        if let Err(e) = send_result {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        // 超时保护：server 无响应且不关 stdout 时避免永久挂起；超时即清理 pending。
        let resp = match self.timeout {
            Some(t) => match tokio::time::timeout(t, rx).await {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(_)) => Err(McpError::Channel),
                Err(_) => Err(McpError::Server(format!("MCP 请求超时（{t:?}）"))),
            },
            None => rx.await.map_err(|_| McpError::Channel),
        };
        let resp = match resp {
            Ok(v) => v,
            Err(e) => {
                self.pending.lock().await.remove(&id);
                return Err(e);
            }
        };
        if let Some(err) = resp.get("error") {
            return Err(McpError::Server(err.to_string()));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let notif = serde_json::json!({"jsonrpc":"2.0","method":method,"params":params});
        let s = serde_json::to_string(&notif)?;
        let mut w = self.write.lock().await;
        w.write_all(s.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok(())
    }

    fn set_on_notification(&self, handler: NotificationHandler) {
        *self.notifications.lock() = Some(handler);
    }

    fn set_on_server_request(&self, handler: ServerRequestHandler) {
        *self.server_requests.lock() = Some(handler);
    }

    fn set_on_close(&self, handler: crate::client::CloseHandler) {
        *self.on_close.lock() = Some(handler);
    }

    async fn close(&self) {
        let mut c = self.child.lock().await;
        let _ = c.kill().await;
        let _ = c.wait().await;
    }
}

// 注：子进程回收依赖 `spawn` 时设置的 `kill_on_drop(true)`——`StdioTransport` drop 时
// 其拥有的 `Child` 一同 drop，自动 kill，杜绝孤儿进程。

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn notification_frames_forward_to_handler_and_responses_do_not() {
        // 子进程先发一条通知帧、再发一条响应帧（id=1 无 pending，应被静默忽略），随后挂住。
        let script = concat!(
            r#"printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}'"#,
            r#" '{"jsonrpc":"2.0","id":1,"result":{}}'; sleep 5"#
        );
        let cfg = McpStdioConfig {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: HashMap::new(),
            timeout_ms: None,
        };
        let t = StdioTransport::spawn(&cfg).await.expect("spawn");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        t.set_on_notification(Arc::new(move |method, _params| {
            let _ = tx.send(method.to_string());
        }));
        let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("通知未到达")
            .expect("信道关闭");
        assert_eq!(first, "notifications/tools/list_changed");
        // 响应帧不得被误路由为通知（通道内不应再有消息）。
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(rx.try_recv().is_err(), "响应帧被误路由为通知");
    }

    #[tokio::test]
    async fn transport_close_fires_on_close_handler() {
        // 子进程静默退出 → stdout EOF → 读循环结束 → 触发断连处理器。
        let cfg = McpStdioConfig {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 0.2".to_string()],
            env: HashMap::new(),
            timeout_ms: None,
        };
        let t = StdioTransport::spawn(&cfg).await.expect("spawn");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        t.set_on_close(Arc::new(move || {
            let _ = tx.send(());
        }));
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("断连未触发 on_close")
            .expect("信道关闭");
    }
}
