//! MCP 协议层：JSON-RPC 2.0 客户端门面（传输无关）。
//!
//! [`McpClient`] 组合 [`initialize`](McpClient::initialize) / `tools/list` /
//! `tools/call` / `resources/*` 等方法序，底层 I/O 委托给 [`McpTransport`]：
//! - [`StdioTransport`](crate::stdio::StdioTransport)：子进程 stdin/stdout 行分隔 JSON
//! - [`HttpTransport`](crate::http::HttpTransport)：Streamable HTTP（POST JSON-RPC + 可选 SSE 响应流）

use std::sync::Arc;
use std::time::Duration;

use agent_config::{McpHttpTransport, McpServerConfig};
use parking_lot::Mutex;
use serde_json::{Value, json};
use thiserror::Error;

use crate::http::HttpTransport;
use crate::sse::SseTransport;
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

    /// 是否为 JSON-RPC `-32601 Method not found`。
    ///
    /// server 错误在传输层被 `serde_json::Value::to_string()` 序列化（形如
    /// `{"code":-32601,"message":"Method not found"}`），故按 code 子串判定。
    /// 用于 H15 容错：不声明能力也不实现 `*/list` 的 server 不应被判为连接失败。
    #[must_use]
    pub fn is_method_not_found(&self) -> bool {
        match self {
            Self::Server(msg) => {
                msg.contains("-32601") || msg.to_ascii_lowercase().contains("method not found")
            }
            _ => false,
        }
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

/// MCP server 暴露的提示词模板元信息（`prompts/list`）。
///
/// 宿主可把每个提示词挂成斜杠命令（Gyre：`/<server>:<prompt>`，对齐 omp
/// `buildMCPPromptCommands`），命中的参数以 `key=value` 形式传入 [`McpClient::get_prompt`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptInfo {
    /// 提示词名（server 内唯一）。
    pub name: String,
    /// 描述（供斜杠命令提示）。
    pub description: String,
    /// 声明参数（`{name, required}` 对；供命令补全与必填校验）。
    pub arguments: Vec<McpPromptArg>,
}

/// 提示词声明的参数（`prompts/list` 的 `arguments[]`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptArg {
    /// 参数名。
    pub name: String,
    /// 是否必填。
    pub required: bool,
}

/// initialize 握手捕获的 server 元信息（对标 omp `MCPServerConnection`）。
#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    /// server 使用说明（initialize 响应的 `instructions` 字段；空串归一为 `None`）。
    /// 供装配层注入 system prompt，聚合访问见 [`crate::McpRegistry::server_instructions`]。
    pub instructions: Option<String>,
    /// server 声明的能力（`capabilities`），用于**能力门控**：
    /// 未声明某能力时不发对应 `*/list` 请求（H15）。否则纯 resources server 会因
    /// `tools/list` 返回 `-32601` 而被判定连接失败并 `close()`。
    pub capabilities: ServerCapabilities,
}

/// 可门控的 server 能力类别（[`McpClient::supports`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpCapability {
    /// `tools/list` / `tools/call`。
    Tools,
    /// `resources/list` / `resources/read`。
    Resources,
    /// `prompts/list` / `prompts/get`。
    Prompts,
}

/// server 声明的能力子集（`initialize` 响应 `capabilities` 的键存在性）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    /// 提供工具（`tools`）。未声明 → 不请求 `tools/list`。
    pub tools: bool,
    /// 提供资源（`resources`）。
    pub resources: bool,
    /// 提供提示词（`prompts`）。
    pub prompts: bool,
    /// 支持 `logging/setLevel`。
    pub logging: bool,
    /// 支持 `completion/complete`。
    pub completions: bool,
    /// 实验能力。
    pub experimental: bool,
}

impl ServerCapabilities {
    /// 从 `capabilities` 对象解析：键**存在**即为支持（规范只要求对象，内容可空）。
    #[must_use]
    pub fn parse(value: Option<&Value>) -> Self {
        let has = |k: &str| value.and_then(|v| v.get(k)).is_some_and(|v| !v.is_null());
        Self {
            tools: has("tools"),
            resources: has("resources"),
            prompts: has("prompts"),
            logging: has("logging"),
            completions: has("completions"),
            experimental: has("experimental"),
        }
    }

    /// 是否声明了**任何**能力。全空视为「server 未声明能力」→ 客户端退化为
    /// 「按方法探测」（发请求并以 `-32601` 容错），而不是直接当成不支持。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !(self.tools
            || self.resources
            || self.prompts
            || self.logging
            || self.completions
            || self.experimental)
    }
}

/// `tools/call` 的单个内容块（保真，对齐 oh-my-pi `MCPContent`）。
#[derive(Debug, Clone, PartialEq)]
pub enum McpContent {
    /// 文本块。
    Text(String),
    /// 图像块（base64 data + mime）。
    Image {
        /// base64 编码的图像数据（MCP 规范即 base64）。
        data: String,
        /// MIME 类型。
        mime_type: String,
    },
    /// 内嵌资源块。
    Resource {
        /// 资源 URI。
        uri: String,
        /// 文本资源内容（blob 资源时为 `None`）。
        text: Option<String>,
        /// MIME 类型。
        mime_type: Option<String>,
    },
    /// 音频 / 未知类型（原样保留，渲染为占位标记）。
    Other(Value),
}

/// `tools/call` 的完整结果（H17：不丢 image / resource / isError / structuredContent）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct McpToolCallOutcome {
    /// 内容块（按 server 返回顺序）。
    pub content: Vec<McpContent>,
    /// server 标记的业务失败（`isError: true`）——与传输/协议错误的 [`McpError`] 不同。
    pub is_error: bool,
    /// 结构化输出（`structuredContent`，2025-06 规范）。
    pub structured: Option<Value>,
}

impl McpToolCallOutcome {
    /// 渲染为供 LLM 阅读的文本（omp `formatMCPContent` 语义）。
    ///
    /// - 文本块之间空行分隔；
    /// - resource 块渲染为 `[Resource: <uri>]\n<text>`；
    /// - image 块渲染为 `[image/<mime>]` 占位（真实像素由
    ///   [`McpToolCallOutcome::first_image`] 供桥接层转 [`agent_core::ToolResult::Image`]）；
    /// - 无内容块但有 `structuredContent` 时渲染该 JSON。
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for block in &self.content {
            match block {
                McpContent::Text(t) => parts.push(t.clone()),
                McpContent::Image { mime_type, .. } => parts.push(format!("[{mime_type}]")),
                McpContent::Resource { uri, text, .. } => parts.push(match text {
                    Some(t) => format!("[Resource: {uri}]\n{t}"),
                    None => format!("[Resource: {uri}]"),
                }),
                McpContent::Other(v) => {
                    let ty = v.get("type").and_then(Value::as_str).unwrap_or("unknown");
                    parts.push(format!("[{ty}]"));
                }
            }
        }
        if parts.is_empty() {
            if let Some(s) = &self.structured {
                return serde_json::to_string_pretty(s).unwrap_or_else(|_| s.to_string());
            }
        }
        parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// 唯一图像块（`(base64_data, mime)`）：恰有一个 image 且无其它负载时可供多模态桥接；
    /// 其余情况返回 `None`（Gyre 的 [`agent_core::ToolResult`] 单图像模型无法同时承载文本）。
    #[must_use]
    pub fn first_image(&self) -> Option<(&str, &str)> {
        let mut images = self.content.iter().filter_map(|c| match c {
            McpContent::Image { data, mime_type } => Some((data.as_str(), mime_type.as_str())),
            _ => None,
        });
        let first = images.next()?;
        if images.next().is_some() {
            return None; // 多图：无法在单图像 ToolResult 中保真表达
        }
        let has_other = self
            .content
            .iter()
            .any(|c| !matches!(c, McpContent::Image { .. }));
        if has_other {
            return None;
        }
        Some(first)
    }
}

/// server→client 通知帧（有 `method` 无 `id`；传输层读循环透传给消费方分发）。
#[derive(Debug, Clone)]
pub struct McpNotification {
    /// JSON-RPC 方法名（如 `notifications/tools/list_changed`）。
    pub method: String,
    /// 通知参数（无参数时为 `Null`）。
    pub params: Value,
}

/// MCP root（`roots/list` 响应项）：`file://` URI + 可选显示名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRoot {
    /// `file://` URI。
    pub uri: String,
    /// 显示名（可选）。
    pub name: Option<String>,
}

impl McpRoot {
    /// 由本地路径构造 `file://` root（非 UTF-8 / 相对路径按原样拼 URI）。
    #[must_use]
    pub fn file(path: &std::path::Path, name: impl Into<String>) -> Self {
        let mut uri = String::from("file://");
        let s = path.to_string_lossy();
        if !s.starts_with('/') {
            uri.push('/');
        }
        uri.push_str(&s);
        Self {
            uri,
            name: Some(name.into()),
        }
    }
}

/// server→client 请求处理器签名（H16）：`(id, method, params)` → `Ok(result)` /
/// `Err((code, message))`。**同步**返回，由传输层负责把响应写回对端。
pub(crate) type ServerRequestHandler =
    Arc<dyn Fn(&Value, &str, &Value) -> Result<Value, (i64, String)> + Send + Sync>;

/// JSON-RPC 标准错误码：方法不存在。
pub(crate) const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;

/// 组装 server 请求的**成功响应**帧（id 原样回带，字符串 id 也支持）。
#[must_use]
pub(crate) fn server_request_result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// 组装 server 请求的**错误响应**帧。
#[must_use]
pub(crate) fn server_request_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// 默认 server→client 请求应答（H16）：
/// - `ping` → `{}`（规范要求客户端必须应答，否则 server 会判对端失联）；
/// - `roots/list` → 已登记的工作区根（未登记时为空数组）；
/// - `sampling/createMessage` / `elicitation/*` → 明确 `-32601`（Gyre 不提供该能力，
///   诚实拒绝优于静默丢弃——server 能据此降级而不是永久挂起）；
/// - 其余 → `-32601`。
pub(crate) fn answer_server_request(
    roots: &[McpRoot],
    method: &str,
    _params: &Value,
) -> Result<Value, (i64, String)> {
    match method {
        "ping" => Ok(json!({})),
        "roots/list" => Ok(json!({
            "roots": roots
                .iter()
                .map(|r| match &r.name {
                    Some(name) => json!({ "uri": r.uri, "name": name }),
                    None => json!({ "uri": r.uri }),
                })
                .collect::<Vec<_>>(),
        })),
        "sampling/createMessage" => Err((
            JSONRPC_METHOD_NOT_FOUND,
            "sampling/createMessage 不受支持：Gyre 不代理 server 的 LLM 采样请求".into(),
        )),
        other if other.starts_with("elicitation/") => Err((
            JSONRPC_METHOD_NOT_FOUND,
            format!("{other} 不受支持：Gyre 无 elicitation UI 通道"),
        )),
        other => Err((
            JSONRPC_METHOD_NOT_FOUND,
            format!("Method not found: {other}"),
        )),
    }
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

    /// 注册 server→client **请求**处理器（有 `method` **且**有 `id` 的帧，H16）。
    ///
    /// 与 [`McpTransport::set_on_notification`] 的区别：请求必须回响应，否则 server
    /// 永久等待（如 `roots/list` / `ping`）。处理器同步返回结果或错误，由传输层写回。
    fn set_on_server_request(&self, _handler: ServerRequestHandler) {}

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
    /// server→client 请求处理器槽（重连换装后重挂到新传输）。
    server_request_handler: parking_lot::Mutex<Option<ServerRequestHandler>>,
    /// 工作区根（`roots/list` 应答内容；H16）。经 [`McpClient::set_roots`] 在 `initialize`
    /// 之前登记——是否声明 `roots` 能力由它是否非空决定。
    roots: Arc<parking_lot::Mutex<Vec<McpRoot>>>,
}

impl McpClient {
    /// 连接 MCP server：按配置分发到 stdio（子进程）或 HTTP（Streamable HTTP）传输。
    ///
    /// # Errors
    /// 启动失败（spawn / stdin+stdout 不可用）或 HTTP 客户端构造失败时返回 [`McpError`]。
    pub async fn connect(cfg: &McpServerConfig) -> Result<Self, McpError> {
        let transport: Box<dyn McpTransport> = match cfg {
            McpServerConfig::Stdio(c) => Box::new(StdioTransport::spawn(c).await?),
            McpServerConfig::Http(c) => match c.transport {
                McpHttpTransport::Streamable => Box::new(HttpTransport::connect(c).await?),
                McpHttpTransport::Sse => Box::new(SseTransport::connect(c).await?),
            },
        };
        Ok(Self::with_transport(transport))
    }

    /// 由传输构造 client：装配 server→client 请求处理器（H16）并返回。
    fn with_transport(transport: Box<dyn McpTransport>) -> Self {
        let roots: Arc<parking_lot::Mutex<Vec<McpRoot>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let handler: ServerRequestHandler = {
            let roots = Arc::clone(&roots);
            Arc::new(move |_id, method, params| {
                answer_server_request(&roots.lock(), method, params)
            })
        };
        transport.set_on_server_request(Arc::clone(&handler));
        Self {
            transport: parking_lot::RwLock::new(Arc::from(transport)),
            server_info: Mutex::new(None),
            notification_handler: parking_lot::Mutex::new(None),
            reconnect_hook: parking_lot::Mutex::new(None),
            server_request_handler: parking_lot::Mutex::new(Some(handler)),
            roots,
        }
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
            McpServerConfig::Http(c) => match c.transport {
                McpHttpTransport::Streamable => {
                    Box::new(HttpTransport::connect_in(c, config_dir).await?)
                }
                McpHttpTransport::Sse => Box::new(SseTransport::connect(c).await?),
            },
        };
        Ok(Self::with_transport(transport))
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
        // H16：声明客户端能力——有工作区根才声明 `roots`（server 据此可能发 roots/list）。
        let mut capabilities = json!({});
        if !self.roots.lock().is_empty() {
            capabilities["roots"] = json!({});
        }
        let result = self
            .request(
                "initialize",
                json!({
                    // 出价当前稳定版（对齐 omp `MCP_PROTOCOL_VERSION`，mcp/types.ts:168）：
                    // AWS Bedrock AgentCore Gateway 等严格网关在查 token vault 之前先校验
                    // 版本，<2025-11-25 的工具调用直接拒绝。server 返回的协商版本在下方
                    // 回传传输层，旧版 server 自动降级。
                    "protocolVersion": "2025-11-25",
                    "capabilities": capabilities,
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

    /// H15 能力门控：`true` = 应该向该 server 发这个类别的 `*/list` 请求。
    ///
    /// - 握手未完成（`server_info` 为 `None`）→ 保守返回 `true`（按方法探测）；
    /// - server 声明了能力集 → 以声明为准；
    /// - server 未声明任何能力（旧实现/极简 server）→ 返回 `true` 并按 `-32601` 容错。
    #[must_use]
    pub fn supports(&self, capability: McpCapability) -> bool {
        match self.server_info.lock().as_ref() {
            None => true,
            Some(info) => {
                if info.capabilities.is_empty() {
                    return true;
                }
                match capability {
                    McpCapability::Tools => info.capabilities.tools,
                    McpCapability::Resources => info.capabilities.resources,
                    McpCapability::Prompts => info.capabilities.prompts,
                }
            }
        }
    }

    /// 登记工作区根（`roots/list` 应答内容，H16）。
    ///
    /// **须在 [`McpClient::initialize`] 之前调用**：是否向 server 声明 `roots` 能力由
    /// 此列表是否非空决定（声明了却答不出根会让 server 侧逻辑走空）。
    pub fn set_roots(&self, roots: Vec<McpRoot>) {
        *self.roots.lock() = roots;
    }

    /// 已登记的工作区根。
    #[must_use]
    pub fn roots(&self) -> Vec<McpRoot> {
        self.roots.lock().clone()
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
    /// H15：server 明确未声明 `tools` 能力时**不发请求**，直接返回空清单——
    /// 纯 resources/prompts server 因此不再被视为连接失败。
    /// 另对 `-32601 Method not found` 容错（部分 server 不声明能力却也不实现该方法）。
    ///
    /// # Errors
    /// 通信失败时返回 [`McpError`]。
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, McpError> {
        if !self.supports(McpCapability::Tools) {
            return Ok(Vec::new());
        }
        match self.request("tools/list", json!({})).await {
            Ok(result) => Ok(parse_tools(&result)),
            Err(e) if e.is_method_not_found() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// 调用工具，返回文本内容拼接（保真版本见 [`McpClient::call_tool_full`]）。
    ///
    /// # Errors
    /// 通信/server error 时返回 [`McpError`]。
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<String, McpError> {
        Ok(self.call_tool_full(name, args).await?.render_text())
    }

    /// 调用工具（H17）：返回**完整内容块**（text / image / resource）+ `isError` +
    /// `structuredContent`，避免 image 与 resource 负载在桥接层被丢弃
    /// （对齐 oh-my-pi `formatMCPContent` / `buildResult`）。
    ///
    /// # Errors
    /// 通信/server error（JSON-RPC error 帧）时返回 [`McpError`]；工具自身的业务失败
    /// 走 `isError: true`（不是 [`McpError`]）。
    pub async fn call_tool_full(
        &self,
        name: &str,
        args: Value,
    ) -> Result<McpToolCallOutcome, McpError> {
        let result = self
            .request("tools/call", json!({"name":name,"arguments":args}))
            .await?;
        Ok(parse_tool_call(&result))
    }

    /// 列出 server 暴露的资源（`resources/list`）。
    ///
    /// H15：server 明确未声明 `resources` 能力 → 返回空清单（不报错）；
    /// `-32601` 同样容错为空清单。
    ///
    /// # Errors
    /// 通信失败时返回 [`McpError`]。
    pub async fn list_resources(&self) -> Result<Vec<McpResource>, McpError> {
        if !self.supports(McpCapability::Resources) {
            return Ok(Vec::new());
        }
        match self.request("resources/list", json!({})).await {
            Ok(result) => Ok(parse_resources(&result)),
            Err(e) if e.is_method_not_found() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// 读取一个资源（`resources/read`），返回文本内容拼接。
    ///
    /// # Errors
    /// 通信失败、资源不存在或 server 不支持 resources 时返回 [`McpError`]。
    pub async fn read_resource(&self, uri: &str) -> Result<String, McpError> {
        let result = self.request("resources/read", json!({"uri":uri})).await?;
        Ok(parse_resource_text(&result))
    }

    /// 列出 server 暴露的提示词模板（`prompts/list`）。
    ///
    /// H15：server 明确未声明 `prompts` 能力 → 返回空清单；`-32601` 同样容错。
    ///
    /// # Errors
    /// 通信失败时返回 [`McpError`]。
    pub async fn list_prompts(&self) -> Result<Vec<McpPromptInfo>, McpError> {
        if !self.supports(McpCapability::Prompts) {
            return Ok(Vec::new());
        }
        match self.request("prompts/list", json!({})).await {
            Ok(result) => Ok(parse_prompts(&result)),
            Err(e) if e.is_method_not_found() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// 取回提示词内容（`prompts/get`），返回各消息文本按序拼接。
    ///
    /// # Errors
    /// 通信失败、提示词不存在或必填参数缺失时返回 [`McpError`]。
    pub async fn get_prompt(&self, name: &str, args: &Value) -> Result<String, McpError> {
        let result = self
            .request("prompts/get", json!({"name": name, "arguments": args}))
            .await?;
        Ok(parse_prompt_messages(&result))
    }

    /// 关闭连接：stdio 终止子进程；HTTP 终止 server 会话。
    pub async fn close(&self) {
        self.transport_arc().close().await;
    }

    /// 由既有传输构造客户端（测试注入口；生产路径走 [`McpClient::connect`]）。
    #[cfg(test)]
    pub(crate) fn from_transport(transport: Box<dyn McpTransport>) -> Self {
        Self::with_transport(transport)
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
        // H16：换装新传输后同样要重挂 server→client 请求处理器，否则重连后 ping/roots 无应答。
        if let Some(handler) = self.server_request_handler.lock().clone() {
            self.transport_arc().set_on_server_request(handler);
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
        capabilities: ServerCapabilities::parse(result.get("capabilities")),
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

/// 解析 tools/call 结果（H17）：完整保留 text / image / resource 块与 `isError`。
fn parse_tool_call(result: &Value) -> McpToolCallOutcome {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(parse_content_block).collect())
        .unwrap_or_default();
    McpToolCallOutcome {
        content,
        is_error: result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        structured: result.get("structuredContent").cloned(),
    }
}

/// 解析单个内容块。
fn parse_content_block(item: &Value) -> McpContent {
    match item.get("type").and_then(Value::as_str) {
        Some("text") => McpContent::Text(
            item.get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        Some("image") => McpContent::Image {
            data: item
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            mime_type: item
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream")
                .to_string(),
        },
        Some("resource") => {
            let res = item.get("resource");
            McpContent::Resource {
                uri: res
                    .and_then(|r| r.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                text: res
                    .and_then(|r| r.get("text"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                mime_type: res
                    .and_then(|r| r.get("mimeType"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }
        }
        _ => McpContent::Other(item.clone()),
    }
}

/// 从 prompts/list 结果解析提示词元信息列表。
fn parse_prompts(result: &Value) -> Vec<McpPromptInfo> {
    result
        .get("prompts")
        .and_then(Value::as_array)
        .map(|ps| {
            ps.iter()
                .filter_map(|p| {
                    let name = p.get("name").and_then(Value::as_str)?;
                    let arguments = p
                        .get("arguments")
                        .and_then(Value::as_array)
                        .map(|args| {
                            args.iter()
                                .filter_map(|a| {
                                    let name = a.get("name").and_then(Value::as_str)?;
                                    Some(McpPromptArg {
                                        name: name.to_string(),
                                        required: a
                                            .get("required")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(McpPromptInfo {
                        name: name.to_string(),
                        description: p
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        arguments,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 从 prompts/get 结果解析消息文本：`messages[]` 的 text 内容按序拼接；
/// 内嵌资源（`type: "resource"`）取 `resource.text`。消息之间空行分隔（对齐 omp）。
fn parse_prompt_messages(result: &Value) -> String {
    result
        .get("messages")
        .and_then(Value::as_array)
        .map(|msgs| {
            msgs.iter()
                .flat_map(|m| {
                    let items = match m.get("content") {
                        Some(Value::Array(items)) => items.clone(),
                        Some(other) => vec![other.clone()],
                        None => Vec::new(),
                    };
                    items
                        .into_iter()
                        .filter_map(|item| match item.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                item.get("text").and_then(Value::as_str).map(str::to_string)
                            }
                            Some("resource") => item
                                .get("resource")
                                .and_then(|r| r.get("text"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
                .join("\n\n")
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
    fn renders_text_blocks_with_blank_line_separator() {
        let result = serde_json::json!({
            "content": [
                {"type": "text", "text": "line1"},
                {"type": "text", "text": "line2"}
            ]
        });
        assert_eq!(parse_tool_call(&result).render_text(), "line1\n\nline2");
        assert_eq!(parse_tool_call(&serde_json::json!({})).render_text(), "");
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

    // ── H16：server→client 请求应答（ping / roots/list / 方法不存在） ─────────────

    /// 记录 initialize 入参并捕获 server 请求处理器的桩传输。
    #[derive(Default)]
    struct RequestStub {
        init_params: parking_lot::Mutex<Option<Value>>,
        handler: parking_lot::Mutex<Option<ServerRequestHandler>>,
    }

    impl RequestStub {
        fn answer(&self, method: &str) -> Result<Value, (i64, String)> {
            let handler = self.handler.lock().clone().expect("handler 已注册");
            handler(&Value::from(1), method, &Value::Null)
        }

        fn initialize_capabilities(&self) -> Value {
            self.init_params
                .lock()
                .clone()
                .and_then(|p| p.get("capabilities").cloned())
                .unwrap_or(Value::Null)
        }
    }

    #[async_trait::async_trait]
    impl McpTransport for RequestStub {
        async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
            if method == "initialize" {
                *self.init_params.lock() = Some(params);
                Ok(json!({"protocolVersion": "2025-11-25"}))
            } else {
                Ok(Value::Null)
            }
        }
        async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }
        fn set_on_server_request(&self, handler: ServerRequestHandler) {
            *self.handler.lock() = Some(handler);
        }
        async fn close(&self) {}
    }

    #[tokio::test]
    async fn server_request_handler_answers_ping_and_roots() {
        // 处理器从 client 装配（`from_transport`）——需要能反查处理器，
        // 故此处直接用共享桩：先建桩，再建 client，再从桩取出处理器。
        let stub = Arc::new(RequestStub::default());
        struct Shared(Arc<RequestStub>);
        #[async_trait::async_trait]
        impl McpTransport for Shared {
            async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
                self.0.request(method, params).await
            }
            async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
                self.0.notify(method, params).await
            }
            fn set_on_server_request(&self, handler: ServerRequestHandler) {
                self.0.set_on_server_request(handler);
            }
            async fn close(&self) {}
        }
        let client = McpClient::from_transport(Box::new(Shared(Arc::clone(&stub))));
        assert_eq!(stub.answer("ping").unwrap(), json!({}));
        // 未登记根 → 空数组（仍必须应答）。
        let listed = stub.answer("roots/list").unwrap();
        assert_eq!(listed["roots"].as_array().map(Vec::len), Some(0));
        // 未支持的方法 → -32601（诚实拒绝，server 可降级而非永久挂起）。
        let err = stub.answer("sampling/createMessage").unwrap_err();
        assert_eq!(err.0, JSONRPC_METHOD_NOT_FOUND);
        let err = stub.answer("frobnicate").unwrap_err();
        assert!(err.1.contains("Method not found"));

        // 登记工作区根后应答内容与 initialize 能力声明同步变化。
        client.set_roots(vec![McpRoot::file(std::path::Path::new("/w"), "workspace")]);
        client.initialize().await.expect("initialize");
        let roots = stub.answer("roots/list").unwrap();
        assert_eq!(roots["roots"][0]["uri"], "file:///w");
        assert_eq!(roots["roots"][0]["name"], "workspace");
        assert!(stub.initialize_capabilities()["roots"].is_object());
    }

    #[tokio::test]
    async fn initialize_omits_roots_capability_without_roots() {
        let stub = Arc::new(RequestStub::default());
        struct Shared(Arc<RequestStub>);
        #[async_trait::async_trait]
        impl McpTransport for Shared {
            async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
                self.0.request(method, params).await
            }
            async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
                self.0.notify(method, params).await
            }
            fn set_on_server_request(&self, handler: ServerRequestHandler) {
                self.0.set_on_server_request(handler);
            }
            async fn close(&self) {}
        }
        let client = McpClient::from_transport(Box::new(Shared(Arc::clone(&stub))));
        client.initialize().await.expect("initialize");
        // 未登记根 → 不声明 roots（声明了却答不出根会让 server 逻辑走空）。
        assert!(stub.initialize_capabilities().get("roots").is_none());
    }

    #[test]
    fn mcp_root_file_uri_shape() {
        let root = McpRoot::file(std::path::Path::new("/home/u/w"), "workspace");
        assert_eq!(root.uri, "file:///home/u/w");
        assert_eq!(root.name.as_deref(), Some("workspace"));
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

    // ── H15：capabilities 解析与能力门控 ────────────────────────────────────────

    #[test]
    fn parses_server_capabilities() {
        let info = parse_server_info(&serde_json::json!({
            "capabilities": {"tools": {}, "resources": {"subscribe": true}, "prompts": {}}
        }));
        assert!(info.capabilities.tools);
        assert!(info.capabilities.resources);
        assert!(info.capabilities.prompts);
        assert!(!info.capabilities.logging);
        assert!(!info.capabilities.is_empty());

        // 未声明任何能力 → is_empty（客户端退化为按方法探测）。
        let none = parse_server_info(&serde_json::json!({"capabilities": {}}));
        assert!(none.capabilities.is_empty());
        assert!(
            parse_server_info(&serde_json::json!({}))
                .capabilities
                .is_empty()
        );
        // null / 非对象 → 同未声明。
        assert!(ServerCapabilities::parse(Some(&Value::Null)).is_empty());
    }

    /// 只声明 resources 的 server：不得因 `tools/list` 返回 `-32601` 而连接失败。
    struct ResourcesOnlyMock;

    #[async_trait::async_trait]
    impl McpTransport for ResourcesOnlyMock {
        async fn request(&self, method: &str, _params: Value) -> Result<Value, McpError> {
            match method {
                "initialize" => Ok(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"resources": {}}
                })),
                // 纯 resources server 对 tools/prompts 一律返回 -32601。
                _ => Err(McpError::Server(
                    r#"{"code":-32601,"message":"Method not found"}"#.to_string(),
                )),
            }
        }
        async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }
        async fn close(&self) {}
    }

    #[tokio::test]
    async fn capability_gating_skips_unsupported_lists() {
        let client = McpClient::from_transport(Box::new(ResourcesOnlyMock));
        client.initialize().await.expect("initialize");
        assert!(!client.supports(McpCapability::Tools));
        assert!(client.supports(McpCapability::Resources));
        assert!(!client.supports(McpCapability::Prompts));
        // 关键：不报错、返回空清单（此前会 Err → 上游 close() 整个连接）。
        assert!(client.list_tools().await.expect("不应报错").is_empty());
        assert!(client.list_prompts().await.expect("不应报错").is_empty());
    }

    /// 未声明能力的旧 server（capabilities 为空）：仍按方法探测，且对 -32601 容错。
    struct NoCapabilitiesMock;

    #[async_trait::async_trait]
    impl McpTransport for NoCapabilitiesMock {
        async fn request(&self, method: &str, _params: Value) -> Result<Value, McpError> {
            match method {
                "initialize" => Ok(serde_json::json!({"protocolVersion": "2024-11-05"})),
                _ => Err(McpError::Server(
                    r#"{"code":-32601,"message":"Method not found"}"#.to_string(),
                )),
            }
        }
        async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }
        async fn close(&self) {}
    }

    #[tokio::test]
    async fn absent_capabilities_fall_back_to_probing() {
        let client = McpClient::from_transport(Box::new(NoCapabilitiesMock));
        client.initialize().await.expect("initialize");
        assert!(client.supports(McpCapability::Tools), "未声明 → 按方法探测");
        assert!(client.list_tools().await.expect("-32601 应容错").is_empty());
    }

    #[test]
    fn method_not_found_detection() {
        assert!(
            McpError::Server(r#"{"code":-32601,"message":"Method not found"}"#.into())
                .is_method_not_found()
        );
        assert!(McpError::Server("Method Not Found".into()).is_method_not_found());
        assert!(
            !McpError::Server(r#"{"code":-32602,"message":"bad"}"#.into()).is_method_not_found()
        );
        assert!(!McpError::Closed.is_method_not_found());
        assert!(McpError::Closed.is_connection_lost());
    }

    // ── H17：tools/call 内容块保真 ──────────────────────────────────────────────

    #[test]
    fn parses_tool_call_content_blocks() {
        let result = serde_json::json!({
            "content": [
                {"type": "text", "text": "done"},
                {"type": "image", "data": "aGk=", "mimeType": "image/png"},
                {"type": "resource", "resource": {"uri": "file:///a.txt", "text": "body"}},
                {"type": "audio", "data": "x"}
            ],
            "isError": true,
            "structuredContent": {"ok": false}
        });
        let outcome = parse_tool_call(&result);
        assert!(outcome.is_error);
        assert_eq!(outcome.structured, Some(serde_json::json!({"ok": false})));
        assert_eq!(outcome.content.len(), 4);
        assert_eq!(outcome.content[0], McpContent::Text("done".into()));
        assert_eq!(
            outcome.content[1],
            McpContent::Image {
                data: "aGk=".into(),
                mime_type: "image/png".into()
            }
        );
        assert_eq!(
            outcome.content[2],
            McpContent::Resource {
                uri: "file:///a.txt".into(),
                text: Some("body".into()),
                mime_type: None
            }
        );
        // 渲染：resource 带 URI 头，image 占位，未知类型保留标记。
        let text = outcome.render_text();
        assert!(text.contains("done"));
        assert!(text.contains("[image/png]"));
        assert!(text.contains("[Resource: file:///a.txt]\nbody"));
        assert!(text.contains("[audio]"));
    }

    #[test]
    fn structured_content_is_rendered_when_content_is_empty() {
        let outcome = parse_tool_call(&serde_json::json!({
            "content": [],
            "structuredContent": {"answer": 42}
        }));
        assert!(outcome.render_text().contains("\"answer\": 42"));
    }

    #[test]
    fn first_image_only_when_exclusively_images() {
        let only_image = parse_tool_call(&serde_json::json!({
            "content": [{"type": "image", "data": "aGk=", "mimeType": "image/png"}]
        }));
        assert_eq!(only_image.first_image(), Some(("aGk=", "image/png")));

        let text_and_image = parse_tool_call(&serde_json::json!({
            "content": [
                {"type": "text", "text": "see"},
                {"type": "image", "data": "aGk=", "mimeType": "image/png"}
            ]
        }));
        assert_eq!(
            text_and_image.first_image(),
            None,
            "文本+图像无法单图像承载"
        );

        let two_images = parse_tool_call(&serde_json::json!({
            "content": [
                {"type": "image", "data": "aGk=", "mimeType": "image/png"},
                {"type": "image", "data": "aGk=", "mimeType": "image/png"}
            ]
        }));
        assert_eq!(two_images.first_image(), None, "多图无法单图像承载");

        let no_content = parse_tool_call(&serde_json::json!({}));
        assert_eq!(no_content.first_image(), None);
        assert!(!no_content.is_error);
    }
}
