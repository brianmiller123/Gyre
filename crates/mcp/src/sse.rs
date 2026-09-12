//! MCP legacy HTTP+SSE 传输（协议 2024-11-05，参考 oh-my-pi `transports/sse.ts`）。
//!
//! 与 Streamable HTTP 的分野：
//! - 连接 = `GET {url}`（`Accept: text/event-stream`）常驻 SSE 读循环
//! - server 经 `event: endpoint` 下发 message 端点（相对/绝对 URL；仅首个生效，
//!   跨源端点拒绝——对齐 omp 的 origin 锁定）；后续请求 `POST` 该端点，任意 2xx
//!   （典型 202 Accepted，无 body）即已受理
//! - 请求的 JSON-RPC 响应与 server→client 通知经同一 SSE 流以 `event: message` 分发
//!
//! 不实现的 legacy 语义（与现有读循环行为对齐）：server→client 请求（有 method
//! 有 id）一律忽略；`MCP-Protocol-Version` 头不参与（协议版本固定为 2024-11-05）。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use agent_config::McpHttpConfig;
use futures::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::client::{
    CloseHandler, McpError, McpTransport, NotificationHandler, ServerRequestHandler,
    resolve_timeout,
};
use crate::http::build_http_parts;

/// MCP legacy HTTP+SSE 传输：SSE 读循环常驻连接，message 端点收发请求。
pub(crate) struct SseTransport {
    http: reqwest::Client,
    /// 用户配置的自定义头（已剥除传输层独占头）。
    extra_headers: reqwest::header::HeaderMap,
    /// 单请求总超时（覆盖 POST + SSE 流内取回；`None` = 不限制）。
    timeout: Option<Duration>,
    /// server 指定的 message 端点（`endpoint` 事件解析结果；连接建立前为 `None`）。
    endpoint: Arc<Mutex<Option<reqwest::Url>>>,
    /// id → 未决响应（读循环分发，request 等待）。
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    /// server→client 通知处理器槽（读循环经此分发）。
    notifications: Arc<Mutex<Option<NotificationHandler>>>,
    /// server→client **请求**处理器槽（H16：读循环据应答 POST 回 message 端点）。
    server_requests: Arc<Mutex<Option<ServerRequestHandler>>>,
    /// 异常断连处理器槽（读循环退出时触发；主动 `close` 前由 [`crate::client::McpClient`] 摘除）。
    on_close: Arc<Mutex<Option<CloseHandler>>>,
    /// SSE 读循环任务句柄（`close` / `Drop` 时 abort）。
    read_task: Mutex<Option<JoinHandle<()>>>,
    /// 流已断开（读循环退出后置位）：后续请求立即报 [`McpError::Closed`]，不再空等。
    closed: Arc<AtomicBool>,
    next_id: AtomicU64,
}

impl SseTransport {
    /// 连接 legacy HTTP+SSE 端点：GET 起 SSE 流并等待 `endpoint` 事件完成握手。
    ///
    /// # Errors
    /// GET 非 2xx、响应非 `text/event-stream`、流提前结束（未收到 `endpoint` 事件）、
    /// endpoint 非法或跨源时返回 [`McpError`]。
    pub(crate) async fn connect(cfg: &McpHttpConfig) -> Result<Self, McpError> {
        let (http, extra_headers) = build_http_parts(cfg)?;
        let timeout = resolve_timeout(cfg.timeout_ms);
        let base: reqwest::Url = cfg
            .url
            .parse()
            .map_err(|e| McpError::Http(format!("非法 MCP URL {:?}: {e}", cfg.url)))?;

        let fut = http
            .get(base.clone())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .headers(extra_headers.clone())
            .send();
        let resp = match timeout {
            Some(t) => tokio::time::timeout(t, fut)
                .await
                .map_err(|_| McpError::Server(format!("MCP 请求超时（{t:?}）")))?,
            None => fut.await,
        }
        .map_err(McpError::from)?;

        let status = resp.status();
        if !status.is_success() {
            // 错误体截断防撑爆日志（对齐 llm 层 4 KiB 上限）。
            let text = truncate_error_body(resp.text().await.unwrap_or_default());
            return Err(McpError::Http(format!("HTTP {status}: {text}")));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !content_type.contains("text/event-stream") {
            return Err(McpError::Http(format!(
                "legacy SSE 端点未返回 text/event-stream（content-type: {content_type}）"
            )));
        }

        let endpoint: Arc<Mutex<Option<reqwest::Url>>> = Arc::new(Mutex::new(None));
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let notifications: Arc<Mutex<Option<NotificationHandler>>> = Arc::new(Mutex::new(None));
        let server_requests: Arc<Mutex<Option<ServerRequestHandler>>> = Arc::new(Mutex::new(None));
        let on_close: Arc<Mutex<Option<CloseHandler>>> = Arc::new(Mutex::new(None));
        let closed = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), McpError>>();

        let read_task = tokio::spawn(read_loop(
            resp,
            base,
            Arc::clone(&endpoint),
            Arc::clone(&pending),
            Arc::clone(&notifications),
            Arc::clone(&server_requests),
            Arc::clone(&on_close),
            Arc::clone(&closed),
            http.clone(),
            extra_headers.clone(),
            ready_tx,
        ));

        // 等待 `endpoint` 事件完成连接握手（畸形流/提前关流经 ready 信道报错）。
        let ready = match timeout {
            Some(t) => tokio::time::timeout(t, ready_rx)
                .await
                .map_err(|_| {
                    read_task.abort();
                    McpError::Server(format!("MCP 请求超时（{t:?}）"))
                })?
                .map_err(|_| McpError::Closed)?,
            None => ready_rx.await.map_err(|_| McpError::Closed)?,
        };
        ready?;

        Ok(Self {
            http,
            extra_headers,
            timeout,
            endpoint,
            pending,
            notifications,
            server_requests,
            on_close,
            read_task: Mutex::new(Some(read_task)),
            closed,
            next_id: AtomicU64::new(1),
        })
    }

    /// 当前 message 端点（未完成握手即断流时为 `None`）。
    fn message_endpoint(&self) -> Result<reqwest::Url, McpError> {
        self.endpoint.lock().clone().ok_or(McpError::Closed)
    }

    /// 组装发往 message 端点的 POST。
    fn post(&self, endpoint: reqwest::Url, body: Vec<u8>) -> reqwest::RequestBuilder {
        self.http
            .post(endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .headers(self.extra_headers.clone())
            .body(body)
    }

    /// 断流 + 拒绝全部未决请求（`close` 与 `Drop` 共用的回收动作）。
    fn shutdown(&self) {
        if let Some(task) = self.read_task.lock().take() {
            task.abort();
        }
        self.closed.store(true, Ordering::SeqCst);
        self.pending.lock().clear();
    }
}

/// SSE 读循环：解析 `endpoint` 事件与 JSON-RPC message；流退出时向连接等待者
/// 报错（未就绪）或标记断流、拒绝全部未决请求并触发断连回调（已就绪）。
#[allow(clippy::too_many_arguments)]
async fn read_loop(
    resp: reqwest::Response,
    base: reqwest::Url,
    endpoint: Arc<Mutex<Option<reqwest::Url>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    notifications: Arc<Mutex<Option<NotificationHandler>>>,
    server_requests: Arc<Mutex<Option<ServerRequestHandler>>>,
    on_close: Arc<Mutex<Option<CloseHandler>>>,
    closed: Arc<AtomicBool>,
    http: reqwest::Client,
    extra_headers: reqwest::header::HeaderMap,
    ready: oneshot::Sender<Result<(), McpError>>,
) {
    use eventsource_stream::Eventsource;

    // Option 包装：ready 信道只能送达一次（连接失败路径提前 return，不再触发尾部）。
    let mut ready = Some(ready);
    let mut endpoint_ready = false;
    let mut events = resp.bytes_stream().eventsource();
    while let Some(ev) = events.next().await {
        let Ok(ev) = ev else {
            tracing::debug!(target: "mcp::sse", "legacy SSE 流读取错误，断开");
            break;
        };
        // `event: endpoint`：server 指定 message 端点。仅首个生效（对齐 omp），
        // 重复事件忽略；解析失败或跨源（防 SSRF 式重定向）即连接失败。
        if ev.event == "endpoint" {
            if endpoint_ready {
                continue;
            }
            let data = ev.data.trim();
            match base.join(data) {
                Ok(u) if u.origin() == base.origin() => {
                    *endpoint.lock() = Some(u);
                    endpoint_ready = true;
                    if let Some(tx) = ready.take() {
                        let _ = tx.send(Ok(()));
                    }
                }
                Ok(u) => {
                    if let Some(tx) = ready.take() {
                        let _ = tx.send(Err(McpError::Http(format!(
                            "legacy SSE endpoint 跨源拒绝: {u}（base: {base}）"
                        ))));
                    }
                    return;
                }
                Err(e) => {
                    if let Some(tx) = ready.take() {
                        let _ = tx.send(Err(McpError::Http(format!(
                            "非法 legacy endpoint {data:?}: {e}"
                        ))));
                    }
                    return;
                }
            }
            continue;
        }

        let data = ev.data.trim();
        // 空行与注释性哨兵（对齐 omp 跳过 `[DONE]`）。
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(data) else {
            tracing::debug!(target: "mcp::sse", "丢弃无法解析的 legacy SSE data");
            continue;
        };
        // MCP 允许 JSON-RPC batch：数组拆为逐条分发。
        let messages: Vec<Value> = match msg {
            Value::Array(items) => items,
            m @ Value::Object(_) => vec![m],
            _ => {
                tracing::debug!(target: "mcp::sse", "丢弃非对象 legacy SSE 消息");
                continue;
            }
        };
        for m in &messages {
            if let Some(reply) = dispatch_message(m, &pending, &notifications, &server_requests) {
                // H16：server→client 请求的响应经 message 端点 POST 回（同一 SSE 流的反向通道）。
                if let Some(ep) = endpoint.lock().clone() {
                    let body = serde_json::to_vec(&reply).unwrap_or_default();
                    let req = http
                        .post(ep)
                        .header(reqwest::header::CONTENT_TYPE, "application/json")
                        .headers(extra_headers.clone())
                        .body(body);
                    tokio::spawn(async move {
                        if let Err(e) = req.send().await {
                            tracing::warn!(target: "mcp::sse", error = %e, "回发 server→client 请求响应失败");
                        }
                    });
                } else {
                    tracing::warn!(target: "mcp::sse", "endpoint 未就绪，无法回发请求响应");
                }
            }
        }
    }

    let Some(ready_tx) = ready.take() else {
        // 已完成连接握手后断流：标记关闭 → 未决请求全部失败（rx → Closed）→ 触发重连监督。
        // 主动 close 的回收路径已先经 McpClient 摘除处理器，不自触发。
        closed.store(true, Ordering::SeqCst);
        pending.lock().clear();
        if let Some(h) = on_close.lock().clone() {
            h();
        }
        return;
    };
    if endpoint_ready {
        let _ = ready_tx.send(Ok(()));
        // ready 被取走后走不到这里；此分支仅闭合类型（连接成功后由上层接管）。
        return;
    }
    // 流退出仍未拿到 `endpoint` 事件——server 提前关流 / 畸形流，连接失败。
    let _ = ready_tx.send(Err(McpError::Closed));
}

/// 分发一条 JSON-RPC 消息。
///
/// - 响应（id + result/error）→ 按 id 唤醒等待者，返回 `None`；
/// - server→client **请求**（有 `method` **且有** `id`）→ 返回待回发的响应帧（H16）；
/// - 通知（有 method 无 id）→ 转发注册的处理器，返回 `None`。
fn dispatch_message(
    m: &Value,
    pending: &Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    notifications: &Mutex<Option<NotificationHandler>>,
    server_requests: &Mutex<Option<ServerRequestHandler>>,
) -> Option<Value> {
    if let Some(method) = m.get("method").and_then(Value::as_str) {
        let Some(id) = m.get("id") else {
            // 通知。
            let params = m.get("params").cloned().unwrap_or(Value::Null);
            match notifications.lock().clone() {
                Some(h) => h(method, &params),
                None => tracing::debug!(target: "mcp::sse", method, "忽略 server 通知"),
            }
            return None;
        };
        // H16：server→client 请求——必须应答（此前直接丢弃 → server 永久等待）。
        let params = m.get("params").cloned().unwrap_or(Value::Null);
        let answer = match server_requests.lock().clone() {
            Some(h) => h(id, method, &params),
            None => Err((
                crate::client::JSONRPC_METHOD_NOT_FOUND,
                format!("Method not found: {method}"),
            )),
        };
        return Some(match answer {
            Ok(result) => crate::client::server_request_result(id, result),
            Err((code, message)) => crate::client::server_request_error(id, code, &message),
        });
    }
    if let Some(id) = m.get("id").and_then(Value::as_u64) {
        match pending.lock().remove(&id) {
            Some(tx) => {
                let _ = tx.send(m.clone());
            }
            None => tracing::debug!(target: "mcp::sse", id, "忽略未知 id 的 legacy SSE 响应"),
        }
    }
    None
}

impl Drop for SseTransport {
    fn drop(&mut self) {
        // 静默回收（并发切换的竞态败者等）：断流 + 未决请求立即失败。
        self.shutdown();
    }
}

#[async_trait::async_trait]
impl McpTransport for SseTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let endpoint = self.message_endpoint()?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        // 插入后复查 closed：读循环刚退出的竞态窗口内不空等（置位先于清 pending，
        // 任何交错下请求要么得到响应要么得到 [`McpError::Closed`]）。
        if self.closed.load(Ordering::SeqCst) {
            self.pending.lock().remove(&id);
            return Err(McpError::Closed);
        }

        let body = match serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })) {
            Ok(b) => b,
            Err(e) => {
                self.pending.lock().remove(&id);
                return Err(e.into());
            }
        };

        // POST 仅表示受理（2xx，典型 202 Accepted 无 body）；真正的 JSON-RPC 响应
        // 经同一 SSE 流的 `event: message` 送达。总超时覆盖 POST + 流内取回。
        let fut = async {
            let resp = self.post(endpoint, body).send().await?;
            let status = resp.status();
            if !status.is_success() {
                let text = truncate_error_body(resp.text().await.unwrap_or_default());
                return Err(McpError::Http(format!("HTTP {status}: {text}")));
            }
            let frame = rx.await.map_err(|_| McpError::Closed)?;
            if let Some(err) = frame.get("error") {
                return Err(McpError::Server(err.to_string()));
            }
            Ok(frame.get("result").cloned().unwrap_or(Value::Null))
        };
        let result = match self.timeout {
            Some(t) => match tokio::time::timeout(t, fut).await {
                Ok(r) => r,
                Err(_) => Err(McpError::Server(format!("MCP 请求超时（{t:?}）"))),
            },
            None => fut.await,
        };
        if result.is_err() {
            // 超时/断流场景清理 pending，防止迟到响应堆积。
            self.pending.lock().remove(&id);
        }
        result
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let endpoint = self.message_endpoint()?;
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))?;
        let fut = self.post(endpoint, body).send();
        let resp = match self.timeout {
            Some(t) => tokio::time::timeout(t, fut)
                .await
                .map_err(|_| McpError::Server(format!("MCP 请求超时（{t:?}）")))?,
            None => fut.await,
        }
        .map_err(McpError::from)?;
        // 任意 2xx 即成功：legacy server 对通知不回包。
        if !resp.status().is_success() {
            return Err(McpError::Http(format!(
                "HTTP {}（通知 {method}）",
                resp.status()
            )));
        }
        Ok(())
    }

    fn set_on_notification(&self, handler: NotificationHandler) {
        *self.notifications.lock() = Some(handler);
    }

    fn set_on_server_request(&self, handler: ServerRequestHandler) {
        *self.server_requests.lock() = Some(handler);
    }

    fn set_on_close(&self, handler: CloseHandler) {
        *self.on_close.lock() = Some(handler);
    }

    async fn close(&self) {
        self.shutdown();
    }
}

/// 错误体截断：4 KiB 上限 + UTF-8 字符边界回退（对齐 llm 层上限，避免多字节
/// 字符在固定字节切片处 panic）。
pub(crate) fn truncate_error_body(text: String) -> String {
    if text.len() <= 4096 {
        return text;
    }
    let mut end = 4096;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}
