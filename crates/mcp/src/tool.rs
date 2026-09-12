//! MCP tool bridge：把 server 工具包装为 agent [`Tool`]。

use std::collections::HashMap;
use std::sync::Arc;

use agent_config::{McpConfig, McpServerConfig};
use agent_core::{
    CapabilityTier, ToolError, ToolResult,
    resource::{ResourceEntry, ResourceError, ResourceResolver},
};
use agent_tools::{Tool, ToolContext};
use async_trait::async_trait;
use futures::future::BoxFuture;
use tokio::sync::mpsc;

use crate::cache::{self, CachedTool, CachedTools};
use crate::client::{
    McpClient, McpError, McpNotification, McpPromptInfo, McpResource, McpToolInfo, notifications,
};
use crate::reconnect::{
    McpConnState, McpServerStatus, ReconnectOptions, ReconnectSupervisor, StatusCell, SuccessFn,
};

/// 把单个 MCP server 工具包装为 agent [`Tool`]。
///
/// 连接方式两态（[`ToolConn`]）：已握手的 server 直接复用共享 [`McpClient`]；
/// 启动预算内未完成握手的 server 以「缓存元信息 + 连接槽」形态注册（对标 omp
/// `DeferredMCPTool`）——元信息立即可见，execute 时等待后台连接就绪再经 `tools/call` 调用。
#[derive(Clone)]
pub struct McpTool {
    info: McpToolInfo,
    /// 对模型暴露的命名空间化名称 `mcp__<server>_<tool>`（防跨 server 同名冲突）。
    alias: String,
    /// 来源 server 名（清单刷新按 server 替换、跨 server 去重 origin key 用）。
    server: String,
    conn: ToolConn,
}

/// 工具与 server 的连接方式。
#[derive(Clone)]
enum ToolConn {
    /// 已连接：直接复用共享 client。
    Live(Arc<McpClient>),
    /// 延迟连接：execute 时等待后台连接就绪（连接失败则回填可读错误）。
    Deferred(Arc<ConnSlot>),
}

/// 延迟连接槽的状态（[`ConnSlot`] 内部，经 `watch` 广播）。
#[derive(Clone)]
enum ConnState {
    /// 后台连接进行中。
    Pending,
    /// 连接就绪。
    Ready(Arc<McpClient>),
    /// 连接失败（原因文本）。
    Failed(String),
}

/// 延迟连接槽：后台连接任务写、Deferred 工具读（对标 omp `waitForConnection`）。
///
/// 用 `tokio::sync::watch` 而非 `Notify`：状态与通知一体，不存在「先检查后等待」
/// 之间丢通知的窗口。
struct ConnSlot {
    tx: tokio::sync::watch::Sender<ConnState>,
}

impl ConnSlot {
    fn new() -> Self {
        let (tx, _rx) = tokio::sync::watch::channel(ConnState::Pending);
        Self { tx }
    }

    fn receiver(&self) -> tokio::sync::watch::Receiver<ConnState> {
        self.tx.subscribe()
    }

    fn set_ready(&self, client: Arc<McpClient>) {
        let _ = self.tx.send_replace(ConnState::Ready(client));
    }

    fn set_failed(&self, error: String) {
        let _ = self.tx.send_replace(ConnState::Failed(error));
    }
}

/// 等待连接就绪（失败返回原因文本）。
async fn wait_ready(
    rx: &mut tokio::sync::watch::Receiver<ConnState>,
) -> Result<Arc<McpClient>, String> {
    loop {
        match rx.borrow_and_update().clone() {
            ConnState::Pending => {}
            ConnState::Ready(client) => return Ok(client),
            ConnState::Failed(e) => return Err(e),
        }
        if rx.changed().await.is_err() {
            return Err("连接槽已关闭（注册表释放）".to_string());
        }
    }
}

/// 启动连接总预算（毫秒，对齐 omp `STARTUP_TIMEOUT_MS`）：超时未完成握手的 server
/// 转入后台连接，不再阻塞启动（omp issue #2100：单个慢 server 曾拖满 30s 请求超时）。
pub const STARTUP_BUDGET_MS: u64 = 250;

/// [`McpRegistry::load`] 的加载参数。
#[derive(Debug, Clone)]
pub struct McpLoadOptions {
    /// 启动连接总预算（见 [`STARTUP_BUDGET_MS`]）。
    pub startup_budget: std::time::Duration,
    /// 工具清单缓存目录：连接成功的 server 落盘快照；预算内未完成的 server 用
    /// 适用快照（版本 + 配置指纹 + TTL 全中）注册 Deferred 工具。
    /// `None` = 不读写缓存，未完成握手的 server 不注册工具（后台连接成功后补注册）。
    pub cache_dir: Option<std::path::PathBuf>,
    /// 客户端工作区根（H16）：非空时在 `initialize` 声明 `roots` 能力，并以之应答
    /// server 的 `roots/list` 请求（如文件系统类 server 用它限定可访问范围）。
    pub roots: Vec<crate::client::McpRoot>,
}

impl Default for McpLoadOptions {
    fn default() -> Self {
        Self {
            startup_budget: std::time::Duration::from_millis(STARTUP_BUDGET_MS),
            cache_dir: None,
            roots: Vec::new(),
        }
    }
}

/// 从缓存快照 mint Deferred 工具（元信息可见、执行时等连接就绪）。
///
/// 快照存 server 端原始工具名，别名按与 live 工具同一规则派生（[`mint_tool_name`]），
/// 因此后台连接完成后同一工具名无缝从 Deferred 换成 live。
fn mint_deferred_tools(slot: &Arc<ConnSlot>, server: &str, cached: &[CachedTool]) -> Vec<McpTool> {
    cached
        .iter()
        .map(|c| McpTool {
            alias: mint_tool_name(server, &c.name),
            server: server.to_string(),
            conn: ToolConn::Deferred(Arc::clone(slot)),
            info: McpToolInfo {
                name: c.name.clone(),
                description: c.description.clone(),
                schema: c.input_schema.clone(),
            },
        })
        .collect()
}

impl McpTool {
    /// 来源 server 名。
    #[must_use]
    pub fn server(&self) -> &str {
        &self.server
    }

    /// 是否处于延迟连接形态（元信息来自缓存快照，执行时等待连接就绪）。
    #[must_use]
    pub fn is_deferred(&self) -> bool {
        matches!(self.conn, ToolConn::Deferred(_))
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.alias
    }
    fn description(&self) -> &str {
        &self.info.description
    }
    fn schema(&self) -> serde_json::Value {
        self.info.schema.clone()
    }
    fn capability(&self) -> CapabilityTier {
        // MCP 工具副作用未知（可能删文件、执行命令、网络请求），保守归为 Execute，
        // 确保破坏性 MCP 工具须经审批门禁而非自动放行。
        CapabilityTier::Execute
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let client = match &self.conn {
            ToolConn::Live(client) => Arc::clone(client),
            ToolConn::Deferred(slot) => {
                let mut rx = slot.receiver();
                wait_ready(&mut rx).await.map_err(|e| {
                    ToolError::Execution(format!(
                        "MCP `{}`: server `{}` 未连接（工具元信息来自缓存快照）：{e}",
                        self.info.name, self.server
                    ))
                })?
            }
        };
        let outcome = client
            .call_tool_full(&self.info.name, input)
            .await
            .map_err(|e| ToolError::Execution(format!("MCP `{}`: {e}", self.info.name)))?;
        // H17：server 标记 isError → 结构化错误（模型据此调整策略），而非伪装成成功文本。
        if outcome.is_error {
            let msg = outcome.render_text();
            return Ok(ToolResult::Error {
                recoverable: true,
                message: if msg.is_empty() {
                    format!("MCP 工具 `{}` 返回 isError", self.info.name)
                } else {
                    msg
                },
            });
        }
        // H17：纯图像结果（单块）→ 多模态 ToolResult，真实像素直达模型。
        if let Some((data, mime)) = outcome.first_image() {
            if let Some(bytes) = decode_base64(data) {
                return Ok(ToolResult::Image {
                    mime: mime.to_string(),
                    data: bytes,
                });
            }
        }
        Ok(ToolResult::text(outcome.render_text()))
    }
}

/// base64 解码（图像块负载）；非法输入返回 `None`（调用方回退文本渲染）。
fn decode_base64(data: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}
/// 工具名消毒：小写化，`[^a-z0-9_]+` → `_`，折叠连续 `_`，去首尾 `_`；
/// 空结果回退 `fallback`（对齐 omp `sanitizeMCPToolNamePart`）。
fn sanitize_name_part(value: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_lowercase() || ch == '_' {
            out.push(ch);
        } else if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_underscore = false;
    for ch in out.chars() {
        if ch == '_' && prev_underscore {
            continue;
        }
        prev_underscore = ch == '_';
        collapsed.push(ch);
    }
    let trimmed = collapsed.trim_matches('_');
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}
/// FNV-1a 64 位 → base36（跨进程稳定；供超长工具名哈希后缀与缓存配置指纹）。
pub(crate) fn fnv1a_base36(value: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in value.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    let mut v = hash;
    while v > 0 {
        out.push(DIGITS[(v % 36) as usize]);
        v /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// 严格校验器工具名上限：OpenAI/Meta 系 `^[a-zA-Z0-9_-]{1,64}$`，超长 400
///（omp #9130）。超限时保留可读前缀 + 全名 8 位 base36 哈希后缀，保证唯一且稳定。
fn cap_tool_name_length(name: String) -> String {
    const MAX: usize = 64;
    if name.len() <= MAX {
        return name;
    }
    let hash = fnv1a_base36(&name);
    let hash = if hash.len() > 8 {
        hash[..8].to_string()
    } else {
        hash
    };
    let keep = MAX.saturating_sub(hash.len() + 1);
    format!("{}_{hash}", &name[..keep])
}

/// 生成 MCP 工具的命名空间化名称：`mcp__<server>_<tool>`。
///
/// 工具名已带 `<server>_` 前缀时去冗余（`puppeteer` + `puppeteer_screenshot` →
/// `mcp__puppeteer_screenshot`）。对齐 omp `createMCPToolName`。
#[must_use]
pub fn mint_tool_name(server: &str, tool: &str) -> String {
    let server = sanitize_name_part(server, "server");
    let mut tool = sanitize_name_part(tool, "tool");
    let prefix = format!("{server}_");
    if let Some(stripped) = tool.strip_prefix(&prefix) {
        tool = stripped.to_string();
    }
    cap_tool_name_length(format!("mcp__{server}_{tool}"))
}

/// 按 (别名, origin key) 元组去重：同别名冲突时保留 origin key（`server\0tool`）
/// 较小者——稳定赢家规则，与装配顺序无关（对齐 omp `deduplicateMCPToolsByName`）。
fn dedupe_by_alias<T>(items: Vec<(String, String, T)>) -> Vec<(String, T)> {
    let mut best: std::collections::HashMap<String, (String, T)> = std::collections::HashMap::new();
    for (alias, origin, payload) in items {
        match best.get(&alias) {
            Some((winner, _)) if *winner <= origin => {
                tracing::warn!(
                    alias = %alias,
                    loser = %origin,
                    winner = %winner,
                    "MCP 工具别名冲突，保留稳定赢家"
                );
            }
            Some(_) | None => {
                best.insert(alias, (origin, payload));
            }
        }
    }
    let mut out: Vec<(String, T)> = best
        .into_iter()
        .map(|(alias, (_, payload))| (alias, payload))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 工具清单变更监听器：server 通知 `tools/list_changed` 且重拉成功后触发，
/// 参数为 server 名；最新清单请重读 [`McpRegistry::tools`] 获取。
pub type ToolsChangedListener = Arc<dyn Fn(&str) + Send + Sync>;

/// 注册表共享态：连接、分 server 工具清单、变更监听器（`Arc` 供通知消费任务持有）。
#[derive(Default)]
struct RegistryShared {
    /// server 名 → client（保活 + 通知重拉 + `mcp://` 资源路由）。
    /// 可运行期追加：启动预算内未完成握手的 server 在后台连接成功后补线。
    named: parking_lot::RwLock<Vec<(String, Arc<McpClient>)>>,
    /// server 名 → 当前工具清单（命名空间化；含 Deferred 形态；读取时跨 server 统一去重）。
    tools: parking_lot::RwLock<HashMap<String, Vec<McpTool>>>,
    /// server 名 → 提示词模板清单（`prompts/list`；连带服务器增删与 `prompts/list_changed` 刷新）。
    prompts: parking_lot::RwLock<HashMap<String, Vec<McpPromptInfo>>>,
    /// 工具清单变更监听器（重拉成功后逐个触发，互相隔离）。
    listeners: parking_lot::Mutex<Vec<ToolsChangedListener>>,
    /// server 名 → 连接状态槽（重连监督 / 后台连接任务写，[`McpRegistry::server_status`] 读）。
    statuses: parking_lot::RwLock<HashMap<String, StatusCell>>,
    /// server 名 → 延迟连接槽（预算内未完成握手的 server；其 Deferred 工具共享等待）。
    slots: HashMap<String, Arc<ConnSlot>>,
    /// 全部配置过的 server 配置（缓存指纹与重连重建传输用）。
    cfgs: HashMap<String, McpServerConfig>,
    /// 工具清单缓存目录（`None` = 不落盘）。
    cache_dir: Option<std::path::PathBuf>,
}

impl RegistryShared {
    /// 取（或建立）server 的状态槽。
    fn status_cell(&self, server: &str) -> StatusCell {
        let mut map = self.statuses.write();
        Arc::clone(map.entry(server.to_string()).or_insert_with(|| {
            Arc::new(parking_lot::Mutex::new(McpServerStatus {
                state: McpConnState::Connecting,
                last_error: None,
            }))
        }))
    }

    /// 写入连接状态（`last_error = None` 表示恢复成功，清空旧错误）。
    fn set_status(&self, server: &str, state: McpConnState, last_error: Option<String>) {
        let cell = self.status_cell(server);
        let mut slot = cell.lock();
        slot.state = state;
        slot.last_error = last_error;
    }

    /// 把 server 当前 live 工具清单落盘为缓存快照（best-effort）。
    ///
    /// 只写 live 工具：Deferred 工具本身就是快照内容，写回等于给陈旧快照续期 TTL。
    /// `config_hash` 取该 server 当前配置的指纹，配置一改旧快照即失效。
    fn store_snapshot(&self, server: &str, tools: &[McpTool]) {
        let Some(dir) = &self.cache_dir else {
            return;
        };
        let Some(cfg) = self.cfgs.get(server) else {
            return;
        };
        let entries: Vec<CachedTool> = tools
            .iter()
            .filter(|t| !t.is_deferred())
            .map(|t| CachedTool {
                // 缓存 server 端原始名（别名每次由 `mint_tool_name` 派生，不落盘）。
                name: t.info.name.clone(),
                description: t.info.description.clone(),
                input_schema: t.info.schema.clone(),
            })
            .collect();
        if entries.is_empty() {
            return;
        }
        let cached = CachedTools {
            server: server.to_string(),
            tools: entries,
            cached_at_ms: cache::now_ms(),
            config_hash: cache::config_fingerprint(cfg),
            version: cache::CACHE_VERSION,
        };
        if let Err(e) = cache::store(dir, &cached) {
            tracing::warn!(server, error = %e, "MCP 工具缓存写入失败");
        }
    }

    /// 接线一个刚握手的 server（启动预算内直连与预算外后台补线共用同一路径）。
    ///
    /// 顺序：通知转发 → 重连监督 → 连接登记 → 清单替换 → 状态置 Connected →
    /// 解析延迟连接槽（在途 Deferred 工具恢复可用）→ 缓存快照 → 变更监听器。
    fn install_live_server(
        self: &Arc<Self>,
        name: &str,
        client: Arc<McpClient>,
        tools: Vec<McpTool>,
        prompts: Vec<McpPromptInfo>,
        notify_tx: &mpsc::UnboundedSender<(String, McpNotification)>,
        reconnect: &ReconnectOptions,
    ) {
        // 通知转发：该 client 的通知带 server 名汇入共享队列。
        let tx = notify_tx.clone();
        let cname = name.to_string();
        client.set_on_notification(Arc::new(move |n| {
            let _ = tx.send((cname.clone(), n));
        }));
        // 重连监督：连接级失败 / 传输断连 → 退避重连 → 恢复刷新。
        if let Some(cfg) = self.cfgs.get(name) {
            let weak_alive = Arc::downgrade(self);
            let alive = Arc::new(move || weak_alive.upgrade().is_some())
                as Arc<dyn Fn() -> bool + Send + Sync>;
            let weak_refresh = Arc::downgrade(self);
            let sname = name.to_string();
            let on_success: SuccessFn =
                Arc::new(move |_| refresh_after_reconnect(weak_refresh.clone(), sname.clone()));
            let hook = ReconnectSupervisor::spawn(
                name.to_string(),
                cfg.clone(),
                Arc::clone(&client),
                self.status_cell(name),
                on_success,
                alive,
                reconnect.clone(),
            );
            client.set_reconnect_hook(hook);
        }
        {
            let mut named = self.named.write();
            if !named.iter().any(|(n, _)| n == name) {
                named.push((name.to_string(), Arc::clone(&client)));
            }
        }
        self.tools.write().insert(name.to_string(), tools.clone());
        self.prompts.write().insert(name.to_string(), prompts);
        self.set_status(name, McpConnState::Connected, None);
        if let Some(slot) = self.slots.get(name) {
            // 在途 Deferred 工具（已分发到运行中 Agent）随槽恢复可执行。
            slot.set_ready(client);
        }
        self.store_snapshot(name, &tools);
        self.fire_tools_changed(name);
    }

    /// server→client 通知分发（对标 omp `#handleServerNotification`）：
    /// tools 清单变更触发重拉，resources/prompts 本变更有消费面时另接，未知忽略。
    async fn handle_notification(&self, server: &str, notif: McpNotification) {
        match notif.method.as_str() {
            notifications::TOOLS_LIST_CHANGED => match self.refresh_server_tools(server).await {
                Ok(count) => {
                    tracing::info!(server, tools = count, "MCP 工具清单已刷新");
                    self.fire_tools_changed(server);
                }
                Err(e) => tracing::warn!(server, error = %e, "MCP 工具清单刷新失败"),
            },
            notifications::RESOURCES_LIST_CHANGED => {
                tracing::info!(server, "收到 resources/list_changed（无消费面，仅记录）");
            }
            notifications::PROMPTS_LIST_CHANGED => {
                match self.refresh_server_prompts(server).await {
                    Ok(count) => tracing::info!(server, prompts = count, "MCP 提示词清单已刷新"),
                    Err(e) => tracing::warn!(server, error = %e, "MCP 提示词清单刷新失败"),
                }
            }
            other => tracing::debug!(server, method = other, "忽略 MCP server 通知"),
        }
    }

    /// 重拉指定 server 的 `tools/list` 并整体替换其清单，返回刷新后的工具数。
    async fn refresh_server_tools(&self, server: &str) -> Result<usize, McpError> {
        let client = self
            .named
            .read()
            .iter()
            .find(|(n, _)| n == server)
            .map(|(_, c)| Arc::clone(c));
        let Some(client) = client else {
            // 通知晚于连接移除等场景：静默跳过。
            tracing::debug!(server, "tools/list_changed 来自未注册的 server，跳过刷新");
            return Ok(0);
        };
        let infos = client.list_tools().await?;
        let count = infos.len();
        let tools = mint_tools(server, &client, infos);
        self.tools.write().insert(server.to_string(), tools.clone());
        self.store_snapshot(server, &tools);
        Ok(count)
    }

    /// 重拉指定 server 的 `prompts/list` 并整体替换其清单，返回刷新后的提示词数。
    async fn refresh_server_prompts(&self, server: &str) -> Result<usize, McpError> {
        let client = self
            .named
            .read()
            .iter()
            .find(|(n, _)| n == server)
            .map(|(_, c)| Arc::clone(c));
        let Some(client) = client else {
            tracing::debug!(server, "prompts/list_changed 来自未注册的 server，跳过刷新");
            return Ok(0);
        };
        let prompts = client.list_prompts().await?;
        let count = prompts.len();
        self.prompts.write().insert(server.to_string(), prompts);
        Ok(count)
    }

    /// 逐个触发工具清单变更监听器（互相隔离：单个 panic 不影响其余与通知循环）。
    fn fire_tools_changed(&self, server: &str) {
        let listeners = self.listeners.lock().clone();
        for listener in listeners {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(server))).is_err()
            {
                tracing::warn!(server, "MCP 工具清单变更监听器 panic，已跳过");
            }
        }
    }
}

/// 多 MCP server 注册表：启动、握手、收集全部工具。
///
/// 单个 server `失败（连接/initialize/list_tools）不阻断其余；失败告警并跳过`。
/// 连接成功的 server 挂接通知消费：`tools/list_changed` 自动重拉刷新清单。
#[derive(Clone, Default)]
pub struct McpRegistry {
    shared: Arc<RegistryShared>,
}

impl McpRegistry {
    /// 从 `[mcp.servers]` 配置加载所有 server 的全部工具。
    ///
    /// 启动预算（[`McpLoadOptions::startup_budget`]，默认 250ms）内未完成握手的 server
    /// **不阻塞启动**：有适用缓存快照时立即以 Deferred 工具注册（元信息可见、执行时等
    /// 连接就绪），无快照则后台连接成功后补注册并触发变更监听器。
    /// 无 server 或全部失败时返回空注册表（不报错，适配无 MCP 的环境）。
    #[must_use]
    pub async fn load(cfg: &McpConfig, opts: &McpLoadOptions) -> Self {
        Self::load_with(cfg, opts, LoadSeams::default()).await
    }

    /// [`McpRegistry::load`] 的可注入变体（连接/重连 seam 供测试驱动）。
    #[must_use]
    pub(crate) async fn load_with(
        cfg: &McpConfig,
        opts: &McpLoadOptions,
        seams: LoadSeams,
    ) -> Self {
        // 配置快照按名排序：HashMap 迭代序不确定，启动顺序与日志需稳定。
        let mut cfgs: Vec<(String, McpServerConfig)> = cfg
            .servers
            .iter()
            .map(|(name, server_cfg)| (name.clone(), server_cfg.clone()))
            .collect();
        cfgs.sort_by(|a, b| a.0.cmp(&b.0));

        // 每 server 一个后台连接任务：connect → initialize → tools/list。
        let mut tasks: Vec<(String, tokio::task::JoinHandle<Result<LiveServer, String>>)> = cfgs
            .iter()
            .map(|(name, server_cfg)| {
                let future = (seams.connect)(server_cfg, &opts.roots);
                (name.clone(), tokio::spawn(future))
            })
            .collect();

        let deadline = tokio::time::Instant::now() + opts.startup_budget;
        let mut live: Vec<LiveEntry> = Vec::new();
        let mut initial: HashMap<String, Vec<McpTool>> = HashMap::new();
        let mut statuses: HashMap<String, StatusCell> = HashMap::new();
        let mut slots: HashMap<String, Arc<ConnSlot>> = HashMap::new();
        let mut pending: Vec<PendingEntry> = Vec::new();
        for (name, mut task) in tasks.drain(..) {
            match tokio::time::timeout_at(deadline, &mut task).await {
                // 预算内完成（成功 / 失败）：settled 在下一轮统一接线。
                Ok(joined) => {
                    let result = joined.unwrap_or_else(|e| Err(format!("连接任务异常终止: {e}")));
                    match result {
                        Ok(live_server) => live.push((
                            name,
                            live_server.client,
                            live_server.infos,
                            live_server.prompts,
                        )),
                        Err(e) => {
                            tracing::warn!(server = %name, error = %e, "MCP server 连接失败，跳过");
                            statuses.insert(
                                name,
                                Arc::new(parking_lot::Mutex::new(McpServerStatus {
                                    state: McpConnState::Failed,
                                    last_error: Some(e),
                                })),
                            );
                        }
                    }
                }
                // 超预算：转后台连接（有适用缓存则先以 Deferred 工具注册）。
                Err(_elapsed) => {
                    let slot = Arc::new(ConnSlot::new());
                    let server_cfg = cfgs
                        .iter()
                        .find(|(n, _)| n == &name)
                        .map(|(_, c)| c.clone())
                        .expect("配置快照含全部 server");
                    match opts
                        .cache_dir
                        .as_ref()
                        .and_then(|dir| cache::load_applicable(dir, &name, &server_cfg))
                    {
                        Some(cached) => {
                            tracing::warn!(
                                server = %name,
                                tools = cached.tools.len(),
                                "MCP server 启动预算内未完成握手，先用缓存快照注册 Deferred 工具"
                            );
                            initial.insert(
                                name.clone(),
                                mint_deferred_tools(&slot, &name, &cached.tools),
                            );
                        }
                        None => tracing::warn!(
                            server = %name,
                            budget_ms = opts.startup_budget.as_millis(),
                            "MCP server 启动预算内未完成握手，后台继续连接（暂无可信工具清单）"
                        ),
                    }
                    statuses.insert(
                        name.clone(),
                        Arc::new(parking_lot::Mutex::new(McpServerStatus {
                            state: McpConnState::Connecting,
                            last_error: None,
                        })),
                    );
                    slots.insert(name.clone(), Arc::clone(&slot));
                    pending.push((name, task, slot));
                }
            }
        }

        // 通知队列 + 监听器 + 后台补线：与 `from_clients` 共用同一套接线路径。
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, McpNotification)>();
        let shared = Arc::new(RegistryShared {
            named: parking_lot::RwLock::new(Vec::new()),
            tools: parking_lot::RwLock::new(initial),
            prompts: parking_lot::RwLock::new(HashMap::new()),
            listeners: parking_lot::Mutex::new(Vec::new()),
            statuses: parking_lot::RwLock::new(statuses),
            slots,
            cfgs: cfgs.iter().cloned().collect(),
            cache_dir: opts.cache_dir.clone(),
        });
        for (name, client, infos, prompts) in live {
            tracing::info!(server = %name, "MCP server 已连接");
            let tools = mint_tools(&name, &client, infos);
            shared.install_live_server(&name, client, tools, prompts, &tx, &seams.reconnect);
        }
        // 后台等待预算外 server：成功补线（清单 + 状态 + 槽 + 缓存 + 监听器），失败记状态。
        for (name, task, slot) in pending {
            let weak = Arc::downgrade(&shared);
            let tx = tx.clone();
            let reconnect = seams.reconnect.clone();
            tokio::spawn(async move {
                let result = task
                    .await
                    .unwrap_or_else(|e| Err(format!("连接任务异常终止: {e}")));
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                match result {
                    Ok(live_server) => {
                        tracing::info!(server = %name, "MCP server 后台连接完成");
                        let tools = mint_tools(&name, &live_server.client, live_server.infos);
                        shared.install_live_server(
                            &name,
                            live_server.client,
                            tools,
                            live_server.prompts,
                            &tx,
                            &reconnect,
                        );
                    }
                    Err(e) => {
                        tracing::warn!(server = %name, error = %e, "MCP server 后台连接失败");
                        slot.set_failed(e.clone());
                        shared.set_status(&name, McpConnState::Failed, Some(e));
                        shared.fire_tools_changed(&name);
                    }
                }
            });
        }
        let weak = Arc::downgrade(&shared);
        tokio::spawn(async move {
            while let Some((server, notif)) = rx.recv().await {
                let Some(shared) = weak.upgrade() else {
                    break;
                };
                shared.handle_notification(&server, notif).await;
            }
        });
        Self { shared }
    }

    /// 由已握手完成的 client 组装注册表，并挂接通知消费（重拉闭环）与重连监督。
    ///
    /// 通知链路：传输读循环 → `McpClient::set_on_notification` → 汇入共享队列 →
    /// 单消费任务顺序分发（见 [`RegistryShared::handle_notification`]）。
    /// 重连链路：连接级失败 / stdio 读循环退出 → `McpClient` 重连 hook →
    /// [`ReconnectSupervisor`]（退避 + 熔断）→ 成功后刷新清单并 fire 变更。
    /// 两个任务均持 `Weak`：注册表释放后任务随之退出，连接可正常回收。
    ///
    /// 仅供测试装配（预握手 client 注入）；生产路径走 [`McpRegistry::load`]。
    #[cfg(test)]
    pub(crate) fn from_clients(
        named: Vec<(String, Arc<McpClient>)>,
        mut initial: HashMap<String, Vec<McpTool>>,
        cfgs: Vec<(String, McpServerConfig)>,
        opts: impl Fn() -> ReconnectOptions,
    ) -> Self {
        // 清单已由调用方 mint：与 named 配对取出，统一走 `install_live_server` 接线。
        let installed: Vec<(String, Arc<McpClient>, Vec<McpTool>)> = named
            .into_iter()
            .map(|(name, client)| {
                let tools = initial.remove(&name).unwrap_or_default();
                (name, client, tools)
            })
            .collect();
        let shared = Arc::new(RegistryShared {
            named: parking_lot::RwLock::new(Vec::new()),
            tools: parking_lot::RwLock::new(initial),
            prompts: parking_lot::RwLock::new(HashMap::new()),
            listeners: parking_lot::Mutex::new(Vec::new()),
            statuses: parking_lot::RwLock::new(HashMap::new()),
            slots: HashMap::new(),
            cfgs: cfgs.iter().cloned().collect(),
            cache_dir: None,
        });
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, McpNotification)>();
        for (name, client, tools) in installed {
            shared.install_live_server(&name, client, tools, Vec::new(), &tx, &opts());
        }
        let weak = Arc::downgrade(&shared);
        tokio::spawn(async move {
            while let Some((server, notif)) = rx.recv().await {
                let Some(shared) = weak.upgrade() else {
                    break;
                };
                shared.handle_notification(&server, notif).await;
            }
        });
        Self { shared }
    }

    /// 所有已加载的 MCP 工具（跨 server 按别名稳定去重后的合并视图）。
    ///
    /// server 通知刷新清单后，消费方应重新调用以获取最新视图。
    #[must_use]
    pub fn tools(&self) -> Vec<McpTool> {
        let map = self.shared.tools.read();
        let candidates: Vec<(String, String, McpTool)> = map
            .values()
            .flatten()
            .map(|t| {
                (
                    t.alias.clone(),
                    format!("{}\0{}", t.server, t.info.name),
                    t.clone(),
                )
            })
            .collect();
        drop(map);
        dedupe_by_alias(candidates)
            .into_iter()
            .map(|(_, tool)| tool)
            .collect()
    }

    /// 是否未加载任何工具（Deferred 工具也算已加载：元信息对模型可见）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shared.tools.read().values().all(Vec::is_empty)
    }

    /// 已连接的 server 名列表（调试 / `mcp://` 提示用；不含仅后台连接中的 server）。
    #[must_use]
    pub fn server_names(&self) -> Vec<String> {
        self.shared
            .named
            .read()
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// 聚合全部已连 server 的 instructions（按配置顺序；未提供或为空者不出现）。
    ///
    /// 供装配层注入 system prompt（对标 omp `getServerInstructions`）：
    /// 返回 `(server 名, instructions 文本)` 列表，拼接格式由装配层决定。
    #[must_use]
    pub fn server_instructions(&self) -> Vec<(String, String)> {
        self.shared
            .named
            .read()
            .iter()
            .filter_map(|(name, client)| {
                let instructions = client.server_info()?.instructions?;
                Some((name.clone(), instructions))
            })
            .collect()
    }

    /// 全部配置 server 的连接状态快照（按 server 名排序）。
    ///
    /// 状态由连接/重连任务维护：`Connecting`（初次握手进行中）/ `Connected`（健康）/
    /// `Reconnecting`（退避重连中或等待下次触发）/ `Open`（熔断开启）/
    /// `Failed`（初次连接失败且无重连通道）+ 最近错误（恢复成功后清空）。
    #[must_use]
    pub fn server_status(&self) -> Vec<(String, McpServerStatus)> {
        let cells = self.shared.statuses.read();
        let mut names: Vec<&String> = cells.keys().collect();
        names.sort();
        names
            .into_iter()
            .map(|name| (name.clone(), cells[name].lock().clone()))
            .collect()
    }

    /// 处于延迟连接形态（元信息来自缓存快照）的 server 名（按名排序）。
    #[must_use]
    pub fn deferred_servers(&self) -> Vec<String> {
        let map = self.shared.tools.read();
        let mut names: Vec<String> = map
            .iter()
            .filter(|(_, tools)| tools.iter().any(McpTool::is_deferred))
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// 全部已连 server 的提示词模板（按 server 名、提示词名排序）。
    ///
    /// 宿主据此挂斜杠命令（Gyre：`/<server>:<prompt>`，对齐 omp `buildMCPPromptCommands`）：
    /// 命令名 = `<server>:<name>`，参数以 `key=value` 传入 [`McpRegistry::execute_prompt`]。
    #[must_use]
    pub fn prompts(&self) -> Vec<(String, McpPromptInfo)> {
        let map = self.shared.prompts.read();
        let mut out: Vec<(String, McpPromptInfo)> = map
            .iter()
            .flat_map(|(server, prompts)| {
                prompts
                    .iter()
                    .map(|p| (server.clone(), p.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
        out
    }

    /// 执行提示词模板（`prompts/get`），返回消息文本（空行分隔）。
    ///
    /// server 未连接（含仅后台连接中）或提示词不存在时返回可读错误文本。
    ///
    /// # Errors
    /// server 未连接 / 通信失败 / server 报错时返回原因字符串（供斜杠命令直接展示）。
    pub async fn execute_prompt(
        &self,
        server: &str,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<String, String> {
        let client = self
            .shared
            .named
            .read()
            .iter()
            .find(|(n, _)| n == server)
            .map(|(_, c)| Arc::clone(c))
            .ok_or_else(|| format!("MCP server `{server}` 未连接"))?;
        client
            .get_prompt(name, args)
            .await
            .map_err(|e| format!("MCP `{server}:{name}`: {e}"))
    }

    /// 注册工具清单变更监听器：server 通知 `tools/list_changed` 且重拉成功后触发。
    ///
    /// 可注册多个；单个监听器 panic 不影响其余与通知循环。注意：监听器内不要捕获
    /// 注册表自身（`Arc` 引用循环会阻止连接释放）；最新清单请重读 [`McpRegistry::tools`]。
    pub fn on_tools_changed(&self, listener: ToolsChangedListener) {
        self.shared.listeners.lock().push(listener);
    }

    /// 按 server 名查找 client（`mcp://` 路由用）。
    fn client(&self, server: &str) -> Result<Arc<McpClient>, ResourceError> {
        self.shared
            .named
            .read()
            .iter()
            .find(|(n, _)| n == server)
            .map(|(_, c)| Arc::clone(c))
            .ok_or_else(|| ResourceError::UnknownServer(server.to_string()))
    }
}

/// 重连成功后的恢复动作：重拉 `tools/list` 整体替换该 server 清单并 fire 变更监听器。
///
/// 独立函数以便 `Arc<dyn Fn>` 装配（闭包内直接 `async move` 会因高阶生命周期推断失败）。
fn refresh_after_reconnect(
    shared: std::sync::Weak<RegistryShared>,
    server: String,
) -> BoxFuture<'static, ()> {
    Box::pin(async move {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        match shared.refresh_server_tools(&server).await {
            Ok(count) => {
                tracing::info!(server = %server, tools = count, "MCP 重连后工具清单已刷新");
                shared.fire_tools_changed(&server);
            }
            Err(e) => tracing::warn!(server = %server, error = %e, "MCP 重连后工具清单刷新失败"),
        }
    })
}

/// 把 server 的工具元信息列表包装为命名空间化的 live [`McpTool`] 列表。
fn mint_tools(server: &str, client: &Arc<McpClient>, infos: Vec<McpToolInfo>) -> Vec<McpTool> {
    infos
        .into_iter()
        .map(|info| McpTool {
            alias: mint_tool_name(server, &info.name),
            server: server.to_string(),
            conn: ToolConn::Live(Arc::clone(client)),
            info,
        })
        .collect()
}

/// 启动预算内完成握手的 server 条目：名称 + client + 工具元信息 + 提示词清单。
type LiveEntry = (String, Arc<McpClient>, Vec<McpToolInfo>, Vec<McpPromptInfo>);

/// 预算外转后台连接的 server 条目：名称 + 连接任务 + 延迟连接槽。
type PendingEntry = (
    String,
    tokio::task::JoinHandle<Result<LiveServer, String>>,
    Arc<ConnSlot>,
);

/// 连接任务的产出：已握手 client + `tools/list` / `prompts/list` 元信息。
pub(crate) struct LiveServer {
    client: Arc<McpClient>,
    infos: Vec<McpToolInfo>,
    /// 提示词模板（server 不声明 prompts 能力时为空；失败不阻断连接）。
    prompts: Vec<McpPromptInfo>,
}

/// 连接 seam：默认真实 `connect → initialize → tools/list`；测试注入脚本化实现
/// （可挂起以触发启动预算超时、或返回错误模拟快速失败）。
pub(crate) type ConnectSeam = Arc<
    dyn Fn(
            &McpServerConfig,
            &[crate::client::McpRoot],
        ) -> BoxFuture<'static, Result<LiveServer, String>>
        + Send
        + Sync,
>;

/// 加载 seam 束（测试注入点；公共 [`McpLoadOptions`] 只暴露用户可调参数）。
pub(crate) struct LoadSeams {
    /// 重连监督参数（退避/熔断 + 测试时钟与传输 seam）。
    pub reconnect: ReconnectOptions,
    /// 连接 seam。
    pub connect: ConnectSeam,
}

impl Default for LoadSeams {
    fn default() -> Self {
        Self {
            reconnect: ReconnectOptions::default(),
            connect: Arc::new(|cfg: &McpServerConfig, roots: &[crate::client::McpRoot]| {
                let cfg = cfg.clone();
                let roots = roots.to_vec();
                Box::pin(async move {
                    let client = McpClient::connect(&cfg)
                        .await
                        .map_err(|e| format!("连接失败: {e}"))?;
                    // H16：登记工作区根（须在 initialize 之前，决定 roots 能力是否声明）。
                    client.set_roots(roots);
                    let client = Arc::new(client);
                    if let Err(e) = client.initialize().await {
                        client.close().await;
                        return Err(format!("initialize 失败: {e}"));
                    }
                    let infos = match client.list_tools().await {
                        Ok(infos) => infos,
                        Err(e) => {
                            client.close().await;
                            return Err(format!("list_tools 失败: {e}"));
                        }
                    };
                    // prompts 为可选能力：H15 能力门控 + 失败均不影响连接与工具面。
                    let prompts = match client.list_prompts().await {
                        Ok(prompts) => prompts,
                        Err(e) => {
                            tracing::debug!(error = %e, "MCP server 未提供 prompts 清单");
                            Vec::new()
                        }
                    };
                    Ok(LiveServer {
                        client,
                        infos,
                        prompts,
                    })
                })
            }),
        }
    }
}

/// MCP 工具的动态源：把注册表当前工具视图接进 agent 工具注册表
/// （[`agent_tools::ToolSource`]）。
///
/// 与 [`McpRegistry::tools`] 一样每次实时求值——运行中 Agent 下一轮重新取 `specs()` 时
/// 即可见 server 端工具增删（`tools/list_changed` 重拉、后台连接完成、重连恢复），
/// 无需重建 Agent 或工具注册表。
#[derive(Clone)]
pub struct McpToolSource {
    registry: Arc<McpRegistry>,
}

impl McpToolSource {
    /// 包装注册表为动态工具源。
    #[must_use]
    pub fn new(registry: Arc<McpRegistry>) -> Self {
        Self { registry }
    }
}

impl agent_tools::ToolSource for McpToolSource {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.registry
            .tools()
            .into_iter()
            .map(|t| {
                let tool: Arc<dyn Tool> = Arc::new(t);
                tool
            })
            .collect()
    }
}

/// `mcp://<server>/<uri>` 资源读取：按 server 名路由到对应 MCP client。
#[async_trait]
impl ResourceResolver for McpRegistry {
    async fn list_resources(&self, server: &str) -> Result<Vec<ResourceEntry>, ResourceError> {
        let client = self.client(server)?;
        client
            .list_resources()
            .await
            .map(|rs| {
                rs.into_iter()
                    .map(|r: McpResource| ResourceEntry {
                        uri: r.uri,
                        name: r.name,
                        description: r.description,
                        mime_type: r.mime_type,
                    })
                    .collect()
            })
            .map_err(|e| classify(server, e))
    }

    async fn read_resource(&self, server: &str, uri: &str) -> Result<String, ResourceError> {
        let client = self.client(server)?;
        client
            .read_resource(uri)
            .await
            .map_err(|e| classify(server, e))
    }
}

/// 把 MCP 错误分类为资源错误：`method not found` / `not supported` 视为不支持 resources。
fn classify(server: &str, e: McpError) -> ResourceError {
    let msg = e.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("-32601") || lower.contains("not found") || lower.contains("not supported") {
        ResourceError::Unsupported(server.to_string())
    } else {
        ResourceError::Read(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::McpTransport;
    use serde_json::{Value, json};

    /// 测试传输桩：可编程 initialize instructions 与 tools/list 结果 + 通知注入口。
    #[derive(Clone)]
    struct MockTransport(Arc<MockState>);

    struct MockState {
        init_instructions: Option<String>,
        /// `tools/list` 返回的原始工具数组（可运行中替换，模拟 server 端清单变更）。
        tools: parking_lot::Mutex<Vec<Value>>,
        /// `prompts/list` 返回的原始提示词数组。
        prompts: parking_lot::Mutex<Vec<Value>>,
        handler: parking_lot::Mutex<Option<crate::client::NotificationHandler>>,
    }

    impl MockTransport {
        fn new(init_instructions: Option<String>, tools: Vec<Value>) -> Self {
            Self(Arc::new(MockState {
                init_instructions,
                tools: parking_lot::Mutex::new(tools),
                prompts: parking_lot::Mutex::new(Vec::new()),
                handler: parking_lot::Mutex::new(None),
            }))
        }

        /// 预置 `prompts/list` 清单（斜杠命令面测试用）。
        fn with_prompts(self, prompts: Vec<Value>) -> Self {
            *self.0.prompts.lock() = prompts;
            self
        }

        /// 运行中替换 server 端提示词清单（下次 prompts/list 返回新集）。
        fn set_prompts(&self, prompts: Vec<Value>) {
            *self.0.prompts.lock() = prompts;
        }

        /// 模拟 server 推送一条通知帧。
        fn emit(&self, method: &str) {
            if let Some(h) = self.0.handler.lock().clone() {
                h(method, &Value::Null);
            }
        }

        /// 运行中替换 server 端工具清单（下次 tools/list 返回新集）。
        fn set_tools(&self, tools: Vec<Value>) {
            *self.0.tools.lock() = tools;
        }
    }

    #[async_trait]
    impl McpTransport for MockTransport {
        async fn request(&self, method: &str, _params: Value) -> Result<Value, McpError> {
            match method {
                "initialize" => Ok(json!({
                    "protocolVersion": "2024-11-05",
                    "instructions": self.0.init_instructions
                })),
                "tools/list" => Ok(json!({"tools": self.0.tools.lock().clone()})),
                "tools/call" => Ok(json!({"content": [{"type": "text", "text": format!(
                    "mock tools/call ok ({})",
                    _params.get("name").and_then(Value::as_str).unwrap_or("?")
                )}]})),
                "prompts/list" => Ok(json!({"prompts": self.0.prompts.lock().clone()})),
                "prompts/get" => Ok(json!({"messages": [
                    {"role": "user", "content": {"type": "text", "text": format!(
                        "prompt {} with {}",
                        _params.get("name").and_then(Value::as_str).unwrap_or("?"),
                        _params.get("arguments").cloned().unwrap_or(Value::Null)
                    )}},
                    {"role": "user", "content": [{"type": "text", "text": "second"}]}
                ]})),
                other => Err(McpError::Server(format!("mock 未实现 {other}"))),
            }
        }
        async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }
        async fn close(&self) {}
        fn set_on_notification(&self, handler: crate::client::NotificationHandler) {
            *self.0.handler.lock() = Some(handler);
        }
    }

    fn tool_value(name: &str) -> Value {
        json!({
            "name": name,
            "description": format!("{name} 描述"),
            "inputSchema": {"type": "object"}
        })
    }

    /// 组装单 mock server 注册表（先握手捕获 instructions，初始清单 = 当前 tools/list）。
    async fn mock_registry(
        mock: MockTransport,
    ) -> (McpRegistry, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let client = Arc::new(McpClient::from_transport(Box::new(mock)));
        client.initialize().await.expect("initialize");
        let infos = client.list_tools().await.expect("list_tools");
        let initial = HashMap::from([("srv".to_string(), mint_tools("srv", &client, infos))]);
        let registry = McpRegistry::from_clients(
            vec![("srv".to_string(), Arc::clone(&client))],
            initial,
            vec![],
            crate::reconnect::ReconnectOptions::default,
        );
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        registry.on_tools_changed(Arc::new(move |server| {
            let _ = tx.send(server.to_string());
        }));
        (registry, rx)
    }

    #[tokio::test]
    async fn tools_list_changed_refreshes_registry_and_fires_listener() {
        let mock = MockTransport::new(None, vec![tool_value("old_tool")]);
        let (registry, mut rx) = mock_registry(mock.clone()).await;
        assert_eq!(registry.tools().len(), 1);
        assert_eq!(registry.tools()[0].name(), "mcp__srv_old_tool");

        // server 端清单变更后推送 tools/list_changed。
        mock.set_tools(vec![tool_value("new_tool_a"), tool_value("new_tool_b")]);
        mock.emit(notifications::TOOLS_LIST_CHANGED);

        let server = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("监听器未触发")
            .expect("信道关闭");
        assert_eq!(server, "srv");
        // 清单整体替换为重拉到的新集（旧工具移除）。
        let names: Vec<String> = registry
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(names, vec!["mcp__srv_new_tool_a", "mcp__srv_new_tool_b"]);
    }

    #[tokio::test]
    async fn non_tools_notifications_are_ignored() {
        let mock = MockTransport::new(None, vec![tool_value("keep")]);
        let (registry, mut rx) = mock_registry(mock.clone()).await;

        // resources / prompts 清单变更与未知通知：不触发监听器、不报错。
        mock.emit(notifications::RESOURCES_LIST_CHANGED);
        mock.emit(notifications::PROMPTS_LIST_CHANGED);
        mock.emit("notifications/custom/event");

        // 最后推一条 tools 变更：单消费任务顺序处理，其监听器触发即证明前三条已消化。
        mock.set_tools(vec![tool_value("keep"), tool_value("extra")]);
        mock.emit(notifications::TOOLS_LIST_CHANGED);
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("监听器未触发")
            .expect("信道关闭");

        let names: Vec<String> = registry
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(names, vec!["mcp__srv_extra", "mcp__srv_keep"]);
        // 三条非 tools 通知均未触发监听器。
        assert!(rx.try_recv().is_err(), "非 tools 变更不得触发监听器");
    }

    #[tokio::test]
    async fn server_instructions_aggregate_across_clients() {
        // server alpha 提供 instructions；beta 未提供（不出现在聚合里）。
        let alpha = MockTransport::new(Some("礼貌使用 GitHub API。".to_string()), vec![]);
        let beta = MockTransport::new(None, vec![]);
        let ca = Arc::new(McpClient::from_transport(Box::new(alpha)));
        let cb = Arc::new(McpClient::from_transport(Box::new(beta)));
        ca.initialize().await.expect("init alpha");
        cb.initialize().await.expect("init beta");
        let registry = McpRegistry::from_clients(
            vec![
                ("alpha".to_string(), Arc::clone(&ca)),
                ("beta".to_string(), Arc::clone(&cb)),
            ],
            HashMap::new(),
            vec![],
            crate::reconnect::ReconnectOptions::default,
        );
        let instructions = registry.server_instructions();
        assert_eq!(
            instructions.len(),
            1,
            "未提供 instructions 的 server 不出现"
        );
        assert_eq!(
            instructions[0],
            ("alpha".to_string(), "礼貌使用 GitHub API。".to_string())
        );
    }

    #[test]
    fn mint_namespaces_server_and_tool() {
        assert_eq!(
            mint_tool_name("github", "create_issue"),
            "mcp__github_create_issue"
        );
        // server 名冗余前缀被剥离。
        assert_eq!(
            mint_tool_name("puppeteer", "puppeteer_screenshot"),
            "mcp__puppeteer_screenshot"
        );
        // 非法字符消毒为 `_`，连续折叠，去首尾。
        assert_eq!(
            mint_tool_name("My Server", "get-item!"),
            "mcp__my_server_get_item"
        );
        // 空消毒结果回退 fallback。
        assert_eq!(mint_tool_name("---", "+++"), "mcp__server_tool");
    }

    #[test]
    fn mint_caps_at_64_with_stable_hash_suffix() {
        let server = "s";
        let tool = "a".repeat(100);
        let name = mint_tool_name(server, &tool);
        assert!(name.len() <= 64, "超长名应截断: {name}");
        // 稳定性：同输入两次生成一致。
        assert_eq!(name, mint_tool_name(server, &tool));
        // 唯一性：不同超长名哈希不同。
        let other = mint_tool_name(server, &"b".repeat(100));
        assert_ne!(name, other);
    }

    #[test]
    fn dedupe_keeps_smallest_origin_per_alias() {
        let items = vec![
            ("mcp__a_tool".to_string(), "a\u{0}tool".to_string(), 1),
            ("mcp__a_tool".to_string(), "b\u{0}tool".to_string(), 2),
            ("mcp__b_tool".to_string(), "b\u{0}tool".to_string(), 3),
        ];
        let out = dedupe_by_alias(items);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], ("mcp__a_tool".to_string(), 1)); // origin 较小者胜出
        assert_eq!(out[1], ("mcp__b_tool".to_string(), 3));
        // 输出按别名排序（注册表顺序稳定）。
        assert!(out[0].0 < out[1].0);
    }

    #[tokio::test]
    async fn empty_config_yields_empty_registry() {
        let cfg = McpConfig::default();
        let reg = McpRegistry::load(&cfg, &McpLoadOptions::default()).await;
        assert!(reg.is_empty());
    }

    /// 会死亡的传输桩：置位后 `tools/call` 报连接丢失（模拟 server 进程崩溃）。
    #[derive(Clone)]
    struct DyingMock(Arc<DyingState>);

    struct DyingState {
        dead: std::sync::atomic::AtomicBool,
        tools: parking_lot::Mutex<Vec<Value>>,
        handler: parking_lot::Mutex<Option<crate::client::NotificationHandler>>,
    }

    impl DyingMock {
        fn new(tools: Vec<Value>) -> Self {
            Self(Arc::new(DyingState {
                dead: std::sync::atomic::AtomicBool::new(false),
                tools: parking_lot::Mutex::new(tools),
                handler: parking_lot::Mutex::new(None),
            }))
        }
        fn die(&self) {
            self.0.dead.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl McpTransport for DyingMock {
        async fn request(&self, method: &str, _params: Value) -> Result<Value, McpError> {
            match method {
                "initialize" => Ok(json!({"protocolVersion": "2024-11-05"})),
                "tools/list" => Ok(json!({"tools": self.0.tools.lock().clone()})),
                "tools/call" => {
                    if self.0.dead.load(std::sync::atomic::Ordering::SeqCst) {
                        Err(McpError::Closed)
                    } else {
                        Ok(json!({"content": [{"type": "text", "text": "ok"}]}))
                    }
                }
                other => Err(McpError::Server(format!("mock 未实现 {other}"))),
            }
        }
        async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }
        async fn close(&self) {}
        fn set_on_notification(&self, handler: crate::client::NotificationHandler) {
            *self.0.handler.lock() = Some(handler);
        }
    }

    /// 初始化必败的传输桩（模拟断连初期 server 尚未恢复）。
    struct DeadTransport;

    #[async_trait]
    impl McpTransport for DeadTransport {
        async fn request(&self, _method: &str, _params: Value) -> Result<Value, McpError> {
            Err(McpError::Closed)
        }
        async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }
        async fn close(&self) {}
    }

    #[tokio::test]
    async fn connection_loss_triggers_backoff_reconnect_refresh_and_listener() {
        // 初始传输：call_tool 报连接丢失（触发重连 hook）。
        let dying = DyingMock::new(vec![tool_value("old")]);
        let client = Arc::new(McpClient::from_transport(Box::new(dying.clone())));
        client.initialize().await.expect("initialize");
        let infos = client.list_tools().await.expect("list_tools");
        let initial = HashMap::from([("srv".to_string(), mint_tools("srv", &client, infos))]);

        // 重连脚本（FIFO）：第 1 次连接 initialize 失败 → 退避；第 2 次成功且清单更新。
        let healthy = MockTransport::new(
            None,
            vec![tool_value("reconnected_a"), tool_value("reconnected_b")],
        );
        let script = Arc::new(parking_lot::Mutex::new(vec![
            Box::new(DeadTransport) as Box<dyn McpTransport>,
            Box::new(healthy) as Box<dyn McpTransport>,
        ]));
        let sleep_log = Arc::new(parking_lot::Mutex::new(Vec::<std::time::Duration>::new()));

        let opts = {
            let script = Arc::clone(&script);
            let sleep_log = Arc::clone(&sleep_log);
            let connect: crate::reconnect::ConnectFn = Arc::new(move |_| {
                let script = Arc::clone(&script);
                Box::pin(async move {
                    let mut script = script.lock();
                    if script.is_empty() {
                        Err(McpError::Closed)
                    } else {
                        Ok(script.remove(0))
                    }
                })
            });
            let sleep: crate::reconnect::SleepFn = Arc::new(move |d| {
                sleep_log.lock().push(d);
                // 让出调度，避免重连循环饿死 current_thread 测试运行时。
                Box::pin(tokio::task::yield_now())
            });
            move || crate::reconnect::ReconnectOptions {
                sleep: sleep.clone(),
                connect: connect.clone(),
                ..Default::default()
            }
        };
        let registry = McpRegistry::from_clients(
            vec![("srv".to_string(), Arc::clone(&client))],
            initial,
            vec![(
                "srv".to_string(),
                McpServerConfig::Stdio(agent_config::McpStdioConfig {
                    command: "true".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                    timeout_ms: None,
                }),
            )],
            opts,
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        registry.on_tools_changed(Arc::new(move |server| {
            let _ = tx.send(server.to_string());
        }));

        // 初始状态 Connected。
        assert_eq!(
            registry.server_status(),
            vec![(
                "srv".to_string(),
                McpServerStatus {
                    state: McpConnState::Connected,
                    last_error: None
                }
            )]
        );

        // 工具调用遭遇连接丢失 → hook → 第 1 次重连失败 → 退避 500ms → 第 2 次成功。
        dying.die();
        let err = client
            .call_tool("t", json!({}))
            .await
            .expect_err("应报连接丢失");
        assert!(err.is_connection_lost(), "错误应归类为连接级失败: {err}");

        let server = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("监听器未触发（重连未完成）")
            .expect("信道关闭");
        assert_eq!(server, "srv");
        // 清单已整体替换为重连后 server 的新集。
        let names: Vec<String> = registry
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["mcp__srv_reconnected_a", "mcp__srv_reconnected_b"]
        );
        // 状态恢复 Connected、清错误；退避间隔恰为 omp delays 前缀（500ms）。
        assert_eq!(
            registry.server_status(),
            vec![(
                "srv".to_string(),
                McpServerStatus {
                    state: McpConnState::Connected,
                    last_error: None
                }
            )]
        );
        assert_eq!(
            *sleep_log.lock(),
            vec![std::time::Duration::from_millis(500)]
        );
    }

    // ── H15 启动预算 + Deferred 工具（seam 注入，无需真实进程） ──

    /// 可控连接 seam：直到 `gate` 放行才完成连接（放行后返回 mock 客户端）。
    /// `fail` 为 `Some(原因)` 时改为放行后报错，模拟后台连接失败。
    fn gated_seam(gate: Arc<tokio::sync::Notify>, fail: Option<String>) -> ConnectSeam {
        Arc::new(
            move |_cfg: &McpServerConfig, _roots: &[crate::client::McpRoot]| {
                let gate = Arc::clone(&gate);
                let fail = fail.clone();
                Box::pin(async move {
                    gate.notified().await;
                    if let Some(e) = fail {
                        return Err(e);
                    }
                    let mock = MockTransport::new(None, vec![tool_value("live_tool")]);
                    let client = Arc::new(McpClient::from_transport(Box::new(mock)));
                    client.initialize().await.map_err(|e| e.to_string())?;
                    let infos = client.list_tools().await.map_err(|e| e.to_string())?;
                    Ok(LiveServer {
                        client,
                        infos,
                        prompts: Vec::new(),
                    })
                })
            },
        )
    }

    fn stub_cfg() -> McpConfig {
        McpConfig {
            servers: HashMap::from([(
                "srv".to_string(),
                McpServerConfig::Stdio(agent_config::McpStdioConfig {
                    command: "stub".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                    timeout_ms: None,
                }),
            )]),
        }
    }

    /// 审批桩：全部放行（本组测试只验证连接路径，不涉门禁）。
    struct AllowAll;
    #[async_trait]
    impl agent_core::ApprovalPolicy for AllowAll {
        fn decide(&self, _req: &agent_core::ApprovalRequest<'_>) -> agent_core::ApprovalDecision {
            agent_core::ApprovalDecision::Allow
        }
        async fn prompt(
            &self,
            _ask: &agent_core::AskMessage,
        ) -> Result<agent_core::AskResponse, agent_core::ToolError> {
            Ok(agent_core::AskResponse::Yes)
        }
    }

    /// 最小 [`ToolContext`]（工具执行只用到 workspace/approval/cancel）。
    fn test_ctx<'a>(
        ws: &'a agent_core::Workspace,
        approval: &'a AllowAll,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> ToolContext<'a> {
        ToolContext {
            workspace: ws,
            approval,
            cancel,
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
            update_tx: None,
            conflicts: None,
            pending_rewrites: None,
            context: None,
            snapshots: None,
            tool_call_id: None,
        }
    }

    /// 沙箱缓存目录 + 一份「适用」快照（指纹对应当前配置）。
    fn seed_cache(tag: &str, cfg: &McpConfig, alias_tool: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gyre-mcp-deferred-{tag}-{n}"));
        cache::store(
            &dir,
            &CachedTools {
                server: "srv".to_string(),
                tools: vec![CachedTool {
                    name: alias_tool.to_string(),
                    description: "上次会话的工具".to_string(),
                    input_schema: json!({"type": "object"}),
                }],
                cached_at_ms: cache::now_ms(),
                config_hash: cache::config_fingerprint(&cfg.servers["srv"]),
                version: cache::CACHE_VERSION,
            },
        )
        .unwrap();
        dir
    }

    fn seams_with(gate: Arc<tokio::sync::Notify>, fail: Option<String>) -> LoadSeams {
        LoadSeams {
            reconnect: ReconnectOptions::default(),
            connect: gated_seam(gate, fail),
        }
    }

    /// 预算耗尽 → 缓存快照立即成为 Deferred 工具；后台连接完成后换成 live 工具并 fire 监听器。
    #[tokio::test]
    async fn budget_exceeded_defers_tools_then_late_connect_swaps_to_live() {
        let cfg = stub_cfg();
        let dir = seed_cache("swap", &cfg, "cached_tool");
        let gate = Arc::new(tokio::sync::Notify::new());
        let opts = McpLoadOptions {
            startup_budget: std::time::Duration::from_millis(1),
            cache_dir: Some(dir.clone()),
            roots: Vec::new(),
        };
        let registry =
            McpRegistry::load_with(&cfg, &opts, seams_with(Arc::clone(&gate), None)).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        registry.on_tools_changed(Arc::new(move |server| {
            let _ = tx.send(server.to_string());
        }));

        // 启动即刻：Deferred 工具（元信息来自快照），监听器尚未触发。
        let tools = registry.tools();
        assert_eq!(tools.len(), 1);
        assert!(tools[0].is_deferred());
        assert_eq!(tools[0].name(), "mcp__srv_cached_tool");
        assert_eq!(tools[0].description(), "上次会话的工具");
        assert_eq!(registry.deferred_servers(), vec!["srv".to_string()]);
        assert!(registry.server_names().is_empty(), "尚未握手完成");
        assert!(rx.try_recv().is_err(), "启动阶段不应触发变更监听器");

        // 放行后台连接 → 工具换成 live，监听器触发，状态转 connected。
        gate.notify_one();
        let server = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("后台连接完成后应 fire 变更监听器")
            .expect("信道关闭");
        assert_eq!(server, "srv");
        let tools = registry.tools();
        assert_eq!(tools.len(), 1);
        assert!(!tools[0].is_deferred(), "后台连接完成后应为 live 工具");
        assert_eq!(tools[0].name(), "mcp__srv_live_tool");
        assert!(registry.deferred_servers().is_empty());
        assert_eq!(
            registry.server_status()[0].1,
            McpServerStatus {
                state: McpConnState::Connected,
                last_error: None
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deferred 工具的 execute 等连接就绪：放行前阻塞，放行后经真实 `tools/call` 返回。
    #[tokio::test]
    async fn deferred_tool_execute_waits_for_connection() {
        let cfg = stub_cfg();
        let dir = seed_cache("wait", &cfg, "cached_tool");
        let gate = Arc::new(tokio::sync::Notify::new());
        let opts = McpLoadOptions {
            startup_budget: std::time::Duration::from_millis(1),
            cache_dir: Some(dir.clone()),
            roots: Vec::new(),
        };
        let registry =
            McpRegistry::load_with(&cfg, &opts, seams_with(Arc::clone(&gate), None)).await;
        let tool = Box::new(registry.tools().into_iter().next().expect("Deferred 工具"));
        assert!(tool.is_deferred());

        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let approval = AllowAll;
        let cancel = tokio_util::sync::CancellationToken::new();
        let ctx = test_ctx(&ws, &approval, &cancel);
        let fut = tool.execute(json!({"x": 1}), &ctx);
        tokio::pin!(fut);

        // 连接未就绪：调用仍在等待（既不返回错误，也不返回结果）。
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut fut)
                .await
                .is_err(),
            "连接未就绪时调用应等待"
        );

        gate.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), fut)
            .await
            .expect("放行后应完成")
            .expect("调用应成功");
        assert!(
            matches!(&result, agent_core::ToolResult::Text(t) if t == "mock tools/call ok (cached_tool)"),
            "应回传远端文本结果，实际 {result:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 后台连接失败：Deferred 工具调用回填可读错误（含 server 名与缓存来源提示），状态转 failed。
    #[tokio::test]
    async fn deferred_tool_execute_reports_failure_after_background_connect_error() {
        let cfg = stub_cfg();
        let dir = seed_cache("fail", &cfg, "cached_tool");
        let gate = Arc::new(tokio::sync::Notify::new());
        let opts = McpLoadOptions {
            startup_budget: std::time::Duration::from_millis(1),
            cache_dir: Some(dir.clone()),
            roots: Vec::new(),
        };
        let registry = McpRegistry::load_with(
            &cfg,
            &opts,
            seams_with(Arc::clone(&gate), Some("连接失败: 拒绝连接".to_string())),
        )
        .await;
        let tool = registry.tools().into_iter().next().expect("Deferred 工具");
        gate.notify_one();

        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let approval = AllowAll;
        let cancel = tokio_util::sync::CancellationToken::new();
        let ctx = test_ctx(&ws, &approval, &cancel);
        // 后台失败落槽后调用立即失败（既不返回结果也不挂起）。
        let err = match tool.execute(json!({}), &ctx).await {
            Ok(r) => panic!("连接失败后不应成功: {r:?}"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("srv"), "错误应指明 server: {err}");
        assert!(err.contains("未连接"), "错误应说明未连接: {err}");
        assert!(err.contains("缓存快照"), "错误应提示元信息来自缓存: {err}");
        assert_eq!(
            registry.server_status()[0].1.state,
            McpConnState::Failed,
            "后台连接失败后状态应为 failed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 提示词面：`prompts/list` 元信息进入注册表、`prompts/get` 取回文本、
    /// `prompts/list_changed` 触发重拉。
    #[tokio::test]
    async fn prompts_aggregate_execute_and_refresh_on_notification() {
        let mock = MockTransport::new(None, vec![tool_value("t")]).with_prompts(vec![json!({
            "name": "review",
            "description": "代码评审",
            "arguments": [{"name": "path", "required": true}]
        })]);
        let client = Arc::new(McpClient::from_transport(Box::new(mock.clone())));
        client.initialize().await.expect("initialize");
        let infos = client.list_tools().await.expect("list_tools");
        let prompts = client.list_prompts().await.expect("list_prompts");
        let mut installed = HashMap::new();
        installed.insert("srv".to_string(), mint_tools("srv", &client, infos));
        let registry = McpRegistry::from_clients(
            vec![("srv".to_string(), Arc::clone(&client))],
            installed,
            vec![],
            ReconnectOptions::default,
        );
        // from_clients 为测试装配（不拉 prompts）：直接经通知路径验证刷新闭环。
        assert!(registry.prompts().is_empty());

        mock.emit(notifications::PROMPTS_LIST_CHANGED);
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let p = registry.prompts();
                if !p.is_empty() {
                    return p;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("prompts/list_changed 应触发重拉");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "srv");
        assert_eq!(got[0].1.name, "review");
        assert_eq!(got[0].1.description, "代码评审");
        assert_eq!(got[0].1.arguments.len(), 1);
        assert!(got[0].1.arguments[0].required);
        assert_eq!(prompts.len(), 1, "连接期已拉到同一份清单");

        // 运行期清单替换：server 端改清单 + list_changed → 注册表换新。
        mock.set_prompts(vec![json!({"name": "explain", "description": "解释"})]);
        mock.emit(notifications::PROMPTS_LIST_CHANGED);
        let refreshed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let p = registry.prompts();
                if p.first().is_some_and(|(_, x)| x.name == "explain") {
                    return p;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("提示词清单替换应生效");
        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].1.name, "explain");

        // 换回 review 供 prompts/get 断言（清单可再次整体替换）。
        mock.set_prompts(vec![json!({
            "name": "review",
            "description": "代码评审",
            "arguments": [{"name": "path", "required": true}]
        })]);
        mock.emit(notifications::PROMPTS_LIST_CHANGED);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if registry
                    .prompts()
                    .first()
                    .is_some_and(|(_, x)| x.name == "review")
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("提示词清单应可再次替换");

        // prompts/get：消息文本按序拼接（多块 + 多消息，空行分隔）。
        let text = registry
            .execute_prompt("srv", "review", &json!({"path": "a.rs"}))
            .await
            .expect("提示词应可取回");
        assert!(text.starts_with("prompt review with "), "实际: {text}");
        assert!(text.ends_with("second"), "多消息应按序拼接: {text}");

        // 未连接 server：可读错误，不 panic。
        let err = registry
            .execute_prompt("ghost", "review", &json!({}))
            .await
            .expect_err("未连接 server 应报错");
        assert!(err.contains("ghost"), "错误应指明 server: {err}");
    }

    /// 无缓存目录：预算耗尽也不注册任何工具，但后台连接成功后补注册并 fire 监听器。
    #[tokio::test]
    async fn budget_exceeded_without_cache_registers_nothing_until_connected() {
        let cfg = stub_cfg();
        let gate = Arc::new(tokio::sync::Notify::new());
        let opts = McpLoadOptions {
            startup_budget: std::time::Duration::from_millis(1),
            cache_dir: None,
            roots: Vec::new(),
        };
        let registry =
            McpRegistry::load_with(&cfg, &opts, seams_with(Arc::clone(&gate), None)).await;
        assert!(registry.is_empty(), "无可信清单时不得凭空注册工具");
        assert_eq!(
            registry.server_status()[0].1.state,
            McpConnState::Connecting
        );

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        registry.on_tools_changed(Arc::new(move |server| {
            let _ = tx.send(server.to_string());
        }));
        gate.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("后台连接完成后应 fire 监听器")
            .expect("信道关闭");
        let names: Vec<String> = registry
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(names, vec!["mcp__srv_live_tool"]);
    }
}
