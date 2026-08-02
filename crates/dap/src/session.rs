//! DAP 客户端会话：stdio 传输、帧读写、握手与请求/响应按 id 配对。
//!
//! 结构：一个读任务（适配器 stdout → 帧解码 → response 路由 / 事件广播）、一个写任务
//! （无界通道 → 适配器 stdin）。`request` 发请求后在 pending map 登记 oneshot 并等待；
//! `send_sync` 同步入队不等待；`close` 尽力 `disconnect` 后关通道并杀子进程。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex as StdMutex;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tracing::{debug, warn};

use crate::frame::{decode_frames, encode_frame};
use crate::{AdapterSpec, DapError};

/// 单个请求的默认超时。
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// 握手等待 `initialized` 事件的宽容超时（超时继续 launch，个别适配器不发该事件）。
const INITIALIZED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// 事件广播容量（消费者慢时丢事件——v1 不推送事件，可接受）。
const EVENT_CAPACITY: usize = 256;

/// 已过滤的 DAP 事件（stopped/continued/output/thread/terminated）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DapEvent {
    /// `stopped`：命中断点 / 单步暂停（body 为事件原文）。
    Stopped(Value),
    /// `continued`：恢复执行。
    Continued(Value),
    /// `output`：调试对象输出（stdout/stderr/console）。
    Output(Value),
    /// `thread`：线程启停。
    Thread(Value),
    /// `terminated`：调试会话终止。
    Terminated(Value),
    /// 其它未归类事件（原样携带，如 `initialized`、`process`），便于后续动作读取。
    Other(String, Value),
}

/// DAP 客户端会话（与单个适配器进程的 stdio 双向通道）。
///
/// 可克隆（共享同一传输与状态）；结束时请显式调用 [`DapSession::close`]，
/// 否则会话析构时（`kill_on_drop`）子进程会被回收。
#[derive(Clone)]
pub struct DapSession {
    spec: AdapterSpec,
    inner: Arc<SessionInner>,
}

struct SessionInner {
    /// 写任务通道（`None` = 已关闭）。std Mutex：仅同步操作，绝不在持锁期间 await。
    write_tx: StdMutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    /// 待响应请求：`request_seq → 响应通道`（oneshot 被丢弃即失败路径）。
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, DapError>>>>,
    /// 事件广播。
    events_tx: broadcast::Sender<DapEvent>,
    /// 子进程句柄（spawn 模式；`from_io` 为 `None`）。
    child: Mutex<Option<Child>>,
    /// 请求序号（自增，从 1 起）。
    seq: AtomicI64,
    /// 会话是否已关闭（读任务退出 / 显式 close / 写失败）。
    closed: AtomicBool,
}

impl DapSession {
    /// 启动适配器进程（stdio 传输）并完成握手：`initialize` → 等待 `initialized` → `launch`。
    ///
    /// `launch_args` 为 DAP `launch` 请求的 arguments（透传；`request` 字段由调用方决定，
    /// 参见 [`crate::DebugTool`]）。
    ///
    /// # Errors
    /// 进程 spawn 失败、握手请求超时/被拒，或帧协议损坏时返回 [`DapError`]。
    pub async fn spawn(spec: &AdapterSpec, launch_args: Value) -> Result<Self, DapError> {
        let mut child = Command::new(&spec.command)
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| DapError::SpawnFailed(format!("{}: {e}", spec.command)))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DapError::SpawnFailed("无法获取适配器 stdout".into()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| DapError::SpawnFailed("无法获取适配器 stdin".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| DapError::SpawnFailed("无法获取适配器 stderr".into()))?;
        let session = Self::from_io(stdout, stdin, spec, launch_args).await?;
        session.inner.child.lock().await.replace(child);
        tokio::spawn(drain_stderr(stderr, spec.name));
        Ok(session)
    }

    /// 从既有读写管道构造会话并完成握手（测试用：跳过进程 spawn，对 reader/writer 抽象做单测）。
    pub(crate) async fn from_io<R, W>(
        reader: R,
        writer: W,
        spec: &AdapterSpec,
        launch_args: Value,
    ) -> Result<Self, DapError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (write_tx, write_rx) = mpsc::unbounded_channel();
        let (events_tx, _) = broadcast::channel(EVENT_CAPACITY);
        let inner = Arc::new(SessionInner {
            write_tx: StdMutex::new(Some(write_tx)),
            pending: Mutex::new(HashMap::new()),
            events_tx,
            child: Mutex::new(None),
            seq: AtomicI64::new(0),
            closed: AtomicBool::new(false),
        });
        tokio::spawn(writer_task(Arc::clone(&inner), writer, write_rx));
        tokio::spawn(reader_task(reader, Arc::clone(&inner)));
        let session = Self {
            spec: spec.clone(),
            inner,
        };
        session.handshake(launch_args).await?;
        Ok(session)
    }

    /// 发送请求并等待配对响应（超时 [`REQUEST_TIMEOUT`]）。
    ///
    /// # Errors
    /// 会话已关闭 / 写失败 / 超时 / 适配器返回错误响应时返回 [`DapError`]。
    pub async fn request(&self, command: &str, args: Value) -> Result<Value, DapError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(DapError::Closed("会话已关闭".into()));
        }
        let seq = self.next_seq();
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(seq, tx);
        let msg = json!({ "seq": seq, "type": "request", "command": command, "arguments": args });
        if let Err(e) = self.send_frame(&msg) {
            self.inner.pending.lock().await.remove(&seq);
            return Err(e);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => {
                // sender 被 fail_all_pending 丢弃（会话已关闭），但保险起见仍清理
                self.inner.pending.lock().await.remove(&seq);
                Err(DapError::Closed("会话已关闭".into()))
            }
            Err(_) => {
                self.inner.pending.lock().await.remove(&seq);
                Err(DapError::Timeout(command.into()))
            }
        }
    }

    /// 同步发送请求（fire-and-forget，不等待响应）。
    ///
    /// 非 `async`：经无界通道即时入队写任务，立即返回；错误仅反映「会话已关闭 /
    /// 队列失败」。需要响应请用 [`DapSession::request`]。
    ///
    /// # Errors
    /// 会话已关闭或写队列失败时返回 [`DapError::Closed`]。
    pub fn send_sync(&self, command: &str, args: &Value) -> Result<(), DapError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(DapError::Closed("会话已关闭".into()));
        }
        let seq = self.next_seq();
        let msg = json!({ "seq": seq, "type": "request", "command": command, "arguments": args });
        self.send_frame(&msg)
    }

    /// 订阅事件流（stopped/continued/output/thread/terminated 及未归类事件）。
    ///
    /// 每次调用返回新的广播订阅，从订阅时刻起接收事件；v1 不主动推送事件，
    /// 后续动作（`stack_trace` 等）读取最新状态。
    #[allow(clippy::unused_async)] // 契约签名要求 async（返回广播订阅句柄）
    pub async fn events(&self) -> broadcast::Receiver<DapEvent> {
        self.inner.events_tx.subscribe()
    }

    /// 关闭会话：尽力 `disconnect` → 关写通道 → 杀子进程 → 释放待响应请求。
    pub async fn close(&self) {
        // 尽力断开（短超时；失败静默，直接杀进程）
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.request("disconnect", json!({ "restart": false, "terminateDebuggee": true })),
        )
        .await;
        if let Some(tx) = take_write_tx(&self.inner) {
            drop(tx); // 写任务退出 → stdin 关闭 → 适配器看到 EOF
        }
        let child = self.inner.child.lock().await.take();
        if let Some(mut child) = child {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.inner.closed.store(true, Ordering::SeqCst);
        fail_all_pending(&self.inner, "会话已关闭").await;
    }

    /// 握手：`initialize` 请求 → 等待 `initialized` 事件 → `launch` 请求。
    async fn handshake(&self, launch_args: Value) -> Result<(), DapError> {
        let mut events = self.events().await; // 先订阅，避免漏掉 initialized
        let init_args = json!({
            "adapterID": self.spec.name,
            "clientID": "gyre-agent",
            "clientName": "gyre",
            "linesStartAt1": true,
            "columnsStartAt1": true,
            "pathFormat": "path",
            "supportsVariableType": true,
            "supportsArgsInLocations": true,
        });
        let _capabilities = self.request("initialize", init_args).await?;
        // 等待 initialized 事件（宽容：超时继续 launch，个别适配器不发该事件）
        let wait = tokio::time::timeout(INITIALIZED_TIMEOUT, async {
            loop {
                match events.recv().await {
                    Ok(DapEvent::Other(name, _)) if name == "initialized" => break,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        if wait.await.is_err() {
            debug!(
                "initialize 后 {}ms 未收到 initialized 事件，继续 launch",
                INITIALIZED_TIMEOUT.as_millis()
            );
        }
        self.request("launch", launch_args).await?;
        Ok(())
    }

    fn next_seq(&self) -> i64 {
        self.inner.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn send_frame(&self, msg: &Value) -> Result<(), DapError> {
        let frame = encode_frame(msg)?;
        let tx = self
            .inner
            .write_tx
            .lock()
            .map_err(|_| DapError::Closed("写通道锁损坏".into()))?
            .clone()
            .ok_or_else(|| DapError::Closed("写通道已关闭".into()))?;
        tx.send(frame).map_err(|_| DapError::Closed("写通道已关闭".into()))
    }
}

/// 取走写通道（关闭会话用）。
fn take_write_tx(inner: &SessionInner) -> Option<mpsc::UnboundedSender<Vec<u8>>> {
    inner.write_tx.lock().ok()?.take()
}

/// 写任务：无界通道 → 适配器 stdin；写失败即标记会话关闭。
async fn writer_task<W: AsyncWrite + Unpin>(
    inner: Arc<SessionInner>,
    mut writer: W,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    while let Some(frame) = rx.recv().await {
        if writer.write_all(&frame).await.is_err() || writer.flush().await.is_err() {
            inner.closed.store(true, Ordering::SeqCst);
            break;
        }
    }
}

/// 读任务：适配器 stdout → 帧解码 → response 路由 / 事件广播；EOF 或协议损坏即收尾。
async fn reader_task<R: AsyncRead + Unpin>(mut reader: R, inner: Arc<SessionInner>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => {
                debug!("DAP 读取失败: {e}");
                break;
            }
        }
        match decode_frames(&buf) {
            Ok((frames, consumed)) => {
                buf.drain(..consumed);
                for msg in frames {
                    dispatch_message(&inner, msg).await;
                }
            }
            Err(e) => {
                warn!("DAP 帧解码失败: {e}；终止会话");
                break;
            }
        }
    }
    inner.closed.store(true, Ordering::SeqCst);
    fail_all_pending(&inner, "适配器进程退出").await;
}

/// 单条 DAP 消息分发：response → pending；event → 广播；request（反向）→ 自动回错误。
async fn dispatch_message(inner: &SessionInner, msg: Value) {
    let kind = msg.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "response" => {
            let Some(request_seq) = msg.get("request_seq").and_then(Value::as_i64) else {
                warn!("DAP response 缺 request_seq: {msg}");
                return;
            };
            let sender = inner.pending.lock().await.remove(&request_seq);
            if let Some(sender) = sender {
                let outcome = if msg.get("success").and_then(Value::as_bool).unwrap_or(false) {
                    Ok(msg.get("body").cloned().unwrap_or(Value::Null))
                } else {
                    let detail = msg
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("未知错误");
                    Err(DapError::Protocol(format!("适配器返回错误: {detail}")))
                };
                let _ = sender.send(outcome);
            }
        }
        "event" => {
            let name = msg.get("event").and_then(Value::as_str).unwrap_or_default();
            let body = msg.get("body").cloned().unwrap_or(Value::Null);
            let event = match name {
                "stopped" => DapEvent::Stopped(body),
                "continued" => DapEvent::Continued(body),
                "output" => DapEvent::Output(body),
                "thread" => DapEvent::Thread(body),
                "terminated" => DapEvent::Terminated(body),
                other => DapEvent::Other(other.to_string(), body),
            };
            let _ = inner.events_tx.send(event);
        }
        "request" => {
            // 反向请求（如 runInTerminal）：v1 不支持，回错误响应避免适配器阻塞等待
            let request_seq = msg.get("seq").and_then(Value::as_i64).unwrap_or(0);
            let seq = inner.seq.fetch_add(1, Ordering::SeqCst) + 1;
            let reply = json!({
                "seq": seq,
                "type": "response",
                "request_seq": request_seq,
                "success": false,
                "message": "reverse request not supported by agent-dap",
            });
            let Ok(frame) = encode_frame(&reply) else {
                return;
            };
            if let Ok(guard) = inner.write_tx.lock() {
                if let Some(tx) = guard.as_ref() {
                    let _ = tx.send(frame);
                }
            }
        }
        _ => {}
    }
}

/// 释放所有待响应请求（会话关闭时避免等待超时）。
async fn fail_all_pending(inner: &SessionInner, message: &str) {
    let mut pending = inner.pending.lock().await;
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(DapError::Closed(message.into())));
    }
}

/// 适配器 stderr → `tracing::debug`（适配器自带日志，不进 DAP 帧流）。
async fn drain_stderr<R: AsyncRead + Unpin>(mut stderr: R, adapter: &'static str) {
    let mut chunk = [0u8; 4096];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&chunk[..n]);
                for line in text.lines() {
                    if !line.trim().is_empty() {
                        debug!(adapter, stderr = %line);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, duplex};
    use tokio::sync::Mutex;

    use super::*;

    /// 桩适配器：读帧、按命令回响应（记录命令序列；threads 后附带 stopped 事件）。
    async fn fake_adapter<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
        mut reader: R,
        mut writer: W,
        log: Arc<Mutex<Vec<String>>>,
    ) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let mut seq = 0i64;
        loop {
            let read = reader.read(&mut chunk).await.expect("读取失败");
            if read == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..read]);
            let (frames, consumed) = decode_frames(&buf).expect("帧解码失败");
            buf.drain(..consumed);
            for msg in frames {
                let command = msg
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let request_seq = msg.get("seq").and_then(Value::as_i64).unwrap_or(0);
                log.lock().await.push(command.clone());
                match command.as_str() {
                    "initialize" => {
                        respond(
                            &mut writer,
                            &mut seq,
                            request_seq,
                            json!({ "supportsConfigurationDoneRequest": true }),
                        )
                        .await;
                        emit(&mut writer, &mut seq, "initialized", json!({})).await;
                    }
                    "disconnect" => {
                        respond(&mut writer, &mut seq, request_seq, json!({})).await;
                        return;
                    }
                    "threads" => {
                        respond(
                            &mut writer,
                            &mut seq,
                            request_seq,
                            json!({ "threads": [{ "id": 1, "name": "main" }] }),
                        )
                        .await;
                        emit(
                            &mut writer,
                            &mut seq,
                            "stopped",
                            json!({ "reason": "breakpoint", "threadId": 1 }),
                        )
                        .await;
                    }
                    "setBreakpoints" => {
                        respond(
                            &mut writer,
                            &mut seq,
                            request_seq,
                            json!({ "breakpoints": [{ "verified": true, "line": 3, "id": 7 }] }),
                        )
                        .await;
                    }
                    _ => respond(&mut writer, &mut seq, request_seq, json!({})).await,
                }
            }
        }
    }

    async fn respond<W: AsyncWrite + Unpin>(
        writer: &mut W,
        seq: &mut i64,
        request_seq: i64,
        body: Value,
    ) {
        *seq += 1;
        let msg =
            json!({ "seq": *seq, "type": "response", "request_seq": request_seq, "success": true, "body": body });
        writer
            .write_all(&encode_frame(&msg).expect("编码"))
            .await
            .expect("写失败");
        writer.flush().await.expect("flush 失败");
    }

    async fn emit<W: AsyncWrite + Unpin>(writer: &mut W, seq: &mut i64, event: &str, body: Value) {
        *seq += 1;
        let msg = json!({ "seq": *seq, "type": "event", "event": event, "body": body });
        writer
            .write_all(&encode_frame(&msg).expect("编码"))
            .await
            .expect("写失败");
        writer.flush().await.expect("flush 失败");
    }

    /// 起一个桩适配器会话（duplex 管道，完成 initialize/launch 握手）。
    async fn spawn_fake() -> (DapSession, Arc<Mutex<Vec<String>>>) {
        let (client, server) = duplex(1 << 16);
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = Arc::clone(&log);
        let (server_r, server_w) = tokio::io::split(server);
        tokio::spawn(fake_adapter(server_r, server_w, log2));
        let (client_r, client_w) = tokio::io::split(client);
        let spec = AdapterSpec {
            name: "fake",
            command: "fake".into(),
            args: vec![],
        };
        let session = DapSession::from_io(
            client_r,
            client_w,
            &spec,
            json!({ "request": "launch", "program": "/tmp/fake" }),
        )
        .await
        .expect("握手应成功");
        (session, log)
    }

    #[tokio::test]
    async fn request_response_pairing_and_events() {
        let (session, _log) = spawn_fake().await;
        // 先订阅事件（threads 请求触发适配器发 stopped）
        let mut events = session.events().await;
        // 并发两个请求：各自 id 配对、响应正确路由
        let (r1, r2) = tokio::join!(
            session.request("threads", json!({})),
            session.request(
                "setBreakpoints",
                json!({ "source": { "path": "/tmp/x.rs" }, "breakpoints": [{ "line": 3 }] })
            ),
        );
        let t1 = r1.expect("threads 应成功");
        assert_eq!(t1["threads"][0]["id"], 1);
        let t2 = r2.expect("setBreakpoints 应成功");
        assert_eq!(t2["breakpoints"][0]["id"], 7);
        // stopped 事件可达
        let ev = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("应收到事件")
            .expect("事件通道不应关闭");
        assert!(matches!(ev, DapEvent::Stopped(_)));
        session.close().await;
    }

    #[tokio::test]
    async fn send_sync_fire_and_forget() {
        let (session, log) = spawn_fake().await;
        session
            .send_sync("continue", &json!({ "threadId": 1 }))
            .expect("send_sync 应成功");
        // 写任务异步落地：轮询日志直到适配器收到 continue
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let recorded = log.lock().await.clone();
            if recorded.iter().any(|c| c == "continue") {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "适配器未收到 continue");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // 会话仍可用
        session
            .request("threads", json!({}))
            .await
            .expect("后续请求应成功");
        session.close().await;
    }

    #[tokio::test]
    async fn close_then_request_fails() {
        let (session, _log) = spawn_fake().await;
        session.close().await;
        let err = session
            .request("threads", json!({}))
            .await
            .expect_err("关闭后请求应失败");
        assert!(matches!(err, DapError::Closed(_)));
    }

    /// 集成冒烟：环境存在 lldb-dap 时才真实启动并 launch（/bin/true 快速退出）。
    /// 无 lldb-dap 环境时显式跳过。
    #[tokio::test]
    async fn lldb_dap_launch_smoke() {
        let Some(lldb) = crate::probe::probe_executable(
            "lldb-dap",
            &std::env::var("PATH").unwrap_or_default(),
        ) else {
            eprintln!("跳过：未找到 lldb-dap");
            return;
        };
        if !std::path::Path::new("/bin/true").exists() {
            eprintln!("跳过：无 /bin/true");
            return;
        }
        let spec = AdapterSpec {
            name: "lldb-dap",
            command: lldb.to_string_lossy().into_owned(),
            args: vec![],
        };
        let launch = json!({ "request": "launch", "program": "/bin/true", "stopOnEntry": false });
        let outcome = tokio::time::timeout(
            Duration::from_secs(30),
            async {
                let session = DapSession::spawn(&spec, launch).await?;
                let threads = session.request("threads", json!({})).await?;
                session.close().await;
                Ok::<_, DapError>(threads)
            },
        )
        .await;
        let threads = outcome
            .expect("lldb-dap 冒烟超时（30s）——环境异常")
            .expect("lldb-dap launch 冒烟应成功");
        assert!(threads.get("threads").is_some(), "threads 响应应含 threads 数组");
    }
}
