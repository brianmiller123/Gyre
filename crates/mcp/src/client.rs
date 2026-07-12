//! MCP stdio 客户端：JSON-RPC 2.0 over 子进程 stdin/stdout。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_config::McpServerConfig;

/// 单次 JSON-RPC 请求超时上限（server 无响应时避免永久挂起）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// MCP stdout 单行最大字节数：超过即丢弃缓冲（防无换行的超长行 OOM）。
const MAX_MCP_LINE_BYTES: usize = 4 * 1024 * 1024;
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};

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
    /// 子进程 stdout 关闭（请求无响应）。
    #[error("MCP 连接已关闭")]
    Closed,
    /// 响应信道失效。
    #[error("MCP 响应信道错误")]
    Channel,
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

/// MCP stdio 客户端（JSON-RPC 2.0，行分隔）。
pub struct McpClient {
    write: Mutex<ChildStdin>,
    child: Mutex<Child>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
}

impl McpClient {
    /// 启动 MCP server 子进程并建立客户端。
    ///
    /// # Errors
    /// 启动失败或 stdin/stdout 不可用时返回 [`McpError`]。
    pub async fn spawn(cfg: &McpServerConfig) -> Result<Self, McpError> {
        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or(McpError::Closed)?;
        let stdout = child.stdout.take().ok_or(McpError::Closed)?;
        // 后台排空 MCP server 的 stderr（不再静默丢弃），便于排查子进程报错。
        if let Some(stderr) = child.stderr.take() {
            let name = cfg.command.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!(target: "mcp::stderr", server = %name, "{line}");
                }
            });
        }

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // 后台读 task：逐行解析 JSON-RPC，按 id 分发响应；stdout 关闭时清空 pending（rx 报错）。
        let pending_clone = Arc::clone(&pending);
        tokio::spawn(async move {
            // 手动有界行读：按 `\n` 切分并对单行设 [`MAX_MCP_LINE_BYTES`] 上限，
            // 杜绝 server 发送无换行的超长行导致内存无界增长。
            use tokio::io::AsyncReadExt;
            let mut reader = stdout;
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                match reader.read(&mut tmp).await {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                            let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
                            let Ok(s) = std::str::from_utf8(&line_bytes) else { continue };
                            let trimmed = s.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                                continue;
                            };
                            if let Some(id) = val.get("id").and_then(|v| v.as_u64()) {
                                let mut p = pending_clone.lock().await;
                                if let Some(tx) = p.remove(&id) {
                                    let _ = tx.send(val);
                                }
                            }
                        }
                        if buf.len() > MAX_MCP_LINE_BYTES {
                            tracing::warn!(
                                target: "mcp::client",
                                "MCP 行超过 {MAX_MCP_LINE_BYTES} 字节上限，丢弃到下一行边界"
                            );
                            // 丢弃到下一个换行（含），保留换行后的完整行数据，避免协议失步。
                            if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                                buf.drain(..=nl);
                            } else {
                                buf.clear();
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        tracing::warn!(target: "mcp::client", "MCP stdout 读取错误: {e}");
                        break;
                    }
                }
            }
            pending_clone.lock().await.clear();
        });

        Ok(Self {
            write: Mutex::new(stdin),
            child: Mutex::new(child),
            next_id: AtomicU64::new(1),
            pending,
        })
    }

    /// 发送 JSON-RPC 请求并等待响应 `result`。
    ///
    /// # Errors
    /// 序列化/IO/超时/server error 时返回 [`McpError`]。
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        // 序列化或写入失败时必须清理 pending，否则对应 id 永久泄漏，rx 也无法被响应。
        let send_result: Result<(), McpError> = async {
            let serialized = serde_json::to_string(&req)?;
            let mut w = self.write.lock().await;
            w.write_all(serialized.as_bytes()).await?;
            w.write_all(b"\n").await?;
            w.flush().await?;
            Ok(())
        }
        .await;
        if let Err(e) = send_result {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        // 超时保护：server 无响应且不关 stdout 时避免永久挂起；超时即清理 pending。
        let resp = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                return Err(McpError::Channel);
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(McpError::Server(format!(
                    "MCP 请求超时（{REQUEST_TIMEOUT:?}）"
                )));
            }
        };
        if let Some(err) = resp.get("error") {
            return Err(McpError::Server(err.to_string()));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// 发送通知（无 id，无响应）。
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let notif = serde_json::json!({"jsonrpc":"2.0","method":method,"params":params});
        let s = serde_json::to_string(&notif)?;
        let mut w = self.write.lock().await;
        w.write_all(s.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok(())
    }

    /// initialize 握手 + initialized 通知。
    ///
    /// # Errors
    /// 握手失败时返回 [`McpError`]。
    pub async fn initialize(&self) -> Result<(), McpError> {
        let _ = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "agent", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        self.notify("notifications/initialized", serde_json::json!({}))
            .await?;
        Ok(())
    }

    /// 列出 server 工具元信息。
    ///
    /// # Errors
    /// 通信失败时返回 [`McpError`]。
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, McpError> {
        let result = self.request("tools/list", serde_json::json!({})).await?;
        Ok(parse_tools(&result))
    }

    /// 调用工具，返回文本内容拼接。
    ///
    /// # Errors
    /// 通信/server error 时返回 [`McpError`]。
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<String, McpError> {
        let result = self
            .request("tools/call", serde_json::json!({"name":name,"arguments":args}))
            .await?;
        Ok(parse_text_content(&result))
    }

    /// 列出 server 暴露的资源（`resources/list`）。
    ///
    /// # Errors
    /// 通信失败或 server 不支持 resources 时返回 [`McpError`]（调用方可据错误判断能力缺失）。
    pub async fn list_resources(&self) -> Result<Vec<McpResource>, McpError> {
        let result = self.request("resources/list", serde_json::json!({})).await?;
        Ok(parse_resources(&result))
    }

    /// 读取一个资源（`resources/read`），返回文本内容拼接。
    ///
    /// # Errors
    /// 通信失败、资源不存在或 server 不支持 resources 时返回 [`McpError`]。
    pub async fn read_resource(&self, uri: &str) -> Result<String, McpError> {
        let result = self
            .request("resources/read", serde_json::json!({"uri":uri}))
            .await?;
        Ok(parse_resource_text(&result))
    }

    /// 终止子进程。
    pub async fn kill(&self) {
        let mut c = self.child.lock().await;
        let _ = c.kill().await;
        let _ = c.wait().await;
    }
}

// 注：子进程回收依赖 `spawn` 时设置的 `kill_on_drop(true)`——`McpClient` drop 时
// 其拥有的 `Child` 一同 drop，自动 kill，杜绝孤儿进程。

/// 从 tools/list 结果解析工具元信息列表。
fn parse_tools(result: &Value) -> Vec<McpToolInfo> {
    let Some(arr) = result.get("tools").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.to_string();
            let description = t
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            let schema = t.get("inputSchema").cloned().unwrap_or(Value::Null);
            Some(McpToolInfo {
                name,
                description,
                schema,
            })
        })
        .collect()
}

/// 从 tools/call 结果提取文本内容（content[].text 拼接）。
fn parse_text_content(result: &Value) -> String {
    result
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// 从 resources/list 结果解析资源元信息列表。
fn parse_resources(result: &Value) -> Vec<McpResource> {
    let Some(arr) = result.get("resources").and_then(|r| r.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|r| {
            let uri = r.get("uri")?.as_str()?.to_string();
            let name = r
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or(&uri)
                .to_string();
            let description = r.get("description").and_then(|d| d.as_str()).map(str::to_string);
            let mime_type = r.get("mimeType").and_then(|m| m.as_str()).map(str::to_string);
            Some(McpResource { uri, name, description, mime_type })
        })
        .collect()
}

/// 从 resources/read 结果提取文本内容（contents[].text 拼接，忽略 blob）。
fn parse_resource_text(result: &Value) -> String {
    result
        .get("contents")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
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
}
