//! 会话级持久内核池（Python / JavaScript 双语言）：NDJSON 协议、串行化执行、空闲/keep
//! 回收。
//!
//! 每个 `session_key` 按语言各对应一个内核进程（py → `python3 -u -c <driver>`；
//! js → `node --input-type=module -e <driver>`，共享命名空间；会话池内部键加
//! `::py`/`::js` 后缀隔离）。同一会话同一语言的并发 `execute` 经 per-session Mutex
//! 串行化；`keep=false` 执行后立即回收，空闲超过 `EvalSettings::idle_timeout`
//! （后台清扫任务，幂等惰性启动）回收，执行期间被取消（`EvalManager::abort`，两种语言
//! 一并回收）时 kill 进程并注销会话。环回桥 token 按 run 轮换：每次 execute 注册新
//! token 并经请求帧下发内核，run 结束（RunGuard drop）即注销。

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio_util::sync::CancellationToken;

use crate::EvalError;
use crate::bridge::BridgeServer;
use crate::driver::PYTHON_DRIVER;
use crate::driver_js::JS_DRIVER;

/// 内核语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    /// Python（`python3 -u -c PYTHON_DRIVER`）。
    Python,
    /// JavaScript（`node --input-type=module -e JS_DRIVER`，Node ≥18）。
    JavaScript,
}

impl Language {
    /// 会话池内部键后缀（同会话不同语言各自独立内核）。
    const fn session_suffix(self) -> &'static str {
        match self {
            Self::Python => "py",
            Self::JavaScript => "js",
        }
    }
}

/// 解析语言标识：`"py"`/`"python"` → [`Language::Python`]，`"js"`/`"javascript"` →
/// [`Language::JavaScript`]。
fn parse_language(language: &str) -> Result<Language, EvalError> {
    match language {
        "py" | "python" => Ok(Language::Python),
        "js" | "javascript" => Ok(Language::JavaScript),
        other => Err(EvalError::UnsupportedLanguage(other.to_string())),
    }
}

/// 内核就绪帧等待上限。
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// 空闲清扫周期上限（实际取 `idle_timeout` 与 15s 的较小值，下限 1s）。
const SWEEP_INTERVAL_CAP: Duration = Duration::from_secs(15);

/// eval 设置（cli 从 `agent_config::EvalConfig` 字段映射构造）。
#[derive(Debug, Clone)]
pub struct EvalSettings {
    /// Python 解释器路径。
    pub python: String,
    /// Node.js 可执行路径（JS 内核；默认取 PATH 上的 `node`，spawn 时探测）。
    pub node: String,
    /// 内核空闲回收超时；同时作为单次执行的墙钟上限。
    pub idle_timeout: Duration,
}

impl EvalSettings {
    /// 从配置字段构造（`EvalConfig` 未从 `agent_config` 根导出，cli 侧按字段映射）。
    ///
    /// Node 路径默认取 PATH 上的 `node`（启动内核时解析；不可用则 JS 执行返回
    /// [`EvalError::NodeMissing`]，不影响 Python）；显式配置经 [`EvalSettings::with_node`]。
    #[must_use]
    pub fn from_parts(python: impl Into<String>, idle_timeout_secs: u64) -> Self {
        Self {
            python: python.into(),
            node: "node".to_string(),
            idle_timeout: Duration::from_secs(idle_timeout_secs),
        }
    }

    /// 指定 Node.js 可执行路径（映射配置 `eval.node`）。
    #[must_use]
    pub fn with_node(mut self, node: impl Into<String>) -> Self {
        self.node = node.into();
        self
    }
}

/// 一次执行结果：用户代码的 stdout/stderr 与 result/error（错误为 traceback 文本）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvalOutput {
    /// 用户 stdout（print 等）。
    pub stdout: String,
    /// 用户 stderr。
    pub stderr: String,
    /// 成功时的末表达式 repr（无表达式则为 `None`）。
    pub result: Option<String>,
    /// 用户代码异常时的 traceback（已截断）。
    pub error: Option<String>,
}

/// 会话级内核池。
pub struct EvalManager {
    settings: EvalSettings,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    sweep_started: Mutex<bool>,
}

/// 单个会话：busy 锁串行化执行，slot 承载内核生命周期。
struct Session {
    /// 串行化同一会话的并发 execute（busy 标志 + Mutex；空闲清扫持锁判定）。
    busy: AsyncMutex<()>,
    /// 内核槽位（`None` = 已回收/已死，下次 execute 重生）。
    slot: AsyncMutex<Option<Arc<Kernel>>>,
    /// 最近使用时间（空闲回收判定）。
    last_used: Mutex<Instant>,
}

impl Session {
    fn new() -> Self {
        Self {
            busy: AsyncMutex::new(()),
            slot: AsyncMutex::new(None),
            last_used: Mutex::new(Instant::now()),
        }
    }

    /// 回收内核（槽位置空并 kill 进程）。
    async fn kill(&self) {
        let kernel = {
            let mut slot = self.slot.lock().await;
            slot.take()
        };
        if let Some(kernel) = kernel {
            kernel.kill().await;
        }
    }
}

/// 单个内核进程句柄。
struct Kernel {
    /// 子进程（kill 时取出）。
    child: AsyncMutex<Option<Child>>,
    /// stdin 写入端。
    stdin: AsyncMutex<Option<ChildStdin>>,
    /// stdout/stderr 帧读取任务。
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
    /// 内核级取消令牌（kill 时触发）。
    cancel: CancellationToken,
    inner: Arc<Inner>,
}

impl Kernel {
    /// kill 进程并回收句柄：取消令牌 → 失败在途 run → kill 子进程 → abort 读取任务。
    async fn kill(&self) {
        self.cancel.cancel();
        self.inner.dead.store(true, Ordering::SeqCst);
        fail_current(&self.inner, "内核进程已回收");
        let child = self.child.lock().await.take();
        if let Some(mut child) = child {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.stdin.lock().await.take();
        self.stdout_task.abort();
        self.stderr_task.abort();
    }
}

/// 内核共享状态（读取任务与 execute 经此交互）。
struct Inner {
    /// 请求序号。
    next_id: AtomicU64,
    /// 在途执行（id → 缓冲/完成信号）。
    pending: Mutex<HashMap<u64, PendingRun>>,
    /// 当前在途 id（非 NDJSON 原生输出挂到其 stderr）。
    current: Mutex<Option<u64>>,
    /// 内核是否已死（EOF / kill）。
    dead: AtomicBool,
    /// 就绪帧通知（spawn 握手）。
    ready: Mutex<Option<oneshot::Sender<()>>>,
}

/// 一次在途执行的状态。
struct PendingRun {
    stdout: Vec<String>,
    stderr: Vec<String>,
    /// 完成结果：`Ok(output)` / `Err(traceback)`。
    done: Option<Result<Option<String>, String>>,
    /// 完成唤醒（execute 等待端；send 失败说明 execute 已放弃 → 顺手清理）。
    wake: Option<oneshot::Sender<()>>,
}

/// 内核 NDJSON 输出帧。
#[derive(Debug, Clone, PartialEq)]
enum Frame {
    /// 内核就绪。
    Ready,
    /// 用户 stdout 片段。
    Stdout { id: u64, text: String },
    /// 用户 stderr 片段。
    Stderr { id: u64, text: String },
    /// 执行成功。
    Result { id: u64, output: Option<String> },
    /// 执行异常。
    Error { id: u64, message: String },
}

/// 解析内核 NDJSON 帧；非 NDJSON / 未知类型返回 `None`。
fn parse_frame(line: &str) -> Option<Frame> {
    let value: Value = serde_json::from_str(line).ok()?;
    let kind = value.get("type")?.as_str()?;
    let id = value.get("id").and_then(Value::as_u64);
    match kind {
        "ready" => Some(Frame::Ready),
        "stdout" => Some(Frame::Stdout {
            id: id?,
            text: value.get("text")?.as_str()?.to_string(),
        }),
        "stderr" => Some(Frame::Stderr {
            id: id?,
            text: value.get("text")?.as_str()?.to_string(),
        }),
        "result" => Some(Frame::Result {
            id: id?,
            output: output_of(&value),
        }),
        "error" => Some(Frame::Error {
            id: id?,
            message: value.get("message")?.as_str()?.to_string(),
        }),
        _ => None,
    }
}

/// 提取 result 帧的 output：null/缺失 → `None`；字符串按原样；其余 `to_string`。
fn output_of(value: &Value) -> Option<String> {
    value.get("output").and_then(|o| {
        if o.is_null() {
            None
        } else if let Some(s) = o.as_str() {
            Some(s.to_string())
        } else {
            Some(o.to_string())
        }
    })
}

/// 编码请求帧：`{"id":N,"code":"…","token":"…"}\n`（token 按 run 轮换下发）。
fn encode_request(id: u64, code: &str, token: &str) -> String {
    let mut line = serde_json::to_string(&json!({"id": id, "code": code, "token": token}))
        .expect("请求帧序列化不可能失败");
    line.push('\n');
    line
}

impl EvalManager {
    /// 构造管理器（空闲清扫任务由 [`EvalManager::ensure_sweeper`] 惰性启动）。
    #[must_use]
    pub fn new(settings: EvalSettings) -> Self {
        Self {
            settings,
            sessions: Mutex::new(HashMap::new()),
            sweep_started: Mutex::new(false),
        }
    }

    /// 幂等启动空闲清扫任务（需 `Arc<Self>` 以持弱引用，随管理器 drop 退出）。
    pub fn ensure_sweeper(self: &Arc<Self>) {
        let mut started = self
            .sweep_started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *started {
            return;
        }
        *started = true;
        let weak = Arc::downgrade(self);
        let interval = self
            .settings
            .idle_timeout
            .clamp(Duration::from_secs(1), SWEEP_INTERVAL_CAP);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // 首个 tick 立即返回，先让出
            loop {
                ticker.tick().await;
                let Some(mgr) = weak.upgrade() else {
                    break;
                };
                mgr.sweep_idle().await;
            }
        });
    }

    /// 清扫空闲会话：超过 `idle_timeout` 未使用且当前不忙（busy 锁可获取）则回收。
    //
    // busy guard 即串行化手段，必须跨回收 await 持有（持锁回收杜绝与 execute 竞态）。
    #[allow(clippy::await_holding_lock)]
    async fn sweep_idle(&self) {
        let now = Instant::now();
        let idle = self.settings.idle_timeout;
        let keys: Vec<String> = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        for key in keys {
            let session = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&key)
                .cloned();
            let Some(session) = session else {
                continue;
            };
            // busy 锁可获取 = 空闲；持锁回收杜绝与 execute 竞态。
            if let Ok(guard) = session.busy.try_lock() {
                let last = *session
                    .last_used
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if now.saturating_duration_since(last) > idle {
                    self.reap_session(&key).await;
                }
                drop(guard);
            }
        }
    }

    /// 从池中移除并回收会话。
    async fn reap_session(&self, key: &str) {
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
        if let Some(session) = session {
            session.kill().await;
        }
    }

    /// 中止并回收会话的**全部语言**内核（执行被取消/中断时调用）。
    pub async fn abort(&self, session_key: &str) {
        for language in [Language::Python, Language::JavaScript] {
            self.reap_session(&format!("{session_key}::{}", language.session_suffix()))
                .await;
        }
    }

    /// 在 `session_key` 会话以指定语言执行一段代码。
    ///
    /// `language` 支持 `"py"`/`"python"` 与 `"js"`/`"javascript"`（同一会话两种语言
    /// 各自独立内核）；`keep=false` 执行后立即回收内核；环回桥 token 按本次 run
    /// 轮换注册（结束即注销）。
    ///
    /// # Errors
    /// 内核缺失/启动失败/超时/死亡等子系统故障返回 [`EvalError`]；用户代码异常
    /// 经 [`EvalOutput::error`] 返回（不视为子系统错误）。
    //
    // busy guard 即「串行化同一会话并发 execute」的实现，必须跨整个 run 持有。
    #[allow(clippy::await_holding_lock)]
    pub async fn execute(
        &self,
        session_key: &str,
        language: &str,
        code: &str,
        keep: bool,
        bridge: &BridgeServer,
    ) -> Result<EvalOutput, EvalError> {
        let language = parse_language(language)?;
        // 语言维度隔离：会话池内部键加语言后缀，同会话 py/js 互不干扰。
        let pool_key = format!("{session_key}::{}", language.session_suffix());
        let session = {
            let mut map = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(
                map.entry(pool_key.clone())
                    .or_insert_with(|| Arc::new(Session::new())),
            )
        };
        // 串行化同一会话的并发 execute。
        let _busy = session.busy.lock().await;
        *session
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
        let kernel = self.ensure_kernel(&session, bridge, language).await?;
        let result = run_once(&kernel, code, self.settings.idle_timeout, bridge).await;
        *session
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
        if !keep {
            self.reap_session(&pool_key).await;
        }
        result
    }

    /// 确保会话持有存活内核（死内核回收后重生；busy 串行保证无并发换核竞态）。
    async fn ensure_kernel(
        &self,
        session: &Arc<Session>,
        bridge: &BridgeServer,
        language: Language,
    ) -> Result<Arc<Kernel>, EvalError> {
        {
            let slot = session.slot.lock().await;
            if let Some(kernel) = slot.as_ref() {
                if !kernel.inner.dead.load(Ordering::SeqCst) {
                    return Ok(Arc::clone(kernel));
                }
            }
        }
        let kernel = spawn_kernel(&self.settings, bridge.addr(), language).await?;
        let old = {
            let mut slot = session.slot.lock().await;
            slot.replace(Arc::clone(&kernel))
        };
        if let Some(old) = old {
            old.kill().await;
        }
        Ok(kernel)
    }
}

async fn spawn_kernel(
    settings: &EvalSettings,
    bridge_addr: SocketAddr,
    language: Language,
) -> Result<Arc<Kernel>, EvalError> {
    // python → `python3 -u -c DRIVER`；js → `node --input-type=module -e DRIVER`
    // （driver 短小，经命令行传入安全；两种内核协议完全同构）。
    let mut cmd = match language {
        Language::Python => {
            let mut cmd = tokio::process::Command::new(&settings.python);
            cmd.arg("-u").arg("-c").arg(PYTHON_DRIVER);
            cmd
        }
        Language::JavaScript => {
            let mut cmd = tokio::process::Command::new(&settings.node);
            cmd.arg("--input-type=module").arg("-e").arg(JS_DRIVER);
            cmd
        }
    };
    cmd.env("GYRE_BRIDGE_URL", bridge_url(bridge_addr))
        .env("GYRE_BRIDGE_TOKEN", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            match language {
                Language::Python => EvalError::PythonMissing(settings.python.clone()),
                Language::JavaScript => EvalError::NodeMissing(settings.node.clone()),
            }
        } else {
            EvalError::KernelSpawnFailed(e.to_string())
        }
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| EvalError::KernelSpawnFailed("无法获取内核 stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| EvalError::KernelSpawnFailed("无法获取内核 stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| EvalError::KernelSpawnFailed("无法获取内核 stderr".into()))?;
    let (ready_tx, ready_rx) = oneshot::channel();
    let inner = Arc::new(Inner {
        next_id: AtomicU64::new(0),
        pending: Mutex::new(HashMap::new()),
        current: Mutex::new(None),
        dead: AtomicBool::new(false),
        ready: Mutex::new(Some(ready_tx)),
    });
    let kernel = Arc::new(Kernel {
        child: AsyncMutex::new(Some(child)),
        stdin: AsyncMutex::new(Some(stdin)),
        stdout_task: tokio::spawn(read_frames(BufReader::new(stdout), Arc::clone(&inner))),
        stderr_task: tokio::spawn(read_frames(BufReader::new(stderr), Arc::clone(&inner))),
        cancel: CancellationToken::new(),
        inner: Arc::clone(&inner),
    });
    tokio::select! {
        biased;
        _ = ready_rx => {}
        () = kernel.cancel.cancelled() => {
            kernel.kill().await;
            return Err(EvalError::Canceled);
        }
        () = tokio::time::sleep(READY_TIMEOUT) => {
            kernel.kill().await;
            return Err(EvalError::KernelSpawnFailed(format!(
                "内核就绪超时（{}s）",
                READY_TIMEOUT.as_secs()
            )));
        }
    }
    Ok(kernel)
}

/// 构造环回桥 URL（与 `bridge::bridge_url` 保持一致，供内核 env 注入）。
fn bridge_url(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

/// 执行一次 run：注册桥 token → 下发请求帧 → 等待完成/取消/超时。
///
/// stdin 写入天然需要持锁跨 await，且 busy 串行已保证同一内核同时至多一个写者。
#[allow(clippy::await_holding_lock)]
async fn run_once(
    kernel: &Arc<Kernel>,
    code: &str,
    run_timeout: Duration,
    bridge: &BridgeServer,
) -> Result<EvalOutput, EvalError> {
    let id = kernel.inner.next_id.fetch_add(1, Ordering::SeqCst) + 1;
    // per-run 轮换 token：注册于桥，经请求帧下发内核，run 结束（guard drop）注销。
    let token = uuid::Uuid::new_v4().simple().to_string();
    let run_cancel = CancellationToken::new();
    let _guard = bridge.register_run(token.clone(), run_cancel.clone()).await;
    let (wake_tx, mut wake_rx) = oneshot::channel();
    kernel
        .inner
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            id,
            PendingRun {
                stdout: Vec::new(),
                stderr: Vec::new(),
                done: None,
                wake: Some(wake_tx),
            },
        );
    *kernel
        .inner
        .current
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id);
    let frame = encode_request(id, code, &token);
    let write_result = {
        let mut guard = kernel.stdin.lock().await;
        match guard.as_mut() {
            Some(stdin) => stdin
                .write_all(frame.as_bytes())
                .await
                .map_err(|e| e.kind()),
            None => Err(io::ErrorKind::BrokenPipe),
        }
    };
    if let Err(kind) = write_result {
        *kernel
            .inner
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        kernel
            .inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        if kernel.inner.dead.load(Ordering::SeqCst) {
            return Err(EvalError::KernelDied);
        }
        return Err(EvalError::Io(io::Error::new(kind, "写入内核 stdin 失败")));
    }
    let outcome = tokio::select! {
        biased;
        r = &mut wake_rx => match r {
            Ok(()) => {
                let entry = kernel
                    .inner
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id);
                match entry {
                    None => Err(EvalError::KernelDied),
                    Some(e) if e.done.is_none() => Err(EvalError::KernelDied),
                    Some(e) => {
                        let stdout = e.stdout.concat();
                        let stderr = e.stderr.concat();
                        match e.done.expect("已由守卫保证") {
                            Ok(output) => Ok(EvalOutput {
                                stdout,
                                stderr,
                                result: output,
                                error: None,
                            }),
                            Err(message) => Ok(EvalOutput {
                                stdout,
                                stderr,
                                result: None,
                                error: Some(message),
                            }),
                        }
                    }
                }
            }
            Err(_) => Err(EvalError::KernelDied),
        },
        () = kernel.cancel.cancelled() => Err(EvalError::Canceled),
        () = tokio::time::sleep(run_timeout) => {
            kernel.kill().await;
            Err(EvalError::Timeout(run_timeout))
        }
    };
    // 收尾：清 current/pending（取消/超时路径）。
    *kernel
        .inner
        .current
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    kernel
        .inner
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&id);
    outcome
}

/// 逐行读取内核流并分发 NDJSON 帧；EOF/错误 → 标记死亡并失败在途 run。
async fn read_frames<R>(mut reader: R, inner: Arc<Inner>)
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_frame(trimmed) {
            Some(frame) => handle_frame(&frame, &inner),
            // 非 NDJSON 原生输出（用户 os.write 等）挂到当前 run 的 stderr。
            None => push_raw(&inner, trimmed),
        }
    }
    inner.dead.store(true, Ordering::SeqCst);
    fail_current(&inner, "内核进程意外退出");
}

/// 分发单帧。
fn handle_frame(frame: &Frame, inner: &Arc<Inner>) {
    match frame {
        Frame::Ready => {
            if let Some(tx) = inner
                .ready
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                let _ = tx.send(());
            }
        }
        Frame::Stdout { id, text } => {
            if let Some(p) = inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(id)
            {
                p.stdout.push(text.clone());
            }
        }
        Frame::Stderr { id, text } => {
            if let Some(p) = inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(id)
            {
                p.stderr.push(text.clone());
            }
        }
        Frame::Result { id, output } => complete(inner, *id, Ok(output.clone())),
        Frame::Error { id, message } => complete(inner, *id, Err(message.clone())),
    }
}

/// 完成一个 run：写入 done 并唤醒 execute；唤醒失败（execute 已放弃）则顺手清理。
fn complete(inner: &Arc<Inner>, id: u64, done: Result<Option<String>, String>) {
    let remove = {
        let mut pending = inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match pending.get_mut(&id) {
            None => false,
            Some(p) => {
                p.done = Some(done);
                match p.wake.take() {
                    Some(tx) => tx.send(()).is_err(),
                    None => false,
                }
            }
        }
    };
    if remove {
        inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }
}

/// 失败当前在途 run（内核死亡/回收路径）。
fn fail_current(inner: &Arc<Inner>, message: &str) {
    let id = inner
        .current
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(id) = id {
        complete(inner, id, Err(message.to_string()));
    }
}

/// 非 NDJSON 原生输出挂到当前 run 的 stderr。
fn push_raw(inner: &Arc<Inner>, text: &str) {
    let id = *inner
        .current
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(id) = id {
        if let Some(p) = inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&id)
        {
            p.stderr.push(text.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 请求帧编码往返。
    #[test]
    fn request_frame_round_trip() {
        let line = encode_request(7, "print('hi')", "tok123");
        assert!(line.ends_with('\n'));
        let value: Value = serde_json::from_str(line.trim()).expect("帧应可解析");
        assert_eq!(value["id"], 7);
        assert_eq!(value["code"], "print('hi')");
        assert_eq!(value["token"], "tok123");
    }

    /// 输出帧解析（含 null output 与垃圾行）。
    #[test]
    fn frame_parsing() {
        assert_eq!(parse_frame(r#"{"type":"ready"}"#), Some(Frame::Ready));
        assert_eq!(
            parse_frame(r#"{"type":"stdout","id":3,"text":"hi\n"}"#),
            Some(Frame::Stdout {
                id: 3,
                text: "hi\n".into()
            })
        );
        assert_eq!(
            parse_frame(r#"{"type":"stderr","id":3,"text":"oops"}"#),
            Some(Frame::Stderr {
                id: 3,
                text: "oops".into()
            })
        );
        assert_eq!(
            parse_frame(r#"{"type":"result","id":3,"ok":true,"output":"2"}"#),
            Some(Frame::Result {
                id: 3,
                output: Some("2".into())
            })
        );
        assert_eq!(
            parse_frame(r#"{"type":"result","id":3,"ok":true,"output":null}"#),
            Some(Frame::Result {
                id: 3,
                output: None
            })
        );
        assert_eq!(
            parse_frame(r#"{"type":"error","id":3,"message":"boom"}"#),
            Some(Frame::Error {
                id: 3,
                message: "boom".into()
            })
        );
        assert_eq!(parse_frame("not json"), None);
        assert_eq!(parse_frame(r#"{"type":"bogus","id":1}"#), None);
    }

    /// 语言解析：别名归一与不支持语言报错。
    #[test]
    fn language_parsing() {
        assert!(matches!(parse_language("py"), Ok(Language::Python)));
        assert!(matches!(parse_language("python"), Ok(Language::Python)));
        assert!(matches!(parse_language("js"), Ok(Language::JavaScript)));
        assert!(matches!(
            parse_language("javascript"),
            Ok(Language::JavaScript)
        ));
        match parse_language("rb") {
            Err(EvalError::UnsupportedLanguage(lang)) => assert_eq!(lang, "rb"),
            other => panic!("应返回 UnsupportedLanguage：{other:?}"),
        }
    }

    /// keep=false 语义：执行后会话被回收（不 spawn 真内核，直接操作池）。
    #[tokio::test]
    async fn reap_removes_session() {
        let mgr = Arc::new(EvalManager::new(EvalSettings {
            python: "python3".into(),
            node: "node".into(),
            idle_timeout: Duration::from_secs(60),
        }));
        let key = "unit|sess::py";
        mgr.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.to_string(), Arc::new(Session::new()));
        assert!(
            mgr.sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(key)
        );
        mgr.reap_session(key).await;
        assert!(
            !mgr.sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(key)
        );
    }
}
