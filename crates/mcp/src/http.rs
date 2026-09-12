//! MCP Streamable HTTP 传输（参考 oh-my-pi `transports/http.ts`）。
//!
//! 语义：
//! - 每个请求 = 一次 `POST`（JSON-RPC 2.0 体），头 `Accept: application/json, text/event-stream`
//! - 响应可为 `application/json`（单响应）或 `text/event-stream`（流内夹带通知/请求）
//! - `initialize` 响应的 `Mcp-Session-Id` 头记录会话，后续请求回传；`close` 发 `DELETE` 终止
//! - `initialize` 协商的 `MCP-Protocol-Version` 仅在握手后的请求携带（规范要求）
//! - 通知（无 id）`POST` 后任意 2xx（含 202 Accepted）即成功
//! - SSE 流内夹带的 server 通知（有 `method` 无 `id`）经注册的通知处理器分发
//!   （[`McpClient::set_on_notification`]），未注册时仅记 debug 日志
//! - 可选 GET SSE 监听（server 主动推送通道）未实现：本客户端仅消费请求响应流内
//!   夹带的通知（与 stdio 路径一致）

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_config::{McpHttpConfig, OAuthCredentials};
use futures::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::client::{
    McpError, McpTransport, NotificationHandler, ServerRequestHandler, resolve_timeout,
};

/// 传输层独占的请求头：用户配置中的同名头（大小写不敏感）被剥除，防注入。
const RESERVED_HEADERS: [&str; 2] = ["mcp-session-id", "mcp-protocol-version"];

/// OAuth 运行时态：随行凭据（401 时可刷新重试）+ 刷新单飞锁。
struct McpAuthRuntime {
    creds: Mutex<Option<OAuthCredentials>>,
    refresh_lock: tokio::sync::Mutex<()>,
    /// 凭据来源目录（刷新后落盘保持一致——与连接装载同一 oauth.toml）。
    config_dir: std::path::PathBuf,
}

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
    /// server→client 通知处理器槽（SSE 流内夹带的通知经此分发）。
    notifications: Mutex<Option<NotificationHandler>>,
    /// server→client **请求**处理器槽（H16：SSE 流内夹带的请求即时应答并 POST 回端点）。
    server_requests: Mutex<Option<ServerRequestHandler>>,
    /// OAuth 凭据态（配置了 `headers.Authorization` 时不装载——显式头优先）。
    auth: Option<McpAuthRuntime>,
    next_id: AtomicU64,
}

impl HttpTransport {
    /// 构造 HTTP 传输（逻辑连接：无握手 socket，`connected` 即「传输可用」）。
    ///
    /// # Errors
    /// 客户端构造失败或配置头非法（非可见 ASCII 值）时返回 [`McpError`]。
    pub async fn connect(cfg: &McpHttpConfig) -> Result<Self, McpError> {
        let config_dir =
            agent_core::platform::config_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        Self::connect_in(cfg, &config_dir).await
    }

    /// [`HttpTransport::connect`] 的可注入变体：显式指定配置目录
    /// （测试 / 嵌入式宿主可托管凭据位置）。
    ///
    /// # Errors
    /// 客户端构造失败或配置头非法（非可见 ASCII 值）时返回 [`McpError`]。
    pub async fn connect_in(
        cfg: &McpHttpConfig,
        config_dir: &std::path::Path,
    ) -> Result<Self, McpError> {
        let (http, extra_headers) = build_http_parts(cfg)?;
        // OAuth 凭据装载：配置头已含 Authorization → 跳过（显式头优先，omp
        // `hasMcpAuthorizationHeader`）；否则读 oauth.toml 的 `mcp_oauth:<url>` 行。
        // 过期先刷后用（10s 超时）；确定性失败清行，非确定性失败保留旧值硬上。
        let auth = if extra_headers.contains_key("authorization") {
            None
        } else {
            load_auth_runtime_in(&http, &cfg.url, config_dir).await
        };
        Ok(Self {
            http,
            url: cfg.url.clone(),
            extra_headers,
            timeout: resolve_timeout(cfg.timeout_ms),
            session: Mutex::new(None),
            protocol_version: Mutex::new(None),
            notifications: Mutex::new(None),
            server_requests: Mutex::new(None),
            auth,
            next_id: AtomicU64::new(1),
        })
    }

    /// 组装基础请求：URL + Accept + 自定义头 + Bearer + 会话/协议版本头。
    fn base_request(&self, method: reqwest::Method) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method, &self.url)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .headers(self.extra_headers.clone());
        if let Some(auth) = &self.auth {
            if let Some(creds) = auth.creds.lock().clone() {
                if let Ok(v) =
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {}", creds.access))
                {
                    req = req.header(reqwest::header::AUTHORIZATION, v);
                }
            }
        }
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
    /// 流内夹带的 server 通知（有 method 无 id）转发给注册的通知处理器；**server→client
    /// 请求（有 method 且有 id）即时应答并 POST 回端点**（H16）。其余消息（其他 id 的响应）
    /// 忽略。
    async fn take_sse_response(
        &self,
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
                if let Some(name) = m.get("method").and_then(Value::as_str) {
                    let params = m.get("params").cloned().unwrap_or(Value::Null);
                    match m.get("id") {
                        // H16：server→client 请求——应答并回发（此前静默丢弃 → server 挂起）。
                        Some(req_id) => {
                            let answer = match self.server_requests.lock().clone() {
                                Some(h) => h(req_id, name, &params),
                                None => Err((
                                    crate::client::JSONRPC_METHOD_NOT_FOUND,
                                    format!("Method not found: {name}"),
                                )),
                            };
                            let frame = match answer {
                                Ok(result) => crate::client::server_request_result(req_id, result),
                                Err((code, message)) => crate::client::server_request_error(
                                    req_id,
                                    code,
                                    message.as_str(),
                                ),
                            };
                            let body = serde_json::to_vec(&frame).unwrap_or_default();
                            let req = self
                                .base_request(reqwest::Method::POST)
                                .header(reqwest::header::CONTENT_TYPE, "application/json")
                                .body(body);
                            // 回发失败只告警：server 侧会自行超时降级，不影响本次请求的结果。
                            if let Err(e) = req.send().await {
                                tracing::warn!(target: "mcp::http", error = %e, "回发 server→client 请求响应失败");
                            }
                        }
                        None => match self.notifications.lock().clone() {
                            Some(h) => h(name, &params),
                            None => tracing::debug!(
                                target: "mcp::http",
                                method = name,
                                "忽略 server 通知"
                            ),
                        },
                    }
                }
            }
        }
        // 流结束仍未等到匹配响应——server 提前关流，请求失败。
        Err(McpError::Closed)
    }
}

/// 按 MCP HTTP 配置构造传输共享部分：剥除传输层独占头后的自定义头 + reqwest 客户端。
/// Streamable HTTP 与 legacy SSE 两个传输体共用。
///
/// # Errors
/// 配置头非法（非可见 ASCII 值）时返回 [`McpError`]。
pub(crate) fn build_http_parts(
    cfg: &McpHttpConfig,
) -> Result<(reqwest::Client, reqwest::header::HeaderMap), McpError> {
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
    Ok((http, extra_headers))
}

/// 从 oauth.toml 装载 `mcp_oauth:<url>` 凭据并做连接期预刷新。
///
/// - 已过期 → 刷新（10s 超时）后落盘；确定性失败（4xx）→ 清行、无凭据继续
///   （后续 401 会透出登录指引）；非确定性失败 → 保留旧值硬上（omp「using
///   existing token」警告语义）。
async fn load_auth_runtime_in(
    http: &reqwest::Client,
    url: &str,
    config_dir: &std::path::Path,
) -> Option<McpAuthRuntime> {
    let store = agent_config::load_oauth(config_dir);
    let mut creds = store.0.get(&crate::oauth::credential_key(url)).cloned()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    if creds.is_expired(now) {
        match tokio::time::timeout(
            crate::oauth::REFRESH_TIMEOUT,
            crate::oauth::refresh(http, &creds),
        )
        .await
        {
            Ok(Ok(fresh)) => {
                if let Err(e) =
                    agent_config::save_oauth(config_dir, &crate::oauth::credential_key(url), &fresh)
                {
                    tracing::warn!("MCP OAuth 刷新凭据落盘失败（沿用内存值）：{e}");
                }
                creds = fresh;
            }
            Ok(Err(e)) if crate::oauth::is_definitive_refresh_failure(&e) => {
                tracing::warn!("MCP OAuth 刷新确定性失败，清除凭据：{e}");
                let _ = agent_config::remove_oauth(config_dir, &crate::oauth::credential_key(url));
                return None;
            }
            Ok(Err(e)) => tracing::warn!("MCP OAuth 刷新失败，沿用现有 token：{e}"),
            Err(_) => tracing::warn!("MCP OAuth 刷新超时，沿用现有 token"),
        }
    }
    Some(McpAuthRuntime {
        creds: Mutex::new(Some(creds)),
        refresh_lock: tokio::sync::Mutex::new(()),
        config_dir: config_dir.to_path_buf(),
    })
}

impl HttpTransport {
    /// 401 处置：单飞刷新（先刷后重试；他人已刷新则直接重试）。
    ///
    /// # Errors
    /// 无凭据（透出登录指引）/ 刷新确定性失败（清凭据并指引重登）。
    async fn refresh_on_unauthorized(&self, used_access: Option<&str>) -> Result<(), McpError> {
        let Some(auth) = &self.auth else {
            return Err(McpError::Http(format!(
                "HTTP 401 Unauthorized——server {} 需要授权：运行 `agent mcp login <名称>`",
                self.url
            )));
        };
        let _guard = auth.refresh_lock.lock().await;
        let current = auth.creds.lock().clone().map(|c| c.access);
        if current.is_some() && current.as_deref() != used_access {
            // 其他请求已完成刷新——直接用新 token 重试。
            return Ok(());
        }
        let Some(creds) = auth.creds.lock().clone() else {
            return Err(McpError::Http(format!(
                "HTTP 401 Unauthorized——server {} 需要授权：运行 `agent mcp login <名称>`",
                self.url
            )));
        };
        let refreshed = tokio::time::timeout(
            crate::oauth::REFRESH_TIMEOUT,
            crate::oauth::refresh(&self.http, &creds),
        )
        .await
        .map_err(|_| McpError::Http("MCP OAuth 刷新超时".to_string()))?;
        match refreshed {
            Ok(fresh) => {
                let _ = agent_config::save_oauth(
                    &auth.config_dir,
                    &crate::oauth::credential_key(&self.url),
                    &fresh,
                );
                *auth.creds.lock() = Some(fresh);
                Ok(())
            }
            Err(e) => {
                if crate::oauth::is_definitive_refresh_failure(&e) {
                    let _ = agent_config::remove_oauth(
                        &auth.config_dir,
                        &crate::oauth::credential_key(&self.url),
                    );
                    *auth.creds.lock() = None;
                    Err(McpError::Http(format!(
                        "HTTP 401——授权已失效且刷新被拒（{e}）。重新登录：`agent mcp login <名称>`"
                    )))
                } else {
                    Err(McpError::Http(format!(
                        "HTTP 401——token 刷新失败（{e}），请重试或重新登录"
                    )))
                }
            }
        }
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
        let send_once = || async {
            let req = self
                .base_request(reqwest::Method::POST)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone());
            self.send(req).await
        };
        let mut resp = send_once().await?;
        let used_access = self
            .auth
            .as_ref()
            .and_then(|a| a.creds.lock().clone())
            .map(|c| c.access);

        // 401 → OAuth 刷新单飞 + 一次性重试（omp `isAuthRefreshableMCPTransport`
        // 路径：有随行凭据才可刷；无凭据时透出登录指引）。
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Err(e) = self.refresh_on_unauthorized(used_access.as_deref()).await {
                let _ = resp.text().await;
                return Err(e);
            }
            resp = send_once().await?;
        }

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
            let hint = if status == reqwest::StatusCode::UNAUTHORIZED {
                format!(
                    "\n需要授权：运行 `agent mcp login <名称>`（server {}）",
                    self.url
                )
            } else {
                String::new()
            };
            return Err(McpError::Http(format!("HTTP {status}: {text}{hint}")));
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if content_type.contains("text/event-stream") {
            let fut = self.take_sse_response(resp, id);
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

    fn set_on_notification(&self, handler: NotificationHandler) {
        *self.notifications.lock() = Some(handler);
    }

    fn set_on_server_request(&self, handler: ServerRequestHandler) {
        *self.server_requests.lock() = Some(handler);
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
