//! `todo` 任务清单工具（移植 oh-my-pi `todo.ts` 语义）。
//!
//! - **单活跃不变量**：任何时刻至多一个 `in_progress`；`start` 新任务时旧的
//!   `in_progress` 自动闭环为 `completed`（omp todo.ts 同语义）。
//! - **阶段机**：`pending` / `in_progress` / `completed` / `abandoned` / `blocked`，
//!   非法迁移（如 complete 一个 abandoned 项）返回可恢复错误。
//! - **持久化**：状态原子落盘 `<workspace>/.gyre/todo.json`，REPL `/todo` 与工具
//!   共享同一 [`TodoState`] 实例（装配层构造一次，跨 Agent 重建存活）。
//! - 自管理状态文件，不触工作区代码——审批取 read 级（与 omp / retain 一致）。

use std::sync::{Arc, Mutex};

use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Concurrency, Tool, ToolContext};

/// 任务阶段（移植 omp todo phases）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoPhase {
    /// 待办。
    Pending,
    /// 进行中（单活跃）。
    InProgress,
    /// 已完成。
    Completed,
    /// 已放弃（不做）。
    Abandoned,
    /// 被阻塞（等待外部输入）。
    Blocked,
}

impl TodoPhase {
    /// 渲染标记（Markdown 列表用）。
    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[~]",
            Self::Completed => "[x]",
            Self::Abandoned => "[-]",
            Self::Blocked => "[!]",
        }
    }

    /// 机器可读名（serde 之外的显示用）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Abandoned => "abandoned",
            Self::Blocked => "blocked",
        }
    }
}

/// 单条任务。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// 稳定 id（`t1`、`t2`…全量替换时重排）。
    pub id: String,
    /// 任务内容（短语，非长文）。
    pub content: String,
    /// 阶段。
    pub phase: TodoPhase,
    /// 阻塞原因（仅 `blocked` 有意义）。
    pub blocked_reason: Option<String>,
}

/// 清单全量状态（`todo.json` 的序列化形态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoList {
    /// 全部条目（含历史 completed/abandoned，供回顾）。
    pub items: Vec<TodoItem>,
    /// 下一个数字序号（从 1 起）。
    pub next_id: u32,
}

impl Default for TodoList {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
        }
    }
}

impl TodoList {
    /// 规整 `next_id`：至少为「现有条目数字 id 最大值 + 1」且 ≥ 1（防旧文件/手改文件 id 回退碰撞）。
    fn normalize(&mut self) {
        let max_num = self
            .items
            .iter()
            .filter_map(|i| i.id.trim_start_matches('t').parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        self.next_id = self.next_id.max(max_num + 1).max(1);
    }
}

/// `排序权重：in_progress` → pending → blocked → completed → abandoned。
const fn phase_rank(p: TodoPhase) -> u8 {
    match p {
        TodoPhase::InProgress => 0,
        TodoPhase::Pending => 1,
        TodoPhase::Blocked => 2,
        TodoPhase::Completed => 3,
        TodoPhase::Abandoned => 4,
    }
}

impl TodoList {
    /// 渲染为 Markdown 清单（活跃在前）。
    #[must_use]
    pub fn render_markdown(&self) -> String {
        if self.items.is_empty() {
            return "（清单为空——用 todo start/write 规划任务）".to_string();
        }
        let mut sorted: Vec<&TodoItem> = self.items.iter().collect();
        sorted.sort_by_key(|i| phase_rank(i.phase));
        let mut out = String::from("# Todo\n\n");
        for item in sorted {
            let reason = item
                .blocked_reason
                .as_deref()
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map_or_else(String::new, |r| format!(" — 阻塞: {r}"));
            let _ = writeln!(
                out,
                "- {} {}: {}{}",
                item.phase.marker(),
                item.id,
                item.content,
                reason
            );
        }
        out
    }

    /// 按 id 查找（接受 `t3` 或 `3`）。
    fn find(&mut self, id: &str) -> Option<&mut TodoItem> {
        let norm = id.trim().trim_start_matches('t');
        self.items
            .iter_mut()
            .find(|i| i.id == id.trim() || i.id.trim_start_matches('t') == norm)
    }

    /// 强制单活跃不变量：多余 `in_progress` 降级为 `pending`（保留首个）。
    fn enforce_single_active(&mut self) {
        let mut seen = false;
        for item in &mut self.items {
            if item.phase == TodoPhase::InProgress {
                if seen {
                    item.phase = TodoPhase::Pending;
                } else {
                    seen = true;
                }
            }
        }
    }
}

/// 跨工具 / REPL 共享的清单状态（持锁改、改后落盘）。
pub struct TodoState {
    inner: Mutex<TodoList>,
    path: Option<std::path::PathBuf>,
}

/// H28：把清单状态暴露给引擎的待办循环端口（eager prelude / 完成提醒）。
///
/// 引擎只读快照；工具仍是唯一写入者（单活跃不变量、落盘、id 重排全在工具层），
/// 因此不存在「引擎与工具各持一份待办」的分叉。
impl agent_core::TodoLoopSource for TodoState {
    fn snapshot(&self) -> agent_core::TodoLoopSnapshot {
        let list = self.snapshot();
        let entries: Vec<agent_core::TodoLoopEntry> = list
            .items
            .iter()
            .map(|i| agent_core::TodoLoopEntry {
                content: i.content.clone(),
                phase: i.phase.as_str().to_string(),
                blocker: i.blocked_reason.clone(),
            })
            .collect();
        let incomplete = entries
            .iter()
            .filter(|e| !agent_core::todo_loop::is_done(&e.phase))
            .count();
        agent_core::TodoLoopSnapshot {
            entries,
            incomplete,
        }
    }
}

impl TodoState {
    /// 从持久化文件加载（缺失/损坏 → 空清单，不阻断启动）。
    ///
    /// `path` 通常是 `<workspace>/.gyre/todo.json`。
    #[must_use]
    pub fn load(path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        let mut list: TodoList = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        list.normalize();
        Self {
            inner: Mutex::new(list),
            path: Some(path),
        }
    }

    /// 纯内存状态（测试用；不落盘）。
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            inner: Mutex::new(TodoList::default()),
            path: None,
        }
    }

    /// 共享句柄。
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// 渲染当前清单（REPL `/todo` 用）。
    #[must_use]
    pub fn render_markdown(&self) -> String {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .render_markdown()
    }

    /// 全量替换清单（RPC `set_todos` 用；H7）。
    ///
    /// id 一律重排为 `t1..tn`（与工具内全量替换语义一致），随后
    /// [`TodoList::normalize`]（单活跃不变量）并落盘；返回落盘后的快照。
    pub fn replace(&self, mut items: Vec<TodoItem>) -> TodoList {
        for (i, item) in items.iter_mut().enumerate() {
            item.id = format!("t{}", i + 1);
        }
        let mut list = TodoList { items, next_id: 1 };
        // 与 `todo` 工具 `write` 同规则：多 active 只保留首个，其余降级 pending。
        list.enforce_single_active();
        list.normalize();
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = list.clone();
        }
        self.persist(&list);
        list
    }

    /// 当前清单快照。
    #[must_use]
    pub fn snapshot(&self) -> TodoList {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 持久化（best-effort：失败仅影响跨会话恢复，不影响当次返回）。
    fn persist(&self, list: &TodoList) {
        if let Some(path) = &self.path {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(json) = serde_json::to_string_pretty(list) {
                let tmp = path.with_extension("json.tmp");
                if std::fs::write(&tmp, json).is_ok() {
                    let _ = std::fs::rename(&tmp, path);
                }
            }
        }
    }
}

/// `todo`：任务规划与进度跟踪（多步任务的自我管理清单）。
pub struct TodoTool {
    state: Arc<TodoState>,
}

impl TodoTool {
    /// 绑定共享状态（与 REPL `/todo` 同一实例）。
    #[must_use]
    pub const fn new(state: Arc<TodoState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }
    fn description(&self) -> &'static str {
        "任务清单：规划多步任务并跟踪进度。单活跃不变量——任何时刻至多一个 in_progress，\
start 新任务自动闭环旧的。多步任务（≥3 步）开工前先 write/start 建清单，\
每完成一步即 complete 并 start 下一步；被外因卡住用 block（写明原因）。"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["view", "write", "init", "append", "start", "update", "complete", "done", "abandon", "drop", "rm", "remove", "block", "unblock", "pending"],
                    "default": "view",
                    "description": "view 查看；write/init 全量替换；append 追加；start 新任务(自动闭环旧 in_progress)；update 改内容；complete/done 完成；abandon/drop 放弃；rm/remove 删除条目；block/unblock 阻塞管理；pending 重新排队（omp 别名已对齐，H32）"
                },
                "items": {
                    "type": "array",
                    "description": "write 专用：完整清单（替换全部），phase 可省略(默认 pending)，至多一个 in_progress",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string", "description": "任务内容（短语）" },
                            "phase": { "type": "string",
                                       "enum": ["pending", "in_progress", "completed", "abandoned", "blocked"] },
                            "blocked_reason": { "type": "string" }
                        },
                        "required": ["content"]
                    }
                },
                "content": { "type": "string", "description": "start/update：任务内容" },
                "id": { "type": "string", "description": "update/complete/abandon/block/unblock/pending：目标 id（如 t3）" },
                "reason": { "type": "string", "description": "block：阻塞原因" }
            }
        })
    }
    fn capability(&self) -> CapabilityTier {
        // 自管理状态文件（.gyre/todo.json），不触工作区——read 级审批（同 retain）。
        CapabilityTier::ReadOnly
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let op = input
            .get("op")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("view")
            .to_string();
        // H32：对齐 omp `TodoOperation` 的别名（init/done/drop/rm），并保留 Gyre 原生名。
        let op = match op.as_str() {
            "init" => "write".to_string(),
            "done" => "complete".to_string(),
            "drop" => "abandon".to_string(),
            "rm" => "remove".to_string(),
            other => other.to_string(),
        };
        let mut list = self
            .state
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let changed;

        match op.as_str() {
            "view" => {
                return Ok(ToolResult::text(list.render_markdown()));
            }
            "write" => {
                let items = input
                    .get("items")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| ToolError::InvalidArgs("write 需要 `items` 数组".into()))?;
                let mut next = TodoList {
                    items: Vec::with_capacity(items.len()),
                    next_id: 1,
                };
                for (i, raw) in items.iter().enumerate() {
                    let content = raw
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|c| !c.is_empty())
                        .ok_or_else(|| {
                            ToolError::InvalidArgs(format!("items[{i}] 缺少非空 content"))
                        })?;
                    let phase = raw
                        .get("phase")
                        .or_else(|| raw.get("status")) // omp `TodoItem.status` 别名（H32）
                        .and_then(serde_json::Value::as_str)
                        .map_or(Ok(TodoPhase::Pending), parse_phase)
                        .map_err(ToolError::InvalidArgs)?;
                    let blocked_reason = raw
                        .get("blocked_reason")
                        .or_else(|| raw.get("blocker")) // omp 字段名（H32）
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|r| !r.is_empty())
                        .map(str::to_string);
                    next.items.push(TodoItem {
                        id: format!("t{}", i + 1),
                        content: truncate_chars(content, 200),
                        phase,
                        blocked_reason,
                    });
                    next.next_id = (i + 2) as u32;
                }
                next.enforce_single_active();
                list = next;
                changed = true;
            }
            "append" => {
                // 追加：保留既有条目（含终态历史），新条目接续编号（对齐 omp `append`）。
                let items = input
                    .get("items")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| ToolError::InvalidArgs("append 需要 `items` 数组".into()))?;
                for (i, raw) in items.iter().enumerate() {
                    let content = raw
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|c| !c.is_empty())
                        .ok_or_else(|| {
                            ToolError::InvalidArgs(format!("items[{i}] 缺少非空 content"))
                        })?;
                    let phase = raw
                        .get("phase")
                        .or_else(|| raw.get("status"))
                        .and_then(serde_json::Value::as_str)
                        .map_or(Ok(TodoPhase::Pending), parse_phase)
                        .map_err(ToolError::InvalidArgs)?;
                    let id = format!("t{}", list.next_id);
                    list.next_id += 1;
                    list.items.push(TodoItem {
                        id,
                        content: truncate_chars(content, 200),
                        phase,
                        blocked_reason: raw
                            .get("blocked_reason")
                            .or_else(|| raw.get("blocker"))
                            .and_then(serde_json::Value::as_str)
                            .map(|r| truncate_chars(r, 200)),
                    });
                }
                list.enforce_single_active();
                changed = true;
            }
            "remove" => {
                // 删除条目（omp `rm`）：按 id 精确移除；删除后重排编号保持 t1..tn 连续。
                let id = required_str(&input, "id", "remove")?;
                let before = list.items.len();
                list.items.retain(|item| item.id != id);
                if list.items.len() == before {
                    return Err(ToolError::InvalidArgs(format!("找不到任务 {id}")));
                }
                for (i, item) in list.items.iter_mut().enumerate() {
                    item.id = format!("t{}", i + 1);
                }
                list.next_id = (list.items.len() + 1) as u32;
                changed = true;
            }
            "start" => {
                let content = required_str(&input, "content", "start")?;
                // 单活跃不变量：旧 in_progress 自动闭环（omp todo.ts 语义）。
                for item in &mut list.items {
                    if item.phase == TodoPhase::InProgress {
                        item.phase = TodoPhase::Completed;
                    }
                }
                let id = format!("t{}", list.next_id);
                list.next_id += 1;
                list.items.push(TodoItem {
                    id,
                    content: truncate_chars(content, 200),
                    phase: TodoPhase::InProgress,
                    blocked_reason: None,
                });
                changed = true;
            }
            "update" => {
                let id = required_str(&input, "id", "update")?;
                let content = required_str(&input, "content", "update")?;
                let item = list
                    .find(id)
                    .ok_or_else(|| ToolError::InvalidArgs(format!("找不到任务 {id}")))?;
                item.content = truncate_chars(content, 200);
                changed = true;
            }
            "complete" | "abandon" | "pending" | "unblock" => {
                let id = required_str(&input, "id", &op)?;
                let phase = match op.as_str() {
                    _ if op == "complete" => TodoPhase::Completed,
                    _ if op == "abandon" => TodoPhase::Abandoned,
                    _ if op == "pending" => TodoPhase::Pending,
                    _ => TodoPhase::Pending, // unblock
                };
                let item = list
                    .find(id)
                    .ok_or_else(|| ToolError::InvalidArgs(format!("找不到任务 {id}")))?;
                // 终态不可逆（completed/abandoned 不能再迁移）。
                if matches!(item.phase, TodoPhase::Completed | TodoPhase::Abandoned)
                    && item.phase != phase
                {
                    return Ok(ToolResult::Error {
                        recoverable: true,
                        message: format!(
                            "任务 {} 已是终态 {}，不可迁移",
                            item.id,
                            item.phase.as_str()
                        ),
                    });
                }
                item.phase = phase;
                item.blocked_reason = None;
                changed = true;
            }
            "block" => {
                let id = required_str(&input, "id", "block")?;
                let reason = input
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .unwrap_or("未说明原因");
                let item = list
                    .find(id)
                    .ok_or_else(|| ToolError::InvalidArgs(format!("找不到任务 {id}")))?;
                item.phase = TodoPhase::Blocked;
                item.blocked_reason = Some(truncate_chars(reason, 200));
                changed = true;
            }
            other => {
                return Err(ToolError::InvalidArgs(format!("未知 op：{other}")));
            }
        }

        if changed {
            let mut guard = self
                .state
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = list.clone();
            drop(guard);
            self.state.persist(&list);
        }
        Ok(ToolResult::text(list.render_markdown()))
    }
}

/// 解析阶段字符串。
fn parse_phase(s: &str) -> Result<TodoPhase, String> {
    match s {
        "pending" => Ok(TodoPhase::Pending),
        "in_progress" => Ok(TodoPhase::InProgress),
        "completed" => Ok(TodoPhase::Completed),
        "abandoned" => Ok(TodoPhase::Abandoned),
        "blocked" => Ok(TodoPhase::Blocked),
        other => Err(format!("未知 phase：{other}")),
    }
}

/// 取必填字符串参数。
fn required_str<'a>(
    input: &'a serde_json::Value,
    key: &str,
    op: &str,
) -> Result<&'a str, ToolError> {
    input
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ToolError::InvalidArgs(format!("{op} 需要非空 `{key}` 参数")))
}

/// 按字符数截断（清单条目是短语，非长文）。
fn truncate_chars(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

use std::fmt::Write as _;

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ApprovalDecision, ApprovalPolicy, ApprovalRequest, Workspace};
    use std::sync::Arc;

    struct NoopApproval;
    #[async_trait::async_trait]
    impl ApprovalPolicy for NoopApproval {
        fn decide(&self, _r: &ApprovalRequest<'_>) -> ApprovalDecision {
            ApprovalDecision::Allow
        }
        async fn prompt(
            &self,
            _ask: &agent_core::AskMessage,
        ) -> Result<agent_core::AskResponse, ToolError> {
            Ok(agent_core::AskResponse::Yes)
        }
    }

    fn ctx<'a>(
        ws: &'a Workspace,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> ToolContext<'a> {
        ToolContext {
            workspace: ws,
            approval: &NoopApproval,
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

    fn ws() -> Workspace {
        Workspace::new(std::env::temp_dir())
    }

    async fn run(tool: &TodoTool, input: serde_json::Value) -> ToolResult {
        let workspace = ws();
        let cancel = tokio_util::sync::CancellationToken::new();
        tool.execute(input, &ctx(&workspace, &cancel))
            .await
            .unwrap()
    }

    fn state() -> Arc<TodoState> {
        TodoState::in_memory().shared()
    }

    #[tokio::test]
    async fn start_enforces_single_active() {
        let tool = TodoTool::new(state());
        run(&tool, serde_json::json!({"op":"start","content":"任务A"})).await;
        let out = run(&tool, serde_json::json!({"op":"start","content":"任务B"})).await;
        let text = out.to_llm_text();
        // 旧 in_progress 自动闭环。
        assert!(text.contains("[x] t1: 任务A"));
        assert!(text.contains("[~] t2: 任务B"));
    }

    #[tokio::test]
    async fn write_replaces_and_demotes_extra_active() {
        let tool = TodoTool::new(state());
        let out = run(
            &tool,
            serde_json::json!({"op":"write","items":[
                {"content":"a","phase":"in_progress"},
                {"content":"b","phase":"in_progress"},
                {"content":"c"}
            ]}),
        )
        .await;
        let text = out.to_llm_text();
        // 多个 in_progress 只保留首个，其余降级 pending。
        assert_eq!(text.matches("[~]").count(), 1);
        assert!(text.contains("[ ] t3: c"));
    }

    #[tokio::test]
    async fn complete_then_terminal_immutable() {
        let tool = TodoTool::new(state());
        run(&tool, serde_json::json!({"op":"start","content":"X"})).await;
        let out = run(&tool, serde_json::json!({"op":"complete","id":"t1"})).await;
        assert!(out.to_llm_text().contains("[x] t1: X"));
        let out = run(&tool, serde_json::json!({"op":"pending","id":"t1"})).await;
        assert!(matches!(out, ToolResult::Error { .. }));
    }

    #[tokio::test]
    async fn block_records_reason() {
        let tool = TodoTool::new(state());
        run(&tool, serde_json::json!({"op":"start","content":"等审核"})).await;
        let out = run(
            &tool,
            serde_json::json!({"op":"block","id":"t1","reason":"等用户提供密钥"}),
        )
        .await;
        assert!(out.to_llm_text().contains("阻塞: 等用户提供密钥"));
        // unblock 清原因。
        let out = run(&tool, serde_json::json!({"op":"unblock","id":"t1"})).await;
        assert!(!out.to_llm_text().contains("阻塞:"));
    }

    #[tokio::test]
    async fn numeric_id_lookup() {
        let tool = TodoTool::new(state());
        run(&tool, serde_json::json!({"op":"start","content":"A"})).await;
        let out = run(&tool, serde_json::json!({"op":"complete","id":"1"})).await;
        assert!(out.to_llm_text().contains("[x] t1: A"));
    }

    #[tokio::test]
    async fn persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("todo.json");
        let state = TodoState::load(&path).shared();
        let tool = TodoTool::new(Arc::clone(&state));
        run(&tool, serde_json::json!({"op":"start","content":"持久化"})).await;
        // 重新加载同一文件。
        let state2 = TodoState::load(&path);
        assert!(
            state2
                .snapshot()
                .items
                .iter()
                .any(|i| i.content == "持久化")
        );
    }

    /// H7：RPC `set_todos` 用的全量替换（id 重排 + 单活跃规整 + 落盘）。
    #[test]
    fn replace_renumbers_normalizes_and_persists() {
        let dir = std::env::temp_dir().join(format!("todo-replace-{}", std::process::id()));
        let path = dir.join("todo.json");
        let state = TodoState::load(&path);
        let list = state.replace(vec![
            TodoItem {
                id: "zzz".into(),
                content: "a".into(),
                phase: TodoPhase::InProgress,
                blocked_reason: None,
            },
            TodoItem {
                id: "yyy".into(),
                content: "b".into(),
                phase: TodoPhase::InProgress,
                blocked_reason: None,
            },
        ]);
        // id 重排为 t1/t2；单活跃不变量：保留首个 in_progress，其余降级 pending
        // （与 `todo` 工具 `write` 的 `enforce_single_active` 同规则）。
        assert_eq!(list.items[0].id, "t1");
        assert_eq!(list.items[1].id, "t2");
        assert_eq!(list.items[0].phase, TodoPhase::InProgress);
        assert_eq!(list.items[1].phase, TodoPhase::Pending);
        assert_eq!(state.snapshot(), list);
        // 落盘可回读。
        let reloaded = TodoState::load(&path).snapshot();
        assert_eq!(reloaded.items.len(), 2);
        assert_eq!(reloaded.next_id, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H32：omp `TodoOperation` 别名（init/done/drop/rm）+ `append` / `remove` 新 op + `status`/`blocker` 字段名。
    #[tokio::test]
    async fn omp_todo_op_aliases_and_append_remove() {
        let tool = TodoTool::new(state());
        // omp `init`（= write）+ `status`/`blocker` 字段名。
        let out = run(
            &tool,
            serde_json::json!({"op":"init","items":[
                {"content":"a","status":"in_progress"},
                {"content":"b","status":"blocked","blocker":"等 CI"}
            ]}),
        )
        .await;
        let text = out.to_llm_text();
        assert!(text.contains("[~] t1: a"), "{text}");
        assert!(text.contains("[!] t2: b"), "{text}");

        // `append`：保留既有条目、编号接续。
        let out = run(
            &tool,
            serde_json::json!({"op":"append","items":[{"content":"c"}]}),
        )
        .await;
        let text = out.to_llm_text();
        assert!(text.contains("t3: c"), "{text}");
        assert!(text.contains("t1: a"), "append 不得清空既有条目: {text}");

        // omp `done`（= complete）。
        let out = run(&tool, serde_json::json!({"op":"done","id":"t1"})).await;
        assert!(
            out.to_llm_text().contains("[x] t1: a"),
            "{}",
            out.to_llm_text()
        );

        // omp `drop`（= abandon）。
        let out = run(&tool, serde_json::json!({"op":"drop","id":"t3"})).await;
        assert!(
            out.to_llm_text().contains("[-] t3: c"),
            "{}",
            out.to_llm_text()
        );

        // omp `rm`（= remove）：删除并重排编号。
        let out = run(&tool, serde_json::json!({"op":"rm","id":"t2"})).await;
        let text = out.to_llm_text();
        assert!(!text.contains("等 CI"), "被删条目不应残留: {text}");
        let list = tool.state.snapshot();
        assert_eq!(list.items.len(), 2, "{list:?}");
        assert_eq!(list.items[0].id, "t1");
        assert_eq!(list.items[1].id, "t2", "删除后编号应重排连续");
        // 删除不存在的 id → 明确错误。
        let ws2 = ws();
        let cancel = tokio_util::sync::CancellationToken::new();
        let err = tool
            .execute(
                serde_json::json!({"op":"rm","id":"t99"}),
                &ctx(&ws2, &cancel),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("t99"), "{err}");
    }
    #[tokio::test]
    async fn view_empty_and_missing_args() {
        let tool = TodoTool::new(state());
        let out = run(&tool, serde_json::json!({})).await;
        assert!(out.to_llm_text().contains("清单为空"));
        let err = tool
            .execute(
                serde_json::json!({"op":"start"}),
                &ctx(&ws(), &tokio_util::sync::CancellationToken::new()),
            )
            .await;
        assert!(err.is_err());
    }

    /// H28：待办端口快照——状态名/阻塞原因/未完成计数与清单一致（引擎提醒据此判定）。
    #[test]
    fn loop_source_snapshot_matches_list_state() {
        let state = TodoState::in_memory();
        state.replace(vec![
            TodoItem {
                id: String::new(),
                content: "写测试".into(),
                phase: TodoPhase::Pending,
                blocked_reason: None,
            },
            TodoItem {
                id: String::new(),
                content: "等上游".into(),
                phase: TodoPhase::Blocked,
                blocked_reason: Some("缺 token".into()),
            },
            TodoItem {
                id: String::new(),
                content: "读代码".into(),
                phase: TodoPhase::Completed,
                blocked_reason: None,
            },
        ]);
        let snap = agent_core::TodoLoopSource::snapshot(&state);
        // 未完成 = pending + blocked（completed/abandoned 不计）。
        assert_eq!(snap.incomplete, 2);
        assert!(snap.has_incomplete());
        assert!(!snap.is_empty());
        let rendered = snap.render_incomplete().unwrap();
        assert!(rendered.contains("- [pending] 写测试"), "{rendered}");
        assert!(
            rendered.contains("- [blocked] 等上游（阻塞：缺 token）"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("读代码"),
            "已完成条目不应出现在提醒里: {rendered}"
        );
        // 全部完成 → 无未完成（提醒静默）。
        state.replace(vec![TodoItem {
            id: String::new(),
            content: "读代码".into(),
            phase: TodoPhase::Completed,
            blocked_reason: None,
        }]);
        let snap = agent_core::TodoLoopSource::snapshot(&state);
        assert_eq!(snap.incomplete, 0);
        assert!(snap.render_incomplete().is_none());
    }
}
