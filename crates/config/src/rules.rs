//! 审批规则引擎与 [`RulesApprovalPolicy`]。
//!
//! 判定链（移植 oh-my-pi approval）：
//! 1. 逐工具 `allow|prompt|deny|ask` 覆盖
//! 2. 命令级 `allow/deny/ask` glob 规则
//! 3. 能力分级（ReadOnly 自动放行）
//! 4. 三档模式 `always-ask / write / yolo`（code/debug 模式下写类自动放行）

use std::sync::Arc;

use agent_core::{
    ApprovalDecision, ApprovalMode, ApprovalPolicy, ApprovalRequest, AskResponse, CapabilityTier,
    Mode, ToolError,
};

use crate::config::{AgentConfig, CommandPattern, ToolApproval};
use crate::PromptResolver;

/// 纯规则引擎：仅做 `decide` 判定，不含交互。
#[derive(Debug, Clone)]
pub struct RulesEngine {
    /// 引用的 agent 配置（含逐工具覆盖与命令规则）。
    pub agent: Arc<AgentConfig>,
}

impl RulesEngine {
    /// 构造规则引擎。
    #[must_use]
    pub fn new(agent: Arc<AgentConfig>) -> Self {
        Self { agent }
    }

    /// 判定一次调用是否需要人工确认（同步、纯函数）。
    #[must_use]
    pub fn decide(&self, request: &ApprovalRequest<'_>) -> ApprovalDecision {
        // 1. 逐工具覆盖（最高优先）
        if let Some(override_) = self.agent.tools.approval.get(request.tool) {
            return match override_ {
                ToolApproval::Allow => ApprovalDecision::Allow,
                ToolApproval::Deny => ApprovalDecision::Deny("被该工具的 deny 覆盖阻止"),
                ToolApproval::Prompt | ToolApproval::Ask => ApprovalDecision::Ask,
            };
        }

        // 2. 命令级规则（仅 shell 类工具）
        if let Some(command) = request.command {
            if matches_any(&self.agent.commands.allow, command, false) {
                return ApprovalDecision::Allow;
            }
            if matches_any(&self.agent.commands.deny, command, true) {
                return ApprovalDecision::Deny("命中命令黑名单");
            }
            if matches_any(&self.agent.commands.ask, command, false) {
                return ApprovalDecision::Ask;
            }
        }

        // 3. 能力分级：只读自动放行
        if request.capability == CapabilityTier::ReadOnly {
            return ApprovalDecision::Allow;
        }

        // 4. 审批模式
        //
        // 模式联动（Zoo-Code mode → 审批门槛）：code/debug（可写）模式下，写类操作即使
        // 在 always-ask 档也自动放行——编码是这两个模式的核心能力，逐次审批会造成
        // 「写一个文件问一次」的体验割裂。ask/architect（只读导向）不受影响，写操作仍询问。
        // 逐工具覆盖（[agent.tools.approval]）优先级最高，可显式 prompt/ask 强制询问。
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
    let has_shell_meta = command.chars().any(|c| {
        matches!(c, ';' | '|' | '&' | '$' | '`' | '\n' | '\r' | '>' | '<')
    });
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
            e.decide(&req("run_command", CapabilityTier::Execute, Some("rm -rf x"))),
            ApprovalDecision::Allow
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
            e.decide(&req("run_command", CapabilityTier::Execute, Some("git status"))),
            ApprovalDecision::Allow
        ));
    }

    #[test]
    fn per_tool_deny_blocks() {
        let mut agent = AgentConfig::default();
        let mut tools = ToolsConfig::default();
        tools.approval.insert("write_file".into(), ToolApproval::Deny);
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
    fn readonly_modes_still_prompt_write() {
        // ask（只读问答）/ architect（只读规划）模式下，写操作仍需询问。
        let e = engine_with(Mode::Ask, ApprovalMode::AlwaysAsk);
        assert!(matches!(
            e.decide(&req("write_file", CapabilityTier::Write, None)),
            ApprovalDecision::Ask
        ));
        let e = engine_with(Mode::Architect, ApprovalMode::AlwaysAsk);
        assert!(matches!(
            e.decide(&req("write_file", CapabilityTier::Write, None)),
            ApprovalDecision::Ask
        ));
    }

    #[test]
    fn per_tool_prompt_overrides_code_mode_auto_write() {
        // 逐工具覆盖优先级最高：code 模式下显式 prompt 仍询问（逃生舱）。
        let mut agent = AgentConfig::default();
        agent.mode = Mode::Code;
        agent.approval_mode = ApprovalMode::AlwaysAsk;
        let mut tools = ToolsConfig::default();
        tools.approval.insert("write_file".into(), ToolApproval::Prompt);
        agent.tools = tools;
        let e = RulesEngine::new(Arc::new(agent));
        assert!(matches!(
            e.decide(&req("write_file", CapabilityTier::Write, None)),
            ApprovalDecision::Ask
        ));
    }
}
