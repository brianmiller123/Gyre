//! JSON-RPC 2.0 传输层：基于 stdio 的 Content-Length 帧协议。
//!
//! LSP 规范要求每条消息前有 `Content-Length: N\r\n\r\n` 头部。
//! 本模块提供 [`LspTransport`] —— 管理子进程生命周期并提供
//! 双向 JSON-RPC 消息传递，内置请求/响应关联。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, error, info, warn};

/// 内部 JSON-RPC 消息。
#[derive(Debug, Clone)]
pub struct JsonRpcMessage {
    /// JSON 载荷（完整请求/响应/通知对象）。
    pub payload: String,
}

/// 待处理请求映射：`id → 响应信道`。
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<String, TransportError>>>>>;

/// 传输层错误。
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("子进程启动失败: {0}")]
    Spawn(std::io::Error),

    #[error("写入 stdin 失败: {0}")]
    Write(std::io::Error),

    #[error("读取 stdout 失败: {0}")]
    Read(std::io::Error),

    #[error("子进程已退出: code={0:?}")]
    ProcessExited(Option<i32>),

    #[error("消息帧解析失败: {0}")]
    Frame(String),

    #[error("请求超时 (>{0:?})")]
    Timeout(Duration),

    #[error("信道已关闭")]
    ChannelClosed,

    #[error("JSON-RPC 错误: code={code}, message={message}")]
    RpcError { code: i64, message: String },
}

/// LSP 传输句柄：持有子进程句柄与通信信道。
///
/// # 生命周期
///
/// ```text
/// LspTransport::spawn("rust-analyzer", &[])
///   → 子进程启动，后台读写任务运行
///   → 调用方通过 request()/notify() 通信
///   → drop(LspTransport) → kill 子进程
/// ```
pub struct LspTransport {
    /// 子进程句柄。
    child: Child,

    /// 出站请求发送端（调用方 → 后台写任务）。
    request_tx: mpsc::Sender<OutboundRequest>,

    /// 入站通知接收端（后台读任务 → 调用方）。
    notification_rx: mpsc::Receiver<JsonRpcMessage>,

    /// 待处理请求映射（共享给读任务用于响应路由）。
    pending: PendingMap,

    /// 后台任务 join handle。
    _read_task: tokio::task::JoinHandle<()>,
    _write_task: tokio::task::JoinHandle<()>,
}

/// 出站请求（含响应信道）。
struct OutboundRequest {
    payload: String,
    /// 非空表示期待响应；空表示通知。
    response_tx: Option<oneshot::Sender<Result<String, TransportError>>>,
}

impl LspTransport {
    /// 启动语言服务器子进程并建立双向 JSON-RPC 通信。
    ///
    /// # Errors
    /// 子进程无法启动时返回 [`TransportError::Spawn`]。
    pub fn spawn(command: &str, args: &[&str], cwd: Option<&std::path::Path>) -> Result<Self, TransportError> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        let mut child = cmd.spawn().map_err(TransportError::Spawn)?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let (request_tx, request_rx) = mpsc::channel::<OutboundRequest>(256);
        let (notification_tx, notification_rx) = mpsc::channel::<JsonRpcMessage>(256);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        // 后台写任务
        let write_task = tokio::spawn(write_loop(stdin, request_rx));

        // 后台读任务（共享 pending map 以路由响应）
        let read_pending = Arc::clone(&pending);
        let read_task = tokio::spawn(read_loop(stdout, notification_tx, read_pending));

        // 后台 stderr 日志
        tokio::spawn(log_stderr(stderr));

        info!(
            command = %command,
            args = ?args,
            pid = child.id(),
            "LSP 传输已启动"
        );

        Ok(Self {
            child,
            request_tx,
            notification_rx,
            pending,
            _read_task: read_task,
            _write_task: write_task,
        })
    }

    /// 发送 JSON-RPC 请求并等待响应。
    ///
    /// # Errors
    /// 写入失败、读取失败、超时或 JSON-RPC 错误时返回对应错误。
    pub async fn request(
        &self,
        id: u64,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, TransportError> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let payload = serde_json::to_string(&request).map_err(|e| TransportError::Frame(e.to_string()))?;

        debug!(method = %method, id = id, "LSP 请求发送");

        let (response_tx, response_rx) = oneshot::channel();

        // 注册待处理请求
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, response_tx);
        }

        // 发送到写任务
        self.request_tx
            .send(OutboundRequest {
                payload,
                response_tx: None, // 响应由读任务通过 pending map 路由
            })
            .await
            .map_err(|_| TransportError::ChannelClosed)?;

        // 等待响应
        let result = match tokio::time::timeout(timeout, response_rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => {
                // oneshot 被取消（发送端 dropped）
                Err(TransportError::ChannelClosed)
            }
            Err(_) => {
                // 超时，清理 pending
                let mut pending = self.pending.lock().await;
                pending.remove(&id);
                Err(TransportError::Timeout(timeout))
            }
        };

        // 清理 pending（以防正常路径未清理）
        {
            let mut pending = self.pending.lock().await;
            pending.remove(&id);
        }

        match result {
            Ok(raw) => {
                // 解析 JSON 响应
                let response: serde_json::Value =
                    serde_json::from_str(&raw).map_err(|e| TransportError::Frame(e.to_string()))?;

                // 检查 JSON-RPC 错误
                if let Some(err) = response.get("error") {
                    let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
                    let message = err
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error")
                        .to_string();
                    return Err(TransportError::RpcError { code, message });
                }

                // 返回 result 字段
                Ok(response.get("result").cloned().unwrap_or(serde_json::Value::Null))
            }
            Err(e) => Err(e),
        }
    }

    /// 发送 JSON-RPC 通知（无响应，不分配 id）。
    ///
    /// # Errors
    /// 写入失败时返回 [`TransportError::Write`]。
    pub async fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), TransportError> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        let payload =
            serde_json::to_string(&notification).map_err(|e| TransportError::Frame(e.to_string()))?;

        self.request_tx
            .send(OutboundRequest {
                payload,
                response_tx: None,
            })
            .await
            .map_err(|_| TransportError::ChannelClosed)?;

        Ok(())
    }

    /// 接收下一条通知消息。
    pub async fn recv_notification(&mut self) -> Option<JsonRpcMessage> {
        self.notification_rx.recv().await
    }

    /// 非阻塞地尝试接收通知。
    pub fn try_recv_notification(&mut self) -> Option<JsonRpcMessage> {
        self.notification_rx.try_recv().ok()
    }

    /// 轮询所有待处理通知（不阻塞）。
    pub fn drain_notifications(&mut self) -> Vec<JsonRpcMessage> {
        let mut msgs = Vec::new();
        while let Ok(msg) = self.notification_rx.try_recv() {
            msgs.push(msg);
        }
        msgs
    }

    /// 获取子进程 ID。
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// 优雅关闭：发送 shutdown + exit，等待进程退出。
    pub async fn shutdown(mut self, timeout: Duration) -> Result<(), TransportError> {
        info!("正在关闭 LSP 传输...");
        let _ = self.request(9999, "shutdown", serde_json::Value::Null, Duration::from_secs(5)).await;
        let _ = self.notify("exit", serde_json::Value::Null).await;

        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(Ok(status)) => {
                info!(code = ?status.code(), "LSP 子进程已退出");
                Ok(())
            }
            Ok(Err(e)) => {
                warn!(error = %e, "等待子进程退出失败");
                Err(TransportError::Read(e))
            }
            Err(_) => {
                warn!("LSP 子进程关闭超时，强制 kill");
                let _ = self.child.kill().await;
                Err(TransportError::Timeout(timeout))
            }
        }
    }
}

// ── 后台任务 ───────────────────────────────────────────────────────────

/// 后台写任务：从信道接收出站消息，以 Content-Length 帧写入 stdin。
async fn write_loop(mut stdin: tokio::process::ChildStdin, mut rx: mpsc::Receiver<OutboundRequest>) {
    while let Some(req) = rx.recv().await {
        let frame = format_frame(&req.payload);
        debug!(len = frame.len(), "写入帧");

        if let Err(e) = stdin.write_all(frame.as_bytes()).await {
            error!(error = %e, "写入 stdin 失败，写循环退出");
            break;
        }
        if let Err(e) = stdin.flush().await {
            error!(error = %e, "flush stdin 失败");
            break;
        }
    }
    debug!("写循环退出");
}

/// 后台读任务：从 stdout 读 Content-Length 帧，路由响应或通知。
async fn read_loop(
    stdout: tokio::process::ChildStdout,
    notification_tx: mpsc::Sender<JsonRpcMessage>,
    pending: PendingMap,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    loop {
        line.clear();

        // 读取 Content-Length 头
        match reader.read_line(&mut line).await {
            Ok(0) => {
                info!("stdout 已关闭，读循环退出");
                break;
            }
            Ok(_) => {}
            Err(e) => {
                error!(error = %e, "读取 Content-Length 头失败");
                break;
            }
        }

        let content_length = match parse_content_length(line.trim()) {
            Some(len) => len,
            None => {
                warn!(line = %line.trim(), "无法解析 Content-Length 头，跳过");
                continue;
            }
        };

        // 跳过空行
        line.clear();
        if let Err(e) = reader.read_line(&mut line).await {
            error!(error = %e, "读取头部空行失败");
            break;
        }

        // 读取 JSON 主体
        let mut body = vec![0u8; content_length];
        if let Err(e) = tokio::io::AsyncReadExt::read_exact(&mut reader, &mut body).await {
            error!(error = %e, len = content_length, "读取 JSON 主体失败");
            break;
        }

        let payload = match String::from_utf8(body) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "JSON 主体非 UTF-8");
                continue;
            }
        };

        debug!(len = content_length, "收到帧");

        // 解析以判断是响应还是通知
        let parsed: serde_json::Value = match serde_json::from_str(&payload) {
            Ok(v) => v,
            Err(e) => {
                error!(error = %e, "JSON 解析失败");
                continue;
            }
        };

        // 判断消息类型：
        // - 有 "id" 且有 "result" 或 "error" → 响应 → 路由到 pending map
        // - 有 "method" 且无 "id" → 通知 → 推到 notification channel
        let has_id = parsed.get("id").is_some();
        let has_method = parsed.get("method").is_some();
        let has_result_or_error = parsed.get("result").is_some() || parsed.get("error").is_some();

        if has_id && has_result_or_error {
            // 这是一个响应
            let id = parsed.get("id").and_then(|v| v.as_u64());
            if let Some(id) = id {
                let mut pending_map = pending.lock().await;
                if let Some(tx) = pending_map.remove(&id) {
                    let _ = tx.send(Ok(payload));
                    debug!(id = id, "响应已路由");
                } else {
                    warn!(id = id, "收到未知请求 ID 的响应（可能已超时）");
                }
            }
        } else if has_method && !has_id {
            // 这是一个通知
            let msg = JsonRpcMessage { payload };
            if notification_tx.send(msg).await.is_err() {
                debug!("通知信道已关闭，读循环退出");
                break;
            }
        } else {
            // 也可能是服务器发起的请求（如 window/workDoneProgress/create）
            // 目前不支持，记录警告
            warn!(?parsed, "收到未预期的消息类型");
        }
    }
    debug!("读循环退出");
}

/// 后台 stderr 日志任务。
async fn log_stderr(stderr: tokio::process::ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    debug!(target: "lsp_server_stderr", "{}", trimmed);
                }
            }
            Err(_) => break,
        }
    }
}

// ── 帧工具函数 ─────────────────────────────────────────────────────────

/// 构建 Content-Length 帧：`Content-Length: {N}\r\n\r\n{json}`
fn format_frame(json: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{json}", json.len())
}

/// 从 `Content-Length: N` 行解析字节数。
fn parse_content_length(header: &str) -> Option<usize> {
    header
        .strip_prefix("Content-Length:")
        .and_then(|s| s.trim().parse().ok())
}
