//! MCP 协议层：JSON-RPC 2.0 客户端门面（传输无关）。
//!
//! [`McpClient`] 组合 [`initialize`](McpClient::initialize) / `tools/list` /
//! `tools/call` / `resources/*` 等方法序，底层 I/O 委托给 [`McpTransport`]：
//! - [`StdioTransport`](crate::stdio::StdioTransport)：子进程 stdin/stdout 行分隔 JSON
//! - [`HttpTransport`](crate::http::HttpTransport)：Streamable HTTP（POST JSON-RPC + 可选 SSE 响应流）

use std::sync::Arc;
use std::time::Duration;

use agent_config::McpServerConfig;
use parking_lot::Mutex;
use serde_json::{Value, json};
use thiserror::Error;

use crate::http::HttpTransport;
use crate::stdio::StdioTransport;

/// MCP 客户端错误。
#[derive(Debug, Error)]
pub enum McpError {
    /// 底层 IO。
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    /// JSON 序列化/反序列化。
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    /// server 返回 JSON-RPC error。
    #[error("MCP server 错误: {0}")]
    Server(String),
    /// 子进程 stdout 关闭 / HTTP SSE 流提前结束（请求无响应）。
    #[error("MCP 连接已关闭")]
    Closed,
    /// 响应信道失效。
    #[error("MCP 响应信道错误")]
    Channel,
    /// HTTP 传输层错误（连接失败、非 2xx 状态、头解析失败等）。
    #[error("HTTP 传输错误: {0}")]
    Http(String),
}

impl From<reqwest::Error> for McpError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.to_string())
    }
}

impl McpError {
    /// 是否连接级失败（传输断开或不可用），区别于 server 正常应答的 JSON-RPC 错误：
    /// - [`McpError::Closed`](Self::Closed) / [`McpError::Channel`](Self::Channel)：
    ///   stdio 子进程退出、读循环终止或 SSE 流提前结束
    /// - [`McpError::Io`](Self::Io)：写管道断裂等本端 IO 失败
    /// - [`McpError::Http`](Self::Http)：连接失败或非 2xx——会话失效（404 等）同样
    ///   需要重建会话，重连即正确恢复
    #[must_use]
    pub fn is_connection_lost(&self) -> bool {
        matches!(
            self,
            Self::Closed | Self::Channel | Self::Io(_) | Self::Http(_)
        )
    }
}

/// MCP server 暴露的工具元信息。
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    /// 工具名。
    pub name: String,
    /// 描述（供 LLM）。
    pub description: String,
    /// 输入参数 JSON Schema。
    pub schema: Value,
}

/// MCP server 暴露的资源元信息（`resources/list`）。
#[derive(Debug, Clone)]
pub struct McpResource {
    /// 资源 URI（server 内唯一）。
    pub uri: String,
    /// 人类可读名称。
    pub name: String,
    /// 描述（可选）。
    pub description: Option<String>,
    /// MIME 类型（可选）。
    pub mime_type: Option<String>,
}

/// initialize 握手捕获的 server 元信息（对标 omp `MCPServerConnection.instructions`）。
#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    /// server 使用说明（initialize 响应的 `instructions` 字段；空串归一为 `None`）。
    /// 供装配层注入 system prompt，聚合访问见 [`crate::McpRegistry::server_instructions`]。
    pub instructions: Option<String>,
}

/// server→client 通知帧（有 `method` 无 `id`；传输层读循环透传给消费方分发）。
#[derive(Debug, Clone)]
pub struct McpNotification {
    /// JSON-RPC 方法名（如 `notifications/tools/list_changed`）。
    pub method: String,
    /// 通知参数（无参数时为 `Null`）。
    pub params: Value,
}

/// 传输层内部的通知处理器签名（协议层只透传 method + params）。
pub(crate) type NotificationHandler = Arc<dyn Fn(&str, &Value) + Send + Sync>;
/// 传输异常断连处理器签名（stdio 读循环退出等；同步回调，不得阻塞）。
pub(crate) type CloseHandler = Arc<dyn Fn() + Send + Sync>;
/// client 级通知处理器签名（协议层包装为 [`NotificationHandler`] 后挂到传输）。
pub(crate) type ClientNotificationHandler = Arc<dyn Fn(McpNotification) + Send + Sync>;

/// 本协议层已知的 server→client 通知方法名（其余通知一律透传给处理器）。
pub(crate) mod notifications {
    /// 工具清单变更。
    pub const TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";
    /// 资源清单变更。
    pub const RESOURCES_LIST_CHANGED: &str = "notifications/resources/list_changed";
    /// 提示词清单变更。
    pub const PROMPTS_LIST_CHANGED: &str = "notifications/prompts/list_changed";
}

/// MCP 传输层抽象：消息帧与 I/O 归传输实现，方法序与 id 关联归本协议层
/// （分层对标 oh-my-pi `MCPTransport`：`request` / `notify` / `close`）。
#[async_trait::async_trait]
pub(crate) trait McpTransport: Send + Sync {
    /// 发送 JSON-RPC 请求并等待响应 `result`。
    ///
    /// # Errors
    /// 传输失败、超时或 server 返回 JSON-RPC error 时返回 [`McpError`]。
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError>;

    /// 发送通知（无 id、无响应）。
    ///
    /// # Errors
    /// 传输失败时返回 [`McpError`]。
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError>;

    /// 记录 `initialize` 协商出的协议版本。仅 HTTP 传输使用（后续请求携带
    /// `MCP-Protocol-Version` 头；stdio 无需）。
    fn set_protocol_version(&self, _version: &str) {}

    /// 注册 server→client 通知处理器（有 `method` 无 `id` 的 JSON-RPC 帧）。
    ///
    /// 读循环解析到通知即同步调用处理器，处理器内不应阻塞（重活自行 `tokio::spawn`）。
    /// 重复注册覆盖前者；未注册时通知仅记 debug 日志。
    fn set_on_notification(&self, _handler: NotificationHandler) {}

    /// 注册传输异常断连处理器（stdio 读循环退出 / stdout 关闭）。主动 `close` 前
    /// 调用方应先摘除处理器，防止回收动作自触发重连（见 [`McpClient::install_transport`]）。
    fn set_on_close(&self, _handler: CloseHandler) {}

    /// 关闭传输：stdio 终止子进程；HTTP 终止 server 会话（DELETE + `Mcp-Session-Id`）。
    async fn close(&self);
}

/// MCP 客户端（JSON-RPC 2.0；stdio 子进程或 Streamable HTTP 端点）。
pub struct McpClient {
    /// 当前传输。`Arc` 化使请求路径克隆快照后立即放锁（不跨 `await` 持锁），
    /// 重连监督器可在请求在途时整体换装新传输（见 [`McpClient::install_transport`]）。
    transport: parking_lot::RwLock<Arc<dyn McpTransport>>,
    /// initialize 捕获的 server 元信息（握手前为 `None`；读经 [`McpClient::server_info`]）。
    server_info: Mutex<Option<ServerInfo>>,
    /// client 级通知处理器槽（重连换装后重挂到新传输）。
    notification_handler: parking_lot::Mutex<Option<ClientNotificationHandler>>,
    /// 重连触发器槽（注册表装配时注入；连接级失败 / 传输关闭时触发，去重由监督器负责）。
    reconnect_hook: parking_lot::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl McpClient {
    /// 连接 MCP server：按配置分发到 stdio（子进程）或 HTTP（Streamable HTTP）传输。
    ///
    /// # Errors
    /// 启动失败（spawn / stdin+stdout 不可用）或 HTTP 客户端构造失败时返回 [`McpError`]。
    pub async fn connect(cfg: &McpServerConfig) -> Result<Self, McpError> {
        let transport: Box<dyn McpTransport> = match cfg {
            McpServerConfig::Stdio(c) => Box::new(StdioTransport::spawn(c).await?),
            McpServerConfig::Http(c) => Box::new(HttpTransport::connect(c).await?),
        };
        Ok(Self {
            transport: parking_lot::RwLock::new(Arc::from(transport)),
            server_info: Mutex::new(None),
            notification_handler: parking_lot::Mutex::new(None),
            reconnect_hook: parking_lot::Mutex::new(None),
        })
    }

    /// [`McpClient::connect`] 的可注入变体：显式指定凭据配置目录
    /// （测试 / 嵌入式宿主可托管 oauth.toml 位置）。
    ///
    /// # Errors
    /// 同 [`McpClient::connect`]。
    pub async fn connect_in_cfg(
        cfg: &McpServerConfig,
        config_dir: &std::path::Path,
    ) -> Result<Self, McpError> {
        let transport: Box<dyn McpTransport> = match cfg {
            McpServerConfig::Stdio(c) => Box::new(StdioTransport::spawn(c).await?),
            McpServerConfig::Http(c) => Box::new(HttpTransport::connect_in(c, config_dir).await?),
        };
        Ok(Self {
            transport: parking_lot::RwLock::new(Arc::from(transport)),
            server_info: Mutex::new(None),
            notification_handler: parking_lot::Mutex::new(None),
            reconnect_hook: parking_lot::Mutex::new(None),
        })
    }

    /// 当前传输快照：克隆 `Arc` 后立即放锁，请求期间不阻塞重连换装。
    fn transport_arc(&self) -> Arc<dyn McpTransport> {
        self.transport.read().clone()
    }

    /// 请求统一出口：连接级失败时触发重连触发器（火后不理，去重由监督器负责）。
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let result = self.transport_arc().request(method, params).await;
        if let Err(e) = &result {
            self.on_connection_error(e);
        }
        result
    }

    /// 通知统一出口（连接失败分类同 [`McpClient::request`]）。
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let result = self.transport_arc().notify(method, params).await;
        if let Err(e) = &result {
            self.on_connection_error(e);
        }
        result
    }

    /// 连接级失败分类出口：触发重连；JSON-RPC server 错误等非连接级失败不触发。
    fn on_connection_error(&self, e: &McpError) {
        if e.is_connection_lost() {
            if let Some(hook) = self.reconnect_hook.lock().clone() {
                hook();
            }
        }
    }

    /// initialize 握手 + initialized 通知。
    ///
    /// 协商出的 `protocolVersion` 回传给传输层（HTTP 后续请求携带
    /// `MCP-Protocol-Version` 头，仅在握手完成后发送——规范要求）；响应中的
    /// `instructions` 捕获至 [`McpClient::server_info`]（对标 omp client.ts:178）。
    ///
    /// # Errors
    /// 握手失败时返回 [`McpError`]。
    pub async fn initialize(&self) -> Result<(), McpError> {
        let result = self
            .request(
                "initialize",
                json!({
                    // 出价当前稳定版（对齐 omp `MCP_PROTOCOL_VERSION`，mcp/types.ts:168）：
                    // AWS Bedrock AgentCore Gateway 等严格网关在查 token vault 之前先校验
                    // 版本，<2025-11-25 的工具调用直接拒绝。server 返回的协商版本在下方
                    // 回传传输层，旧版 server 自动降级。
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "agent", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        if let Some(v) = result.get("protocolVersion").and_then(Value::as_str) {
            self.transport_arc().set_protocol_version(v);
        }
        *self.server_info.lock() = Some(parse_server_info(&result));
        self.notify("notifications/initialized", json!({})).await
    }

    /// initialize 捕获的 server 元信息（握手前为 `None`）。
    #[must_use]
    pub fn server_info(&self) -> Option<ServerInfo> {
        self.server_info.lock().clone()
    }

    /// 注册 server→client 通知处理器（如 `notifications/tools/list_changed`）。
    ///
    /// 通知经传输层读循环同步透传，处理器内不应阻塞（重活自行 `tokio::spawn`）；
    /// 重复注册覆盖前者。消费方按 [`McpNotification::method`] 自行分发
    /// （[`crate::McpRegistry`] 已内置 `tools/list_changed` 的重拉闭环）。
    pub fn set_on_notification(&self, handler: Arc<dyn Fn(McpNotification) + Send + Sync>) {
        let wrapped = wrap_notification_handler(Arc::clone(&handler));
        *self.notification_handler.lock() = Some(handler);
        self.transport_arc().set_on_notification(wrapped);
    }

    /// 列出 server 工具元信息。
    ///
    /// # Errors
    /// 通信失败时返回 [`McpError`]。
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, McpError> {
        let result = self.request("tools/list", json!({})).await?;
        Ok(parse_tools(&result))
    }

    /// 调用工具，返回文本内容拼接。
    ///
    /// # Errors
    /// 通信/server error 时返回 [`McpError`]。
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<String, McpError> {
        let result = self
            .request("tools/call", json!({"name":name,"arguments":args}))
            .await?;
        Ok(parse_text_content(&result))
    }

    /// 列出 server 暴露的资源（`resources/list`）。
    ///
    /// # Errors
    /// 通信失败或 server 不支持 resources 时返回 [`McpError`]（调用方可据错误判断能力缺失）。
    pub async fn list_resources(&self) -> Result<Vec<McpResource>, McpError> {
        let result = self.request("resources/list", json!({})).await?;
        Ok(parse_resources(&result))
    }

    /// 读取一个资源（`resources/read`），返回文本内容拼接。
    ///
    /// # Errors
    /// 通信失败、资源不存在或 server 不支持 resources 时返回 [`McpError`]。
    pub async fn read_resource(&self, uri: &str) -> Result<String, McpError> {
        let result = self.request("resources/read", json!({"uri":uri})).await?;
        Ok(parse_resource_text(&result))
    }

    /// 关闭连接：stdio 终止子进程；HTTP 终止 server 会话。
    pub async fn close(&self) {
        self.transport_arc().close().await;
    }

    /// 由既有传输构造客户端（测试注入口；生产路径走 [`McpClient::connect`]）。
    #[cfg(test)]
    pub(crate) fn from_transport(transport: Box<dyn McpTransport>) -> Self {
        Self {
            transport: parking_lot::RwLock::new(Arc::from(transport)),
            server_info: Mutex::new(None),
            notification_handler: parking_lot::Mutex::new(None),
            reconnect_hook: parking_lot::Mutex::new(None),
        }
    }

    /// 注入重连触发器（注册表装配时调用一次）：连接级请求失败与传输异步断连都会触发。
    pub(crate) fn set_reconnect_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.reconnect_hook.lock() = Some(Arc::clone(&hook));
        // 传输异步断连（stdio 读循环退出）同样驱动重连；HTTP 无常驻流，
        // 走请求失败路径（见 [`McpError::is_connection_lost`]）。
        let close = Arc::clone(&hook);
        self.transport_arc().set_on_close(Arc::new(move || close()));
    }

    /// 重连换装：安装新传输并重挂通知 / 断连处理器；旧传输异步回收（火后不理，
    /// 对齐 omp：不阻塞重连退避循环）。
    ///
    /// 换装后须调用 [`McpClient::initialize`] 重新握手；期间既有工具句柄
    /// （共享同一 `Arc<McpClient>`）自动路由到新传输。
    pub(crate) async fn install_transport(&self, transport: Box<dyn McpTransport>) {
        let old = {
            let mut slot = self.transport.write();
            std::mem::replace(&mut *slot, Arc::from(transport))
        };
        // 摘除旧传输断连处理器：主动回收不得自触发重连（对齐 omp：先 detach onClose 再 close）。
        old.set_on_close(Arc::new(|| {}));
        if let Some(handler) = self.notification_handler.lock().clone() {
            self.transport_arc()
                .set_on_notification(wrap_notification_handler(handler));
        }
        if let Some(hook) = self.reconnect_hook.lock().clone() {
            let close = Arc::clone(&hook);
            self.transport_arc().set_on_close(Arc::new(move || close()));
        }
        tokio::spawn(async move {
            old.close().await;
        });
    }
}

/// 把 client 级通知处理器包装为传输层签名（注册与重连换装两处复用）。
fn wrap_notification_handler(
    handler: Arc<dyn Fn(McpNotification) + Send + Sync>,
) -> NotificationHandler {
    Arc::new(move |method, params| {
        handler(McpNotification {
            method: method.to_string(),
            params: params.clone(),
        });
    })
}

/// 从 initialize 结果解析 server 元信息（本客户端关心的子集）。
fn parse_server_info(result: &Value) -> ServerInfo {
    ServerInfo {
        instructions: result
            .get("instructions")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned),
    }
}

/// 解析单 server 超时配置：缺省 30s，`0` = 不限制。
pub(crate) fn resolve_timeout(timeout_ms: Option<u64>) -> Option<Duration> {
    match timeout_ms {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(Duration::from_secs(30)),
    }
}

/// 从 tools/list 结果解析工具元信息列表。
fn parse_tools(result: &Value) -> Vec<McpToolInfo> {
    result
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|t| McpToolInfo {
                    name: t
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    description: t
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    schema: t.get("inputSchema").cloned().unwrap_or(Value::Null),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 从 tools/call 结果提取文本内容（content[].text 拼接）。
fn parse_text_content(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// 从 resources/list 结果解析资源元信息列表。
fn parse_resources(result: &Value) -> Vec<McpResource> {
    result
        .get("resources")
        .and_then(Value::as_array)
        .map(|rs| {
            rs.iter()
                .map(|r| McpResource {
                    uri: r
                        .get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: r
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    description: r
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    mime_type: r
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 从 resources/read 结果提取文本内容（contents[].text 拼接，忽略 blob）。
fn parse_resource_text(result: &Value) -> String {
    result
        .get("contents")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tools_list() {
        let result = serde_json::json!({
            "tools": [
                {"name": "read_file", "description": "读取文件", "inputSchema": {"type": "object"}},
                {"name": "search", "description": "搜索", "inputSchema": {}}
            ]
        });
        let tools = parse_tools(&result);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(tools[0].description, "读取文件");
    }

    #[test]
    fn parses_tools_empty_when_no_tools_field() {
        let result = serde_json::json!({});
        assert!(parse_tools(&result).is_empty());
    }

    #[test]
    fn parses_text_content() {
        let result = serde_json::json!({
            "content": [
                {"type": "text", "text": "line1"},
                {"type": "text", "text": "line2"}
            ]
        });
        assert_eq!(parse_text_content(&result), "line1\nline2");
    }

    #[test]
    fn parses_text_content_empty() {
        assert_eq!(parse_text_content(&serde_json::json!({})), "");
    }

    #[test]
    fn resolve_timeout_defaults_and_disables() {
        assert_eq!(resolve_timeout(None), Some(Duration::from_secs(30)));
        assert_eq!(resolve_timeout(Some(0)), None);
        assert_eq!(
            resolve_timeout(Some(1234)),
            Some(Duration::from_millis(1234))
        );
    }

    #[test]
    fn parses_initialize_instructions() {
        // 常规：字符串直取。
        let info = parse_server_info(&serde_json::json!({"instructions": "谨慎使用工具"}));
        assert_eq!(info.instructions.as_deref(), Some("谨慎使用工具"));
        // 空串归一为 None（聚合时不产生空条目）。
        assert_eq!(
            parse_server_info(&serde_json::json!({"instructions": ""})).instructions,
            None
        );
        // 字段缺省 / 非字符串类型：忽略。
        assert_eq!(
            parse_server_info(&serde_json::json!({"protocolVersion": "1"})).instructions,
            None
        );
        assert_eq!(
            parse_server_info(&serde_json::json!({"instructions": 42})).instructions,
            None
        );
    }

    /// 最小传输桩：initialize 返回固定 result，其余返回 `Null`。
    struct InitMock(Value);

    #[async_trait::async_trait]
    impl McpTransport for InitMock {
        async fn request(&self, method: &str, _params: Value) -> Result<Value, McpError> {
            if method == "initialize" {
                Ok(self.0.clone())
            } else {
                Ok(Value::Null)
            }
        }
        async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }
        async fn close(&self) {}
    }

    #[tokio::test]
    async fn initialize_captures_server_info() {
        let client = McpClient::from_transport(Box::new(InitMock(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "instructions": " hello "
        }))));
        assert!(client.server_info().is_none(), "握手前不应有元信息");
        client.initialize().await.expect("initialize");
        let info = client.server_info().expect("握手后应有元信息");
        assert_eq!(info.instructions.as_deref(), Some(" hello "));
    }
}
