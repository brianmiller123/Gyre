//! H29：goal loop——目标状态机、`goal` 工具与续跑提示。
//!
//! 移植 oh-my-pi `goals/runtime.ts` + `goals/state.ts` 的核心语义，落到 Gyre 的
//! 「一个 run = 一次用户 prompt」模型上：
//!
//! - **状态机**（[`GoalStatus`]）：`active` → `complete` / `dropped` / `paused` /
//!   `budget-limited`；`paused`/`budget-limited` 可 `resume` 回 `active`。
//! - **目标级预算**：目标可带 `token_budget`（缺省沿用会话 `[goals]` 预算）；
//!   记账口径与 [`GoalState::billed`] 一致（input + cache_write + output）。
//! - **续跑**：模型在目标仍 `active` 时停止 → 引擎在停止边界注入 continuation
//!   提示并继续下一轮（[`GoalState::note_continuation`] 设上限，防无限循环）。
//! - **完成即退出**：`goal({op:"complete"})` 是唯一把目标置 `complete` 的入口
//!   （提示词要求先核对当前仓库状态，再调用）。
//!
//! 与 `[goals]` 会话预算（P0-3）的关系：会话预算是**兜底**（无论目标是否存在都
//! 生效），目标预算是**目标自己的额度**；两者任一超限都触发 `budget-limited`。

use std::sync::Arc;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use agent_tools::{Tool, ToolContext};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// H29：可持久化的目标快照（`<cwd>/.gyre/goal.json`）。
///
/// 只存目标的**身份与状态**（objective/status/预算/续跑数/时间戳），不存用量记账
/// （`usage` 是进程内累计，重启后由会话自身的用量基线重建）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalSnapshot {
    /// 目标描述。
    pub objective: String,
    /// 状态（线协议名）。
    pub status: String,
    /// 目标级 token 预算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    /// 已续跑轮数。
    #[serde(default)]
    pub continuations: usize,
    /// 续跑上限。
    #[serde(default = "default_max_continuations")]
    pub max_continuations: usize,
    /// 最近更新时间（Unix 秒）。
    #[serde(default)]
    pub updated_at_unix: u64,
}

const fn default_max_continuations() -> usize {
    DEFAULT_MAX_CONTINUATIONS
}

/// 目标持久化文件路径（`<cwd>/.gyre/goal.json`；与 `todo.json` 同目录约定）。
#[must_use]
pub fn goal_file(cwd: &std::path::Path) -> std::path::PathBuf {
    cwd.join(".gyre").join("goal.json")
}

/// 原子写入目标快照（同目录临时文件 + rename；失败只影响跨会话恢复）。
///
/// # Errors
/// 目录创建/写入/rename 失败。
pub fn save_goal(cwd: &std::path::Path, state: &GoalState) -> std::io::Result<()> {
    let path = goal_file(cwd);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let Some(snapshot) = state.snapshot() else {
        // 无目标（drop 之后）→ 删除持久化文件，避免下次启动复活已放弃的目标。
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    };
    let body = serde_json::to_string_pretty(&snapshot).unwrap_or_else(|_| "{}".into());
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, body.as_bytes())?;
    match std::fs::rename(&tmp, &path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 读取持久化目标快照（缺失/损坏 → `None`，不阻断启动）。
#[must_use]
pub fn load_goal(cwd: &std::path::Path) -> Option<GoalSnapshot> {
    let text = std::fs::read_to_string(goal_file(cwd)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 默认续跑上限（目标未显式配置时）。
pub const DEFAULT_MAX_CONTINUATIONS: usize = 8;

/// 目标状态（移植 omp `GoalStatus`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStatus {
    /// 活跃：引擎会在停止边界续跑。
    Active,
    /// 已暂停（用户中断 / `/goal pause`）；`resume` 可恢复。
    Paused,
    /// 预算耗尽：只允许收尾，不再续跑。
    BudgetLimited,
    /// 已完成（仅 `goal({op:"complete"})` 可置位）。
    Complete,
    /// 已放弃（`goal({op:"drop"})` 或会话重置）。
    Dropped,
}

impl GoalStatus {
    /// 线协议名（工具输出与 `/goal` 显示共用）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::BudgetLimited => "budget-limited",
            Self::Complete => "complete",
            Self::Dropped => "dropped",
        }
    }

    /// 是否终结态（不能 resume）。
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Dropped)
    }

    /// 是否计入预算记账（`active` / `budget-limited`，同 omp `isAccountingStatus`）。
    #[must_use]
    pub const fn is_accounting(self) -> bool {
        matches!(self, Self::Active | Self::BudgetLimited)
    }
}

/// P0-3：goals 目标预算配置（cli 从 `[goals]` 映射）。
#[derive(Debug, Clone)]
pub struct GoalBudget {
    /// token 累计上限（记账口径 input + `cache_write` + output；`0` = 不限）。
    pub token_budget: u64,
    /// 墙钟上限（`Duration::ZERO` = 不限）。
    pub time_budget: std::time::Duration,
    /// 超限后硬停（`true`：下一停止边界直接结束；`false`：注入提醒后续跑一轮让模型收尾）。
    pub hard_stop: bool,
}

impl GoalBudget {
    /// 空预算（不限）。
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            token_budget: 0,
            time_budget: std::time::Duration::ZERO,
            hard_stop: false,
        }
    }
}

/// P0-3 + H29：goals 记账状态与目标状态机（跨 Agent 重建共享）。
///
/// 无目标时 `objective == None`、`status == Dropped`——仅做会话级预算记账，
/// 行为与引入 H29 之前完全一致（向后兼容 `/goal` 的 set/extend）。
#[derive(Debug)]
pub struct GoalState {
    /// 会话级预算（`/goal` 可运行时调整）。
    pub budget: GoalBudget,
    /// 当前目标（`None` = 本会话未创建目标）。
    pub objective: Option<String>,
    /// 目标状态。
    pub status: GoalStatus,
    /// 目标级 token 预算（`None` = 沿用 [`Self::budget`]）。
    pub token_budget: Option<u64>,
    /// 续跑上限。
    pub max_continuations: usize,
    /// 已续跑轮数。
    pub continuations: usize,
    /// 已注入过超限提醒（一次性，防「注入→超限→再注入」无限续跑循环）。
    pub notified: bool,
    /// 首次用量时间（记账起点）。
    pub start: Option<std::time::Instant>,
    /// 累计用量。
    pub usage: agent_core::Usage,
}

impl GoalState {
    /// 新状态（无目标；仅会话预算）。
    #[must_use]
    pub fn new(budget: GoalBudget) -> Self {
        Self {
            budget,
            objective: None,
            status: GoalStatus::Dropped,
            token_budget: None,
            max_continuations: DEFAULT_MAX_CONTINUATIONS,
            continuations: 0,
            notified: false,
            start: None,
            usage: agent_core::Usage::default(),
        }
    }

    /// 设置续跑上限（构建期；0 表示不限制）。
    #[must_use]
    pub const fn with_max_continuations(mut self, n: usize) -> Self {
        self.max_continuations = n;
        self
    }

    /// 记账口径：input + `cache_write` + output（`cache_read` 为折扣价不计，同 omp `GoalRuntime`）。
    #[must_use]
    pub const fn billed(&self) -> u64 {
        self.usage.input_tokens + self.usage.cache_write_tokens + self.usage.output_tokens
    }

    /// 生效的 token 预算（目标级优先，缺省回落会话级；`None` = 不限）。
    #[must_use]
    pub fn effective_token_budget(&self) -> Option<u64> {
        match self.token_budget {
            Some(b) => Some(b),
            None if self.budget.token_budget > 0 => Some(self.budget.token_budget),
            None => None,
        }
    }

    /// 剩余 token（无预算 → `None`）。
    #[must_use]
    pub fn remaining_tokens(&self) -> Option<u64> {
        self.effective_token_budget()
            .map(|b| b.saturating_sub(self.billed()))
    }

    /// 当前是否超限（token 或墙钟）。
    #[must_use]
    pub fn exceeded(&self) -> bool {
        if let Some(budget) = self.effective_token_budget()
            && self.billed() >= budget
        {
            return true;
        }
        if !self.budget.time_budget.is_zero()
            && let Some(start) = self.start
            && start.elapsed() >= self.budget.time_budget
        {
            return true;
        }
        false
    }

    /// 累计本轮用量；返回「本次首次超限」（已提醒过返回 `false`，防重复注入）。
    ///
    /// H29：超限时若目标处于 `active`，同时翻转为 `budget-limited`（记账状态保留，
    /// 但引擎不再续跑，只允许收尾）。
    pub fn note_usage(&mut self, usage: &agent_core::Usage) -> bool {
        if self.start.is_none() {
            self.start = Some(std::time::Instant::now());
        }
        self.usage.add(usage);
        if self.notified {
            return false;
        }
        if self.exceeded() {
            self.notified = true;
            if self.status == GoalStatus::Active {
                self.status = GoalStatus::BudgetLimited;
            }
            true
        } else {
            false
        }
    }

    /// 是否存在目标（含已终结目标——`/goal` 显示与 `create` 冲突判定用）。
    #[must_use]
    pub const fn has_goal(&self) -> bool {
        self.objective.is_some()
    }

    /// 目标是否活跃（引擎续跑条件）。
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.objective.is_some() && matches!(self.status, GoalStatus::Active)
    }

    /// 人类可读状态摘要（`/goal` 显示与预算提醒注入共用）。
    #[must_use]
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("token {}", self.billed())];
        if let Some(budget) = self.effective_token_budget() {
            parts.push(format!("/{budget}"));
        }
        if let Some(start) = self.start {
            parts.push(format!("时间 {}s", start.elapsed().as_secs()));
            if !self.budget.time_budget.is_zero() {
                parts.push(format!("/{}s", self.budget.time_budget.as_secs()));
            }
        }
        parts.join("，")
    }

    /// H29：导出可持久化快照（无目标 → `None`）。
    #[must_use]
    pub fn snapshot(&self) -> Option<GoalSnapshot> {
        let objective = self.objective.clone()?;
        Some(GoalSnapshot {
            objective,
            status: self.status.as_str().to_string(),
            token_budget: self.token_budget,
            continuations: self.continuations,
            max_continuations: self.max_continuations,
            updated_at_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        })
    }

    /// H29：从快照恢复目标状态。
    ///
    /// **重启不自动续跑**：快照里是 `active` 时恢复为 `paused`（对齐 omp
    /// `onThreadResumed` 的保守语义）——自动继续跑一个可能已经过期的目标比让用户
    /// 显式 `goal resume` 更危险。其余状态原样恢复。
    pub fn restore(&mut self, snapshot: GoalSnapshot) {
        self.objective = Some(snapshot.objective);
        self.status = match snapshot.status.as_str() {
            "active" => GoalStatus::Paused,
            "budget-limited" => GoalStatus::BudgetLimited,
            "complete" => GoalStatus::Complete,
            "dropped" => GoalStatus::Dropped,
            _ => GoalStatus::Paused,
        };
        self.token_budget = snapshot.token_budget;
        self.continuations = snapshot.continuations;
        if snapshot.max_continuations > 0 {
            self.max_continuations = snapshot.max_continuations;
        }
        self.notified = false;
    }

    /// 目标摘要（`/goal` 显示：状态 + objective 首行）。
    #[must_use]
    pub fn goal_summary(&self) -> Option<String> {
        let objective = self.objective.as_deref()?;
        let first = objective.lines().next().unwrap_or("").trim();
        Some(format!(
            "goal [{}] {}（续跑 {}/{}）",
            self.status.as_str(),
            first,
            self.continuations,
            if self.max_continuations == 0 {
                "∞".to_string()
            } else {
                self.max_continuations.to_string()
            }
        ))
    }

    /// 创建目标：已存在未终结目标时报错（对齐 omp `createGoal`）。
    ///
    /// # Errors
    /// objective 为空、token 预算非正数、或已有未终结目标。
    pub fn create(&mut self, objective: &str, token_budget: Option<u64>) -> Result<(), String> {
        let objective = objective.trim();
        if objective.is_empty() {
            return Err("objective 不能为空".into());
        }
        validate_token_budget(token_budget)?;
        if self.has_goal() && !self.status.is_terminal() {
            return Err(format!(
                "本会话已有目标（状态 {}），如需更换请用 op=replace",
                self.status.as_str()
            ));
        }
        self.objective = Some(objective.to_string());
        self.status = GoalStatus::Active;
        self.token_budget = token_budget;
        self.continuations = 0;
        self.notified = false;
        Ok(())
    }

    /// 替换目标：仅在目标活跃/受预算限制时允许（对齐 omp `replaceGoal`）。
    ///
    /// # Errors
    /// 无活跃目标、objective 为空或 token 预算非法。
    pub fn replace(&mut self, objective: &str, token_budget: Option<u64>) -> Result<(), String> {
        let objective = objective.trim();
        if objective.is_empty() {
            return Err("objective 不能为空".into());
        }
        validate_token_budget(token_budget)?;
        if !self.status.is_accounting() {
            return Err("当前没有活跃目标，无法替换（如需新建请用 op=create）".into());
        }
        self.objective = Some(objective.to_string());
        self.status = GoalStatus::Active;
        self.token_budget = token_budget;
        self.continuations = 0;
        self.notified = false;
        Ok(())
    }

    /// 标记完成（唯一完成入口）。
    ///
    /// # Errors
    /// 无目标、目标已终结。
    pub fn complete(&mut self) -> Result<(), String> {
        if !self.has_goal() {
            return Err("当前没有目标，无法完成".into());
        }
        match self.status {
            GoalStatus::Complete => Err("目标已完成".into()),
            GoalStatus::Dropped => Err("目标已放弃，无法标记完成".into()),
            _ => {
                self.status = GoalStatus::Complete;
                Ok(())
            }
        }
    }

    /// 暂停（中断时由宿主调用；可 `resume`）。
    ///
    /// # Errors
    /// 无目标。
    pub fn pause(&mut self) -> Result<(), String> {
        if !self.has_goal() {
            return Err("当前没有目标，无法暂停".into());
        }
        if self.status.is_terminal() {
            return Err(format!("目标已 {}，无法暂停", self.status.as_str()));
        }
        self.status = GoalStatus::Paused;
        Ok(())
    }

    /// 恢复（`paused` / `budget-limited` → `active`）。
    ///
    /// # Errors
    /// 无目标、目标已完成或已放弃。
    pub fn resume(&mut self) -> Result<(), String> {
        if !self.has_goal() {
            return Err("当前没有目标，无法恢复".into());
        }
        if self.status == GoalStatus::Complete {
            return Err("目标已完成，无法恢复".into());
        }
        if self.status == GoalStatus::Dropped {
            return Err("目标已放弃，无法恢复（如需新建请用 op=create）".into());
        }
        self.status = GoalStatus::Active;
        // 恢复即重新给额度：清掉超限标记，让新一轮用量可再次触发提醒。
        self.notified = false;
        Ok(())
    }

    /// 放弃目标（清空目标状态，保留预算记账）。
    pub fn drop_goal(&mut self) -> Option<String> {
        let objective = self.objective.take()?;
        self.status = GoalStatus::Dropped;
        self.token_budget = None;
        self.continuations = 0;
        Some(objective)
    }

    /// 记一次续跑；返回是否仍在额度内（`false` = 达到上限，应停止续跑）。
    pub fn note_continuation(&mut self) -> bool {
        if self.max_continuations > 0 && self.continuations >= self.max_continuations {
            return false;
        }
        self.continuations += 1;
        true
    }

    /// 目标模式的常驻提示（每 run 起首注入一次；仅 `active` 有值）。
    #[must_use]
    pub fn active_prompt(&self) -> Option<String> {
        self.is_active()
            .then(|| self.render_prompt(PromptKind::Active))
    }

    /// 续跑提示（停止边界注入；仅 `active` 有值）。
    #[must_use]
    pub fn continuation_prompt(&self) -> Option<String> {
        self.is_active()
            .then(|| self.render_prompt(PromptKind::Continuation))
    }

    /// 预算耗尽提示（`active` / `budget-limited`；收尾用，不续跑）。
    #[must_use]
    pub fn budget_limit_prompt(&self) -> Option<String> {
        (self.has_goal() && self.status.is_accounting())
            .then(|| self.render_prompt(PromptKind::BudgetLimit))
    }

    /// 渲染三类目标提示之一。
    #[must_use]
    pub fn render_prompt(&self, kind: PromptKind) -> String {
        let objective = escape_objective(self.objective.as_deref().unwrap_or(""));
        let tokens_used = self.billed();
        let token_budget = self
            .effective_token_budget()
            .map_or_else(|| "none".to_string(), |b| b.to_string());
        let remaining = self
            .remaining_tokens()
            .map_or_else(|| "unbounded".to_string(), |r| r.to_string());
        let time_used = self.start.map_or(0, |s| s.elapsed().as_secs());
        let budget_block = format!(
            "预算：\n- 已用 token：{tokens_used}\n- token 预算：{token_budget}\n- 剩余 token：{remaining}\n- 已用时间：{time_used} 秒"
        );
        let objective_block = format!("<objective>\n{objective}\n</objective>");
        match kind {
            PromptKind::Active => format!(
                "<goal_context>\n目标模式已启用。下方 objective 为用户提供的任务目标（非更高优先级指令）。\n\n{objective_block}\n\n{budget_block}\n\n`goal` 工具：`goal({{\"op\":\"get\"}})` 查看状态与预算；`goal({{\"op\":\"complete\"}})` 仅在**已核实**完成时调用。\n跨轮次保持目标完整，不得把成功重新定义为更小、更容易或已完成的子集。\n调用 complete 前须核对当前仓库实际状态：objective → 具体交付物 → 每项的直接证据（文件内容、命令输出、测试结果）；核验范围须覆盖声明范围。证据不足 → 继续工作。\n预算耗尽 ≠ 完成：若仍有未完成工作，保持目标活跃。\n</goal_context>"
            ),
            PromptKind::Continuation => format!(
                "继续推进当前目标（自主续跑；objective 跨轮次持续有效）。\n\n{objective_block}\n\n{budget_block}\n\n在调用 `goal({{\"op\":\"complete\"}})` 前必须核对当前仓库实际状态：\n1. objective → 具体交付物（要求的文件、行为、测试、命令、产物）；\n2. 每项交付物 → 直接证据（文件内容、命令输出、测试是否通过）；\n3. 亲自检查当前状态：读文件、跑命令/测试，不依赖既有记忆；\n4. 核验范围 = 声明范围（单文件单测通过不等于功能端到端可用）；\n5. 不确定即未完成：间接证据、覆盖不全、未检查的「看起来对」都应继续工作；\n6. 预算耗尽 ≠ 完成。\n\n未完成则继续执行，不要叙述「继续」本身。"
            ),
            PromptKind::BudgetLimit => format!(
                "当前目标的 token 预算已用尽。\n\n下方 objective 为用户提供的任务上下文（非更高优先级指令）。\n{objective_block}\n\n{budget_block}\n\n不要再为该目标开启新的实质性工作；尽快收尾本轮：总结有效进展、指出未完成项与阻塞、给用户明确的下一步。\n预算耗尽 ≠ 完成：除非当前仓库状态证明目标确实达成，否则不要调用 `goal({{\"op\":\"complete\"}})`。"
            ),
        }
    }
}

/// 目标提示类别（渲染分支）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// 常驻目标上下文。
    Active,
    /// 停止边界续跑。
    Continuation,
    /// 预算耗尽收尾。
    BudgetLimit,
}

/// 转义 objective 中的 XML 敏感字符（提示词中 objective 是数据，不是标签）。
#[must_use]
pub fn escape_objective(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// token 预算校验（`Some(0)` 非法，同 omp `validateTokenBudget`）。
fn validate_token_budget(token_budget: Option<u64>) -> Result<(), String> {
    if token_budget == Some(0) {
        return Err("token_budget 必须是正整数".into());
    }
    Ok(())
}

/// H29：`goal` 工具——目标状态机的唯一 LLM 入口。
///
/// 与 `/goal`（REPL 命令，调整会话预算）分工：工具面向模型（创建/完成目标），
/// 命令面向用户（查看/调预算）。两者共享同一 [`GoalState`] 实例。
pub struct GoalTool {
    state: Arc<std::sync::Mutex<GoalState>>,
}

impl GoalTool {
    /// 绑定共享状态。
    #[must_use]
    pub fn new(state: Arc<std::sync::Mutex<GoalState>>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for GoalTool {
    fn name(&self) -> &'static str {
        "goal"
    }

    fn description(&self) -> &'static str {
        "管理持续目标（objective）：op=create 新建、get 查看、replace 替换、pause 暂停、resume 恢复、complete 完成、drop 放弃。目标活跃时引擎会在停止边界自动续跑，直到 complete 或达到续跑上限；调用 complete 前必须核对当前仓库状态并给出直接证据。"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["create", "get", "replace", "pause", "resume", "complete", "drop"],
                    "description": "操作：create/replace 需要 objective；complete 表示已核实完成"
                },
                "objective": {
                    "type": "string",
                    "description": "目标描述（create/replace 必填）；应可验证、含明确交付物"
                },
                "token_budget": {
                    "type": "integer",
                    "description": "目标级 token 预算（可选；缺省沿用会话 [goals] 预算）"
                }
            },
            "required": ["op"]
        })
    }

    fn capability(&self) -> CapabilityTier {
        // 仅改会话内目标状态，不触工作区文件。
        CapabilityTier::ReadOnly
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let op = input
            .get("op")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `op` 参数".into()))?;
        let objective = input
            .get("objective")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let token_budget = match input.get("token_budget") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .ok_or_else(|| ToolError::InvalidArgs("token_budget 必须是正整数".into()))?,
            ),
        };

        let result = {
            let mut g = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match op {
                "create" | "replace" => {
                    let objective = objective.as_deref().unwrap_or("");
                    let r = if op == "create" {
                        g.create(objective, token_budget)
                    } else {
                        g.replace(objective, token_budget)
                    };
                    r.map(|()| format!("goal {op} 成功：{}", g.goal_summary().unwrap_or_default()))
                }
                "get" => Ok(g
                    .goal_summary()
                    .unwrap_or_else(|| "（当前无目标）".to_string())),
                "pause" => g.pause().map(|()| "目标已暂停".to_string()),
                "resume" => g
                    .resume()
                    .map(|()| format!("目标已恢复：{}", g.goal_summary().unwrap_or_default())),
                "complete" => g.complete().map(|()| {
                    let budget = g.summary();
                    format!("目标已完成（{budget}）。请在最终回答中向用户汇报预算使用情况。")
                }),
                "drop" => Ok(g.drop_goal().map_or_else(
                    || "（当前无目标）".to_string(),
                    |o| format!("已放弃目标：{o}"),
                )),
                other => Err(format!("未知 op：{other}")),
            }
        };

        match result {
            Ok(text) => {
                // H29：状态变更即落盘（best-effort：持久化失败只影响跨会话恢复）。
                let cwd = ctx.workspace.root();
                if let Ok(state) = self.state.lock() {
                    if let Err(e) = save_goal(&cwd, &state) {
                        tracing::warn!(error = %e, "目标持久化失败");
                    }
                }
                Ok(ToolResult::text(text))
            }
            Err(msg) => Err(ToolError::InvalidArgs(msg)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> GoalState {
        GoalState::new(GoalBudget::unlimited())
    }

    #[test]
    fn create_requires_no_unfinished_goal_and_rejects_bad_budget() {
        let mut s = state();
        assert!(s.create("", None).is_err(), "空 objective 拒绝");
        assert!(s.create("修 H29", Some(0)).is_err(), "0 预算拒绝");
        s.create("修 H29", Some(100)).unwrap();
        assert!(s.is_active());
        assert_eq!(s.remaining_tokens(), Some(100));
        // 已存在未终结目标 → 拒绝（对齐 omp「cannot create a new goal」）。
        assert!(s.create("另一个目标", None).is_err());
        // replace 可换目标。
        s.replace("换个目标", Some(50)).unwrap();
        assert_eq!(s.objective.as_deref(), Some("换个目标"));
        // 终结后可再次创建。
        s.complete().unwrap();
        assert!(s.create("新目标", None).is_ok());
    }

    #[test]
    fn status_transitions_and_resume_rules() {
        let mut s = state();
        assert!(s.resume().is_err(), "无目标不可 resume");
        assert!(s.pause().is_err(), "无目标不可 pause");
        s.create("obj", None).unwrap();
        s.pause().unwrap();
        assert_eq!(s.status, GoalStatus::Paused);
        assert!(!s.is_active(), "暂停不续跑");
        assert!(s.resume().is_ok());
        assert!(s.is_active());
        s.complete().unwrap();
        assert!(s.resume().is_err(), "完成不可 resume");
        assert!(s.complete().is_err(), "重复 complete 报错");
        // drop 后不可 resume，可 create。
        s.create("obj2", None).unwrap();
        assert_eq!(s.drop_goal().as_deref(), Some("obj2"));
        assert!(s.resume().is_err(), "放弃不可 resume");
        assert!(s.complete().is_err(), "放弃不可 complete");
    }

    #[test]
    fn goal_budget_flips_to_budget_limited_and_prompts_follow_state() {
        let mut s = state();
        s.create("obj", Some(100)).unwrap();
        assert!(s.active_prompt().is_some());
        assert!(s.continuation_prompt().is_some());
        assert!(
            s.budget_limit_prompt().is_some(),
            "active 也属于记账状态（可收尾）"
        );
        // 记账到超限：首次触发 → budget-limited，且不再续跑。
        assert!(!s.note_usage(&usage(60, 0, 0, 0)));
        assert!(s.note_usage(&usage(50, 0, 0, 0)), "累计 110 ≥ 100 首次超限");
        assert_eq!(s.status, GoalStatus::BudgetLimited);
        assert!(s.continuation_prompt().is_none(), "受预算限制不再续跑");
        assert!(s.budget_limit_prompt().is_some(), "仍可注入收尾提示");
        // resume 清超限标记并回 active；预算未提高时下一次记账会再次翻回 budget-limited
        //（对齐 omp：额度未变则超限判定仍成立），但提醒标记已被重置、可再次注入。
        s.resume().unwrap();
        assert_eq!(s.status, GoalStatus::Active);
        assert!(!s.notified, "resume 应重置一次性提醒标记");
        assert!(
            s.note_usage(&usage(0, 0, 0, 0)),
            "预算未提高时再次记账应重新触发超限"
        );
        assert_eq!(s.status, GoalStatus::BudgetLimited);
    }

    #[test]
    fn session_budget_is_fallback_and_continuations_are_capped() {
        let mut s = GoalState::new(GoalBudget {
            token_budget: 10,
            ..GoalBudget::unlimited()
        });
        s.create("obj", None).unwrap();
        assert_eq!(s.effective_token_budget(), Some(10), "缺省回落会话预算");
        assert!(s.note_usage(&usage(20, 0, 0, 0)));
        assert_eq!(s.status, GoalStatus::BudgetLimited);

        // 续跑额度：上限内 true，达上限 false（防无限续跑）。
        let mut s = state();
        s.create("obj", None).unwrap();
        s.max_continuations = 2;
        assert!(s.note_continuation());
        assert!(s.note_continuation());
        assert!(!s.note_continuation(), "达上限后拒绝续跑");
        assert_eq!(s.continuations, 2);
    }

    #[test]
    fn prompts_escape_objective_and_carry_budget() {
        let mut s = state();
        s.create("<script> & 目标", Some(1000)).unwrap();
        let p = s.active_prompt().unwrap();
        assert!(p.contains("&lt;script&gt; &amp; 目标"), "{p}");
        assert!(!p.contains("<script>"), "objective 不得作为标签注入");
        assert!(p.contains("token 预算：1000"), "{p}");
        let c = s.continuation_prompt().unwrap();
        assert!(c.contains("继续推进当前目标"), "{c}");
        assert!(c.contains("预算耗尽 ≠ 完成"), "{c}");
    }

    #[test]
    fn goal_tool_ops_roundtrip() {
        let s = Arc::new(std::sync::Mutex::new(state()));
        let tool = GoalTool::new(Arc::clone(&s));
        // H29：工具会落盘 `<cwd>/.gyre/goal.json`，用临时工作区避免污染仓库。
        let dir = tempfile::tempdir().unwrap();
        let ws = agent_core::Workspace::new(dir.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let ctx = ToolContext {
            workspace: &ws,
            approval: &Yolo,
            cancel: &cancel,
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
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // get：无目标。
            let out = tool
                .execute(serde_json::json!({"op": "get"}), &ctx)
                .await
                .unwrap();
            assert!(
                out.to_llm_text().contains("无目标"),
                "{}",
                out.to_llm_text()
            );
            // create 缺 objective → 参数错误。
            assert!(
                tool.execute(serde_json::json!({"op": "create"}), &ctx)
                    .await
                    .is_err()
            );
            let out = tool
                .execute(
                    serde_json::json!({"op": "create", "objective": "做完 H29", "token_budget": 5}),
                    &ctx,
                )
                .await
                .unwrap();
            assert!(
                out.to_llm_text().contains("goal create 成功"),
                "{}",
                out.to_llm_text()
            );
            // H29：状态变更即落盘（跨会话可恢复）。
            let snap = load_goal(dir.path()).expect("create 后应落盘");
            assert_eq!(snap.objective, "做完 H29");
            assert_eq!(snap.status, "active");
            assert_eq!(snap.token_budget, Some(5));
            let out = tool
                .execute(serde_json::json!({"op": "get"}), &ctx)
                .await
                .unwrap();
            assert!(
                out.to_llm_text().contains("active"),
                "{}",
                out.to_llm_text()
            );
            // complete 后 get 反映终结态。
            tool.execute(serde_json::json!({"op": "complete"}), &ctx)
                .await
                .unwrap();
            assert_eq!(s.lock().unwrap().status, GoalStatus::Complete);
            // 未知 op。
            assert!(
                tool.execute(serde_json::json!({"op": "bogus"}), &ctx)
                    .await
                    .is_err()
            );
        });
    }

    /// H29：快照往返 + 落盘/读回 + 「重启不自动续跑」（active → paused）。
    #[test]
    fn goal_snapshot_roundtrip_and_restore_pauses_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = state();
        s.create("把 H29 做完", Some(50_000)).unwrap();
        s.note_continuation();
        assert!(save_goal(dir.path(), &s).is_ok());
        // 文件落在 `.gyre/goal.json`，可读回且字段保真。
        assert!(goal_file(dir.path()).exists());
        let snap = load_goal(dir.path()).expect("应能读回快照");
        assert_eq!(snap.objective, "把 H29 做完");
        assert_eq!(snap.status, "active");
        assert_eq!(snap.token_budget, Some(50_000));
        assert_eq!(snap.continuations, 1);
        assert_eq!(snap.max_continuations, DEFAULT_MAX_CONTINUATIONS);

        // 恢复：active → paused（重启不自动续跑），其余字段保留。
        let mut restored = state();
        restored.restore(snap.clone());
        assert_eq!(restored.status, GoalStatus::Paused);
        assert_eq!(restored.continuations, 1);
        assert_eq!(restored.token_budget, Some(50_000));
        assert!(!restored.is_active(), "恢复后不得自动续跑");

        // 已终结状态原样恢复。
        let mut done = state();
        done.create("x", None).unwrap();
        done.complete().unwrap();
        assert!(save_goal(dir.path(), &done).is_ok());
        let mut restored_done = state();
        restored_done.restore(load_goal(dir.path()).unwrap());
        assert_eq!(restored_done.status, GoalStatus::Complete);

        // 放弃目标 → 持久化文件被删除（下次启动不复活）。
        done.drop_goal();
        assert!(save_goal(dir.path(), &done).is_ok());
        assert!(!goal_file(dir.path()).exists(), "drop 后应删除快照文件");
        assert!(load_goal(dir.path()).is_none());
        // 损坏文件不阻断启动。
        std::fs::create_dir_all(goal_file(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(goal_file(dir.path()), "{not json").unwrap();
        assert!(load_goal(dir.path()).is_none());
    }

    /// 测试用审批（goal 工具不触发审批；占位满足 ToolContext）。
    struct Yolo;

    #[async_trait::async_trait]
    impl agent_core::ApprovalPolicy for Yolo {
        fn decide(&self, _r: &agent_core::ApprovalRequest<'_>) -> agent_core::ApprovalDecision {
            agent_core::ApprovalDecision::Allow
        }
        async fn prompt(
            &self,
            _a: &agent_core::AskMessage,
        ) -> Result<agent_core::AskResponse, ToolError> {
            Ok(agent_core::AskResponse::Yes)
        }
    }

    fn usage(input: u64, out: u64, cr: u64, cw: u64) -> agent_core::Usage {
        agent_core::Usage {
            input_tokens: input,
            output_tokens: out,
            cache_read_tokens: cr,
            cache_write_tokens: cw,
            cost_usd: 0.0,
        }
    }
}
