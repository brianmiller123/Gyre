//! 审批规则引擎与 [`RulesApprovalPolicy`]。
//!
//! 判定链（移植 oh-my-pi approval）：
//! 0. **yolo 短路（最高优先级）**：全自动放行，仅保留 deny 安全护栏与模式写入硬约束
//! 1. 绝对安全护栏：逐工具 `deny` + 命令 `deny` 黑名单
//! 2. **模式写入硬约束**：ask（只读——写类一律 Deny）/ architect（仅 `plans/` 下 `.md` 放行，
//!    其余写 Deny）。作为模式契约，**硬拒绝不被逐工具 allow 绕过**，且 yolo 下仍生效
//! 3. 逐工具 `allow|prompt|ask` 覆盖（deny 已在 1 处理）
//! 4. 命令级 `allow/ask` glob 规则（deny 已在 1 处理）
//! 5. 能力分级（ReadOnly 自动放行）
//! 6. 三档模式 `always-ask / write / yolo`（code/debug 模式下写类自动放行）
//!
//! 注意：yolo 压制一切「需确认」门禁（逐工具 ask/prompt、命令 ask glob），
//! 但 deny 黑名单与模式写入硬约束始终生效——「不再询问」不等于「放弃安全 / 放弃只读契约」。

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use agent_core::{
    ApprovalDecision, ApprovalMode, ApprovalPolicy, ApprovalRequest, AskResponse, CapabilityTier,
    Mode, ToolError,
};

use crate::PromptResolver;
use crate::config::{AgentConfig, CommandPattern, ToolApproval};

/// 纯规则引擎：仅做 `decide` 判定，不含交互。
#[derive(Debug, Clone)]
pub struct RulesEngine {
    /// 引用的 agent 配置（含逐工具覆盖与命令规则）。
    pub agent: Arc<AgentConfig>,
    /// 工作区根（解析写入路径，判定 architect 模式的 `plans/` 约束）。
    /// `None`（如单测）时退化为词法相对路径判定。
    workspace_root: Option<PathBuf>,
}

impl RulesEngine {
    /// 构造规则引擎。
    #[must_use]
    pub fn new(agent: Arc<AgentConfig>) -> Self {
        Self {
            agent,
            workspace_root: None,
        }
    }

    /// 注入工作区根（生产装配调用）：用于把 architect 模式的写入路径解析为相对工作区，
    /// 从而正确判定是否落在 `plans/` 下。未调用时退化为词法相对路径判定。
    #[must_use]
    pub fn with_workspace_root(mut self, root: Option<PathBuf>) -> Self {
        self.workspace_root = root;
        self
    }

    /// 模式写入硬约束：ask（只读）/ architect（仅 `plans/` 下 `.md`）。
    ///
    /// 返回值语义：
    /// - `Some(Deny)` —— 该模式硬拒绝此写操作。作为模式契约的硬护栏，**即使 yolo 也生效**
    ///   （与逐工具/命令 deny 同级），确保「ask 只读」「architect 不碰代码」不被绕过。
    /// - `Some(Allow)` —— architect 写入 `plans/` 下 `.md`：直接放行（规划文档是其核心产出，
    ///   如同 code/debug 自动放行写操作，避免逐文件审批）。
    /// - `None` —— 非写操作，或 code/debug 模式：交由常规判定链。
    fn mode_write_guard(&self, request: &ApprovalRequest<'_>) -> Option<ApprovalDecision> {
        // 仅约束写能力（write_file / apply_hashline / replace_block / ast_rewrite /
        // github 写动作 / MCP 写工具）。读 / 执行 / 网络不受此约束。
        if request.capability != CapabilityTier::Write {
            return None;
        }
        match self.agent.mode {
            Mode::Ask => Some(ApprovalDecision::Deny(
                "ask 模式为只读，禁止任何写操作",
            )),
            Mode::Architect => {
                if architect_write_allowed(request.tool, request.args, self.workspace_root.as_deref())
                {
                    Some(ApprovalDecision::Allow)
                } else {
                    Some(ApprovalDecision::Deny(
                        "architect 模式仅允许编辑 plans/ 目录下的 markdown 文件",
                    ))
                }
            }
            Mode::Code | Mode::Debug => None,
        }
    }

    /// 判定一次调用是否需要人工确认（同步、纯函数）。
    ///
    /// 判定链（详见模块文档）：yolo 短路 → 绝对 deny 护栏 → 模式写入硬约束 →
    /// 逐工具 allow/prompt/ask → 命令 allow/ask → 能力分级 → 三档审批模式。
    #[must_use]
    pub fn decide(&self, request: &ApprovalRequest<'_>) -> ApprovalDecision {
        // 0. yolo 短路（最高优先级）：跳过一切「需要确认」门禁——逐工具 prompt/ask、
        //    命令 ask glob、能力分级、模式联动一律放行，agent 全自动执行、不再弹审批。
        //
        //    安全语义：yolo 是「不再询问」，不是「放弃安全护栏 / 只读契约」。故 deny 仍优先于 yolo：
        //      - 逐工具 deny（[agent.tools.approval] 写 "deny"）
        //      - 命令 deny 黑名单（[agent.commands.deny]）
        //      - 模式写入硬约束（ask 只读 / architect 仅 plans/.md）
        //    三者在 yolo 下也照常拦截。若要连 deny 也一并压过（绝对放行），删除对应 deny 规则即可。
        if self.agent.approval_mode == ApprovalMode::Yolo {
            return self.decide_deny_only(request);
        }

        // 1. 绝对安全护栏（最高优先）：逐工具 deny + 命令 deny 黑名单。
        //    先于模式约束与逐工具 allow，确保安全黑名单永不被绕过。
        if matches!(
            self.agent.tools.approval.get(request.tool),
            Some(ToolApproval::Deny)
        ) {
            return ApprovalDecision::Deny("被该工具的 deny 覆盖阻止");
        }
        if let Some(command) = request.command {
            if matches_any(&self.agent.commands.deny, command, true) {
                return ApprovalDecision::Deny("命中命令黑名单");
            }
        }

        // 2. 模式写入硬约束（ask 只读 / architect 仅 plans/ 下 .md）。
        //    作为模式契约：ask 全只读、architect 仅编辑 plans/ 下 markdown，**硬拒绝**——
        //    不被后续逐工具 allow 覆盖（这是只读模式的核心保证）。architect 写 plans/.md
        //    则直接放行（核心产出，免逐次审批）。（deny 已在步骤 1 决出，故 per-tool deny
        //    仍优先于本约束对 plans/.md 的 Allow。）
        if let Some(decision) = self.mode_write_guard(request) {
            return decision;
        }

        // 3. 逐工具 allow / prompt / ask 覆盖（deny 已在步骤 1 处理）。
        if let Some(override_) = self.agent.tools.approval.get(request.tool) {
            return match override_ {
                ToolApproval::Allow => ApprovalDecision::Allow,
                ToolApproval::Deny => ApprovalDecision::Deny("被该工具的 deny 覆盖阻止"),
                ToolApproval::Prompt | ToolApproval::Ask => ApprovalDecision::Ask,
            };
        }

        // 4. 命令级 allow / ask 规则（仅 shell 类工具；deny 已在步骤 1 处理）。
        if let Some(command) = request.command {
            if matches_any(&self.agent.commands.allow, command, false) {
                return ApprovalDecision::Allow;
            }
            if matches_any(&self.agent.commands.ask, command, false) {
                return ApprovalDecision::Ask;
            }
        }

        // 5. 能力分级：只读自动放行
        if request.capability == CapabilityTier::ReadOnly {
            return ApprovalDecision::Allow;
        }

        // 6. 审批模式
        //
        // 模式联动（Zoo-Code mode → 审批门槛）：code/debug（可写）模式下，写类操作即使
        // 在 always-ask 档也自动放行——编码是这两个模式的核心能力，逐次审批会造成
        // 「写一个文件问一次」的体验割裂。ask/architect 的写操作已在步骤 2 被模式约束拦截
        // （ask 硬拒、architect 仅 plans/.md），此处仅决定 code/debug 的写类放行与其余的询问。
        // （yolo 已在步骤 0 短路，此处 Yolo 分支不可达，保留以满足穷尽匹配。）
        match self.agent.approval_mode {
            ApprovalMode::Yolo => ApprovalDecision::Allow,
            ApprovalMode::Write => {
                if request.capability == CapabilityTier::Write {
                    ApprovalDecision::Allow
                } else {
                    ApprovalDecision::Ask
                }
            }
            ApprovalMode::AlwaysAsk => {
                if matches!(self.agent.mode, Mode::Code | Mode::Debug)
                    && request.capability == CapabilityTier::Write
                {
                    ApprovalDecision::Allow
                } else {
                    ApprovalDecision::Ask
                }
            }
        }
    }

    /// yolo 模式专用判定：仅保留 deny 安全护栏与模式写入硬约束，其余一律放行。
    ///
    /// yolo 压制所有「需确认」门禁（逐工具 ask/prompt、命令 ask glob），但以下硬护栏不受影响：
    /// 逐工具 deny、命令 deny 黑名单、以及模式写入硬约束（ask 只读 / architect 仅 plans/.md）。
    /// 这让 `--approval-mode yolo` 真正实现全自动，同时避免 `rm -rf *` 等硬黑名单与只读契约
    /// 在全自动下被绕过。
    #[must_use]
    fn decide_deny_only(&self, request: &ApprovalRequest<'_>) -> ApprovalDecision {
        // 逐工具 deny：硬拦截。
        if matches!(
            self.agent.tools.approval.get(request.tool),
            Some(ToolApproval::Deny)
        ) {
            return ApprovalDecision::Deny("被该工具的 deny 覆盖阻止");
        }
        // 命令 deny 黑名单：硬拦截（含危险元字符的复合命令也照常匹配 deny）。
        if let Some(command) = request.command {
            if matches_any(&self.agent.commands.deny, command, true) {
                return ApprovalDecision::Deny("命中命令黑名单");
            }
        }
        // 模式写入硬约束：ask/architect 的只读契约即使 yolo 也生效（只读不可被全自动绕过）。
        if let Some(ApprovalDecision::Deny(reason)) = self.mode_write_guard(request) {
            return ApprovalDecision::Deny(reason);
        }
        ApprovalDecision::Allow
    }
}

// ── 模式写入约束的纯函数辅助 ────────────────────────────────────────────

/// architect 模式允许写入的工作区子目录（设计文档存放处）。
const ARCHITECT_PLAN_DIR: &str = "plans";

/// architect 写入目标是否**全部**落在允许范围（工作区 `plans/` 下、`.md` 后缀）。
///
/// 任一目标越界、或无法提取目标路径（github 写动作 / MCP 写工具无文件路径）→ 保守拒绝。
fn architect_write_allowed(
    tool: &str,
    args: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> bool {
    let targets = collect_write_targets(tool, args);
    if targets.is_empty() {
        return false;
    }
    targets
        .iter()
        .all(|t| is_plans_markdown(t, workspace_root))
}

/// 收集写工具的目标文件路径（相对工作区根的原始字符串，可能多个）。
///
/// - `write_file` / `replace_block` / `ast_rewrite`：顶层 `path`。
/// - `apply_hashline`：解析 `patch` 内 `[path#hash]` 段头与 `MV dest`（可一次编辑多文件）；
///   无段头时回退顶层 `path`。
/// - 其余写工具（github 写动作 / MCP 写工具）：无文件路径 → 返回空（上游保守拒绝）。
///
/// 注意：仅做**词法**提取（不触文件系统），与 hashline 段头格式保持一致，
/// 避免把重依赖（agent-hashline → agent-tools）引入本 crate。
fn collect_write_targets(tool: &str, args: &serde_json::Value) -> Vec<String> {
    if tool == "apply_hashline" {
        if let Some(patch) = args.get("patch").and_then(|v| v.as_str()) {
            let targets = collect_hashline_targets(patch);
            if !targets.is_empty() {
                return targets;
            }
        }
    }
    args.get("path")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .into_iter()
        .collect()
}

/// 从 hashline patch 文本词法提取所有目标路径：段头 `[path]` / `[path#hash]` 与 `MV dest`。
///
/// 与 hashline 解析器的段头规则一致：去前导空白后整行被方括号包裹即为段头。
/// 正文行以 `+` 起首，不会被误判为段头。
fn collect_hashline_targets(patch: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in patch.lines() {
        let t = raw.trim_start();
        // 段头 [path] / [path#hash]：取 `#` 前为路径（无 `#` 则整段为路径）。
        if let Some(body) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let path = body.split_once('#').map_or(body, |(p, _)| p).trim();
            if !path.is_empty() {
                out.push(path.to_owned());
            }
            continue;
        }
        // 文件移动：`MV dest`（关键字后须跟空白，避免误匹配以 MV 开头的路径）。
        if let Some(rest) = t
            .strip_prefix("MV")
            .filter(|s| s.starts_with(char::is_whitespace))
            .map(str::trim)
        {
            if !rest.is_empty() {
                out.push(rest.to_owned());
            }
        }
    }
    out
}

/// 判定单个原始路径是否为 architect 允许的写入目标（工作区 `plans/` 下、`.md` 后缀）。
///
/// 先做**词法归一化**（消除 `.`/`..`）再判前缀，杜绝 `plans/../../etc/passwd` 这类
/// 目录穿越绕过 `starts_with("plans")` 检查。
fn is_plans_markdown(raw: &str, workspace_root: Option<&Path>) -> bool {
    // 协议资源路径（skill:// memory:// mcp:// http(s):// 等）非文件写入目标。
    if raw.is_empty() || raw.contains("://") {
        return false;
    }
    let Some(rel) = target_relative(Path::new(raw), workspace_root) else {
        return false;
    };
    rel.starts_with(ARCHITECT_PLAN_DIR)
        && rel
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

/// 把写入路径归一化为「相对工作区根」的词法路径。
///
/// - 相对路径：直接词法归一化（消除 `.`/`..`）。
/// - 绝对路径：需 `workspace_root`；归一化后若不以根为前缀则视为越界（返回 `None`）。
/// - 词法归一化后仍带前导 `..`（越出根）的相对路径，由调用方经 `starts_with` 自然判否。
fn target_relative(path: &Path, workspace_root: Option<&Path>) -> Option<PathBuf> {
    let normalized = lexical_normalize(path);
    if path.is_absolute() {
        let root = workspace_root?;
        let norm_root = lexical_normalize(root);
        normalized.strip_prefix(&norm_root).ok().map(PathBuf::from)
    } else {
        Some(normalized)
    }
}

/// 词法归一化：消除 `.`/`..` 组件；`..` 越界保留为前导组件（由调用方判定）。
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component<'_>> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                _ => out.push(comp),
            },
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// 配置驱动的审批策略：`decide` 走规则，`prompt` 委托前端注入的回调。
///
/// 前端（CLI/Web）在构造时注入 [`PromptResolver`]：
/// - CLI：从 stdin 读取 y/n
/// - Web：经 WebSocket 推送 Ask 并等待 `Respond` 回执
pub struct RulesApprovalPolicy {
    rules: RulesEngine,
    prompt_resolver: PromptResolver,
}

impl RulesApprovalPolicy {
    /// 构造：规则引擎 + 前端交互回调。
    #[must_use]
    pub fn new(rules: RulesEngine, prompt_resolver: PromptResolver) -> Self {
        Self {
            rules,
            prompt_resolver,
        }
    }
}

#[async_trait::async_trait]
impl ApprovalPolicy for RulesApprovalPolicy {
    fn decide(&self, request: &ApprovalRequest<'_>) -> ApprovalDecision {
        self.rules.decide(request)
    }

    async fn prompt(&self, ask: &agent_core::AskMessage) -> Result<AskResponse, ToolError> {
        (self.prompt_resolver)(ask.clone()).await
    }
}

/// 命令是否匹配任一 glob 规则。
///
/// **安全**：命令中若含 shell 元字符（`;` `|` `&` `$` 反引号 换行等），则**永远不匹配**
/// allow/ask 规则——杜绝 `cargo build; rm -rf /` 这类拼接命令借 `cargo *` 白名单绕过审批。
/// deny 规则仍照常匹配（黑名单优先拦截）。
fn matches_any(rules: &[CommandPattern], command: &str, is_deny: bool) -> bool {
    // 含危险元字符的复合命令：deny 规则正常匹配（拦截），allow/ask 规则永不放行。
    let has_shell_meta = command
        .chars()
        .any(|c| matches!(c, ';' | '|' | '&' | '$' | '`' | '\n' | '\r' | '>' | '<'));
    if has_shell_meta && !is_deny {
        return false;
    }
    rules.iter().any(|rule| {
        glob::Pattern::new(rule.pattern())
            .map(|compiled| compiled.matches(command))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use serde_json::json;

    use crate::config::{AgentConfig, CommandPattern, CommandRules, ToolsConfig};
    use agent_core::{ApprovalRequest, CapabilityTier, Mode};

    fn engine(mode: ApprovalMode) -> RulesEngine {
        let mut agent = AgentConfig::default();
        agent.approval_mode = mode;
        RulesEngine::new(Arc::new(agent))
    }

    fn req<'a>(tool: &'a str, cap: CapabilityTier, cmd: Option<&'a str>) -> ApprovalRequest<'a> {
        ApprovalRequest {
            tool,
            capability: cap,
            command: cmd,
            args: &serde_json::Value::Null,
        }
    }

    /// 带 JSON 参数的审批请求（用于模式写入约束的路径判定）。
    fn req_args<'a>(
        tool: &'a str,
        cap: CapabilityTier,
        args: &'a serde_json::Value,
    ) -> ApprovalRequest<'a> {
        ApprovalRequest {
            tool,
            capability: cap,
            command: None,
            args,
        }
    }

    #[test]
    fn command_pattern_parses_simple_string() {
        let toml_src = r#"
allow = ["ls", "cat", "grep"]
deny = []
ask = []
"#;
        let parsed: CommandRules = toml::from_str(toml_src).expect("parse");
        assert_eq!(parsed.allow.len(), 3);
        assert_eq!(parsed.allow[0].pattern(), "ls");
        assert_eq!(parsed.allow[1].pattern(), "cat");
        assert_eq!(parsed.allow[2].pattern(), "grep");
    }

    #[test]
    fn command_pattern_parses_full_struct() {
        let toml_src = r#"
[[allow]]
pattern = "cargo *"
[[deny]]
pattern = "rm -rf *"
ask = []
"#;
        let parsed: CommandRules = toml::from_str(toml_src).expect("parse");
        assert_eq!(parsed.allow.len(), 1);
        assert_eq!(parsed.allow[0].pattern(), "cargo *");
        assert_eq!(parsed.deny.len(), 1);
        assert_eq!(parsed.deny[0].pattern(), "rm -rf *");
    }

    #[test]
    fn readonly_always_allowed() {
        let e = engine(ApprovalMode::AlwaysAsk);
        assert!(matches!(
            e.decide(&req("read_file", CapabilityTier::ReadOnly, None)),
            ApprovalDecision::Allow
        ));
    }

    #[test]
    fn yolo_allows_execute() {
        let e = engine(ApprovalMode::Yolo);
        assert!(matches!(
            e.decide(&req(
                "run_command",
                CapabilityTier::Execute,
                Some("rm -rf x")
            )),
            ApprovalDecision::Allow
        ));
    }

    #[test]
    fn yolo_overrides_per_tool_ask() {
        // yolo 为判定链最高优先级：逐工具 ask/prompt 也被压过，全自动放行。
        // （config.example.toml 默认带 run_command = "ask"，yolo 必须能压过它才真正全自动。）
        let mut agent = AgentConfig::default();
        agent.approval_mode = ApprovalMode::Yolo;
        let mut tools = ToolsConfig::default();
        tools
            .approval
            .insert("run_command".into(), ToolApproval::Ask);
        agent.tools = tools;
        let e = RulesEngine::new(Arc::new(agent));
        assert!(matches!(
            e.decide(&req("run_command", CapabilityTier::Execute, Some("ls"))),
            ApprovalDecision::Allow
        ));
    }

    #[test]
    fn yolo_overrides_command_ask_glob() {
        // yolo 压过命令级 ask glob（docker * 不再弹确认）。
        let mut agent = AgentConfig::default();
        agent.approval_mode = ApprovalMode::Yolo;
        agent.commands = CommandRules {
            allow: vec![],
            deny: vec![],
            ask: vec![CommandPattern::Simple("docker *".into())],
        };
        let e = RulesEngine::new(Arc::new(agent));
        assert!(matches!(
            e.decide(&req(
                "run_command",
                CapabilityTier::Execute,
                Some("docker ps")
            )),
            ApprovalDecision::Allow
        ));
    }

    #[test]
    fn yolo_still_respects_per_tool_deny() {
        // 安全护栏：yolo 不压过逐工具 deny。
        let mut agent = AgentConfig::default();
        agent.approval_mode = ApprovalMode::Yolo;
        let mut tools = ToolsConfig::default();
        tools
            .approval
            .insert("write_file".into(), ToolApproval::Deny);
        agent.tools = tools;
        let e = RulesEngine::new(Arc::new(agent));
        assert!(matches!(
            e.decide(&req("write_file", CapabilityTier::Write, None)),
            ApprovalDecision::Deny(_)
        ));
    }

    #[test]
    fn yolo_still_respects_command_deny_blacklist() {
        // 安全护栏：yolo 不压过命令 deny 黑名单（rm -rf * 仍拦截）。
        let mut agent = AgentConfig::default();
        agent.approval_mode = ApprovalMode::Yolo;
        agent.commands = CommandRules {
            allow: vec![],
            deny: vec![CommandPattern::Simple("rm -rf *".into())],
            ask: vec![],
        };
        let e = RulesEngine::new(Arc::new(agent));
        assert!(matches!(
            e.decide(&req(
                "run_command",
                CapabilityTier::Execute,
                Some("rm -rf /tmp/x")
            )),
            ApprovalDecision::Deny(_)
        ));
    }

    #[test]
    fn always_ask_asks_execute() {
        let e = engine(ApprovalMode::AlwaysAsk);
        assert!(matches!(
            e.decide(&req("run_command", CapabilityTier::Execute, Some("ls"))),
            ApprovalDecision::Ask
        ));
    }

    #[test]
    fn command_allowlist_allows() {
        let mut agent = AgentConfig::default();
        agent.commands = CommandRules {
            allow: vec![CommandPattern::Simple("git status".into())],
            deny: vec![],
            ask: vec![],
        };
        let e = RulesEngine::new(Arc::new(agent));
        assert!(matches!(
            e.decide(&req(
                "run_command",
                CapabilityTier::Execute,
                Some("git status")
            )),
            ApprovalDecision::Allow
        ));
    }

    #[test]
    fn per_tool_deny_blocks() {
        let mut agent = AgentConfig::default();
        let mut tools = ToolsConfig::default();
        tools
            .approval
            .insert("write_file".into(), ToolApproval::Deny);
        agent.tools = tools;
        agent.mode = Mode::Code;
        let e = RulesEngine::new(Arc::new(agent));
        assert!(matches!(
            e.decide(&req("write_file", CapabilityTier::Write, None)),
            ApprovalDecision::Deny(_)
        ));
    }

    fn engine_with(mode: Mode, approval: ApprovalMode) -> RulesEngine {
        let mut agent = AgentConfig::default();
        agent.mode = mode;
        agent.approval_mode = approval;
        RulesEngine::new(Arc::new(agent))
    }

    #[test]
    fn code_mode_auto_approves_write_under_always_ask() {
        // code（编码）模式下，写类操作即使 always-ask 也自动放行（核心能力）。
        let e = engine_with(Mode::Code, ApprovalMode::AlwaysAsk);
        assert!(matches!(
            e.decide(&req("write_file", CapabilityTier::Write, None)),
            ApprovalDecision::Allow
        ));
        // 执行类仍需询问（保持安全）。
        assert!(matches!(
            e.decide(&req("run_command", CapabilityTier::Execute, Some("ls"))),
            ApprovalDecision::Ask
        ));
    }

    #[test]
    fn debug_mode_auto_approves_write() {
        let e = engine_with(Mode::Debug, ApprovalMode::AlwaysAsk);
        assert!(matches!(
            e.decide(&req("write_file", CapabilityTier::Write, None)),
            ApprovalDecision::Allow
        ));
    }

    #[test]
    fn readonly_modes_block_write() {
        // ask（只读问答）/ architect（只读规划）模式下，写操作被**硬拒绝**（Deny），
        // 而非弹窗询问——只读是模式契约，不可绕过。
        let e = engine_with(Mode::Ask, ApprovalMode::AlwaysAsk);
        assert!(matches!(
            e.decide(&req("write_file", CapabilityTier::Write, None)),
            ApprovalDecision::Deny(_)
        ));
        let e = engine_with(Mode::Architect, ApprovalMode::AlwaysAsk);
        // architect 写入非 plans/ 路径（无路径参数）→ 拒绝
        assert!(matches!(
            e.decide(&req("write_file", CapabilityTier::Write, None)),
            ApprovalDecision::Deny(_)
        ));
    }

    #[test]
    fn ask_mode_execute_still_asks() {
        // ask 模式：写硬拒绝，但执行类（run_command）仍弹窗询问（用户选定语义）。
        let e = engine_with(Mode::Ask, ApprovalMode::AlwaysAsk);
        assert!(matches!(
            e.decide(&req("run_command", CapabilityTier::Execute, Some("ls"))),
            ApprovalDecision::Ask
        ));
    }

    #[test]
    fn architect_execute_still_asks() {
        // architect 模式：执行类（只读命令）仍弹窗询问，未被写入约束影响。
        let e = engine_with(Mode::Architect, ApprovalMode::AlwaysAsk);
        assert!(matches!(
            e.decide(&req("run_command", CapabilityTier::Execute, Some("ls"))),
            ApprovalDecision::Ask
        ));
    }

    #[test]
    fn architect_allows_plans_markdown_write() {
        let e = engine_with(Mode::Architect, ApprovalMode::AlwaysAsk);
        let args = json!({"path": "plans/roadmap.md", "content": "# 计划"});
        assert!(matches!(
            e.decide(&req_args("write_file", CapabilityTier::Write, &args)),
            ApprovalDecision::Allow
        ));
        // 前导 ./ 与大写后缀也应放行
        let args = json!({"path": "./plans/sub/Note.MD"});
        assert!(matches!(
            e.decide(&req_args("write_file", CapabilityTier::Write, &args)),
            ApprovalDecision::Allow
        ));
    }

    #[test]
    fn architect_denies_non_plans_or_non_markdown() {
        let e = engine_with(Mode::Architect, ApprovalMode::AlwaysAsk);
        // 非 plans/ 目录
        let args = json!({"path": "src/main.rs", "content": ""});
        assert!(matches!(
            e.decide(&req_args("write_file", CapabilityTier::Write, &args)),
            ApprovalDecision::Deny(_)
        ));
        // plans/ 下但非 markdown
        let args = json!({"path": "plans/diagram.png"});
        assert!(matches!(
            e.decide(&req_args("write_file", CapabilityTier::Write, &args)),
            ApprovalDecision::Deny(_)
        ));
        // 协议资源路径
        let args = json!({"path": "memory://summary"});
        assert!(matches!(
            e.decide(&req_args("write_file", CapabilityTier::Write, &args)),
            ApprovalDecision::Deny(_)
        ));
    }

    #[test]
    fn architect_denies_path_traversal() {
        // plans/../../etc 必须被词法归一化拦截，不能绕过 starts_with("plans")。
        let e = engine_with(Mode::Architect, ApprovalMode::AlwaysAsk);
        let args = json!({"path": "plans/../../etc/passwd.md", "content": ""});
        assert!(matches!(
            e.decide(&req_args("write_file", CapabilityTier::Write, &args)),
            ApprovalDecision::Deny(_)
        ));
    }

    #[test]
    fn architect_absolute_path_with_workspace_root() {
        let e = engine_with(Mode::Architect, ApprovalMode::AlwaysAsk)
            .with_workspace_root(Some(PathBuf::from("/home/u/proj")));
        // 工作区内 plans/ 下 → 放行
        let args = json!({"path": "/home/u/proj/plans/x.md"});
        assert!(matches!(
            e.decide(&req_args("write_file", CapabilityTier::Write, &args)),
            ApprovalDecision::Allow
        ));
        // 工作区外 → 拒绝
        let args = json!({"path": "/etc/passwd.md"});
        assert!(matches!(
            e.decide(&req_args("write_file", CapabilityTier::Write, &args)),
            ApprovalDecision::Deny(_)
        ));
    }

    #[test]
    fn architect_hashline_multifile_checked() {
        let e = engine_with(Mode::Architect, ApprovalMode::AlwaysAsk);
        // apply_hashline 段头内嵌越界路径 → 拒绝（即便顶层 path 是 plans/）
        let patch = "[plans/a.md#1a2b]\nSWAP 1.=1:\n+ok\n[src/main.rs#3c4d]\nSWAP 1.=1:\n+bad";
        let args = json!({"patch": patch, "path": "plans/fallback.md"});
        assert!(matches!(
            e.decide(&req_args("apply_hashline", CapabilityTier::Write, &args)),
            ApprovalDecision::Deny(_)
        ));
        // 全部段头落在 plans/.md → 放行
        let patch = "[plans/a.md#1a2b]\nSWAP 1.=1:\n+ok\n[plans/b.md]\nSWAP 1.=1:\n+ok";
        let args = json!({"patch": patch});
        assert!(matches!(
            e.decide(&req_args("apply_hashline", CapabilityTier::Write, &args)),
            ApprovalDecision::Allow
        ));
    }

    #[test]
    fn per_tool_allow_cannot_bypass_ask_readonly() {
        // ask 只读为硬契约：显式逐工具 allow 仍被模式约束拒绝（步骤 2 先于步骤 3）。
        let mut agent = AgentConfig::default();
        agent.mode = Mode::Ask;
        agent.approval_mode = ApprovalMode::AlwaysAsk;
        let mut tools = ToolsConfig::default();
        tools
            .approval
            .insert("write_file".into(), ToolApproval::Allow);
        agent.tools = tools;
        let e = RulesEngine::new(Arc::new(agent));
        assert!(matches!(
            e.decide(&req("write_file", CapabilityTier::Write, None)),
            ApprovalDecision::Deny(_)
        ));
    }

    #[test]
    fn ask_readonly_survives_yolo() {
        // 只读契约即使在 yolo 下也生效（不被全自动绕过）。
        let e = engine_with(Mode::Ask, ApprovalMode::Yolo);
        assert!(matches!(
            e.decide(&req("write_file", CapabilityTier::Write, None)),
            ApprovalDecision::Deny(_)
        ));
        // 但 yolo 仍自动放行执行类（与既有 yolo 语义一致）
        assert!(matches!(
            e.decide(&req("run_command", CapabilityTier::Execute, Some("ls"))),
            ApprovalDecision::Allow
        ));
    }

    #[test]
    fn architect_readonly_survives_yolo() {
        // architect 非 plans/ 写入在 yolo 下仍被拒。
        let e = engine_with(Mode::Architect, ApprovalMode::Yolo);
        let args = json!({"path": "src/main.rs"});
        assert!(matches!(
            e.decide(&req_args("write_file", CapabilityTier::Write, &args)),
            ApprovalDecision::Deny(_)
        ));
    }

    #[test]
    fn per_tool_prompt_overrides_code_mode_auto_write() {
        // 逐工具覆盖优先级最高：code 模式下显式 prompt 仍询问（逃生舱）。
        let mut agent = AgentConfig::default();
        agent.mode = Mode::Code;
        agent.approval_mode = ApprovalMode::AlwaysAsk;
        let mut tools = ToolsConfig::default();
        tools
            .approval
            .insert("write_file".into(), ToolApproval::Prompt);
        agent.tools = tools;
        let e = RulesEngine::new(Arc::new(agent));
        assert!(matches!(
            e.decide(&req("write_file", CapabilityTier::Write, None)),
            ApprovalDecision::Ask
        ));
    }
}
