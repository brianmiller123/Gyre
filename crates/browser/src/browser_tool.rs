//! BrowserTool：headless chromium + CDP 的浏览器自动化工具。
//!
//! 动作（v1）：
//! - `navigate(url)` — `Page.navigate` + 等待 `Page.loadEventFired`（5s 超时）
//! - `evaluate(js)` — `Runtime.evaluate`（`returnByValue` + `awaitPromise`）
//! - `screenshot(selector?, full_page?)` — `Page.captureScreenshot`，PNG 落盘
//!   `<workspace>/.gyre/artifacts/<uuid>.png` 并返回路径
//! - `click(selector)` / `text(selector, text)` / `scroll(selector?/direction?/amount?)` —
//!   `Runtime.evaluate` 内用 DOM API 模拟
//! - `close` — 击杀进程树
//!
//! 首个动作懒启动浏览器（`Exclusive` + 可中断，响应 [`ToolContext::cancel`]）。
//! v1 不做：多标签页、网络拦截、headless 之外模式。

use std::time::Duration;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::cdp::{CdpConnection, CdpError};
use crate::launcher::{BrowserProcess, LaunchOptions};

/// 导航后等待加载事件的超时。
const NAV_TIMEOUT: Duration = Duration::from_secs(5);
/// 浏览器启动等待 `DevTools` 端点的超时。
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

/// 浏览器会话：chromium 进程 + CDP 连接 + 附加的页面目标会话。
struct BrowserSession {
    proc: BrowserProcess,
    cdp: CdpConnection,
    session_id: String,
}

impl BrowserSession {
    /// 连接 `DevTools` 端点，新建 about:blank 页面目标并附加（flatten 会话），启用 Page/Runtime 域。
    async fn new(proc: BrowserProcess) -> Result<Self, ToolError> {
        let cdp = CdpConnection::connect(proc.ws_url())
            .await
            .map_err(tool_err)?;
        // 确定性：不依赖初始标签页是否存在，始终新建一个页面目标
        let target = cdp
            .send("Target.createTarget", json!({ "url": "about:blank" }))
            .await
            .map_err(tool_err)?;
        let target_id = target
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Execution("Target.createTarget 响应缺少 targetId".into()))?;
        let attached = cdp
            .send(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await
            .map_err(tool_err)?;
        let session_id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Execution("Target.attachToTarget 响应缺少 sessionId".into()))?
            .to_string();
        cdp.send_in_session("Page.enable", json!({}), &session_id)
            .await
            .map_err(tool_err)?;
        cdp.send_in_session("Runtime.enable", json!({}), &session_id)
            .await
            .map_err(tool_err)?;
        Ok(Self {
            proc,
            cdp,
            session_id,
        })
    }

    /// 会话内 CDP 命令（自动带 `sessionId`）。
    async fn cmd(&self, method: &str, params: Value) -> Result<Value, ToolError> {
        self.cdp
            .send_in_session(method, params, &self.session_id)
            .await
            .map_err(tool_err)
    }

    /// `Runtime.evaluate`（returnByValue + awaitPromise），返回 `result.value`。
    async fn eval_value(&self, js: &str) -> Result<Value, ToolError> {
        let resp = self
            .cmd(
                "Runtime.evaluate",
                json!({ "expression": js, "returnByValue": true, "awaitPromise": true }),
            )
            .await?;
        if let Some(ex) = resp.get("exceptionDetails") {
            let text = ex.get("text").and_then(Value::as_str).unwrap_or("JS 异常");
            let desc = ex
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("");
            return Err(ToolError::Execution(format!("JS 异常: {text} {desc}")));
        }
        Ok(resp
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// 关闭：击杀 chromium 进程树（CDP 连接随 drop 断开）。
    async fn close(self) -> Result<(), ToolError> {
        self.proc.close().await.map_err(tool_err)
    }
}

/// 把 crate 内错误映射为工具执行错误。
fn tool_err<E: std::fmt::Display>(e: E) -> ToolError {
    ToolError::Execution(e.to_string())
}

/// 把字符串编码为 JS 字符串字面量（经 `serde_json` 转义，防注入）。
fn js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// 工具输入 JSON Schema（纯函数，供测试）。
#[must_use]
pub fn browser_schema() -> Value {
    json!({
        "type": "object",
        "required": ["action"],
        "properties": {
            "action": {
                "type": "string",
                "enum": ["navigate", "evaluate", "screenshot", "click", "text", "scroll", "close"],
                "description": "浏览器操作：navigate 打开页面；evaluate 执行 JS；screenshot 截图；click/text/scroll 模拟交互；close 关闭浏览器"
            },
            "url": { "type": "string", "description": "navigate：要打开的 URL" },
            "js": { "type": "string", "description": "evaluate：要执行的 JavaScript（returnByValue + awaitPromise 取回结果）" },
            "selector": { "type": "string", "description": "screenshot/click/text/scroll：CSS 选择器" },
            "text": { "type": "string", "description": "text：填入输入框的文本" },
            "full_page": { "type": "boolean", "description": "screenshot：true 时截取整页而非视口" },
            "direction": { "type": "string", "enum": ["up", "down"], "description": "scroll：无 selector 时的滚动方向（默认 down）" },
            "amount": { "type": "integer", "description": "scroll：滚动像素数（默认 500）" }
        }
    })
}

/// 注入 system prompt 的 browser 工具使用指引（启用时由装配层追加）。
pub const PROMPT_SECTION: &str = "<browser>\n\
browser 工具已启用：headless Chromium + CDP 最小客户端。\n\
动作：navigate(url) 打开页面；evaluate(js) 在页面执行 JavaScript（returnByValue 取回，JSON 序列化）；\n\
screenshot(selector?, full_page?) 截图（PNG 落盘 .gyre/artifacts/ 并返回路径）；\n\
click(selector) / text(selector, text) / scroll(selector?/direction?/amount?) 模拟交互；close 关闭浏览器。\n\
首次动作自动启动浏览器，close 后下次动作重新启动。\n\
</browser>";

/// 浏览器自动化工具（`name = "browser"`）。
pub struct BrowserTool {
    state: Mutex<Option<BrowserSession>>,
}

impl BrowserTool {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    /// 参数校验（先于浏览器启动，避免无效调用拉起进程）。
    fn validate(action: &str, input: &Value) -> Result<(), ToolError> {
        let has = |k: &str| {
            input
                .get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .is_some_and(|s| !s.is_empty())
        };
        match action {
            "navigate" => {
                if !has("url") {
                    return Err(ToolError::InvalidArgs("navigate 需要 'url' 参数".into()));
                }
            }
            "evaluate" => {
                if !has("js") {
                    return Err(ToolError::InvalidArgs("evaluate 需要 'js' 参数".into()));
                }
            }
            "click" => {
                if !has("selector") {
                    return Err(ToolError::InvalidArgs("click 需要 'selector' 参数".into()));
                }
            }
            "text" => {
                if !has("selector") || !has("text") {
                    return Err(ToolError::InvalidArgs(
                        "text 需要 'selector' 与 'text' 参数".into(),
                    ));
                }
            }
            "screenshot" | "scroll" | "close" => {}
            other => return Err(ToolError::InvalidArgs(format!("未知动作: {other}"))),
        }
        Ok(())
    }

    /// 懒启动浏览器会话（首个非 close 动作触发）。
    async fn ensure_session(
        guard: &mut Option<BrowserSession>,
    ) -> Result<&mut BrowserSession, ToolError> {
        if guard.is_none() {
            let proc = BrowserProcess::launch(LaunchOptions {
                timeout: Some(LAUNCH_TIMEOUT),
                ..LaunchOptions::default()
            })
            .await
            .map_err(tool_err)?;
            let sess = BrowserSession::new(proc).await.map_err(tool_err)?;
            *guard = Some(sess);
        }
        guard
            .as_mut()
            .ok_or_else(|| ToolError::Execution("browser: 会话不可用".into()))
    }

    async fn navigate(
        &self,
        sess: &mut BrowserSession,
        input: &Value,
        cancel: &CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let url = input
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("navigate 需要 'url' 参数".into()))?;
        // 记录事件水位：只认 navigate 之后的 loadEventFired，忽略导航前遗留事件
        let since = sess.cdp.event_seq();
        let session_id = sess.session_id.clone();
        let resp = sess.cmd("Page.navigate", json!({ "url": url })).await?;
        if let Some(err) = resp.get("errorText").and_then(Value::as_str) {
            return Err(ToolError::Execution(format!("导航失败: {err}")));
        }
        let loaded = tokio::select! {
            r = sess.cdp.wait_for_event_since(
                since,
                |e| e.method == "Page.loadEventFired" && e.session_id.as_deref() == Some(session_id.as_str()),
                NAV_TIMEOUT,
            ) => r,
            () = cancel.cancelled() => Err(CdpError::Closed),
        };
        let note = match loaded {
            Ok(ev) => match ev.params.get("errorText").and_then(Value::as_str) {
                Some(e) => format!("（加载报错: {e}）"),
                None => String::new(),
            },
            Err(_) => "（5s 超时，页面可能仍在加载）".to_string(),
        };
        Ok(ToolResult::text(format!("已导航到 {url}{note}")))
    }

    async fn evaluate(
        &self,
        sess: &mut BrowserSession,
        input: &Value,
    ) -> Result<ToolResult, ToolError> {
        let js = input
            .get("js")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("evaluate 需要 'js' 参数".into()))?;
        let resp = sess
            .cmd(
                "Runtime.evaluate",
                json!({ "expression": js, "returnByValue": true, "awaitPromise": true }),
            )
            .await?;
        if let Some(ex) = resp.get("exceptionDetails") {
            let text = ex.get("text").and_then(Value::as_str).unwrap_or("JS 异常");
            let desc = ex
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("");
            return Err(ToolError::Execution(format!("JS 异常: {text} {desc}")));
        }
        let value = resp.get("result").and_then(|r| r.get("value")).cloned();
        let out = match value {
            Some(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()),
            None => "(undefined)".to_string(),
        };
        Ok(ToolResult::text(out))
    }

    async fn screenshot(
        &self,
        sess: &mut BrowserSession,
        input: &Value,
        ctx: &agent_tools::ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let selector = input.get("selector").and_then(Value::as_str);
        let full_page = input
            .get("full_page")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(sel) = selector {
            // 先滚动到目标元素，再截当前视口
            let js = format!(
                "(() => {{ const el = document.querySelector({sel}); if (!el) return {{ok:false, reason:'selector 未命中'}}; el.scrollIntoView({{behavior:'instant', block:'center'}}); return {{ok:true}}; }})()",
                sel = js_string(sel)
            );
            let r = sess.eval_value(&js).await?;
            if r.get("ok").and_then(Value::as_bool) != Some(true) {
                let reason = r
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("未知原因");
                return Err(ToolError::Execution(format!("截图定位失败: {reason}")));
            }
        }
        let mut params = json!({ "format": "png" });
        if full_page {
            params["captureBeyondViewport"] = Value::Bool(true);
        }
        let resp = sess.cmd("Page.captureScreenshot", params).await?;
        let b64 = resp
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Execution("截图响应缺少 data".into()))?;
        let png = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| ToolError::Execution(format!("截图 base64 解码失败: {e}")))?;
        let path = save_screenshot(ctx, &png).await?;
        Ok(ToolResult::text(format!(
            "截图已保存: {path}（{} 字节）",
            png.len()
        )))
    }

    async fn click(
        &self,
        sess: &mut BrowserSession,
        input: &Value,
    ) -> Result<ToolResult, ToolError> {
        let sel = input
            .get("selector")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("click 需要 'selector' 参数".into()))?;
        let js = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return {{ok:false, reason:'selector 未命中'}}; el.click(); return {{ok:true, tag: el.tagName}}; }})()",
            sel = js_string(sel)
        );
        let r = sess.eval_value(&js).await?;
        if r.get("ok").and_then(Value::as_bool) == Some(true) {
            let tag = r.get("tag").and_then(Value::as_str).unwrap_or("");
            Ok(ToolResult::text(format!("已点击 <{tag}>（{sel}）")))
        } else {
            let reason = r
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("未知原因");
            Err(ToolError::Execution(format!("click 失败: {reason}")))
        }
    }

    async fn text(
        &self,
        sess: &mut BrowserSession,
        input: &Value,
    ) -> Result<ToolResult, ToolError> {
        let sel = input
            .get("selector")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("text 需要 'selector' 参数".into()))?;
        let text = input
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("text 需要 'text' 参数".into()))?;
        // 设值 + 派发 input/change 事件，兼容常见前端框架（React 等）
        let js = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return {{ok:false, reason:'selector 未命中'}}; el.focus(); el.value = {text}; el.dispatchEvent(new Event('input', {{bubbles:true}})); el.dispatchEvent(new Event('change', {{bubbles:true}})); return {{ok:true, tag: el.tagName}}; }})()",
            sel = js_string(sel),
            text = js_string(text),
        );
        let r = sess.eval_value(&js).await?;
        if r.get("ok").and_then(Value::as_bool) == Some(true) {
            let tag = r.get("tag").and_then(Value::as_str).unwrap_or("");
            Ok(ToolResult::text(format!("已填入 <{tag}>（{sel}）")))
        } else {
            let reason = r
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("未知原因");
            Err(ToolError::Execution(format!("text 失败: {reason}")))
        }
    }

    async fn scroll(
        &self,
        sess: &mut BrowserSession,
        input: &Value,
    ) -> Result<ToolResult, ToolError> {
        if let Some(sel) = input
            .get("selector")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let js = format!(
                "(() => {{ const el = document.querySelector({sel}); if (!el) return {{ok:false, reason:'selector 未命中'}}; el.scrollIntoView({{behavior:'instant', block:'center'}}); return {{ok:true}}; }})()",
                sel = js_string(sel)
            );
            let r = sess.eval_value(&js).await?;
            if r.get("ok").and_then(Value::as_bool) != Some(true) {
                let reason = r
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("未知原因");
                return Err(ToolError::Execution(format!("scroll 失败: {reason}")));
            }
            Ok(ToolResult::text(format!("已滚动到 {sel}")))
        } else {
            let direction = input
                .get("direction")
                .and_then(Value::as_str)
                .unwrap_or("down");
            let sign = if direction == "up" { -1 } else { 1 };
            let amount = input.get("amount").and_then(Value::as_i64).unwrap_or(500);
            let js = format!(
                "(() => {{ window.scrollBy({{top: {sign} * {amount}, behavior: 'instant'}}); return {{ok:true, y: window.scrollY}}; }})()"
            );
            let r = sess.eval_value(&js).await?;
            let y = r.get("y").and_then(Value::as_i64).unwrap_or(0);
            Ok(ToolResult::text(format!("已滚动（当前 scrollY={y}）")))
        }
    }
}

impl Default for BrowserTool {
    fn default() -> Self {
        Self::new()
    }
}

/// 截图落盘：`<workspace>/.gyre/artifacts/<uuid>.png`（与 shake 归档同一目录惯例）。
async fn save_screenshot(
    ctx: &agent_tools::ToolContext<'_>,
    png: &[u8],
) -> Result<String, ToolError> {
    let dir = ctx.workspace.root().join(".gyre").join("artifacts");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(ToolError::Io)?;
    let id = uuid::Uuid::new_v4().simple();
    let path = dir.join(format!("{id}.png"));
    tokio::fs::write(&path, png).await.map_err(ToolError::Io)?;
    Ok(path.display().to_string())
}

#[async_trait]
impl agent_tools::Tool for BrowserTool {
    fn name(&self) -> &'static str {
        "browser"
    }

    fn description(&self) -> &'static str {
        "控制 headless chromium 浏览网页：navigate(url) 打开页面；evaluate(js) 在页面执行 JavaScript（returnByValue 取回结果）；screenshot(selector?, full_page?) 截图保存；click(selector)、text(selector, text)、scroll(selector?/direction?/amount?) 模拟交互；close 关闭浏览器实例。"
    }

    fn schema(&self) -> Value {
        browser_schema()
    }

    fn capability(&self) -> CapabilityTier {
        CapabilityTier::Execute
    }

    fn interruptible(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &agent_tools::ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Execution("browser: 操作已被取消".into()));
        }
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 'action' 参数".into()))?;
        // 先校验参数再启动浏览器：无效调用不得拉起进程
        Self::validate(action, &input)?;
        let mut guard = self.state.lock().await;
        if action == "close" {
            let Some(sess) = guard.take() else {
                return Ok(ToolResult::text("browser: 无运行中的浏览器实例"));
            };
            sess.close().await?;
            Ok(ToolResult::text("browser: 已关闭浏览器实例"))
        } else {
            let sess = Self::ensure_session(&mut guard).await?;
            match action {
                "navigate" => self.navigate(sess, &input, ctx.cancel).await,
                "evaluate" => self.evaluate(sess, &input).await,
                "screenshot" => self.screenshot(sess, &input, ctx).await,
                "click" => self.click(sess, &input).await,
                "text" => self.text(sess, &input).await,
                "scroll" => self.scroll(sess, &input).await,
                other => Err(ToolError::InvalidArgs(format!("未知动作: {other}"))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::Workspace;
    use agent_core::{ApprovalDecision, ApprovalRequest, AskMessage, AskResponse};
    use agent_tools::{Concurrency, Tool, ToolContext};

    /// 测试用审批策略：一律放行（工具测试不涉及审批交互）。
    struct AlwaysAllow;

    #[async_trait]
    impl agent_core::ApprovalPolicy for AlwaysAllow {
        fn decide(&self, _request: &ApprovalRequest<'_>) -> ApprovalDecision {
            ApprovalDecision::Allow
        }

        async fn prompt(&self, _ask: &AskMessage) -> Result<AskResponse, ToolError> {
            Err(ToolError::Execution("测试策略不应触发 prompt".into()))
        }
    }

    /// 全局放行策略实例（`&'static`，供测试上下文借用）。
    static ALWAYS_ALLOW: AlwaysAllow = AlwaysAllow;

    /// 构造测试执行上下文。
    fn ctx<'a>(workspace: &'a Workspace, cancel: &'a CancellationToken) -> ToolContext<'a> {
        ToolContext {
            workspace,
            approval: &ALWAYS_ALLOW,
            cancel,
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
            update_tx: None,
            conflicts: None,
            pending_rewrites: None,
            context: None,
        }
    }

    #[tokio::test]
    async fn tool_schema_shape() {
        let tool = BrowserTool::new();
        assert_eq!(tool.name(), "browser");
        assert!(tool.description().contains("navigate"));

        let schema = tool.schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"][0], "action");
        let actions: Vec<&str> = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("action 应为枚举数组")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for a in [
            "navigate",
            "evaluate",
            "screenshot",
            "click",
            "text",
            "scroll",
            "close",
        ] {
            assert!(actions.contains(&a), "schema 应包含动作 {a}");
        }

        assert_eq!(tool.capability(), CapabilityTier::Execute);
        assert_eq!(tool.concurrency(), Concurrency::Exclusive);
        assert!(tool.interruptible(), "长时工具应可中断");
    }

    #[tokio::test]
    async fn close_without_browser_is_noop() {
        let ws = Workspace::current_dir();
        let cancel = CancellationToken::new();
        let tool = BrowserTool::new();
        let out = tool
            .execute(json!({"action": "close"}), &ctx(&ws, &cancel))
            .await
            .unwrap();
        assert_eq!(out, ToolResult::text("browser: 无运行中的浏览器实例"));
    }

    #[tokio::test]
    async fn missing_action_rejected() {
        let ws = Workspace::current_dir();
        let cancel = CancellationToken::new();
        let tool = BrowserTool::new();
        let err = tool
            .execute(json!({}), &ctx(&ws, &cancel))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "实际 {err:?}");
    }

    #[tokio::test]
    async fn unknown_action_rejected_without_launch() {
        let ws = Workspace::current_dir();
        let cancel = CancellationToken::new();
        let tool = BrowserTool::new();
        let err = tool
            .execute(json!({"action": "open"}), &ctx(&ws, &cancel))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "实际 {err:?}");
    }

    #[tokio::test]
    async fn navigate_without_url_rejected_without_launch() {
        let ws = Workspace::current_dir();
        let cancel = CancellationToken::new();
        let tool = BrowserTool::new();
        let err = tool
            .execute(json!({"action": "navigate"}), &ctx(&ws, &cancel))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "实际 {err:?}");
    }

    #[tokio::test]
    async fn cancelled_ctx_rejected() {
        let ws = Workspace::current_dir();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let tool = BrowserTool::new();
        let err = tool
            .execute(json!({"action": "close"}), &ctx(&ws, &cancel))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)), "实际 {err:?}");
    }

    /// 真实 chromium 集成测试：仅在 `BROWSER_TEST=1` 且环境具备 chromium 时运行。
    #[tokio::test]
    #[ignore = "需要真实 chromium（BROWSER_TEST=1 才运行）"]
    async fn real_browser_navigate_and_evaluate() {
        if std::env::var("BROWSER_TEST").as_deref() != Ok("1") {
            return;
        }
        let ws = Workspace::current_dir();
        let cancel = CancellationToken::new();
        let tool = BrowserTool::new();
        let out = tool
            .execute(
                json!({"action": "navigate", "url": "about:blank"}),
                &ctx(&ws, &cancel),
            )
            .await
            .expect("navigate 应成功");
        assert!(
            out.to_llm_text().contains("已导航到 about:blank"),
            "实际 {}",
            out.to_llm_text()
        );

        let out = tool
            .execute(
                json!({"action": "evaluate", "js": "location.href"}),
                &ctx(&ws, &cancel),
            )
            .await
            .expect("evaluate 应成功");
        assert!(
            out.to_llm_text().contains("about:blank"),
            "实际 {}",
            out.to_llm_text()
        );

        // 截图：PNG 落盘到 <workspace>/.gyre/artifacts/ 并返回路径
        let out = tool
            .execute(
                json!({"action": "screenshot", "full_page": true}),
                &ctx(&ws, &cancel),
            )
            .await
            .expect("screenshot 应成功");
        let text = out.to_llm_text();
        assert!(text.contains("截图已保存"), "实际 {text}");
        let path = text
            .strip_prefix("截图已保存: ")
            .and_then(|t| t.split('（').next())
            .expect("应返回落盘路径");
        let bytes = tokio::fs::read(path).await.expect("截图文件应可读");
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "应为 PNG 魔数"
        );

        let out = tool
            .execute(json!({"action": "close"}), &ctx(&ws, &cancel))
            .await
            .expect("close 应成功");
        assert!(
            out.to_llm_text().contains("已关闭"),
            "实际 {}",
            out.to_llm_text()
        );
    }
}
