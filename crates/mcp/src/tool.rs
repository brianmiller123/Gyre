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

use crate::client::{
    McpClient, McpError, McpNotification, McpResource, McpToolInfo, notifications,
};
use crate::reconnect::{
    McpConnState, McpServerStatus, ReconnectOptions, ReconnectSupervisor, StatusCell, SuccessFn,
};

/// 把单个 MCP server 工具包装为 agent [`Tool`]。
///
/// 持有共享的 [`McpClient`]（多工具复用同一连接）。execute 时经 `tools/call` 调用远端工具。
#[derive(Clone)]
pub struct McpTool {
    info: McpToolInfo,
    /// 对模型暴露的命名空间化名称 `mcp__<server>_<tool>`（防跨 server 同名冲突）。
    alias: String,
    /// 来源 server 名（清单刷新按 server 替换、跨 server 去重 origin key 用）。
    server: String,
    client: Arc<McpClient>,
}

impl McpTool {
    /// 来源 server 名。
    #[must_use]
    pub fn server(&self) -> &str {
        &self.server
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
        let text = self
            .client
            .call_tool(&self.info.name, input)
            .await
            .map_err(|e| ToolError::Execution(format!("MCP `{}`: {e}", self.info.name)))?;
        Ok(ToolResult::text(text))
    }
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
/// FNV-1a 64 位 → base36（跨进程稳定，供超长名哈希后缀）。
fn fnv1a_base36(value: &str) -> String {
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
    /// server 名 → client（保活 + 通知重拉 + `mcp://` 资源路由；按配置顺序）。
    named: Vec<(String, Arc<McpClient>)>,
    /// server 名 → 当前工具清单（命名空间化；读取时跨 server 统一去重）。
    tools: parking_lot::RwLock<HashMap<String, Vec<McpTool>>>,
    /// 工具清单变更监听器（重拉成功后逐个触发，互相隔离）。
    listeners: parking_lot::Mutex<Vec<ToolsChangedListener>>,
    /// server 名 → 连接状态槽（重连监督任务写，[`McpRegistry::server_status`] 读）。
    statuses: HashMap<String, StatusCell>,
}

impl RegistryShared {
    /// server→client 通知分发（对标 omp `#handleServerNotification`）：
    /// tools 清单变更触发重拉，resources/prompts 本版本无消费面仅记日志，未知忽略。
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
                tracing::info!(server, "收到 prompts/list_changed（无消费面，仅记录）");
            }
            other => tracing::debug!(server, method = other, "忽略 MCP server 通知"),
        }
    }

    /// 重拉指定 server 的 `tools/list` 并整体替换其清单，返回刷新后的工具数。
    async fn refresh_server_tools(&self, server: &str) -> Result<usize, McpError> {
        let Some(client) = self.named.iter().find(|(n, _)| n == server).map(|(_, c)| c) else {
            // 通知晚于连接移除等场景：静默跳过。
            tracing::debug!(server, "tools/list_changed 来自未注册的 server，跳过刷新");
            return Ok(0);
        };
        let infos = client.list_tools().await?;
        let count = infos.len();
        self.tools.write().insert(
            server.to_string(),
            mint_tools(server, &Arc::clone(client), infos),
        );
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
    /// 无 server 或全部失败时返回空注册表（不报错，适配无 MCP 的环境）。
    #[must_use]
    pub async fn load(cfg: &McpConfig) -> Self {
        let mut named: Vec<(String, Arc<McpClient>)> = Vec::new();
        let mut initial: HashMap<String, Vec<McpTool>> = HashMap::new();
        let cfgs: Vec<(String, McpServerConfig)> = cfg
            .servers
            .iter()
            .map(|(name, server_cfg)| (name.clone(), server_cfg.clone()))
            .collect();
        for (name, server_cfg) in &cfgs {
            match McpClient::connect(server_cfg).await {
                Ok(client) => {
                    let client = Arc::new(client);
                    if let Err(e) = client.initialize().await {
                        tracing::warn!(server = %name, error = %e, "MCP server initialize 失败，跳过");
                        client.close().await;
                        continue;
                    }
                    match client.list_tools().await {
                        Ok(infos) => {
                            initial.insert(name.clone(), mint_tools(name, &client, infos));
                            named.push((name.clone(), Arc::clone(&client)));
                            tracing::info!(server = %name, "MCP server 已连接");
                        }
                        Err(e) => {
                            tracing::warn!(server = %name, error = %e, "MCP server list_tools 失败，跳过");
                            client.close().await;
                        }
                    }
                }
                Err(e) => tracing::warn!(server = %name, error = %e, "MCP server 连接失败，跳过"),
            }
        }
        Self::from_clients(named, initial, cfgs, ReconnectOptions::default)
    }

    /// 由已握手完成的 client 组装注册表，并挂接通知消费（重拉闭环）与重连监督。
    ///
    /// 通知链路：传输读循环 → `McpClient::set_on_notification` → 汇入共享队列 →
    /// 单消费任务顺序分发（见 [`RegistryShared::handle_notification`]）。
    /// 重连链路：连接级失败 / stdio 读循环退出 → `McpClient` 重连 hook →
    /// [`ReconnectSupervisor`]（退避 + 熔断）→ 成功后刷新清单并 fire 变更。
    /// 两个任务均持 `Weak`：注册表释放后任务随之退出，连接可正常回收。
    pub(crate) fn from_clients(
        named: Vec<(String, Arc<McpClient>)>,
        initial: HashMap<String, Vec<McpTool>>,
        cfgs: Vec<(String, McpServerConfig)>,
        opts: impl Fn() -> ReconnectOptions,
    ) -> Self {
        let mut statuses: HashMap<String, StatusCell> = HashMap::new();
        for (name, _) in &named {
            statuses.insert(
                name.clone(),
                Arc::new(parking_lot::Mutex::new(McpServerStatus {
                    state: McpConnState::Connected,
                    last_error: None,
                })),
            );
        }
        let shared = Arc::new(RegistryShared {
            named: named.clone(),
            tools: parking_lot::RwLock::new(initial),
            listeners: parking_lot::Mutex::new(Vec::new()),
            statuses,
        });
        // 通知转发：每个 client 的通知带上 server 名汇入同一队列；
        // 重连监督：连接级失败 / 传输断连 → 退避重连 → 恢复刷新。
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, McpNotification)>();
        for (name, client) in &named {
            let tx = tx.clone();
            let cname = name.clone();
            client.set_on_notification(Arc::new(move |n| {
                let _ = tx.send((cname.clone(), n));
            }));
            let Some((_, server_cfg)) = cfgs.iter().find(|(n, _)| n == name) else {
                continue;
            };
            let weak_alive = Arc::downgrade(&shared);
            let alive = Arc::new(move || weak_alive.upgrade().is_some())
                as Arc<dyn Fn() -> bool + Send + Sync>;
            let weak_refresh = Arc::downgrade(&shared);
            let sname = name.clone();
            let on_success: SuccessFn =
                Arc::new(move |_| refresh_after_reconnect(weak_refresh.clone(), sname.clone()));
            let hook = ReconnectSupervisor::spawn(
                name.clone(),
                server_cfg.clone(),
                Arc::clone(client),
                Arc::clone(&shared.statuses[name]),
                on_success,
                alive,
                opts(),
            );
            client.set_reconnect_hook(hook);
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
        let candidates: Vec<(String, String, McpTool)> = self
            .shared
            .named
            .iter()
            .filter_map(|(name, _)| map.get(name))
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

    /// 是否未加载任何工具。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shared.tools.read().values().all(Vec::is_empty)
    }

    /// 已连接的 server 名列表（调试 / `mcp://` 提示用）。
    #[must_use]
    pub fn server_names(&self) -> Vec<&str> {
        self.shared.named.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// 聚合全部已连 server 的 instructions（按配置顺序；未提供或为空者不出现）。
    ///
    /// 供装配层注入 system prompt（对标 omp `getServerInstructions`）：
    /// 返回 `(server 名, instructions 文本)` 列表，拼接格式由装配层决定。
    #[must_use]
    pub fn server_instructions(&self) -> Vec<(String, String)> {
        self.shared
            .named
            .iter()
            .filter_map(|(name, client)| {
                let instructions = client.server_info()?.instructions?;
                Some((name.clone(), instructions))
            })
            .collect()
    }

    /// 各 server 连接状态快照（按配置顺序；供 `/mcp status` 类自省，本版本不接 CLI）。
    ///
    /// 状态由重连监督任务维护：`Connected`（健康）/ `Reconnecting`（退避重连中或
    /// 等待下次触发）/ `Open`（熔断开启）+ 最近错误（恢复成功后清空）。
    #[must_use]
    pub fn server_status(&self) -> Vec<(String, McpServerStatus)> {
        self.shared
            .named
            .iter()
            .filter_map(|(name, _)| {
                let cell = self.shared.statuses.get(name)?;
                Some((name.clone(), cell.lock().clone()))
            })
            .collect()
    }

    /// 注册工具清单变更监听器：server 通知 `tools/list_changed` 且重拉成功后触发。
    ///
    /// 可注册多个；单个监听器 panic 不影响其余与通知循环。注意：监听器内不要捕获
    /// 注册表自身（`Arc` 引用循环会阻止连接释放）；最新清单请重读 [`McpRegistry::tools`]。
    pub fn on_tools_changed(&self, listener: ToolsChangedListener) {
        self.shared.listeners.lock().push(listener);
    }

    /// 按 server 名查找 client（`mcp://` 路由用）。
    fn client(&self, server: &str) -> Result<&Arc<McpClient>, ResourceError> {
        self.shared
            .named
            .iter()
            .find(|(n, _)| n == server)
            .map(|(_, c)| c)
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

/// 把 server 的工具元信息列表包装为命名空间化的 [`McpTool`] 列表。
fn mint_tools(server: &str, client: &Arc<McpClient>, infos: Vec<McpToolInfo>) -> Vec<McpTool> {
    infos
        .into_iter()
        .map(|info| McpTool {
            alias: mint_tool_name(server, &info.name),
            server: server.to_string(),
            client: Arc::clone(client),
            info,
        })
        .collect()
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
        handler: parking_lot::Mutex<Option<crate::client::NotificationHandler>>,
    }

    impl MockTransport {
        fn new(init_instructions: Option<String>, tools: Vec<Value>) -> Self {
            Self(Arc::new(MockState {
                init_instructions,
                tools: parking_lot::Mutex::new(tools),
                handler: parking_lot::Mutex::new(None),
            }))
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
        let reg = McpRegistry::load(&cfg).await;
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
}
