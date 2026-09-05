//! MCP 协议层：JSON-RPC 2.0 客户端门面（传输无关）。
//!
//! [`McpClient`] 组合 [`initialize`](McpClient::initialize) / `tools/list` /
//! `tools/call` / `resources/*` 等方法序，底层 I/O 委托给 [`McpTransport`]：
//! - [`StdioTransport`](crate::stdio::StdioTransport)：子进程 stdin/stdout 行分隔 JSON
//! - [`HttpTransport`](crate::http::HttpTransport)：Streamable HTTP（POST JSON-RPC + 可选 SSE 响应流）

use std::time::Duration;

use agent_config::McpServerConfig;
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

    /// 关闭传输：stdio 终止子进程；HTTP 终止 server 会话（DELETE + `Mcp-Session-Id`）。
    async fn close(&self);
}

/// MCP 客户端（JSON-RPC 2.0；stdio 子进程或 Streamable HTTP 端点）。
pub struct McpClient {
    transport: Box<dyn McpTransport>,
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
        Ok(Self { transport })
    }

    /// initialize 握手 + initialized 通知。
    ///
    /// 协商出的 `protocolVersion` 回传给传输层（HTTP 后续请求携带
    /// `MCP-Protocol-Version` 头，仅在握手完成后发送——规范要求）。
    ///
    /// # Errors
    /// 握手失败时返回 [`McpError`]。
    pub async fn initialize(&self) -> Result<(), McpError> {
        let result = self
            .transport
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
            self.transport.set_protocol_version(v);
        }
        self.transport
            .notify("notifications/initialized", json!({}))
            .await
    }

    /// 列出 server 工具元信息。
    ///
    /// # Errors
    /// 通信失败时返回 [`McpError`]。
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, McpError> {
        let result = self.transport.request("tools/list", json!({})).await?;
        Ok(parse_tools(&result))
    }

    /// 调用工具，返回文本内容拼接。
    ///
    /// # Errors
    /// 通信/server error 时返回 [`McpError`]。
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<String, McpError> {
        let result = self
            .transport
            .request("tools/call", json!({"name":name,"arguments":args}))
            .await?;
        Ok(parse_text_content(&result))
    }

    /// 列出 server 暴露的资源（`resources/list`）。
    ///
    /// # Errors
    /// 通信失败或 server 不支持 resources 时返回 [`McpError`]（调用方可据错误判断能力缺失）。
    pub async fn list_resources(&self) -> Result<Vec<McpResource>, McpError> {
        let result = self.transport.request("resources/list", json!({})).await?;
        Ok(parse_resources(&result))
    }

    /// 读取一个资源（`resources/read`），返回文本内容拼接。
    ///
    /// # Errors
    /// 通信失败、资源不存在或 server 不支持 resources 时返回 [`McpError`]。
    pub async fn read_resource(&self, uri: &str) -> Result<String, McpError> {
        let result = self
            .transport
            .request("resources/read", json!({"uri":uri}))
            .await?;
        Ok(parse_resource_text(&result))
    }

    /// 关闭连接：stdio 终止子进程；HTTP 终止 server 会话。
    pub async fn close(&self) {
        self.transport.close().await;
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
}
