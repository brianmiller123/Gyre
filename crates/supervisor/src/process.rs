//! 进程托管（进程内最小实现）：对齐 oh-my-pi hub 工具的 launch 面
//! （start / ps / logs / send / stop / restart / describe）。
//!
//! 与 omp broker 守护进程实现的有意偏差（本 crate 不引 broker）：
//! - 无 broker 子进程：[`ProcessManager`] 直接用 `tokio::process` 拉起并看护；
//! - 无磁盘持久化、无 detached/persist 生命周期：记录仅存活于本进程内存；
//! - 无 PTY：stdin/stdout/stderr 均为管道（方向键等只是字节序列，无行编辑语义）；
//! - 进程退出不主动推送消息：调用方用 [`ProcessManager::logs`]（follow）或
//!   [`ProcessManager::ps`] 轮询。
//!
//! 启动即独立进程组（unix `process_group(0)`）：stop/signal 对整组发送信号，
//! 覆盖「服务拉起子进程」的常见形态（与 browser 工具击杀 chromium 同一套路）。

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use regex::Regex;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// 进程名规则（对齐 omp：字母数字开头，允许 `._-`，最长 48 字符）。
const NAME_PATTERN: &str = r"^[a-zA-Z0-9][a-zA-Z0-9._-]{0,47}$";

/// 每进程日志环形缓冲容量（行）；超限丢弃最旧行，`seq` 单调不回退。
const LOG_CAP: usize = 10_000;

/// 状态/退出/就绪轮询周期。
const POLL: Duration = Duration::from_millis(20);

/// 日志 follow / 就绪等待的检查周期。
const READY_TICK: Duration = Duration::from_millis(25);

/// 自动重启退避序列（毫秒）：500ms→1s→2s→4s→8s，5 次上限。
const DEFAULT_BACKOFF_MS: [u64; 5] = [500, 1_000, 2_000, 4_000, 8_000];

/// `stop` 默认宽限秒数（SIGTERM → 等 → SIGKILL）。
const DEFAULT_STOP_GRACE_SECS: u64 = 5;

/// 进程托管错误（中文 Display）。
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// 目标进程不在册或已终结。
    #[error("进程 {0} 不存在或已退出")]
    NotFound(String),
    /// 同名进程仍在运行。
    #[error("进程 {0} 已在运行")]
    AlreadyRunning(String),
    /// 进程名不符合规则。
    #[error("进程名非法：{0}（须匹配 ^[a-zA-Z0-9][a-zA-Z0-9._-]{{0,47}}$）")]
    InvalidName(String),
    /// 参数形状/取值非法。
    #[error("参数非法：{0}")]
    InvalidArgs(String),
    /// 拉起进程失败（二进制不存在、权限等）。
    #[error("进程启动失败：{0}")]
    Spawn(#[from] std::io::Error),
    /// 运行期 IO 失败（stdin 写入、序列化等）。
    #[error("进程 IO 失败：{0}")]
    Io(String),
    /// 当前平台不支持的能力（如非 unix 的信号语义）。
    #[error("暂不支持：{0}")]
    Unsupported(String),
}

/// 重启策略。显式 `stop` 不计为失败，不会触发重启。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// 不重启。
    #[default]
    No,
    /// 非零退出（含信号终止）才重启。
    OnFailure,
    /// 任何退出都重启。
    Always,
}

/// 就绪条件：`log` 正则与 `port` 端口须全部满足（未设置的条件视为已满足）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadySpec {
    /// 就绪日志正则（Rust regex；编译失败 → start 报错）。
    pub log: Option<String>,
    /// 就绪端口（对 127.0.0.1 探测 TCP 连接）。
    pub port: Option<u16>,
    /// 就绪超时秒数（默认 30；超时 → 状态 Failed，进程保留可 stop）。
    pub timeout_secs: u64,
}

impl Default for ReadySpec {
    fn default() -> Self {
        Self {
            log: None,
            port: None,
            timeout_secs: 30,
        }
    }
}

/// 启动规格（`describe` 原样回显）。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessSpec {
    /// 进程名（唯一键；live 重名拒绝，completed 重名替换记录）。
    pub name: String,
    /// 可执行文件。
    pub application: String,
    /// 参数列表。
    pub args: Vec<String>,
    /// 工作目录（缺省继承当前目录）。
    pub cwd: Option<PathBuf>,
    /// 附加环境变量（叠加在继承环境之上）。
    pub env: BTreeMap<String, String>,
    /// 就绪条件。
    pub ready: ReadySpec,
    /// 重启策略。
    pub restart: RestartPolicy,
}

/// 日志流。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    /// 标准输出。
    Stdout,
    /// 标准错误。
    Stderr,
}

impl LogStream {
    /// 人类可读标签。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// 单行进程日志（`seq` 为进程内单调行序号，cursor 即按它翻页）。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessLogLine {
    /// 行序号（从 1 起，跨重启累计）。
    pub seq: u64,
    /// 来源流。
    pub stream: LogStream,
    /// 行文本（已去行尾换行）。
    pub text: String,
}

/// 进程生命周期状态。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    /// 拉起中 / 等待就绪 / 重启退避中。
    Starting,
    /// 就绪条件已全部满足。
    Running,
    /// 已退出（`code` 为退出码；被信号终止时为 `None`）。
    Exited {
        /// 退出码。
        code: Option<i32>,
    },
    /// 失败（就绪超时或重启预算耗尽；附中文原因）。
    Failed {
        /// 原因。
        reason: String,
    },
    /// 被显式 stop 终止（不算 failure，不触发重启）。
    Stopped,
}

impl ProcessState {
    /// 是否终态（不会再变化）。
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Exited { .. } | Self::Failed { .. } | Self::Stopped
        )
    }
}

/// 单进程状态快照（`ps` 行）。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessStatus {
    /// 进程名。
    pub name: String,
    /// 最近一次拉起的 pid。
    pub pid: Option<u32>,
    /// 生命周期状态。
    pub state: ProcessState,
    /// 最近一次拉起时间（毫秒 epoch）。
    pub started_at: u64,
    /// 自动重启次数（手动 restart 归零）。
    pub restarts: u32,
}

/// `start` / `restart` 的返回信息。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessInfo {
    /// 进程名。
    pub name: String,
    /// pid（拉起失败时为 `None`）。
    pub pid: Option<u32>,
    /// 本次拉起时间（毫秒 epoch）。
    pub started_at: u64,
}

/// `logs` 返回的一页日志。
#[derive(Debug, Clone)]
pub struct LogPage {
    /// 续读游标：传回 `logs` 的 `cursor` 即可接着读。
    pub cursor: u64,
    /// 本页日志行（按 `seq` 升序）。
    pub lines: Vec<ProcessLogLine>,
}

/// 信号（对齐 omp launch 的 signal 枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGINT。
    Sigint,
    /// SIGTERM。
    Sigterm,
    /// SIGHUP。
    Sighup,
    /// SIGQUIT。
    Sigquit,
    /// SIGKILL。
    Sigkill,
}

impl Signal {
    /// 从 `SIGINT` 等字符串解析（大小写不敏感）。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim().to_ascii_uppercase();
        match s.as_str() {
            "SIGINT" => Some(Self::Sigint),
            "SIGTERM" => Some(Self::Sigterm),
            "SIGHUP" => Some(Self::Sighup),
            "SIGQUIT" => Some(Self::Sigquit),
            "SIGKILL" => Some(Self::Sigkill),
            _ => None,
        }
    }
}

#[cfg(unix)]
impl From<Signal> for nix::sys::signal::Signal {
    fn from(s: Signal) -> Self {
        match s {
            Signal::Sigint => Self::SIGINT,
            Signal::Sigterm => Self::SIGTERM,
            Signal::Sighup => Self::SIGHUP,
            Signal::Sigquit => Self::SIGQUIT,
            Signal::Sigkill => Self::SIGKILL,
        }
    }
}

/// 进程托管管理器：进程内注册表 + 看护任务。
///
/// `Clone` 廉价（内部 `Arc`）；所有方法可从异步上下文直接调用。
#[derive(Clone)]
pub struct ProcessManager {
    shared: Arc<Shared>,
}

struct Shared {
    procs: Mutex<BTreeMap<String, Arc<ProcEntry>>>,
    backoff_ms: [u64; 5],
}

/// 本轮拉起的就绪标志（每轮 incarnation 独立）。
struct ReadyFlags {
    log: AtomicBool,
    port: AtomicBool,
}

/// 单进程在册记录。
struct ProcEntry {
    spec: ProcessSpec,
    /// 已编译的 ready.log 正则（start 时校验，避免看护路径重复编译）。
    ready_log: Option<Regex>,
    /// 进程 stdin（管道；send 写入）。
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    core: Mutex<ProcCore>,
}

/// 进程可变状态。锁为短临界区：任何 await 前必须释放。
struct ProcCore {
    /// 拉起代数：每次 spawn 自增；看护任务据此识别自己是否已被 replace。
    incarnation: u64,
    /// 最近一次拉起的 pid。
    pid: Option<u32>,
    /// 最近一次拉起时间（毫秒 epoch）。
    started_at: u64,
    /// 自动重启次数。
    restarts: u32,
    /// 生命周期状态。
    state: ProcessState,
    /// 子进程是否仍在运行（退出观察由看护任务负责）。
    alive: bool,
    /// 用户显式 stop 请求（不再重启）。
    stop_requested: bool,
    /// 当前子进程（带 incarnation 代标，防止旧看护任务误触新进程）。
    child: Option<(u64, tokio::process::Child)>,
    /// 日志环形缓冲。
    lines: VecDeque<ProcessLogLine>,
    /// 下一条日志的 seq。
    next_seq: u64,
}

/// 短临界区取锁； panic 后的毒锁按内部值继续（状态机自恢复，无谓连带崩溃）。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 进程名规则编译产物。
static NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(NAME_PATTERN).expect("内置进程名正则必然合法"));

/// 当前时间戳（毫秒 epoch）；时钟不可用时回退 0。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// unix：向进程组发信号（子进程以 `process_group(0)` 独立成组，组 id 即 pid）。
#[cfg(unix)]
fn kill_group(pid: u32, sig: nix::sys::signal::Signal) {
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), sig);
}

impl ProcEntry {
    fn new(spec: ProcessSpec, ready_log: Option<Regex>) -> Self {
        Self {
            spec,
            ready_log,
            stdin: tokio::sync::Mutex::new(None),
            core: Mutex::new(ProcCore {
                incarnation: 0,
                pid: None,
                started_at: now_ms(),
                restarts: 0,
                state: ProcessState::Starting,
                alive: false,
                stop_requested: false,
                child: None,
                lines: VecDeque::new(),
                next_seq: 1,
            }),
        }
    }

    fn status(&self) -> ProcessStatus {
        let c = lock(&self.core);
        ProcessStatus {
            name: self.spec.name.clone(),
            pid: c.pid,
            state: c.state.clone(),
            started_at: c.started_at,
            restarts: c.restarts,
        }
    }
}

/// 记录是否「活跃」：仍占名字（live 重名拒绝 start）。
fn is_live(c: &ProcCore) -> bool {
    c.alive || matches!(c.state, ProcessState::Starting | ProcessState::Running)
}

impl ProcessManager {
    /// 构造（重启退避 500ms→8s，5 次上限）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                procs: Mutex::new(BTreeMap::new()),
                backoff_ms: DEFAULT_BACKOFF_MS,
            }),
        }
    }

    /// 测试钩子：缩短重启退避（仅测试构建存在）。
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_backoff_ms(backoff_ms: [u64; 5]) -> Self {
        Self {
            shared: Arc::new(Shared {
                procs: Mutex::new(BTreeMap::new()),
                backoff_ms,
            }),
        }
    }

    fn lookup(&self, name: &str) -> Result<Arc<ProcEntry>, ProcessError> {
        lock(&self.shared.procs)
            .get(name)
            .cloned()
            .ok_or_else(|| ProcessError::NotFound(name.to_string()))
    }

    /// 拉起进程并阻塞至就绪条件全部满足 / 超时 / 提前退出。
    ///
    /// - 名称须匹配 `^[a-zA-Z0-9][a-zA-Z0-9._-]{0,47}$`；
    /// - live 重名 → [`ProcessError::AlreadyRunning`]；completed 重名 → 替换记录；
    /// - 就绪超时 → 状态 `Failed` 但进程保留，可 `stop`；
    /// - 拉起本身失败 → 记录不入册，返回 [`ProcessError::Spawn`]。
    pub async fn start(&self, spec: ProcessSpec) -> Result<ProcessInfo, ProcessError> {
        if !NAME_RE.is_match(&spec.name) {
            return Err(ProcessError::InvalidName(spec.name.clone()));
        }
        if spec.application.trim().is_empty() {
            return Err(ProcessError::InvalidArgs(
                "start 需要 application 参数".into(),
            ));
        }
        if spec.ready.port.is_some_and(|p| p == 0) {
            return Err(ProcessError::InvalidArgs("ready.port 须为 1-65535".into()));
        }
        let ready_log =
            match spec.ready.log.as_deref() {
                Some(p) if !p.trim().is_empty() => Some(Regex::new(p).map_err(|e| {
                    ProcessError::InvalidArgs(format!("ready.log 正则编译失败：{e}"))
                })?),
                _ => None,
            };
        let name = spec.name.clone();
        let entry = Arc::new(ProcEntry::new(spec, ready_log));
        {
            let mut map = lock(&self.shared.procs);
            if let Some(existing) = map.get(&name) {
                if is_live(&lock(&existing.core)) {
                    return Err(ProcessError::AlreadyRunning(name));
                }
            }
            map.insert(name.clone(), Arc::clone(&entry));
        }
        if let Err(e) = spawn_incarnation(&entry).await {
            // 拉起失败：移除占位记录，不留半成品。
            let mut map = lock(&self.shared.procs);
            if map.get(&name).is_some_and(|cur| Arc::ptr_eq(cur, &entry)) {
                map.remove(&name);
            }
            return Err(e);
        }
        tokio::spawn(life_task(Arc::clone(&self.shared), Arc::clone(&entry)));
        wait_ready(&entry).await;
        let c = lock(&entry.core);
        Ok(ProcessInfo {
            name,
            pid: c.pid,
            started_at: c.started_at,
        })
    }

    /// 全部进程快照（按名称排序）。
    #[must_use]
    pub async fn ps(&self) -> Vec<ProcessStatus> {
        let entries: Vec<_> = lock(&self.shared.procs).values().cloned().collect();
        entries.iter().map(|e| e.status()).collect()
    }

    /// 读取进程日志。
    ///
    /// - `cursor`：上次返回的行序号，只取其后的行（缺省 0 = 从头）；
    /// - `grep`：行过滤正则（Rust regex，编译失败报错）；
    /// - `head`：true 取匹配区段前 `lines` 行，false（默认）取尾部；
    /// - `lines`：行数上限（0 = 默认 100，封顶 1000）；
    /// - `follow`：无新行时等待至 `timeout_secs`。
    #[allow(clippy::too_many_arguments)] // 日志读取参数面，对齐 omp hub logs 的查询形态。
    pub async fn logs(
        &self,
        name: &str,
        cursor: Option<u64>,
        grep: Option<String>,
        head: bool,
        lines: usize,
        follow: bool,
        timeout_secs: u64,
    ) -> Result<LogPage, ProcessError> {
        let entry = self.lookup(name)?;
        let re = match grep.as_deref() {
            Some(p) if !p.trim().is_empty() => Some(
                Regex::new(p)
                    .map_err(|e| ProcessError::InvalidArgs(format!("grep 正则编译失败：{e}")))?,
            ),
            _ => None,
        };
        let limit = if lines == 0 { 100 } else { lines.min(1_000) };
        let from = cursor.unwrap_or(0);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            let page = collect_page(&entry, from, re.as_ref(), head, limit);
            if !page.lines.is_empty() || !follow {
                return Ok(page);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(page);
            }
            tokio::time::sleep(READY_TICK).await;
        }
    }

    /// 向进程 stdin 写入 `text`（`enter` 时追加换行）与按键序列。
    pub async fn send(
        &self,
        name: &str,
        text: &str,
        enter: bool,
        keys: &[String],
    ) -> Result<(), ProcessError> {
        let entry = self.lookup(name)?;
        let mut payload = Vec::with_capacity(text.len() + keys.len() * 2);
        payload.extend_from_slice(text.as_bytes());
        if enter && !text.is_empty() {
            payload.push(b'\n');
        }
        for k in keys {
            let bytes = key_bytes(k).ok_or_else(|| {
                ProcessError::InvalidArgs(format!(
                    "不支持的按键 {k}（支持 ENTER TAB ESCAPE CTRL_C CTRL_D UP DOWN LEFT RIGHT）"
                ))
            })?;
            payload.extend_from_slice(bytes);
        }
        if payload.is_empty() {
            return Err(ProcessError::InvalidArgs(
                "send 无可写内容（text 为空且 keys 为空）".into(),
            ));
        }
        let mut guard = entry.stdin.lock().await;
        let stdin = guard.as_mut().ok_or_else(|| {
            ProcessError::InvalidArgs("进程 stdin 不可用（进程可能已退出）".into())
        })?;
        stdin
            .write_all(&payload)
            .await
            .map_err(|e| ProcessError::Io(e.to_string()))?;
        stdin
            .flush()
            .await
            .map_err(|e| ProcessError::Io(e.to_string()))?;
        Ok(())
    }

    /// 向进程组发送信号（unix killpg；非 unix 仅支持 SIGKILL）。
    pub async fn signal(&self, name: &str, sig: Signal) -> Result<(), ProcessError> {
        let entry = self.lookup(name)?;
        let pid = {
            let c = lock(&entry.core);
            if !c.alive {
                return Err(ProcessError::NotFound(name.to_string()));
            }
            c.pid
        };
        let Some(pid) = pid else {
            return Err(ProcessError::NotFound(name.to_string()));
        };
        #[cfg(unix)]
        kill_group(pid, sig.into());
        #[cfg(not(unix))]
        {
            let _ = pid;
            if sig == Signal::Sigkill {
                let mut c = lock(&entry.core);
                if let Some((_, child)) = c.child.as_mut() {
                    let _ = child.start_kill();
                }
            } else {
                return Err(ProcessError::Unsupported(
                    "非 unix 平台仅支持 SIGKILL".into(),
                ));
            }
        }
        Ok(())
    }

    /// 停止进程：SIGTERM → 等待 `timeout_secs` 秒 → SIGKILL（unix 杀整组）。
    /// 幂等：已终结的记录原样返回其状态。
    pub async fn stop(&self, name: &str, timeout_secs: u64) -> Result<ProcessStatus, ProcessError> {
        let entry = self.lookup(name)?;
        if !is_live(&lock(&entry.core)) {
            // 已终结：幂等返回（在锁外取快照，std Mutex 不可重入）。
            return Ok(entry.status());
        }
        lock(&entry.core).stop_requested = true;
        terminate(&entry, Duration::from_secs(timeout_secs)).await;
        // 等看护任务把终态落定（killpg 与状态写入间有一个轮询周期）。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let settled = {
                let c = lock(&entry.core);
                c.state.is_terminal() && !c.alive
            };
            if settled {
                return Ok(entry.status());
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(entry.status());
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// 用存档规格重启：终止当前实例（若有）→ 重新拉起 → 阻塞至就绪。
    /// 手动重启不计入自动重启预算（`restarts` 归零）。
    pub async fn restart(&self, name: &str) -> Result<ProcessInfo, ProcessError> {
        let entry = self.lookup(name)?;
        {
            let mut c = lock(&entry.core);
            // 代数 +1：旧看护任务在退出裁决处因 incarnation 不匹配静默退场。
            c.incarnation += 1;
            c.stop_requested = false;
            c.restarts = 0;
            c.state = ProcessState::Starting;
        }
        terminate(&entry, Duration::from_secs(DEFAULT_STOP_GRACE_SECS)).await;
        spawn_incarnation(&entry).await?;
        tokio::spawn(life_task(Arc::clone(&self.shared), Arc::clone(&entry)));
        wait_ready(&entry).await;
        let c = lock(&entry.core);
        Ok(ProcessInfo {
            name: entry.spec.name.clone(),
            pid: c.pid,
            started_at: c.started_at,
        })
    }

    /// 启动规格 + 当前状态（JSON）。
    pub async fn describe(&self, name: &str) -> Result<serde_json::Value, ProcessError> {
        let entry = self.lookup(name)?;
        let spec =
            serde_json::to_value(&entry.spec).map_err(|e| ProcessError::Io(e.to_string()))?;
        let status =
            serde_json::to_value(entry.status()).map_err(|e| ProcessError::Io(e.to_string()))?;
        Ok(serde_json::json!({ "spec": spec, "status": status }))
    }
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 按键名 → 字节序列（对齐 omp KEY_INPUT）。
fn key_bytes(key: &str) -> Option<&'static [u8]> {
    let k = key.trim().to_ascii_uppercase();
    match k.as_str() {
        "ENTER" => Some(b"\r"),
        "TAB" => Some(b"\t"),
        "ESCAPE" => Some(b"\x1b"),
        "CTRL_C" => Some(b"\x03"),
        "CTRL_D" => Some(b"\x04"),
        "UP" => Some(b"\x1b[A"),
        "DOWN" => Some(b"\x1b[B"),
        "LEFT" => Some(b"\x1b[D"),
        "RIGHT" => Some(b"\x1b[C"),
        _ => None,
    }
}

/// 汇一页日志：`from` 之后、grep 过滤、按 head 取前/取尾 `limit` 行。
fn collect_page(
    entry: &ProcEntry,
    from: u64,
    re: Option<&Regex>,
    head: bool,
    limit: usize,
) -> LogPage {
    let c = lock(&entry.core);
    let matched: Vec<&ProcessLogLine> = c
        .lines
        .iter()
        .filter(|l| l.seq > from && re.is_none_or(|r| r.is_match(&l.text)))
        .collect();
    let chosen: Vec<ProcessLogLine> = if head {
        matched.iter().take(limit).map(|l| (*l).clone()).collect()
    } else {
        matched
            .iter()
            .rev()
            .take(limit)
            .rev()
            .map(|l| (*l).clone())
            .collect()
    };
    let cursor = chosen.last().map_or(from, |l| l.seq);
    LogPage {
        cursor,
        lines: chosen,
    }
}

/// 终止当前实例：SIGTERM → 等宽限 → SIGKILL；等退出被看护任务确认（alive=false）。
async fn terminate(entry: &ProcEntry, grace: Duration) {
    #[cfg(unix)]
    {
        let pid = {
            let c = lock(&entry.core);
            if !c.alive {
                return;
            }
            c.pid
        };
        if let Some(pid) = pid {
            kill_group(pid, nix::sys::signal::Signal::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        {
            let mut c = lock(&entry.core);
            if let Some((_, child)) = c.child.as_mut() {
                let _ = child.start_kill();
            }
        }
    }
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        {
            let c = lock(&entry.core);
            if !c.alive {
                return;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    // 宽限用尽：强杀。
    #[cfg(unix)]
    {
        let pid = lock(&entry.core).pid;
        if let Some(pid) = pid {
            kill_group(pid, nix::sys::signal::Signal::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let mut c = lock(&entry.core);
        if let Some((_, child)) = c.child.as_mut() {
            let _ = child.start_kill();
        }
    }
    // 等 reaper 落定（最多再等一个宽限）。
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        {
            let c = lock(&entry.core);
            if !c.alive {
                return;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// 拉起一轮实例（incarnation）：spawn、接管管道、起泵/探针/就绪哨兵。
/// 子进程入 `core.child` 槽位（带代标；看护任务随后按代领取）。
async fn spawn_incarnation(entry: &Arc<ProcEntry>) -> Result<(), ProcessError> {
    let spec = &entry.spec;
    let mut cmd = tokio::process::Command::new(&spec.application);
    cmd.args(&spec.args);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // 独立进程组：stop/signal 对整树统一击杀（与 browser 工具同套路）。
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let flags = Arc::new(ReadyFlags {
        log: AtomicBool::new(entry.ready_log.is_none()),
        port: AtomicBool::new(spec.ready.port.is_none()),
    });
    let inc;
    {
        let mut c = lock(&entry.core);
        c.incarnation += 1;
        inc = c.incarnation;
        c.pid = pid;
        c.alive = true;
        c.state = ProcessState::Starting;
        c.started_at = now_ms();
        c.child = Some((inc, child));
    }
    *entry.stdin.lock().await = stdin;
    if let Some(out) = stdout {
        tokio::spawn(pump(
            Arc::clone(entry),
            out,
            LogStream::Stdout,
            Arc::clone(&flags),
        ));
    }
    if let Some(err) = stderr {
        tokio::spawn(pump(
            Arc::clone(entry),
            err,
            LogStream::Stderr,
            flags.clone(),
        ));
    }
    if let Some(port) = spec.ready.port {
        tokio::spawn(prober(
            port,
            flags.clone(),
            Duration::from_secs(spec.ready.timeout_secs),
        ));
    }
    tokio::spawn(ready_watch(
        Arc::clone(entry),
        flags,
        inc,
        Duration::from_secs(spec.ready.timeout_secs),
    ));
    Ok(())
}

/// 看护任务：等当前实例退出 → 按策略裁决（终态 or 退避重启）。
async fn life_task(shared: Arc<Shared>, entry: Arc<ProcEntry>) {
    let policy = entry.spec.restart;
    let mut restarts: u32 = 0;
    loop {
        let my_inc = lock(&entry.core).incarnation;
        let mut child = match take_child(&entry, my_inc) {
            Some(c) => c,
            None => return, // 槽位已换代（被 restart 取代）
        };
        let code = wait_exit(&mut child).await;
        {
            let mut c = lock(&entry.core);
            c.alive = false;
            c.child = None;
            if c.incarnation != my_inc {
                return; // 已被 restart 取代：状态归新生命周期写
            }
            if c.stop_requested {
                c.state = ProcessState::Stopped;
                return;
            }
            let clean = code == Some(0);
            match policy {
                RestartPolicy::No => {
                    c.state = ProcessState::Exited { code };
                    return;
                }
                RestartPolicy::OnFailure if clean => {
                    c.state = ProcessState::Exited { code };
                    return;
                }
                _ if restarts as usize >= shared.backoff_ms.len() => {
                    c.state = if clean {
                        ProcessState::Exited { code }
                    } else {
                        ProcessState::Failed {
                            reason: format!(
                                "自动重启 {restarts} 次后仍退出（退出码 {}）",
                                code.map_or_else(|| "信号终止".to_string(), |c| c.to_string())
                            ),
                        }
                    };
                    return;
                }
                _ => {
                    restarts += 1;
                    c.restarts = restarts;
                    // 退避期间保持 Starting（即将重生）。
                }
            }
        }
        let backoff = Duration::from_millis(
            shared.backoff_ms[usize::try_from(restarts - 1)
                .unwrap_or(0)
                .min(shared.backoff_ms.len() - 1)],
        );
        tokio::time::sleep(backoff).await;
        {
            let c = lock(&entry.core);
            if c.incarnation != my_inc {
                return;
            }
            if c.stop_requested {
                // 退避期间被 stop：直接终态，不再重生。
                drop(c);
                lock(&entry.core).state = ProcessState::Stopped;
                return;
            }
        }
        match spawn_incarnation(&entry).await {
            Ok(_) => {}
            Err(e) => {
                let mut c = lock(&entry.core);
                if c.incarnation == my_inc {
                    c.alive = false;
                    c.state = ProcessState::Failed {
                        reason: format!("重启失败：{e}"),
                    };
                }
                return;
            }
        }
    }
}

/// 取出本代子进程（代数不匹配 → None，调用方退场）。
fn take_child(entry: &ProcEntry, my_inc: u64) -> Option<tokio::process::Child> {
    let mut c = lock(&entry.core);
    match c.child.take() {
        Some((inc, child)) if inc == my_inc => Some(child),
        other => {
            c.child = other; // 别人的代，放回去
            None
        }
    }
}

/// 轮询等待子进程退出，返回退出码（信号终止为 None）。
async fn wait_exit(child: &mut tokio::process::Child) -> Option<i32> {
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return status.code();
        }
        tokio::time::sleep(POLL).await;
    }
}

/// stdout/stderr 泵：逐行入环形缓冲，喂就绪正则。
async fn pump<R>(entry: Arc<ProcEntry>, reader: R, stream: LogStream, flags: Arc<ReadyFlags>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = tokio::io::BufReader::new(reader).lines();
    while let Ok(Some(text)) = lines.next_line().await {
        let mut c = lock(&entry.core);
        let seq = c.next_seq;
        c.next_seq += 1;
        c.lines.push_back(ProcessLogLine {
            seq,
            stream,
            text: text.trim_end_matches('\r').to_string(),
        });
        while c.lines.len() > LOG_CAP {
            c.lines.pop_front();
        }
        if let Some(re) = &entry.ready_log {
            if !flags.log.load(Ordering::Relaxed) && re.is_match(&text) {
                flags.log.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// 端口探针：循环 TCP 连接 127.0.0.1:port 直至连通或就绪窗口结束。
async fn prober(port: u16, flags: Arc<ReadyFlags>, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if flags.port.load(Ordering::Relaxed) {
            return;
        }
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            flags.port.store(true, Ordering::Relaxed);
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(READY_TICK).await;
    }
}

/// 就绪哨兵：条件齐 → Running；超时 → Failed（进程保留）；提前终结 → 退出。
async fn ready_watch(entry: Arc<ProcEntry>, flags: Arc<ReadyFlags>, inc: u64, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if flags.log.load(Ordering::Relaxed) && flags.port.load(Ordering::Relaxed) {
            let mut c = lock(&entry.core);
            if c.incarnation == inc && c.state == ProcessState::Starting {
                c.state = ProcessState::Running;
            }
            return;
        }
        {
            let c = lock(&entry.core);
            if c.incarnation != inc || c.state.is_terminal() {
                return; // 退出裁决已落定，别覆盖
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let mut c = lock(&entry.core);
            if c.incarnation == inc && c.state == ProcessState::Starting {
                c.state = ProcessState::Failed {
                    reason: format!("就绪超时（{}s）：ready 条件未满足", timeout.as_secs()),
                };
            }
            return;
        }
        tokio::time::sleep(READY_TICK).await;
    }
}

/// start/restart 阻塞等待：至状态离开 Starting（就绪/终结/就绪超时）或兜底截止。
async fn wait_ready(entry: &ProcEntry) {
    let budget = Duration::from_secs(entry.spec.ready.timeout_secs) + Duration::from_secs(2);
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        {
            let c = lock(&entry.core);
            if !matches!(c.state, ProcessState::Starting) {
                return;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(READY_TICK).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 /bin/sh -c 规格（unix 专属测试用）。
    fn sh_spec(name: &str, script: &str) -> ProcessSpec {
        ProcessSpec {
            name: name.to_string(),
            application: "/bin/sh".into(),
            args: vec!["-c".into(), script.to_string()],
            cwd: None,
            env: BTreeMap::new(),
            ready: ReadySpec::default(),
            restart: RestartPolicy::No,
        }
    }

    /// 轮询等待状态满足断言（避免 flaky sleep 断言）。
    async fn wait_state(
        mgr: &ProcessManager,
        name: &str,
        pred: impl Fn(&ProcessState) -> bool,
        timeout_ms: u64,
    ) -> ProcessStatus {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let st = mgr
                .ps()
                .await
                .into_iter()
                .find(|p| p.name == name)
                .expect("进程记录应存在");
            if pred(&st.state) {
                return st;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "等待状态超时：{st:?}"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    fn stopped_or_exited(st: &ProcessState) -> bool {
        st.is_terminal()
    }

    #[tokio::test]
    async fn invalid_name_and_args_rejected() {
        let mgr = ProcessManager::new();
        let err = mgr
            .start(sh_spec("-bad", "true"))
            .await
            .expect_err("应拒绝非法名");
        assert!(err.to_string().contains("进程名非法"), "{err}");

        let err = mgr
            .start(ProcessSpec {
                application: String::new(),
                ..sh_spec("ok-name", "true")
            })
            .await
            .expect_err("应拒绝空 application");
        assert!(err.to_string().contains("application"), "{err}");

        let mut spec = sh_spec("ok-name", "true");
        spec.ready.log = Some("(unclosed".into());
        let err = mgr.start(spec).await.expect_err("应拒绝坏正则");
        assert!(err.to_string().contains("正则编译失败"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn duplicate_live_name_rejected_then_replaced() {
        let mgr = ProcessManager::new();
        mgr.start(sh_spec("web", "sleep 30")).await.expect("启动");
        let err = mgr
            .start(sh_spec("web", "sleep 30"))
            .await
            .expect_err("重名应拒绝");
        assert!(err.to_string().contains("已在运行"), "{err}");

        let st = mgr.stop("web", 5).await.expect("停止");
        assert_eq!(st.state, ProcessState::Stopped);

        // completed（stopped）重名：允许，替换记录。
        mgr.start(sh_spec("web", "echo done"))
            .await
            .expect("重启同名");
        wait_state(&mgr, "web", stopped_or_exited, 5_000).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exit_code_streams_and_logs() {
        let mgr = ProcessManager::new();
        mgr.start(sh_spec("job", "echo hello; echo err-line >&2; exit 7"))
            .await
            .expect("启动");
        let st = wait_state(&mgr, "job", stopped_or_exited, 5_000).await;
        assert_eq!(st.state, ProcessState::Exited { code: Some(7) });

        let page = mgr
            .logs("job", None, None, false, 100, false, 0)
            .await
            .expect("日志");
        let texts: Vec<_> = page
            .lines
            .iter()
            .map(|l| (l.stream, l.text.as_str()))
            .collect();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert_eq!(texts[0], (LogStream::Stdout, "hello"));
        assert_eq!(texts[1], (LogStream::Stderr, "err-line"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn logs_cursor_paging_and_head_tail() {
        let mgr = ProcessManager::new();
        mgr.start(sh_spec("pager", "printf 'a\\nb\\nc\\nd\\n'"))
            .await
            .expect("启动");
        wait_state(&mgr, "pager", stopped_or_exited, 5_000).await;

        // 头部翻页：2 行 + 游标。
        let page = mgr
            .logs("pager", None, None, true, 2, false, 0)
            .await
            .expect("日志");
        let texts: Vec<_> = page.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "b"]);
        assert_eq!(page.cursor, 2);
        let page = mgr
            .logs("pager", Some(2), None, true, 2, false, 0)
            .await
            .expect("续读");
        let texts: Vec<_> = page.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["c", "d"]);

        // 尾部取 2 行。
        let page = mgr
            .logs("pager", None, None, false, 2, false, 0)
            .await
            .expect("尾读");
        let texts: Vec<_> = page.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["c", "d"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn logs_grep_filters_lines() {
        let mgr = ProcessManager::new();
        mgr.start(sh_spec("greppy", "printf 'foo\\nbar\\nbaz\\n'"))
            .await
            .expect("启动");
        wait_state(&mgr, "greppy", stopped_or_exited, 5_000).await;
        let page = mgr
            .logs("greppy", None, Some("ba".into()), true, 100, false, 0)
            .await
            .expect("日志");
        let texts: Vec<_> = page.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["bar", "baz"]);

        // 非法 grep 正则报错。
        let err = mgr
            .logs("greppy", None, Some("(bad".into()), true, 100, false, 0)
            .await
            .expect_err("非法正则应报错");
        assert!(err.to_string().contains("grep 正则编译失败"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn logs_follow_waits_for_new_output() {
        let mgr = ProcessManager::new();
        mgr.start(sh_spec("follower", "echo first; sleep 0.3; echo second"))
            .await
            .expect("启动");
        let page = mgr
            .logs("follower", None, None, true, 100, false, 0)
            .await
            .expect("日志");
        assert_eq!(page.lines[0].text, "first");
        // follow：等新输出到达（上限 5s，实际 ~0.3s）。
        let page = mgr
            .logs("follower", Some(page.cursor), None, true, 100, true, 5)
            .await
            .expect("follow 日志");
        assert_eq!(page.lines.last().expect("应有新行").text, "second");
        mgr.stop("follower", 5).await.expect("清理");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ready_log_regex_reaches_running() {
        let mgr = ProcessManager::new();
        let mut spec = sh_spec("ready-log", "echo READY-marker; sleep 30");
        spec.ready.log = Some("READY.marker".into());
        spec.ready.timeout_secs = 5;
        mgr.start(spec).await.expect("启动");
        let st = wait_state(&mgr, "ready-log", |s| *s == ProcessState::Running, 5_000).await;
        assert!(st.pid.is_some());
        mgr.stop("ready-log", 5).await.expect("清理");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ready_port_probe_reaches_running() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("占端口");
        let port = listener.local_addr().expect("addr").port();
        let mgr = ProcessManager::new();
        let mut spec = sh_spec("ready-port", "sleep 30");
        spec.ready.port = Some(port);
        spec.ready.timeout_secs = 5;
        mgr.start(spec).await.expect("启动");
        wait_state(&mgr, "ready-port", |s| *s == ProcessState::Running, 5_000).await;
        mgr.stop("ready-port", 5).await.expect("清理");
        drop(listener);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ready_timeout_marks_failed_but_process_survives() {
        let mgr = ProcessManager::new();
        let mut spec = sh_spec("slow-ready", "sleep 30");
        spec.ready.log = Some("never-appears".into());
        spec.ready.timeout_secs = 1;
        mgr.start(spec).await.expect("启动（超时不算 start 失败）");
        let st = wait_state(
            &mgr,
            "slow-ready",
            |s| matches!(s, ProcessState::Failed { reason } if reason.contains("就绪超时")),
            5_000,
        )
        .await;
        // 进程保留：仍可 stop。
        let after = mgr.stop("slow-ready", 5).await.expect("stop 幸存进程");
        assert_eq!(after.state, ProcessState::Stopped);
        assert_ne!(st.state, ProcessState::Running);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_kills_whole_process_group() {
        let pid_file = std::env::temp_dir().join(format!("gyre-proc-group-{}", std::process::id()));
        let _ = std::fs::remove_file(&pid_file);
        let script = format!("sleep 30 & echo $! > {}; wait", pid_file.display());
        let mgr = ProcessManager::new();
        let info = mgr.start(sh_spec("grouped", &script)).await.expect("启动");
        let sh_pid = info.pid.expect("pid");

        // 等 shell 写出子 pid。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let child_pid: u32 = loop {
            if let Ok(s) = std::fs::read_to_string(&pid_file) {
                if let Ok(p) = s.trim().parse::<u32>() {
                    break p;
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "子 pid 未写出");
            tokio::time::sleep(POLL).await;
        };
        assert!(
            std::path::Path::new(&format!("/proc/{child_pid}")).exists(),
            "子进程应在运行"
        );

        mgr.stop("grouped", 5).await.expect("停止");
        // 整组击杀：shell 与其后代 sleep 都应消失（轮询等待 /proc 收敛）。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let sh_gone = !std::path::Path::new(&format!("/proc/{sh_pid}")).exists();
            let child_gone = !std::path::Path::new(&format!("/proc/{child_pid}")).exists();
            if sh_gone && child_gone {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "进程组未完全退出");
            tokio::time::sleep(POLL).await;
        }
        let _ = std::fs::remove_file(&pid_file);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_reuses_spec_and_resets_budget() {
        let mgr = ProcessManager::new();
        mgr.start(sh_spec("restarted", "echo v1"))
            .await
            .expect("启动");
        wait_state(&mgr, "restarted", stopped_or_exited, 5_000).await;

        let info = mgr.restart("restarted").await.expect("重启");
        assert!(info.pid.is_some());
        let st = wait_state(&mgr, "restarted", stopped_or_exited, 5_000).await;
        assert_eq!(st.state, ProcessState::Exited { code: Some(0) });
        assert_eq!(st.restarts, 0, "手动重启不计入自动重启预算");
        // 两轮输出都留在环形缓冲（seq 跨重启累计）。
        let page = mgr
            .logs("restarted", None, None, false, 100, false, 0)
            .await
            .expect("日志");
        assert_eq!(page.lines.len(), 2, "{:?}", page.lines);
        assert_eq!(page.lines[0].seq, 1);
        assert_eq!(page.lines[1].seq, 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_kills_running_instance() {
        let mgr = ProcessManager::new();
        let first = mgr.start(sh_spec("hot", "sleep 30")).await.expect("启动");
        let info = mgr.restart("hot").await.expect("重启运行中进程");
        assert_ne!(first.pid, info.pid, "应换新 pid");
        let st = wait_state(&mgr, "hot", |s| *s == ProcessState::Running, 5_000).await;
        assert_eq!(st.pid, info.pid);
        mgr.stop("hot", 5).await.expect("清理");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn on_failure_auto_restarts_with_budget_cap() {
        let mgr = ProcessManager::with_backoff_ms([10, 10, 10, 10, 10]);
        let mut spec = sh_spec("flappy", "exit 3");
        spec.restart = RestartPolicy::OnFailure;
        mgr.start(spec).await.expect("启动");
        // 轮询直至重启预算耗尽（确定性终态，不靠 sleep 计数）。
        let st = wait_state(
            &mgr,
            "flappy",
            |s| matches!(s, ProcessState::Failed { reason } if reason.contains("自动重启")),
            10_000,
        )
        .await;
        assert_eq!(st.restarts, 5);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_suppresses_auto_restart() {
        let mgr = ProcessManager::with_backoff_ms([10, 10, 10, 10, 10]);
        let mut spec = sh_spec("spinner", "while true; do sleep 0.2; done");
        spec.restart = RestartPolicy::Always;
        mgr.start(spec).await.expect("启动");
        wait_state(&mgr, "spinner", |s| *s == ProcessState::Running, 5_000).await;
        let st = mgr.stop("spinner", 5).await.expect("停止");
        assert_eq!(st.state, ProcessState::Stopped);
        // 用户 stop 不算 failure：短暂等待后仍是 Stopped（无重生）。
        tokio::time::sleep(Duration::from_millis(400)).await;
        let now = mgr
            .ps()
            .await
            .into_iter()
            .find(|p| p.name == "spinner")
            .expect("记录保留");
        assert_eq!(now.state, ProcessState::Stopped, "不应重生：{now:?}");
        assert_eq!(now.restarts, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signal_delivers_to_group() {
        let mgr = ProcessManager::new();
        mgr.start(sh_spec(
            "trapped",
            "trap 'exit 42' TERM; while true; do sleep 0.2; done",
        ))
        .await
        .expect("启动");
        wait_state(&mgr, "trapped", |s| *s == ProcessState::Running, 5_000).await;
        mgr.signal("trapped", Signal::Sigterm)
            .await
            .expect("发信号");
        let st = wait_state(&mgr, "trapped", stopped_or_exited, 5_000).await;
        assert_eq!(
            st.state,
            ProcessState::Exited { code: Some(42) },
            "trap 生效说明信号送达"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_writes_stdin_with_keys() {
        let mgr = ProcessManager::new();
        mgr.start(sh_spec(
            "echoer",
            "read line; echo got:$line; read k; echo key:$k",
        ))
        .await
        .expect("启动");
        wait_state(&mgr, "echoer", |s| *s == ProcessState::Running, 5_000).await;

        // 文本 + enter（默认换行）：sh read 以行消费。
        mgr.send("echoer", "hello", true, &[])
            .await
            .expect("写文本");
        mgr.send("echoer", "k2", true, &[]).await.expect("写第二行");
        // 纯按键（ENTER=\r）也是合法写入（管道下只是字节序列）。
        mgr.send("echoer", "", false, &["ENTER".into()])
            .await
            .expect("写按键");
        wait_state(&mgr, "echoer", stopped_or_exited, 5_000).await;
        let page = mgr
            .logs("echoer", None, None, false, 100, false, 0)
            .await
            .expect("日志");
        let texts: Vec<_> = page.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["got:hello", "key:k2"], "{texts:?}");

        // 非法按键 / 空载荷报错。
        let err = mgr
            .send("echoer", "x", false, &["WARP".into()])
            .await
            .expect_err("非法键");
        assert!(err.to_string().contains("不支持的按键"), "{err}");
        let err = mgr
            .send("echoer", "", false, &[])
            .await
            .expect_err("空载荷");
        assert!(err.to_string().contains("无可写内容"), "{err}");
    }

    #[tokio::test]
    async fn unknown_process_errors_are_not_found() {
        let mgr = ProcessManager::new();
        let err = mgr
            .logs("ghost", None, None, true, 100, false, 0)
            .await
            .expect_err("不存在");
        assert!(err.to_string().contains("不存在或已退出"), "{err}");
        let err = mgr.stop("ghost", 1).await.expect_err("不存在");
        assert!(err.to_string().contains("不存在或已退出"), "{err}");
        let err = mgr.restart("ghost").await.expect_err("不存在");
        assert!(err.to_string().contains("不存在或已退出"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn describe_returns_spec_and_status_json() {
        let mgr = ProcessManager::new();
        let mut spec = sh_spec("described", "echo hi");
        spec.restart = RestartPolicy::OnFailure;
        mgr.start(spec).await.expect("启动");
        wait_state(&mgr, "described", stopped_or_exited, 5_000).await;
        let v = mgr.describe("described").await.expect("describe");
        assert_eq!(v["spec"]["name"], "described");
        assert_eq!(v["spec"]["application"], "/bin/sh");
        assert_eq!(v["spec"]["restart"], "on_failure");
        assert!(
            v["status"]["state"].is_object() || v["status"]["state"].is_string(),
            "state 形状：{v}"
        );
        let err = mgr.describe("ghost").await.expect_err("不存在");
        assert!(err.to_string().contains("不存在或已退出"), "{err}");
    }
}
