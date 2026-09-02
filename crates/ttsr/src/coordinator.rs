//! TTSR 协调器：agent 循环的对接面。
//!
//! 职责（移植 oh-my-pi `session/ttsr-coordinator.ts` 的循环对接子集）：
//! - 轮次边界：`on_turn_start` 清缓冲；
//! - 流式中：`check_text_delta` / `check_thinking_delta` 命中可中断规则 → 返回规则名，
//!   由循环中断流、丢弃部分输出、注入后重试（discard 模式）；
//! - `MessageEnd`：`check_tool_calls` 对工具载荷做快照匹配——`Always` 规则 → 丢弃整条
//!   assistant 重试；`Never` 规则 → 折叠为工具结果前导提醒（不打断执行）；
//! - 注入持久化：注入消息带 `[ttsr-injection:…]` 标记，会话加载时经
//!   [`TtsrCoordinator::restore_from_messages`] 恢复抑制状态（压缩/分支切换不丢）。

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use agent_core::AgentMessage;

use crate::matcher::{TtsrManager, digest_for, path_of, tool_calls_of};
use crate::rule::{InterruptMode, Rule};

/// 注入消息的持久化标记前缀（`[ttsr-injection:name1,name2]`）。
pub const INJECTION_MARKER: &str = "[ttsr-injection:";

/// TTSR 配置（装配层注入）。
#[derive(Debug, Clone, Default)]
pub struct TtsrConfig {
    /// 总开关（`None` = 有规则即启用）。
    pub enabled: Option<bool>,
    /// 禁用的规则名。
    pub disabled_rules: Vec<String>,
}

/// 工具调用检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutcome {
    /// 无命中。
    None,
    /// 命中可中断规则：调用方应丢弃本条 assistant 消息、注入后重试。
    Abort(Vec<String>),
    /// 命中非打断规则：提醒已按 `tool_call_id` 暂存，执行结果回填时折叠。
    Reminders,
}

/// 协调器（线程安全：`Manager` 与提醒表各自互斥）。
pub struct TtsrCoordinator {
    manager: Mutex<TtsrManager>,
    /// `tool_call_id` → 待折叠的提醒文本（多规则合并）。
    reminders: Mutex<HashMap<String, String>>,
    /// 配置快照（enabled 判定由装配层完成）。
    _config: TtsrConfig,
}

impl TtsrCoordinator {
    /// 构造协调器。
    pub fn new(config: TtsrConfig, rules: Vec<Rule>) -> Self {
        Self {
            manager: Mutex::new(TtsrManager::new(rules, &config.disabled_rules)),
            reminders: Mutex::new(HashMap::new()),
            _config: config,
        }
    }

    /// 从已加载的会话消息恢复注入状态（扫描 `[ttsr-injection:…]` 标记）。
    pub fn restore_from_messages(&self, messages: &[AgentMessage]) {
        let mut names = Vec::new();
        for msg in messages {
            if let AgentMessage::User(u) = msg {
                for block in &u.content {
                    if let agent_core::UserContent::Text { text } = block {
                        if let Some(names_in) = parse_marker(text) {
                            names.extend(names_in);
                        }
                    }
                }
            }
        }
        if !names.is_empty() {
            self.manager
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .restore(&names);
        }
    }

    /// 本轮开始（流式首个增量前调用）。
    pub fn on_turn_start(&self) {
        self.manager
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .on_turn_start();
    }

    /// 检查文本流增量。命中返回**可中断**规则名（调用方中断流并重试）。
    pub fn check_text_delta(&self, delta: &str) -> Vec<String> {
        self.check_delta("text", delta)
    }

    /// 检查思考流增量。命中返回可中断规则名。
    pub fn check_thinking_delta(&self, delta: &str) -> Vec<String> {
        self.check_delta("thinking", delta)
    }

    fn check_delta(&self, stream: &str, delta: &str) -> Vec<String> {
        self.manager
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .check_delta(stream, delta)
    }

    /// `MessageEnd` 时检查工具调用载荷。
    ///
    /// `message` 为最终化（含 transform/harmony 改写后）的 assistant 消息。
    pub fn check_tool_calls(&self, message: &agent_core::AssistantMessage) -> ToolOutcome {
        let calls = tool_calls_of(message);
        if calls.is_empty() {
            return ToolOutcome::None;
        }
        let mut manager = self.manager.lock().unwrap_or_else(PoisonError::into_inner);
        let mut abort: Vec<String> = Vec::new();
        let mut has_reminders = false;
        let mut pending: Vec<(String, String)> = Vec::new(); // (tool_call_id, reminder)
        for (id, name, args) in &calls {
            let digest = digest_for(name, args);
            let path = path_of(args);
            let hits = manager.check_tool_call(name, path.as_deref(), &digest);
            for rule_name in hits {
                match manager.interrupt_mode(&rule_name) {
                    InterruptMode::Always => {
                        if !abort.contains(&rule_name) {
                            abort.push(rule_name);
                        }
                    }
                    InterruptMode::Never => {
                        let Some(body) = manager.rule_body(&rule_name) else {
                            continue;
                        };
                        pending.push((
                            id.clone(),
                            format!(
                                "<system-reminder reason=\"rule_violation\" rule=\"{rule_name}\">\n{body}\n</system-reminder>"
                            ),
                        ));
                    }
                }
            }
        }
        // 尽早释放 manager 锁，再写提醒表（避免跨锁保持）。
        drop(manager);
        if !pending.is_empty() {
            let mut reminders = self
                .reminders
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for (id, reminder) in pending {
                reminders
                    .entry(id)
                    .and_modify(|cur| {
                        cur.push('\n');
                        cur.push_str(&reminder);
                    })
                    .or_insert(reminder);
                has_reminders = true;
            }
            drop(reminders);
        }
        if !abort.is_empty() {
            ToolOutcome::Abort(abort)
        } else if has_reminders {
            ToolOutcome::Reminders
        } else {
            ToolOutcome::None
        }
    }

    /// 取走某工具调用的提醒（执行结果回填时折叠；未执行路径自然丢弃）。
    pub fn take_reminder(&self, tool_call_id: &str) -> Option<String> {
        self.reminders
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(tool_call_id)
    }

    /// 渲染注入消息全文（含持久化标记 + system-interrupt 包装）。
    #[must_use]
    pub fn render_injection(&self, rule_names: &[String]) -> String {
        let mut manager = self.manager.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = format!("{INJECTION_MARKER}{}]", rule_names.join(","));
        out.push('\n');
        for name in rule_names {
            if let Some(body) = manager.rule_body(name) {
                out.push_str("<system-interrupt reason=\"rule_violation\" rule=\"");
                out.push_str(name);
                out.push_str("\">\n");
                out.push_str(body);
                out.push_str("\n</system-interrupt>\n");
                manager.mark_injected(name);
            }
        }
        out
    }
}

/// 解析注入标记：`[ttsr-injection:name1,name2]` → 规则名列表。
#[must_use]
pub fn parse_marker(text: &str) -> Option<Vec<String>> {
    let rest = text.strip_prefix(INJECTION_MARKER)?;
    let end = rest.find(']')?;
    let names = &rest[..end];
    let names: Vec<String> = names
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if names.is_empty() { None } else { Some(names) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::parse_rule;

    fn text_rule(name: &str, condition: &str) -> Rule {
        parse_rule(
            name,
            &format!("---\ncondition: [{condition:?}]\n---\nbody {name}"),
        )
        .unwrap()
    }

    fn coordinator(rules: Vec<Rule>) -> TtsrCoordinator {
        TtsrCoordinator::new(TtsrConfig::default(), rules)
    }

    #[test]
    fn render_injection_has_marker_and_interrupt() {
        let c = coordinator(vec![text_rule("leak", r"Box::leak")]);
        let text = c.render_injection(&["leak".into()]);
        assert!(text.starts_with("[ttsr-injection:leak]"));
        assert!(text.contains("<system-interrupt reason=\"rule_violation\" rule=\"leak\">"));
        assert!(text.contains("body leak"));
        assert!(text.contains("</system-interrupt>"));
    }

    #[test]
    fn marker_roundtrip_restores_suppression() {
        let c = coordinator(vec![text_rule("leak", r"Box::leak")]);
        let text = c.render_injection(&["leak".into()]);
        // 模拟会话加载：标记消息 → restore → 抑制。
        let msgs = vec![AgentMessage::user_text(text)];
        let c2 = coordinator(vec![text_rule("leak", r"Box::leak")]);
        c2.restore_from_messages(&msgs);
        c2.on_turn_start();
        assert!(c2.check_text_delta("Box::leak").is_empty());
    }

    #[test]
    fn parse_marker_extracts_names() {
        assert_eq!(
            parse_marker("[ttsr-injection:a,b] hello"),
            Some(vec!["a".into(), "b".into()])
        );
        assert_eq!(parse_marker("[ttsr-injection:a]"), Some(vec!["a".into()]));
        assert!(parse_marker("plain text").is_none());
        assert!(parse_marker("[ttsr-injection:]").is_none());
    }

    #[test]
    fn tool_always_aborts_never_reminds() {
        let c = coordinator(vec![
            parse_rule(
                "hard",
                "---\nscope: tool:write_file\ncondition: [secret]\n---\nbody hard",
            )
            .unwrap(),
            parse_rule(
                "soft",
                "---\nscope: tool:write_file\ninterruptMode: never\ncondition: [hint]\n---\nbody soft",
            )
            .unwrap(),
        ]);
        c.on_turn_start();
        let hard = agent_core::AssistantMessage {
            content: vec![agent_core::ContentBlock::ToolCall {
                id: "t1".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({ "path": "a.txt", "content": "secret" }),
            }],
            usage: agent_core::Usage::default(),
            model: "m".into(),
            stop_reason: Some(agent_core::StopReason::ToolUse),
            stop_details: None,
        };
        assert_eq!(
            c.check_tool_calls(&hard),
            ToolOutcome::Abort(vec!["hard".into()])
        );

        let soft = agent_core::AssistantMessage {
            content: vec![agent_core::ContentBlock::ToolCall {
                id: "t2".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({ "path": "a.txt", "content": "hint here" }),
            }],
            usage: agent_core::Usage::default(),
            model: "m".into(),
            stop_reason: Some(agent_core::StopReason::ToolUse),
            stop_details: None,
        };
        assert_eq!(c.check_tool_calls(&soft), ToolOutcome::Reminders);
        let rem = c.take_reminder("t2").expect("提醒应暂存");
        assert!(rem.contains("<system-reminder reason=\"rule_violation\" rule=\"soft\">"));
        assert!(c.take_reminder("t2").is_none(), "取走后不应重复");
    }

    #[test]
    fn tool_abort_marks_injected_once() {
        let c = coordinator(vec![
            parse_rule(
                "hard",
                "---\nscope: tool:write_file\ncondition: [secret]\n---\nbody",
            )
            .unwrap(),
        ]);
        c.on_turn_start();
        let msg = |content: &str| agent_core::AssistantMessage {
            content: vec![agent_core::ContentBlock::ToolCall {
                id: "t".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({ "path": "a.txt", "content": content }),
            }],
            usage: agent_core::Usage::default(),
            model: "m".into(),
            stop_reason: Some(agent_core::StopReason::ToolUse),
            stop_details: None,
        };
        assert_eq!(
            c.check_tool_calls(&msg("secret")),
            ToolOutcome::Abort(vec!["hard".into()])
        );
        // 注入（mark）后：同轮内不再触发（once 抑制）。
        let _ = c.render_injection(&["hard".into()]);
        assert_eq!(c.check_tool_calls(&msg("secret")), ToolOutcome::None);
    }
}
