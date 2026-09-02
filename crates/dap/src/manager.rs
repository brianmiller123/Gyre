//! `debug` 工具、单会话管理器与断点注册表。
//!
//! [`DebugTool`] 实现 [`agent_tools::Tool`]（name=`debug`、Execute 级、
//! [`Concurrency::Exclusive`]、interruptible），按 `action` 分发 14 个动作：
//! `launch`/`attach`/`continue`/`pause`/`next`/`step_in`/`step_out`/`threads`/
//! `stack_trace`/`scopes`/`variables`/`evaluate`/`set_breakpoint`/`remove_breakpoint`。
//!
//! v1 单会话：[`DapManager`] 内部 `Mutex<Option<DapSession>>`，launch/attach 后自动记忆，
//! 后续动作无需再传会话；`session` 句柄参数被接受但忽略（无 id 用默认）。

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use agent_tools::{Concurrency, Tool, ToolContext};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::probe::probe_executable;
use crate::session::DapSession;
use crate::{AdapterSpec, DapError, DapSettings};

/// 单调试会话句柄：内部 `Mutex<Option<DapSession>>`，auto-launch 记忆
/// （launch/attach 后所有动作自动作用于该会话）。
pub struct DapManager {
    inner: Mutex<Option<DapSession>>,
}

impl DapManager {
    /// 空管理器（无会话）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// 当前会话（无则 [`DapError::NoSession`]）。
    ///
    /// # Errors
    /// 尚未 launch/attach 时返回 [`DapError::NoSession`]。
    pub async fn session(&self) -> Result<DapSession, DapError> {
        self.inner
            .lock()
            .await
            .as_ref()
            .cloned()
            .ok_or(DapError::NoSession)
    }

    /// 替换当前会话（launch/attach 后调用）。
    pub async fn set_session(&self, session: Option<DapSession>) {
        *self.inner.lock().await = session;
    }
}

impl Default for DapManager {
    fn default() -> Self {
        Self::new()
    }
}

/// `debug` 工具：DAP 调试器客户端（14 动作）。
///
/// - `launch` / `attach`：创建会话（`adapter` 缺省 auto：按 PATH 探测
///   lldb-dap → dlv → debugpy；未找到报「未找到调试适配器 …（尝试: …）」）；
/// - `continue` / `pause` / `next` / `step_in` / `step_out`：执行控制；
/// - `threads` / `stack_trace` / `scopes` / `variables` / `evaluate`：状态读取与求值
///   （返回 JSON 文本）；
/// - `set_breakpoint` / `remove_breakpoint`：断点管理（source+line → 断点 id）。
///
/// v1 单会话：launch/attach 后所有动作作用于该会话；暂停/断点事件不阻塞工具返回，
/// 可经后续动作（`stack_trace` / `threads` 等）读取。
pub struct DebugTool {
    settings: DapSettings,
    manager: DapManager,
    breakpoints: Arc<Mutex<Breakpoints>>,
}

impl DebugTool {
    /// 以给定适配器配置构造工具。
    #[must_use]
    pub fn new(settings: DapSettings) -> Self {
        Self {
            settings,
            manager: DapManager::new(),
            breakpoints: Arc::new(Mutex::new(Breakpoints::default())),
        }
    }

    /// 会话管理器句柄（供外部读取会话状态）。
    #[must_use]
    pub const fn manager(&self) -> &DapManager {
        &self.manager
    }

    /// 当前会话（无则 [`DapError::NoSession`]）。
    async fn session(&self) -> Result<DapSession, DapError> {
        self.manager.session().await
    }

    /// 解析适配器：显式名 → 该适配器（PATH 探测 + `-m` 模块检查）；auto → 逐个探测首个可用。
    async fn resolve_adapter(&self, name: Option<&str>) -> Result<AdapterSpec, DapError> {
        let path_env = std::env::var("PATH").unwrap_or_default();
        let candidates: Vec<&AdapterSpec> = match name {
            Some(n) => {
                let spec = self
                    .settings
                    .adapters
                    .iter()
                    .find(|a| a.name == n)
                    .ok_or_else(|| adapter_missing(n, &self.settings.adapters))?;
                vec![spec]
            }
            None => self.settings.adapters.iter().collect(),
        };
        let err_name = name.unwrap_or("auto");
        for spec in candidates {
            let Some(exe) = probe_executable(&spec.command, &path_env) else {
                continue;
            };
            if module_check_ok(spec, &exe).await {
                return Ok(spec.clone());
            }
        }
        Err(adapter_missing(err_name, &self.settings.adapters))
    }

    /// launch/attach：解析适配器 → spawn 会话（握手）→ 记忆到 manager。
    async fn launch_or_attach(
        &self,
        action: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let spec = self
            .resolve_adapter(input.get("adapter").and_then(Value::as_str))
            .await?;
        // launch 参数透传：剔除工具级键，其余原样作为 DAP launch/attach arguments
        let mut launch_args = input.clone();
        if let Value::Object(map) = &mut launch_args {
            map.remove("action");
            map.remove("adapter");
            map.remove("session");
        }
        // 部分适配器（debugpy/dlv）要求 arguments.request；缺省补上，用户显式值优先
        if let Value::Object(map) = &mut launch_args {
            map.entry("request").or_insert(Value::String(action.into()));
        }
        let spawn = DapSession::spawn(&spec, launch_args);
        let session = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => {
                return Err(ToolError::Execution("调试会话启动被取消".into()));
            }
            result = spawn => result?,
        };
        self.manager.set_session(Some(session)).await;
        Ok(ToolResult::text(format!(
            "调试会话已启动（适配器 {}，动作 {action}）",
            spec.name
        )))
    }

    /// continue：无需线程 id（`thread_id` 可选透传）。
    async fn simple_request(
        &self,
        action: &str,
        dap_command: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let session = self.session().await?;
        let resp = dap_request(&session, ctx, dap_command, dap_args(input, &[])).await?;
        let continued_all = resp
            .get("allThreadsContinued")
            .and_then(Value::as_bool)
            .map(|b| format!("，allThreadsContinued: {b}"))
            .unwrap_or_default();
        Ok(ToolResult::text(format!("{action} 成功{continued_all}")))
    }

    /// `pause`/`next`/`step_in`/`step_out`：需线程 id（显式 `thread_id` 或 `threads` 首线程）。
    async fn thread_action(
        &self,
        action: &str,
        dap_command: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let session = self.session().await?;
        let tid = self.require_thread_id(input, ctx).await?;
        let args = dap_args(input, &[("threadId", tid)]);
        dap_request(&session, ctx, dap_command, args).await?;
        Ok(ToolResult::text(format!("{action} 成功")))
    }

    /// `stack_trace`：需线程 id，返回结构化 JSON。
    async fn stack_trace_action(
        &self,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let session = self.session().await?;
        let tid = self.require_thread_id(input, ctx).await?;
        let args = dap_args(input, &[("threadId", tid)]);
        let resp = dap_request(&session, ctx, "stackTrace", args).await?;
        Ok(ToolResult::text(compact_json(&resp)))
    }

    /// scopes：需 `frame_id`（来自 `stack_trace`），返回结构化 JSON。
    async fn scopes_action(
        &self,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let session = self.session().await?;
        let frame_id = input.get("frame_id").cloned().ok_or_else(|| {
            ToolError::InvalidArgs("scopes 需要 'frame_id'（来自 stack_trace 的 frame id）".into())
        })?;
        let resp = dap_request(&session, ctx, "scopes", json!({ "frameId": frame_id })).await?;
        Ok(ToolResult::text(compact_json(&resp)))
    }

    /// variables：需 `variables_reference`（来自 `scopes`/`variables`），返回结构化 JSON。
    async fn variables_action(
        &self,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let session = self.session().await?;
        let reference = input.get("variables_reference").cloned().ok_or_else(|| {
            ToolError::InvalidArgs(
                "variables 需要 'variables_reference'（来自 scopes/variables 的 variablesReference）"
                    .into(),
            )
        })?;
        let resp = dap_request(
            &session,
            ctx,
            "variables",
            json!({ "variablesReference": reference }),
        )
        .await?;
        Ok(ToolResult::text(compact_json(&resp)))
    }

    /// evaluate：需 expression，返回结构化 JSON（result / variablesReference）。
    async fn evaluate_action(
        &self,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let session = self.session().await?;
        let expression = input
            .get("expression")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::InvalidArgs("evaluate 需要 'expression'（要求值的表达式）".into())
            })?
            .to_string();
        let args = dap_args(input, &[("expression", Value::String(expression))]);
        let resp = dap_request(&session, ctx, "evaluate", args).await?;
        Ok(ToolResult::text(compact_json(&resp)))
    }

    /// `set_breakpoint`：登记（`source`+`line` → id）并整源下发（DAP 整源替换语义）。
    async fn set_breakpoint(
        &self,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let session = self.session().await?; // 先确认有会话
        let source = input
            .get("source")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::InvalidArgs("set_breakpoint 需要 'source'（源文件路径）".into())
            })?
            .to_string();
        let line = input.get("line").and_then(Value::as_u64).ok_or_else(|| {
            ToolError::InvalidArgs("set_breakpoint 需要 'line'（1-based 行号）".into())
        })?;
        {
            let mut bps = self.breakpoints.lock().await;
            bps.add(&source, line);
        }
        self.sync_source_breakpoints(&session, &source, ctx).await?;
        let record = self
            .breakpoints
            .lock()
            .await
            .get(&source, line)
            .ok_or_else(|| ToolError::Execution("断点登记失败（内部状态不一致）".into()))?;
        Ok(ToolResult::text(compact_json(&json!({
            "breakpoint_id": record.id,
            "source": source,
            "line": line,
            "verified": record.verified,
            "dap_id": record.dap_id,
        }))))
    }

    /// `remove_breakpoint`：按 id 移除并整源重发剩余断点。
    async fn remove_breakpoint(
        &self,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let session = self.session().await?;
        let id = input
            .get("breakpoint_id")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ToolError::InvalidArgs(
                    "remove_breakpoint 需要 'breakpoint_id'（set_breakpoint 返回）".into(),
                )
            })?;
        let (source, remaining) = {
            let mut bps = self.breakpoints.lock().await;
            bps.remove(id)
                .ok_or_else(|| ToolError::InvalidArgs(format!("断点 {id} 不存在")))?
        };
        self.sync_source_breakpoints(&session, &source, ctx).await?;
        Ok(ToolResult::text(format!(
            "断点 {id}（{source}）已移除，剩余行号: {remaining:?}"
        )))
    }

    /// 向适配器整源下发断点（`setBreakpoints` 为整源替换语义），并回写确认状态。
    async fn sync_source_breakpoints(
        &self,
        session: &DapSession,
        source: &str,
        ctx: &ToolContext<'_>,
    ) -> Result<(), ToolError> {
        let lines = self.breakpoints.lock().await.lines_of(source);
        let args = json!({
            "source": { "path": source },
            "breakpoints": lines.iter().map(|l| json!({ "line": l })).collect::<Vec<_>>(),
        });
        let resp = dap_request(session, ctx, "setBreakpoints", args).await?;
        let dap_bps: Vec<Value> = resp
            .get("breakpoints")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.breakpoints
            .lock()
            .await
            .apply_response(source, &dap_bps);
        Ok(())
    }

    /// 线程 id：显式 `thread_id` 优先，否则取 `threads` 首个线程的 id。
    async fn require_thread_id(
        &self,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Result<Value, ToolError> {
        if let Some(v) = input.get("thread_id") {
            return Ok(v.clone());
        }
        let session = self.session().await?;
        let resp = dap_request(&session, ctx, "threads", json!({})).await?;
        resp.get("threads")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|t| t.get("id"))
            .cloned()
            .ok_or_else(|| {
                ToolError::Execution("无法确定线程 id：threads 响应为空，请显式传 thread_id".into())
            })
    }
}

impl Default for DebugTool {
    fn default() -> Self {
        Self::new(DapSettings::default())
    }
}

#[async_trait::async_trait]
impl Tool for DebugTool {
    fn name(&self) -> &'static str {
        "debug"
    }

    fn description(&self) -> &'static str {
        "Debug Adapter Protocol client: launch/attach a program, continue/pause/step, \
         list threads, read stack traces/scopes/variables, evaluate expressions, and \
         manage breakpoints. Supports lldb-dap (C/C++/Rust), dlv dap (Go), debugpy (Python)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "launch", "attach", "continue", "pause", "next", "step_in",
                        "step_out", "threads", "stack_trace", "scopes", "variables",
                        "evaluate", "set_breakpoint", "remove_breakpoint"
                    ],
                    "description": "The DAP operation to perform."
                },
                "adapter": {
                    "type": "string",
                    "description": "Adapter name: lldb-dap / dlv / debugpy. Default: auto (first found on PATH)."
                },
                "session": {
                    "type": "string",
                    "description": "Session handle id (v1: single session, ignored)."
                },
                "source": {
                    "type": "string",
                    "description": "Source file path (set_breakpoint)."
                },
                "line": {
                    "type": "integer",
                    "description": "1-based line number (set_breakpoint)."
                },
                "thread_id": {
                    "type": "integer",
                    "description": "Thread id (continue/pause/next/step_in/step_out/stack_trace). Default: first thread."
                },
                "frame_id": {
                    "type": "integer",
                    "description": "Stack frame id from stack_trace (scopes/evaluate)."
                },
                "variables_reference": {
                    "type": "integer",
                    "description": "Variables reference from scopes/variables (variables)."
                },
                "expression": {
                    "type": "string",
                    "description": "Expression to evaluate (evaluate)."
                },
                "breakpoint_id": {
                    "type": "integer",
                    "description": "Breakpoint id returned by set_breakpoint (remove_breakpoint)."
                }
            },
            "additionalProperties": true
        })
    }

    fn capability(&self) -> CapabilityTier {
        CapabilityTier::Execute
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    fn interruptible(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 'action' 参数".into()))?;
        // v1 单会话：接受 session 句柄参数但忽略（无 id 用默认会话）
        if let Some(sid) = input.get("session").and_then(Value::as_str) {
            if sid != "default" {
                tracing::debug!(session = sid, "debug 工具 v1 单会话，忽略会话句柄");
            }
        }
        match action {
            "launch" | "attach" => self.launch_or_attach(action, &input, ctx).await,
            "continue" => {
                self.simple_request("continue", "continue", &input, ctx)
                    .await
            }
            "pause" => self.thread_action("pause", "pause", &input, ctx).await,
            "next" => self.thread_action("next", "next", &input, ctx).await,
            "step_in" => self.thread_action("step_in", "stepIn", &input, ctx).await,
            "step_out" => self.thread_action("step_out", "stepOut", &input, ctx).await,
            "threads" => self.json_request("threads", json!({}), ctx).await,
            "stack_trace" => self.stack_trace_action(&input, ctx).await,
            "scopes" => self.scopes_action(&input, ctx).await,
            "variables" => self.variables_action(&input, ctx).await,
            "evaluate" => self.evaluate_action(&input, ctx).await,
            "set_breakpoint" => self.set_breakpoint(&input, ctx).await,
            "remove_breakpoint" => self.remove_breakpoint(&input, ctx).await,
            other => Err(ToolError::InvalidArgs(format!("未知 action: {other}"))),
        }
    }
}

impl DebugTool {
    /// 通用「发请求 → JSON 结果」动作（threads 等）。
    async fn json_request(
        &self,
        command: &str,
        args: Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let session = self.session().await?;
        let resp = dap_request(&session, ctx, command, args).await?;
        Ok(ToolResult::text(compact_json(&resp)))
    }
}

/// 断点注册表：`source → 断点列表`；每条断点有本工具自增 id 与适配器返回的 DAP id。
///
/// DAP `setBreakpoints` 为「整源替换」语义：设置/移除后按源重发全部剩余断点。
#[derive(Debug, Default)]
pub struct Breakpoints {
    next_id: u64,
    by_source: HashMap<String, Vec<BreakpointRecord>>,
}

/// 单个断点记录。
#[derive(Debug, Clone)]
pub struct BreakpointRecord {
    /// 本工具断点 id（`remove_breakpoint` 用）。
    pub id: u64,
    /// 源文件路径。
    pub source: String,
    /// 1-based 行号。
    pub line: u64,
    /// 适配器返回的 DAP breakpoint id（部分适配器不返回）。
    pub dap_id: Option<i64>,
    /// 适配器是否确认命中。
    pub verified: bool,
}

impl Breakpoints {
    /// 登记断点（同 source+line 去重，返回已有 id）。
    pub fn add(&mut self, source: &str, line: u64) -> u64 {
        let records = self.by_source.entry(source.to_string()).or_default();
        if let Some(existing) = records.iter().find(|r| r.line == line) {
            return existing.id;
        }
        self.next_id += 1;
        records.push(BreakpointRecord {
            id: self.next_id,
            source: source.to_string(),
            line,
            dap_id: None,
            verified: false,
        });
        self.next_id
    }

    /// 移除断点，返回 `(所属 source, 该 source 剩余行号)`；无此 id 返回 `None`。
    pub fn remove(&mut self, id: u64) -> Option<(String, Vec<u64>)> {
        for (source, records) in &mut self.by_source {
            if let Some(pos) = records.iter().position(|r| r.id == id) {
                records.remove(pos);
                let remaining = records.iter().map(|r| r.line).collect();
                return Some((source.clone(), remaining));
            }
        }
        None
    }

    /// 某 source 的全部断点行号（升序）。
    #[must_use]
    pub fn lines_of(&self, source: &str) -> Vec<u64> {
        let Some(records) = self.by_source.get(source) else {
            return Vec::new();
        };
        let mut lines: Vec<u64> = records.iter().map(|r| r.line).collect();
        lines.sort_unstable();
        lines
    }

    /// 取单条记录（source+line）。
    #[must_use]
    pub fn get(&self, source: &str, line: u64) -> Option<BreakpointRecord> {
        self.by_source
            .get(source)
            .and_then(|rs| rs.iter().find(|r| r.line == line))
            .cloned()
    }

    /// 用 `setBreakpoints` 响应更新确认状态（按行号匹配）。
    pub fn apply_response(&mut self, source: &str, dap_bps: &[Value]) {
        let Some(records) = self.by_source.get_mut(source) else {
            return;
        };
        for record in records.iter_mut() {
            for bp in dap_bps {
                let line = bp.get("line").and_then(Value::as_u64);
                if line.is_some_and(|l| l == record.line) {
                    record.verified = bp.get("verified").and_then(Value::as_bool).unwrap_or(false);
                    record.dap_id = bp.get("id").and_then(Value::as_i64);
                }
            }
        }
    }
}

/// 发送 DAP 请求并响应取消（interruptible：steering 中途打断时尽快让出）。
async fn dap_request(
    session: &DapSession,
    ctx: &ToolContext<'_>,
    command: &str,
    args: Value,
) -> Result<Value, ToolError> {
    let fut = session.request(command, args);
    tokio::select! {
        biased;
        () = ctx.cancel.cancelled() => Err(ToolError::Execution("调试操作被取消".into())),
        result = fut => result.map_err(ToolError::from),
    }
}

/// 工具参数 → DAP 请求参数：剔除工具级键，`thread_id`→`threadId`、
/// `frame_id`→`frameId`、`variables_reference`→`variablesReference`，其余键透传。
fn dap_args(input: &Value, overrides: &[(&str, Value)]) -> Value {
    let mut map = serde_json::Map::new();
    if let Value::Object(obj) = input {
        for (key, value) in obj {
            match key.as_str() {
                "action" | "adapter" | "session" => {}
                "thread_id" => {
                    map.insert("threadId".into(), value.clone());
                }
                "frame_id" => {
                    map.insert("frameId".into(), value.clone());
                }
                "variables_reference" => {
                    map.insert("variablesReference".into(), value.clone());
                }
                other => {
                    map.insert(other.into(), value.clone());
                }
            }
        }
    }
    for (key, value) in overrides {
        map.insert((*key).into(), value.clone());
    }
    Value::Object(map)
}

/// 构造「未找到调试适配器」错误（契约消息格式：`未找到调试适配器 {name}（尝试: …）`）。
fn adapter_missing(name: &str, adapters: &[AdapterSpec]) -> DapError {
    let tried = adapters
        .iter()
        .map(|a| {
            if a.args.is_empty() {
                a.command.clone()
            } else {
                format!("{} {}", a.command, a.args.join(" "))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    DapError::AdapterNotFound(format!("{name}（尝试: {tried}）"))
}

/// `python -m <mod>` 类适配器：额外验证模块可导入，避免 spawn 后握手超时。
async fn module_check_ok(spec: &AdapterSpec, exe: &Path) -> bool {
    let Some(m_pos) = spec.args.iter().position(|a| a == "-m") else {
        return true;
    };
    let Some(module) = spec.args.get(m_pos + 1) else {
        return true;
    };
    let check = tokio::process::Command::new(exe)
        .args(["-c", &format!("import {module}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await;
    matches!(check, Ok(out) if out.status.success())
}

/// `DapError` → 工具执行错误（中文 message 透传）。
impl From<DapError> for agent_core::ToolError {
    fn from(e: DapError) -> Self {
        Self::Execution(e.to_string())
    }
}

/// 紧凑 JSON 文本（工具返回结构化结果）。
fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoints_add_remove_lines() {
        let mut bps = Breakpoints::default();
        let id = bps.add("/tmp/a.rs", 10);
        let dup = bps.add("/tmp/a.rs", 10);
        assert_eq!(id, dup, "同 source+line 应去重");
        let id2 = bps.add("/tmp/a.rs", 20);
        assert_ne!(id, id2);
        bps.add("/tmp/b.rs", 1);
        assert_eq!(bps.lines_of("/tmp/a.rs"), vec![10, 20]);
        assert_eq!(bps.lines_of("/tmp/unknown.rs"), Vec::<u64>::new());
        let (source, remaining) = bps.remove(id).expect("移除应成功");
        assert_eq!(source, "/tmp/a.rs");
        assert_eq!(remaining, vec![20]);
        assert!(bps.remove(999).is_none());
    }

    #[test]
    fn breakpoints_apply_response() {
        let mut bps = Breakpoints::default();
        let id = bps.add("/tmp/a.rs", 10);
        bps.apply_response(
            "/tmp/a.rs",
            &[json!({ "id": 7, "line": 10, "verified": true })],
        );
        let record = bps.get("/tmp/a.rs", 10).expect("记录应存在");
        assert_eq!(record.id, id);
        assert_eq!(record.dap_id, Some(7));
        assert!(record.verified);
    }

    #[test]
    fn dap_args_maps_keys() {
        let input = json!({
            "action": "step_in",
            "adapter": "dlv",
            "thread_id": 3,
            "frame_id": 9,
            "granularity": "statement"
        });
        let args = dap_args(&input, &[("threadId", json!(3))]);
        assert_eq!(args["threadId"], 3);
        assert_eq!(args["frameId"], 9);
        assert_eq!(args["granularity"], "statement");
        assert!(args.get("action").is_none());
        assert!(args.get("adapter").is_none());
    }
}
