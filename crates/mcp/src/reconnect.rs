//! MCP 重连监督：指数退避 + 每 server 熔断（移植 oh-my-pi `mcp/manager.ts` 语义）。
//!
//! 每 server 一个监督任务，状态机：
//!
//! ```text
//!        连接级失败（请求失败 / stdio 读循环退出）
//! Connected ────────────────────────────────► Reconnecting
//!     ▲                                          │
//!     │ 半开探活成功（换装传输 + 重握手 + 清单刷新）│ ①退避重试（500/1000/2000/4000ms）耗尽
//!     │                                          ▼
//!     │                                    Reconnecting（保持不可用，等待下次触发）
//!     │                                          │ 触发且 30s 窗口内周期数 > 5
//!     │                                          ▼
//!     └─────── 探活成功（close 熔断）◄── 半开探活 ◄── Open（冷却 30s 内不再尝试）
//! ```
//!
//! 对齐 omp 的关键语义：
//! - 重连去重：进行中的周期吞掉新触发（omp `#pendingReconnections` 共享未决重连）
//! - 爆发熔断：滑动窗口统计重连周期起点，超限即 open（omp `#tripReconnectBreaker`）
//! - 回收不自触发：换装/回收旧传输前先摘除断连处理器（omp 先 detach `onClose` 再 close）
//! - 重试耗尽不清工具：陈旧工具保留注册，调用失败会再次驱动重连（omp `#doReconnect`）

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use agent_config::McpServerConfig;
use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::client::{McpClient, McpError, McpTransport};
use crate::http::HttpTransport;
use crate::stdio::StdioTransport;

/// 重连退避间隔（omp manager.ts:887 `delays`）：首试 + 4 次重试，共 5 次尝试。
pub(crate) const RECONNECT_DELAYS_MS: [u64; 4] = [500, 1_000, 2_000, 4_000];
/// 爆发统计滑动窗口，兼作熔断冷却时长（omp manager.ts:86 `RECONNECT_BURST_WINDOW_MS`）。
pub(crate) const RECONNECT_BURST_WINDOW_MS: u64 = 30_000;
/// 窗口内重连周期上限，超过即熔断（omp manager.ts:87 `RECONNECT_BURST_LIMIT`；
/// 判定对齐 omp `recent.length > limit`：窗口内第 6 个周期触发）。
pub(crate) const RECONNECT_BURST_LIMIT: usize = 5;

/// 每 server 连接状态（供 `/mcp status` 类自省；本版本仅 API 暴露，不接 CLI）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpConnState {
    /// 连接健康。
    Connected,
    /// 重连进行中，或重试耗尽后等待下次触发（连接不可用）。
    Reconnecting,
    /// 熔断开启：冷却期内不再尝试，冷却后单次半开探活。
    Open,
}

/// server 连接状态快照 + 最近一次错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    /// 连接状态。
    pub state: McpConnState,
    /// 最近一次失败原因（恢复成功后清空）。
    pub last_error: Option<String>,
}

/// 状态槽：监督任务写、注册表读。
pub(crate) type StatusCell = Arc<Mutex<McpServerStatus>>;
/// 重连成功回调：刷新工具清单 + fire 变更监听器（tool.rs 装配时注入）。
pub(crate) type SuccessFn = Arc<dyn Fn(&str) -> BoxFuture<'static, ()> + Send + Sync>;

/// 休眠 seam：生产为真实 `tokio::time::sleep`；测试注入即时返回并记录间隔的假时钟。
pub(crate) type SleepFn = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;
/// 传输构造 seam：生产按配置分发 stdio/HTTP；测试注入脚本化的假传输序列。
pub(crate) type ConnectFn = Arc<
    dyn Fn(&McpServerConfig) -> BoxFuture<'static, Result<Box<dyn McpTransport>, McpError>>
        + Send
        + Sync,
>;

/// 重连监督参数（常量对齐 omp；`sleep` / `connect` 为测试 seam）。
#[derive(Clone)]
pub(crate) struct ReconnectOptions {
    /// 重连退避间隔（两次尝试之间）；尝试总数 = `delays.len() + 1`。
    pub delays: Vec<Duration>,
    /// 爆发统计滑动窗口（兼熔断冷却时长）。
    pub burst_window: Duration,
    /// 窗口内重连周期上限，超过即熔断。
    pub burst_limit: usize,
    /// 休眠 seam。
    pub sleep: SleepFn,
    /// 传输构造 seam。
    pub connect: ConnectFn,
}

impl Default for ReconnectOptions {
    fn default() -> Self {
        Self {
            delays: RECONNECT_DELAYS_MS.map(Duration::from_millis).to_vec(),
            burst_window: Duration::from_millis(RECONNECT_BURST_WINDOW_MS),
            burst_limit: RECONNECT_BURST_LIMIT,
            sleep: Arc::new(|d| Box::pin(tokio::time::sleep(d))),
            connect: Arc::new(|cfg: &McpServerConfig| {
                let cfg = cfg.clone();
                Box::pin(async move {
                    match &cfg {
                        McpServerConfig::Stdio(c) => StdioTransport::spawn(c)
                            .await
                            .map(|t| Box::new(t) as Box<dyn McpTransport>),
                        McpServerConfig::Http(c) => HttpTransport::connect(c)
                            .await
                            .map(|t| Box::new(t) as Box<dyn McpTransport>),
                    }
                })
            }),
        }
    }
}

/// 每 server 重连监督器：独立任务串行消费重连触发。
pub(crate) struct ReconnectSupervisor {
    /// server 名（状态展示与刷新回调路由用）。
    name: String,
    /// server 配置（重建传输用）。
    cfg: McpServerConfig,
    /// 共享 client（换装传输后既有工具句柄自动路由到新传输）。
    client: Arc<McpClient>,
    /// 连接状态槽。
    status: StatusCell,
    /// 重连成功回调（刷新工具清单 + fire 变更监听器；tool.rs 注入）。
    on_success: SuccessFn,
    /// 存活探针：注册表释放后监督任务随之退出，不再空转重建传输。
    alive: Arc<dyn Fn() -> bool + Send + Sync>,
    /// 参数（含测试 seam）。
    opts: ReconnectOptions,
    /// 爆发统计滑动窗口（记录重连周期起点时刻）。
    crashes: Mutex<Vec<Instant>>,
    /// 周期进行中标志（触发去重；`Fn()` 回调内同步 CAS，不得阻塞）。
    busy: Arc<AtomicBool>,
}

impl ReconnectSupervisor {
    /// 启动监督任务，返回重连触发 hook（[`McpClient::set_reconnect_hook`] 注入）。
    pub(crate) fn spawn(
        name: String,
        cfg: McpServerConfig,
        client: Arc<McpClient>,
        status: StatusCell,
        on_success: SuccessFn,
        alive: Arc<dyn Fn() -> bool + Send + Sync>,
        opts: ReconnectOptions,
    ) -> Arc<dyn Fn() + Send + Sync> {
        let sup = Arc::new(Self {
            name,
            cfg,
            client,
            status,
            on_success,
            alive,
            opts,
            crashes: Mutex::new(Vec::new()),
            busy: Arc::new(AtomicBool::new(false)),
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let hook_sup = Arc::clone(&sup);
        tokio::spawn(async move {
            while rx.recv().await.is_some() {
                sup.cycle().await;
                sup.busy.store(false, Ordering::Release);
            }
        });
        Arc::new(move || {
            // 去重：周期进行中新触发直接吞掉（周期内的尝试已覆盖恢复语义，
            // 对齐 omp：并发重连共享同一未决周期而非排队）。
            if hook_sup
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let _ = tx.send(());
            }
        })
    }

    /// 单轮重连周期：记录爆发窗口 → 熔断检查 → 退避重试。
    async fn cycle(&self) {
        if !(self.alive)() {
            return;
        }
        self.record_crash();
        if self.burst_exceeded() {
            self.open_until_healed().await;
            return;
        }
        self.set(McpConnState::Reconnecting, None);
        let delays = &self.opts.delays;
        let mut last_err = String::from("重连失败");
        for attempt in 0..=delays.len() {
            if !(self.alive)() {
                return;
            }
            match self.attempt().await {
                Ok(()) => {
                    self.heal().await;
                    return;
                }
                Err(e) => {
                    last_err = e.to_string();
                    if attempt < delays.len() {
                        (self.opts.sleep)(delays[attempt]).await;
                    }
                }
            }
        }
        // 重试耗尽：保持 Reconnecting（连接不可用）；陈旧工具保留注册，
        // 后续调用失败会再次触发（对齐 omp：不因重试失败移除工具）。
        self.set(McpConnState::Reconnecting, Some(last_err));
    }

    /// 熔断 open：冷却（= 爆发窗口）内不再尝试，之后单次半开探活直至恢复。
    async fn open_until_healed(&self) {
        loop {
            if !(self.alive)() {
                return;
            }
            self.set(
                McpConnState::Open,
                Some(format!(
                    "重连爆发（{:?} 窗口内 > {} 个周期），熔断开启，冷却后单次探活",
                    self.opts.burst_window, self.opts.burst_limit
                )),
            );
            (self.opts.sleep)(self.opts.burst_window).await;
            // 半开探活：单次尝试。成功则 close 熔断；失败记错误后重新冷却。
            match self.attempt().await {
                Ok(()) => {
                    self.heal().await;
                    return;
                }
                Err(e) => self.set(McpConnState::Open, Some(e.to_string())),
            }
        }
    }

    /// 单次重连尝试：构造新传输 → 换装（旧传输摘除断连处理器后异步回收）→ 重新 initialize。
    async fn attempt(&self) -> Result<(), McpError> {
        let transport = (self.opts.connect)(&self.cfg).await?;
        self.client.install_transport(transport).await;
        self.client.initialize().await
    }

    /// 恢复：状态回 Connected、清错误，再刷新工具清单并 fire 变更
    /// （清单刷新失败仅告警——连接本身已健康）。
    async fn heal(&self) {
        self.set(McpConnState::Connected, None);
        (self.on_success)(&self.name).await;
    }

    /// 记录一个重连周期起点，先淘汰窗口外旧条目（对齐 omp 滑动窗口剪枝）。
    fn record_crash(&self) {
        let mut w = self.crashes.lock();
        w.retain(|t| t.elapsed() < self.opts.burst_window);
        w.push(Instant::now());
    }

    /// 是否超过爆发上限（窗口内周期数 > 上限，对齐 omp 严格大于判定）。
    fn burst_exceeded(&self) -> bool {
        self.crashes.lock().len() > self.opts.burst_limit
    }

    fn set(&self, state: McpConnState, last_error: Option<String>) {
        *self.status.lock() = McpServerStatus { state, last_error };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use serde_json::{Value, json};

    /// 记录型假时钟：即时返回并记录请求的休眠间隔。
    fn recording_sleeper(log: Arc<Mutex<Vec<Duration>>>) -> SleepFn {
        Arc::new(move |d| {
            log.lock().push(d);
            // 必须让出一次调度：即时返回的假时钟若不让出，重连循环会饿死
            // current_thread 测试运行时（真实 tokio::time::sleep 天然让出）。
            Box::pin(tokio::task::yield_now())
        })
    }

    /// 脚本化传输：initialize 可编程失败；close 时触发已注册的断连处理器
    /// （模拟 stdio 读循环随 kill 退出——若回收前未摘除处理器将自触发重连）。
    struct FakeTransport {
        fail_initialize: bool,
        on_close: Mutex<Option<crate::client::CloseHandler>>,
    }

    #[async_trait::async_trait]
    impl McpTransport for FakeTransport {
        async fn request(&self, method: &str, _params: Value) -> Result<Value, McpError> {
            if method == "initialize" && self.fail_initialize {
                return Err(McpError::Closed);
            }
            Ok(json!({}))
        }
        async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }
        async fn close(&self) {
            if let Some(h) = self.on_close.lock().clone() {
                h();
            }
        }
        fn set_on_close(&self, handler: crate::client::CloseHandler) {
            *self.on_close.lock() = Some(handler);
        }
    }

    /// 测试台架：注入假时钟 + 脚本化传输序列的监督器。
    struct Rig {
        hook: Arc<dyn Fn() + Send + Sync>,
        sleeps: Arc<Mutex<Vec<Duration>>>,
        connects: Arc<AtomicUsize>,
        successes: Arc<AtomicUsize>,
        status: StatusCell,
        /// 放行开关：false 时每次连接尝试都失败。
        allow_success: Arc<AtomicBool>,
    }

    impl Rig {
        /// 前 `fail_first` 次连接的 initialize 失败，其后成功（`usize::MAX` = 全失败）。
        fn new(fail_first: usize) -> Self {
            let sleeps = Arc::new(Mutex::new(Vec::new()));
            let connects = Arc::new(AtomicUsize::new(0));
            let successes = Arc::new(AtomicUsize::new(0));
            let allow = Arc::new(AtomicBool::new(true));
            let opts = ReconnectOptions {
                sleep: recording_sleeper(Arc::clone(&sleeps)),
                connect: {
                    let connects = Arc::clone(&connects);
                    let allow = Arc::clone(&allow);
                    Arc::new(move |_| {
                        let n = connects.fetch_add(1, Ordering::AcqRel);
                        let ok = allow.load(Ordering::Acquire) && n >= fail_first;
                        let fut = async move {
                            Ok(Box::new(FakeTransport {
                                fail_initialize: !ok,
                                on_close: Mutex::new(None),
                            }) as Box<dyn McpTransport>)
                        };
                        Box::pin(fut)
                    })
                },
                ..ReconnectOptions::default()
            };
            let status = Arc::new(Mutex::new(McpServerStatus {
                state: McpConnState::Connected,
                last_error: None,
            }));
            let successes_clone = Arc::clone(&successes);
            let on_success = Arc::new(move |_name: &str| {
                successes_clone.fetch_add(1, Ordering::AcqRel);
                let fut = std::future::ready(());
                Box::pin(fut) as BoxFuture<'static, ()>
            });
            let client = Arc::new(McpClient::from_transport(Box::new(FakeTransport {
                fail_initialize: false,
                on_close: Mutex::new(None),
            })));
            let hook = ReconnectSupervisor::spawn(
                "srv".to_string(),
                stdio_cfg(),
                Arc::clone(&client),
                Arc::clone(&status),
                on_success,
                Arc::new(|| true),
                opts,
            );
            client.set_reconnect_hook(hook.clone());
            Self {
                hook,
                sleeps,
                connects,
                successes,
                status,
                allow_success: allow,
            }
        }

        fn state(&self) -> McpConnState {
            self.status.lock().state
        }
    }

    fn stdio_cfg() -> McpServerConfig {
        McpServerConfig::Stdio(agent_config::McpStdioConfig {
            command: "true".to_string(),
            args: vec![],
            env: Default::default(),
            timeout_ms: None,
        })
    }

    /// 轮询等待谓词成立（监督任务在独立 tokio task 中推进）。
    async fn wait_until(f: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !f() {
            assert!(tokio::time::Instant::now() < deadline, "等待监督器推进超时");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn reconnect_backs_off_then_recovers() {
        let rig = Rig::new(2); // 第 1、2 次连接失败，第 3 次成功
        (rig.hook)();
        wait_until(|| rig.successes.load(Ordering::Acquire) == 1).await;
        // 退避间隔 = omp delays 前缀：失败一次睡 500ms，再失败睡 1000ms。
        assert_eq!(
            *rig.sleeps.lock(),
            vec![Duration::from_millis(500), Duration::from_millis(1_000)]
        );
        let st = rig.status.lock().clone();
        assert_eq!(st.state, McpConnState::Connected);
        assert_eq!(st.last_error, None);
        // 回收旧传输（close 触发其断连处理器）不得自触发新周期：
        // 稳定后连接尝试数恰为 3（1 个周期 = 3 次尝试）。
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            rig.connects.load(Ordering::Acquire),
            3,
            "回收旧传输不得自触发多余重连周期"
        );
    }

    #[tokio::test]
    async fn burst_failures_open_circuit_then_half_open_probe_recovers() {
        let rig = Rig::new(0);
        rig.allow_success.store(false, Ordering::Release); // 全部连接失败
        // 前 5 个周期正常执行（每周期 5 次尝试），均失败。
        for i in 1..=RECONNECT_BURST_LIMIT {
            (rig.hook)();
            wait_until(|| rig.connects.load(Ordering::Acquire) >= i * 5).await;
            assert_eq!(rig.state(), McpConnState::Reconnecting, "周期 {i} 后未熔断");
        }
        // 第 6 次触发：窗口内第 6 个周期 → 熔断 open。
        (rig.hook)();
        wait_until(|| rig.state() == McpConnState::Open).await;
        // 冷却 = 爆发窗口（假时钟即时返回并记录）。
        assert!(
            rig.sleeps
                .lock()
                .contains(&Duration::from_millis(RECONNECT_BURST_WINDOW_MS)),
            "熔断后应先冷却一个窗口时长再探活"
        );
        // 半开探活：放行连接 → 恢复 Connected（close 熔断）。
        rig.allow_success.store(true, Ordering::Release);
        wait_until(|| rig.state() == McpConnState::Connected).await;
        assert_eq!(rig.successes.load(Ordering::Acquire), 1);
        let st = rig.status.lock().clone();
        assert_eq!(st.last_error, None);
    }

    #[tokio::test]
    async fn retry_exhausted_keeps_reconnecting_status() {
        let rig = Rig::new(usize::MAX); // 永远失败
        (rig.hook)();
        wait_until(|| rig.connects.load(Ordering::Acquire) >= 5).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let st = rig.status.lock().clone();
        assert_eq!(st.state, McpConnState::Reconnecting, "重试耗尽保持重连中");
        assert!(st.last_error.is_some(), "应记录最近错误");
        // 退避序列完整：500/1000/2000/4000（5 次尝试之间 4 次休眠）。
        assert_eq!(
            *rig.sleeps.lock(),
            vec![
                Duration::from_millis(500),
                Duration::from_millis(1_000),
                Duration::from_millis(2_000),
                Duration::from_millis(4_000)
            ]
        );
    }
}
