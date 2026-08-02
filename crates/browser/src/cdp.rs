//! CDP（Chrome DevTools Protocol）最小传输层。
//!
//! 基于 tokio-tungstenite 的 WebSocket JSON-RPC 帧收发：
//! - 请求/响应按自增 `id` 配对（`HashMap<u64, oneshot>`），发送超时 10s（可配置）；
//! - 事件帧（`Runtime.consoleAPICalled` / `Runtime.exceptionThrown` /
//!   `Page.loadEventFired` 等）分流进内部队列，经 [`CdpConnection::wait_for_event`]
//!   按方法名 + 来源会话谓词等待；
//! - 附加会话命令（`Target.attachToTarget flatten:true`）经 `sessionId` 字段路由。
//!
//! 帧编解码（[`encode_request`] / [`parse_frame`]）为纯函数，便于单元测试。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// 默认命令超时（10s）。
pub const DEFAULT_SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// CDP 传输错误。
#[derive(Debug, thiserror::Error)]
pub enum CdpError {
    /// WebSocket 连接建立失败。
    #[error("CDP 连接失败: {0}")]
    Connect(String),
    /// WebSocket 读写失败。
    #[error("WebSocket 错误: {0}")]
    Ws(String),
    /// JSON 编解码失败。
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    /// 命令等待超时。
    #[error("CDP 命令超时（{0:?}）")]
    Timeout(Duration),
    /// 连接已关闭（对端断开 / 本端释放）。
    #[error("CDP 连接已关闭")]
    Closed,
    /// 协议层错误（CDP `error` 字段）。
    #[error("CDP 协议错误 {code}: {message}")]
    Protocol { code: i64, message: String },
}

/// 事件帧（方法 + 参数 + 来源会话）。
#[derive(Debug, Clone)]
pub struct CdpEvent {
    /// 事件方法名（如 `Page.loadEventFired`）。
    pub method: String,
    /// 事件参数。
    pub params: Value,
    /// 来源会话（attached target 会话才有）。
    pub session_id: Option<String>,
    /// 入队序号（单调递增；用于区分导航前后的同类事件）。
    pub seq: u64,
}

/// 解析后的入站帧。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IncomingFrame {
    /// 请求响应（`id` + `result`，或 `id` + `error`）。
    Response {
        /// 对应请求 id。
        id: u64,
        /// 成功结果（`result` 字段；错误时为空对象）。
        result: Value,
        /// 协议错误（`error` 字段）。
        error: Option<(i64, String)>,
    },
    /// 事件帧（`method` + `params` [+ `sessionId`]）。
    Event {
        /// 事件方法名。
        method: String,
        /// 事件参数。
        params: Value,
        /// 来源会话。
        session_id: Option<String>,
    },
}

/// 封装 JSON-RPC 请求帧（纯函数，供测试）。
pub(crate) fn encode_request(
    id: u64,
    method: &str,
    params: &Value,
    session_id: Option<&str>,
) -> Value {
    let mut frame = json!({ "id": id, "method": method, "params": params });
    if let Some(sid) = session_id {
        frame["sessionId"] = Value::String(sid.to_string());
    }
    frame
}

/// 解析入站帧（纯函数，供测试）：有 `id` → 响应；有 `method` → 事件；否则无法识别。
pub(crate) fn parse_frame(v: &Value) -> Option<IncomingFrame> {
    if let Some(id) = v.get("id").and_then(Value::as_u64) {
        let result = v.get("result").cloned().unwrap_or(Value::Null);
        let error = v.get("error").map(|e| {
            let code = e.get("code").and_then(Value::as_i64).unwrap_or(-1);
            let message = e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("未知错误")
                .to_string();
            (code, message)
        });
        return Some(IncomingFrame::Response { id, result, error });
    }
    if let Some(method) = v.get("method").and_then(Value::as_str) {
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        let session_id = v
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        return Some(IncomingFrame::Event {
            method: method.to_string(),
            params,
            session_id,
        });
    }
    None
}

/// 事件队列：先进先出。唤醒经连接级 [`Notify`]（避免守卫借用跨 await）。
#[derive(Default)]
struct EventStore {
    queue: VecDeque<CdpEvent>,
    next_seq: u64,
}

/// CDP WebSocket 连接：id 配对 + 事件分流。
pub struct CdpConnection {
    tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>>>,
    events: Arc<Mutex<EventStore>>,
    notify: Arc<Notify>,
    closed: Arc<AtomicBool>,
    next_id: AtomicU64,
    send_timeout: Duration,
    /// 读写任务句柄（保持任务存活；连接释放时随通道自然退出）。
    _tasks: (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>),
}

impl CdpConnection {
    /// 连接浏览器级 DevTools WebSocket 端点（默认 10s 命令超时）。
    ///
    /// # Errors
    /// 连接建立失败时返回 [`CdpError::Connect`]。
    pub async fn connect(ws_url: &str) -> Result<Self, CdpError> {
        Self::connect_with_timeout(ws_url, DEFAULT_SEND_TIMEOUT).await
    }

    /// 连接（可自定义命令超时，测试用短超时）。
    ///
    /// # Errors
    /// 连接建立失败时返回 [`CdpError::Connect`]。
    pub async fn connect_with_timeout(
        ws_url: &str,
        send_timeout: Duration,
    ) -> Result<Self, CdpError> {
        let (ws, _resp) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| CdpError::Connect(e.to_string()))?;
        let (mut sink, stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let events = Arc::new(Mutex::new(EventStore::default()));
        let notify = Arc::new(Notify::new());
        let closed = Arc::new(AtomicBool::new(false));

        let writer_closed = Arc::clone(&closed);
        let writer = tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if let Err(e) = sink.send(Message::text(text)).await {
                    tracing::debug!(error = %e, "CDP 写入失败，终止 writer");
                    writer_closed.store(true, Ordering::Relaxed);
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let reader = tokio::spawn(reader_task(
            stream,
            Arc::clone(&pending),
            Arc::clone(&events),
            Arc::clone(&notify),
            Arc::clone(&closed),
        ));

        Ok(Self {
            tx,
            pending,
            events,
            notify,
            closed,
            next_id: AtomicU64::new(1),
            send_timeout,
            _tasks: (writer, reader),
        })
    }

    /// 浏览器级命令（无会话路由）。
    ///
    /// # Errors
    /// 超时 / 连接关闭 / 协议错误时返回 [`CdpError`]。
    pub async fn send(&self, method: &str, params: Value) -> Result<Value, CdpError> {
        self.request(method, params, None).await
    }

    /// 附加会话内命令（`sessionId` 路由，用于 `Page`/`Runtime` 等页面域）。
    ///
    /// # Errors
    /// 超时 / 连接关闭 / 协议错误时返回 [`CdpError`]。
    pub async fn send_in_session(
        &self,
        method: &str,
        params: Value,
        session_id: &str,
    ) -> Result<Value, CdpError> {
        self.request(method, params, Some(session_id)).await
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, CdpError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(CdpError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (otx, orx) = oneshot::channel();
        {
            let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(id, otx);
        }
        let frame = encode_request(id, method, &params, session_id);
        let text = serde_json::to_string(&frame)?;
        self.tx.send(text).map_err(|_| CdpError::Closed)?;
        let received = tokio::time::timeout(self.send_timeout, orx)
            .await
            .map_err(|_| CdpError::Timeout(self.send_timeout))?;
        let res = received.map_err(|_| CdpError::Closed)?;
        res
    }

    /// 当前事件入队水位（配合 [`Self::wait_for_event_since`] 区分新旧事件）。
    #[must_use]
    pub fn event_seq(&self) -> u64 {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_seq
    }

    /// 取出满足谓词的首个事件（队列非空且命中则立即返回，否则 `None`）。
    pub fn try_take_event(&self, pred: impl Fn(&CdpEvent) -> bool) -> Option<CdpEvent> {
        let mut store = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let idx = store.queue.iter().position(pred)?;
        store.queue.remove(idx)
    }

    /// 等待满足谓词的事件（限时）。
    ///
    /// # Errors
    /// 超时返回 [`CdpError::Timeout`]。
    pub async fn wait_for_event(
        &self,
        pred: impl Fn(&CdpEvent) -> bool,
        timeout: Duration,
    ) -> Result<CdpEvent, CdpError> {
        self.wait_for_event_since(0, pred, timeout).await
    }

    /// 等待 `seq >= since` 且满足谓词的事件（限时）。
    ///
    /// `since` 取导航发起前的 [`Self::event_seq`]，避免命中导航前遗留的同类事件。
    ///
    /// # Errors
    /// 超时返回 [`CdpError::Timeout`]。
    pub async fn wait_for_event_since(
        &self,
        since: u64,
        pred: impl Fn(&CdpEvent) -> bool,
        timeout: Duration,
    ) -> Result<CdpEvent, CdpError> {
        let matched = |e: &CdpEvent| e.seq >= since && pred(e);
        tokio::time::timeout(timeout, async {
            loop {
                // 先武装通知再查队列，避免「查后入队」漏唤醒。
                let notified = self.notify.notified();
                tokio::pin!(notified);
                if let Some(e) = self.try_take_event(matched) {
                    return e;
                }
                notified.as_mut().await;
            }
        })
        .await
        .map_err(|_| CdpError::Timeout(timeout))
    }

    /// 取出队列中全部事件（控制台消息等）并清空。
    #[must_use]
    pub fn drain_events(&self) -> Vec<CdpEvent> {
        let mut store = self.events.lock().unwrap_or_else(|e| e.into_inner());
        store.queue.drain(..).collect()
    }
}

/// 读循环：解析帧 → 响应配对 / 事件入队；连接断开时失败所有在途请求。
async fn reader_task(
    mut stream: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>>>,
    events: Arc<Mutex<EventStore>>,
    notify: Arc<Notify>,
    closed: Arc<AtomicBool>,
) {
    loop {
        let Some(msg) = stream.next().await else {
            break;
        };
        match msg {
            Ok(Message::Text(t)) => match serde_json::from_str::<Value>(t.as_str()) {
                Ok(v) => dispatch_frame(v, &pending, &events, &notify),
                Err(e) => tracing::warn!(error = %e, "CDP 文本帧 JSON 解析失败"),
            },
            Ok(Message::Binary(b)) => match serde_json::from_slice::<Value>(&b) {
                Ok(v) => dispatch_frame(v, &pending, &events, &notify),
                Err(e) => tracing::warn!(error = %e, "CDP 二进制帧 JSON 解析失败"),
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => {} // Ping / Pong / Frame 忽略
            Err(e) => {
                tracing::debug!(error = %e, "CDP 读取失败");
                break;
            }
        }
    }
    closed.store(true, Ordering::Relaxed);
    fail_all_pending(&pending, || CdpError::Closed);
    notify.notify_waiters();
}

/// 按帧类型分流：响应 → 配对 oneshot；事件 → 入队 + 唤醒。
fn dispatch_frame(
    v: Value,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>>>,
    events: &Arc<Mutex<EventStore>>,
    notify: &Arc<Notify>,
) {
    let Some(frame) = parse_frame(&v) else {
        tracing::warn!(frame = %v, "CDP 帧无法识别");
        return;
    };
    match frame {
        IncomingFrame::Response { id, result, error } => {
            let mut map = pending.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(tx) = map.remove(&id) {
                let outcome = match error {
                    Some((code, message)) => Err(CdpError::Protocol { code, message }),
                    None => Ok(result),
                };
                let _ = tx.send(outcome);
            } else {
                tracing::debug!(id, "收到未匹配的响应 id");
            }
        }
        IncomingFrame::Event {
            method,
            params,
            session_id,
        } => {
            let mut store = events.lock().unwrap_or_else(|e| e.into_inner());
            let seq = store.next_seq;
            store.next_seq += 1;
            store.queue.push_back(CdpEvent {
                method,
                params,
                session_id,
                seq,
            });
            notify.notify_waiters();
        }
    }
}

/// 连接断开：失败全部在途请求（错误经构造器惰性创建，避免 `CdpError` 需 `Clone`）。
fn fail_all_pending<F>(
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>>>,
    err: F,
) where
    F: Fn() -> CdpError,
{
    let mut map = pending.lock().unwrap_or_else(|e| e.into_inner());
    for (_, tx) in map.drain() {
        let _ = tx.send(Err(err()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_request_packs_method_params_and_session() {
        let v = encode_request(
            7,
            "Page.navigate",
            &json!({"url": "https://example.com"}),
            Some("s1"),
        );
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "Page.navigate");
        assert_eq!(v["params"]["url"], "https://example.com");
        assert_eq!(v["sessionId"], "s1");

        let v2 = encode_request(8, "Target.getTargets", &json!({}), None);
        assert!(v2.get("sessionId").is_none(), "无会话时不应带 sessionId");
    }

    #[test]
    fn parse_frame_distinguishes_response_and_event() {
        let resp = parse_frame(&json!({"id": 1, "result": {"ok": true}})).unwrap();
        assert_eq!(
            resp,
            IncomingFrame::Response {
                id: 1,
                result: json!({"ok": true}),
                error: None
            }
        );

        let err =
            parse_frame(&json!({"id": 2, "error": {"code": -32000, "message": "boom"}})).unwrap();
        match err {
            IncomingFrame::Response {
                id,
                error: Some((code, msg)),
                ..
            } => {
                assert_eq!(id, 2);
                assert_eq!(code, -32000);
                assert_eq!(msg, "boom");
            }
            other => panic!("应为错误响应，实际 {other:?}"),
        }

        let ev = parse_frame(&json!({
            "method": "Page.loadEventFired", "params": {"timestamp": 1.0}, "sessionId": "s9"
        }))
        .unwrap();
        match ev {
            IncomingFrame::Event {
                method,
                params,
                session_id,
            } => {
                assert_eq!(method, "Page.loadEventFired");
                assert_eq!(params["timestamp"], 1.0);
                assert_eq!(session_id.as_deref(), Some("s9"));
            }
            other => panic!("应为事件帧，实际 {other:?}"),
        }

        assert!(parse_frame(&json!({"foo": "bar"})).is_none());
    }

    /// 起一个回显式 mock CDP 服务器：对每个请求按 handler 返回帧序列，随后关连接。
    async fn mock_server(
        mut on_frame: impl FnMut(&Value) -> Vec<Value> + Send + 'static,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(msg) = ws.next().await {
                let Ok(Message::Text(t)) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else {
                    continue;
                };
                let replies = on_frame(&v);
                for r in replies {
                    let _ = ws
                        .send(Message::text(serde_json::to_string(&r).unwrap()))
                        .await;
                }
            }
        });
        (format!("ws://{addr}"), handle)
    }

    #[tokio::test]
    async fn response_pairs_with_request_id() {
        let (url, _srv) = mock_server(|v| {
            // 原样回 id，结果里回显方法名 → 验证配对到各自的请求
            vec![json!({"id": v["id"], "result": {"method": v["method"], "pong": true}})]
        })
        .await;
        let cdp = CdpConnection::connect_with_timeout(&url, Duration::from_secs(2))
            .await
            .unwrap();

        let r1 = cdp.send("A.method", json!({"n": 1})).await.unwrap();
        assert_eq!(r1["pong"], true);
        assert_eq!(r1["method"], "A.method");

        // 并发两个请求各自配对
        let (a, b) = tokio::join!(cdp.send("One", json!({})), cdp.send("Two", json!({})));
        assert_eq!(a.unwrap()["method"], "One");
        assert_eq!(b.unwrap()["method"], "Two");
    }

    #[tokio::test]
    async fn protocol_error_surfaces() {
        let (url, _srv) = mock_server(|v| {
            vec![json!({"id": v["id"], "error": {"code": -32602, "message": "invalid params"}})]
        })
        .await;
        let cdp = CdpConnection::connect_with_timeout(&url, Duration::from_secs(2))
            .await
            .unwrap();
        let err = cdp.send("X", json!({})).await.unwrap_err();
        assert!(
            matches!(err, CdpError::Protocol { code: -32602, .. }),
            "实际 {err:?}"
        );
    }

    #[tokio::test]
    async fn events_queued_and_waitable() {
        let (url, _srv) = mock_server(|v| {
            vec![
                json!({"method": "Runtime.consoleAPICalled", "params": {"type": "log"}, "sessionId": "s1"}),
                json!({"method": "Page.loadEventFired", "params": {}}),
                json!({"id": v["id"], "result": {}}),
            ]
        })
        .await;
        let cdp = CdpConnection::connect_with_timeout(&url, Duration::from_secs(2))
            .await
            .unwrap();
        cdp.send("Page.navigate", json!({"url": "about:blank"}))
            .await
            .unwrap();

        let ev = cdp
            .wait_for_event(
                |e| e.method == "Page.loadEventFired",
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert_eq!(ev.method, "Page.loadEventFired");

        // 未命中谓词的事件应保留在队列（console 先于 loadEventFired 入队 → seq 更小）
        let console = cdp
            .try_take_event(|e| e.method == "Runtime.consoleAPICalled")
            .expect("console 事件应仍在队列");
        assert_eq!(console.session_id.as_deref(), Some("s1"));
        assert!(console.seq < ev.seq, "seq 应单调递增");

        // drain 清空
        assert!(cdp.drain_events().is_empty());
    }

    #[tokio::test]
    async fn request_times_out_when_no_reply() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _srv = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            // 保持连接但不回包
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let cdp = CdpConnection::connect_with_timeout(
            &format!("ws://{addr}"),
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        let err = cdp.send("X", json!({})).await.unwrap_err();
        assert!(matches!(err, CdpError::Timeout(_)), "实际 {err:?}");
    }

    #[tokio::test]
    async fn send_after_close_errors_fast() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _srv = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.close(None).await;
        });
        let cdp =
            CdpConnection::connect_with_timeout(&format!("ws://{addr}"), Duration::from_secs(2))
                .await
                .unwrap();
        // 等服务器完成关闭（读循环退出、closed 标记置位）
        tokio::time::sleep(Duration::from_millis(200)).await;
        let err = cdp.send("X", json!({})).await.unwrap_err();
        assert!(matches!(err, CdpError::Closed), "实际 {err:?}");
    }

    #[tokio::test]
    async fn connect_failure_reports_error() {
        // 绑定后立即释放端口 → 连接必然被拒
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let err = match CdpConnection::connect(&format!("ws://{addr}/devtools/browser/x")).await {
            Err(e) => e,
            Ok(_) => panic!("连接应失败"),
        };
        assert!(matches!(err, CdpError::Connect(_)), "实际 {err:?}");
    }
}
