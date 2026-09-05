//! MCP Streamable HTTP 传输（参考 oh-my-pi `transports/http.ts`）。
//!
//! 语义：
//! - 每个请求 = 一次 `POST`（JSON-RPC 2.0 体），头 `Accept: application/json, text/event-stream`
//! - 响应可为 `application/json`（单响应）或 `text/event-stream`（流内夹带通知/请求）
//! - `initialize` 响应的 `Mcp-Session-Id` 头记录会话，后续请求回传；`close` 发 `DELETE` 终止
//! - `initialize` 协商的 `MCP-Protocol-Version` 仅在握手后的请求携带（规范要求）
//! - 通知（无 id）`POST` 后任意 2xx（含 202 Accepted）即成功
//! - 可选 GET SSE 监听（server 主动推送）未实现：与 stdio 路径一致，本客户端不消费
//!   server 主动通知，流内夹带的通知仅记 debug 日志

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_config::McpHttpConfig;
use futures::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::client::{McpError, McpTransport, resolve_timeout};

/// 传输层独占的请求头：用户配置中的同名头（大小写不敏感）被剥除，防注入。
const RESERVED_HEADERS: [&str; 2] = ["mcp-session-id", "mcp-protocol-version"];

/// MCP Streamable HTTP 传输：请求路径无状态（每次 POST 独立），仅会话头为连接态。
pub(crate) struct HttpTransport {
    http: reqwest::Client,
    url: String,
    /// 用户配置的自定义头（已剥除传输层独占头）。
    extra_headers: reqwest::header::HeaderMap,
    /// 单请求总超时（覆盖 POST + 响应/SSE 取回；`None` = 不限制）。
    timeout: Option<Duration>,
    /// server 会话 id（initialize 响应的 `Mcp-Session-Id` 头）。
    session: Mutex<Option<String>>,
    /// initialize 协商出的协议版本（握手后请求携带 `MCP-Protocol-Version`）。
    protocol_version: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl HttpTransport {
    /// 构造 HTTP 传输（逻辑连接：无握手 socket，`connected` 即「传输可用」）。
    ///
    /// # Errors
    /// 客户端构造失败或配置头非法（非可见 ASCII 值）时返回 [`McpError`]。
    pub async fn connect(cfg: &McpHttpConfig) -> Result<Self, McpError> {
        let mut extra_headers = reqwest::header::HeaderMap::new();
        for (k, v) in &cfg.headers {
            if RESERVED_HEADERS.iter().any(|r| k.eq_ignore_ascii_case(r)) {
                continue;
            }
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| McpError::Http(format!("非法请求头名 {k:?}: {e}")))?;
            // 头值支持 `${ENV}` 展开（对齐 api_key / socks5.password 的敏感字段惯例，
            // 避免 token 明文落盘）。
            let expanded = agent_config::expand_env(v);
            let value = reqwest::header::HeaderValue::from_str(&expanded)
                .map_err(|e| McpError::Http(format!("非法请求头值 {k:?}: {e}")))?;
            extra_headers.insert(name, value);
        }
        let http = reqwest::Client::builder()
            .user_agent(agent_core::platform::default_llm_user_agent())
            .build()?;
        Ok(Self {
            http,
            url: cfg.url.clone(),
            extra_headers,
            timeout: resolve_timeout(cfg.timeout_ms),
            session: Mutex::new(None),
            protocol_version: Mutex::new(None),
            next_id: AtomicU64::new(1),
        })
    }

    /// 组装基础请求：URL + Accept + 自定义头 + 会话/协议版本头。
    fn base_request(&self, method: reqwest::Method) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method, &self.url)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .headers(self.extra_headers.clone());
        if let Some(s) = self.session.lock().clone() {
            req = req.header("Mcp-Session-Id", s);
        }
        if let Some(v) = self.protocol_version.lock().clone() {
            req = req.header("MCP-Protocol-Version", v);
        }
        req
    }

    /// 发送请求体并应用总超时。
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, McpError> {
        let fut = req.send();
        match self.timeout {
            Some(t) => tokio::time::timeout(t, fut)
                .await
                .map_err(|_| McpError::Server(format!("MCP 请求超时（{t:?}）")))?,
            None => fut.await,
        }
        .map_err(McpError::from)
    }

    /// 从 SSE 响应流中取回 `expected_id` 对应的 JSON-RPC 响应。
    ///
    /// 流内夹带的 server 通知（有 method 无 id）记 debug 日志；其余消息（其他 id 的
    /// 响应、server→client 请求）与 stdio 读循环行为一致——忽略。
    async fn take_sse_response(
        resp: reqwest::Response,
        expected_id: u64,
    ) -> Result<Value, McpError> {
        use eventsource_stream::Eventsource;

        let mut events = resp.bytes_stream().eventsource();
        while let Some(ev) = events.next().await {
            let ev = ev.map_err(|e| McpError::Http(format!("SSE 流读取失败: {e}")))?;
            let data = ev.data.trim();
            if data.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(data) else {
                tracing::debug!(target: "mcp::http", "丢弃无法解析的 SSE data");
                continue;
            };
            // MCP 允许 JSON-RPC batch：数组拆为逐条分发。
            let messages: Vec<Value> = match msg {
                Value::Array(items) => items,
                m @ Value::Object(_) => vec![m],
                _ => {
                    tracing::debug!(target: "mcp::http", "丢弃非对象 SSE 消息");
                    continue;
                }
            };
            for m in messages {
                let id_matches = m.get("id").and_then(Value::as_u64) == Some(expected_id);
                let has_payload = m.get("result").is_some() || m.get("error").is_some();
                if id_matches && has_payload {
                    if let Some(err) = m.get("error") {
                        return Err(McpError::Server(err.to_string()));
                    }
                    return Ok(m.get("result").cloned().unwrap_or(Value::Null));
                }
                if m.get("method").is_some() && m.get("id").is_none() {
                    let name = m.get("method").and_then(Value::as_str).unwrap_or("?");
                    tracing::debug!(target: "mcp::http", method = name, "忽略 server 通知");
                }
            }
        }
        // 流结束仍未等到匹配响应——server 提前关流，请求失败。
        Err(McpError::Closed)
    }
}

#[async_trait::async_trait]
impl McpTransport for HttpTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        let req = self
            .base_request(reqwest::Method::POST)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        let resp = self.send(req).await?;

        // 会话头捕获：initialize 响应（及其后任意响应）可刷新会话 id。
        if let Some(s) = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock() = Some(s.to_string());
        }

        let status = resp.status();
        if !status.is_success() {
            // 错误体截断防撑爆日志（对齐 llm 层 4 KiB 上限）。
            let text = resp.text().await.unwrap_or_default();
            let text = if text.len() > 4096 {
                &text[..4096]
            } else {
                &text
            };
            return Err(McpError::Http(format!("HTTP {status}: {text}")));
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if content_type.contains("text/event-stream") {
            let fut = Self::take_sse_response(resp, id);
            return match self.timeout {
                Some(t) => tokio::time::timeout(t, fut)
                    .await
                    .map_err(|_| McpError::Server(format!("MCP 请求超时（{t:?}）")))?,
                None => fut.await,
            };
        }

        let resp_json: Value = resp.json().await?;
        if let Some(err) = resp_json.get("error") {
            return Err(McpError::Server(err.to_string()));
        }
        Ok(resp_json.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))?;
        let req = self
            .base_request(reqwest::Method::POST)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        let resp = self.send(req).await?;
        // 202 Accepted 与任意 2xx 均视为成功（规范：通知无响应体要求）。
        if !resp.status().is_success() {
            return Err(McpError::Http(format!(
                "HTTP {}（通知 {method}）",
                resp.status()
            )));
        }
        Ok(())
    }

    fn set_protocol_version(&self, version: &str) {
        *self.protocol_version.lock() = Some(version.to_string());
    }

    async fn close(&self) {
        // 尽力而为：DELETE + Mcp-Session-Id 终止 server 会话；失败不阻断退出。
        let Some(session) = self.session.lock().take() else {
            return;
        };
        let req = self
            .base_request(reqwest::Method::DELETE)
            .header("Mcp-Session-Id", session);
        let fut = async move {
            if let Ok(resp) = req.send().await {
                let _ = resp.text().await;
            }
        };
        if let Some(t) = self.timeout {
            let _ = tokio::time::timeout(t, fut).await;
        } else {
            fut.await;
        }
    }
}
