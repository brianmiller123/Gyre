//! TTSR 匹配引擎：按流缓冲累积增量，逐规则门控（作用域 / 路径 / 重复策略）后正则
//! 或 ast-grep 匹配。
//!
//! 移植 oh-my-pi `export/ttsr.ts` `TtsrManager` 的同步子集：每流（text / thinking /
//! tool:<name>）一个缓冲；`check_delta` 追加增量后匹配；`check_snapshot` 整体替换后匹配
//! （工具载荷一次性到位）。全部同步、无 IO（ast-grep 对内存文本直接匹配）。

use std::collections::{HashMap, HashSet};

use crate::rule::{InterruptMode, Repeat, Rule};
use agent_core::message::ContentBlock;

/// 匹配结果：命中的规则名列表（去重，保持注册顺序）。
pub type MatchResult = Vec<String>;

/// 流式匹配器。线程安全：整个 session 单实例，经 `Mutex` 串行访问。
#[derive(Debug, Default)]
pub struct TtsrManager {
    /// 全部规则（已过滤 disabled）。
    rules: Vec<Rule>,
    /// 已注入规则名（repeat=once 的规则注入后抑制；`restore` 恢复持久化状态）。
    injected: HashSet<String>,
    /// 每流缓冲（stream key → 累积文本）。
    buffers: HashMap<String, String>,
    /// 当前轮号（每轮模型调用 +1；供后续 after-gap 策略使用）。
    turn: u64,
}

impl TtsrManager {
    /// 构造匹配器。
    ///
    /// `disabled` 为配置层禁用的规则名（`ttsr.disabledRules`）。
    pub fn new(rules: Vec<Rule>, disabled: &[String]) -> Self {
        let disabled: HashSet<&str> = disabled.iter().map(String::as_str).collect();
        let mut kept = Vec::new();
        for r in rules {
            if disabled.contains(r.name.as_str()) {
                tracing::info!("ttsr: 规则 {} 被配置禁用", r.name);
            } else {
                kept.push(r);
            }
        }
        Self {
            rules: kept,
            injected: HashSet::new(),
            buffers: HashMap::new(),
            turn: 0,
        }
    }

    /// 本轮开始：清空缓冲、轮号 +1。
    pub fn on_turn_start(&mut self) {
        self.turn += 1;
        self.buffers.clear();
    }

    /// 恢复持久化的注入状态（会话加载时扫描 `[ttsr-injection:…]` 标记消息）。
    /// 仅 `repeat=once` 的规则被抑制（`always` 规则天然不抑制）。
    pub fn restore(&mut self, names: &[String]) {
        for name in names {
            if self
                .rules
                .iter()
                .any(|r| r.name == *name && r.repeat == Repeat::Once)
            {
                self.injected.insert(name.clone());
            }
        }
    }

    /// 全部规则名（注册顺序）。
    #[must_use]
    pub fn rule_names(&self) -> Vec<String> {
        self.rules.iter().map(|r| r.name.clone()).collect()
    }

    /// 规则是否已注入（repeat=once 抑制判定）。
    #[must_use]
    pub fn is_injected(&self, name: &str) -> bool {
        self.injected.contains(name)
    }

    /// 检查流式增量（text / thinking）。命中返回规则名（按注册顺序，去重）。
    ///
    /// 文本/思考作用域 v1 仅返回可中断（`interruptMode: always`）的规则——命中即由
    /// 调用方中断流并注入重试。
    pub fn check_delta(&mut self, stream: &str, delta: &str) -> MatchResult {
        if delta.is_empty() {
            return Vec::new();
        }
        let buf = self.buffers.entry(stream.to_string()).or_default();
        buf.push_str(delta);
        self.match_buffer(stream)
    }

    /// 检查快照（整体替换缓冲后匹配；用于工具载荷一次性到位）。
    pub fn check_snapshot(&mut self, stream: &str, text: &str) -> MatchResult {
        self.buffers.insert(stream.to_string(), text.to_string());
        self.match_buffer(stream)
    }

    /// 检查单次工具调用（MessageEnd 快照）：按工具名 + 路径门 + 载荷 digest 匹配。
    ///
    /// 返回命中的规则名（含 `interruptMode: never` 的规则——调用方据此决定折叠提醒
    /// 还是丢弃重试）。
    pub fn check_tool_call(&mut self, tool: &str, path: Option<&str>, digest: &str) -> MatchResult {
        let mut hits = Vec::new();
        for rule in &self.rules {
            if !self.can_trigger(rule) {
                continue;
            }
            if !rule.covers_tool(tool, path) {
                continue;
            }
            if rule.matches_condition(digest) {
                hits.push(rule.name.clone());
            } else if let Some(pattern) = &rule.ast_condition {
                if let Some(lang) =
                    path.and_then(|p| agent_ast::SupportLang::from_path(std::path::Path::new(p)))
                {
                    if agent_ast::search(
                        digest,
                        lang,
                        pattern,
                        agent_ast::AstMatchStrictness::Smart,
                    )
                    .is_ok_and(|m| !m.is_empty())
                    {
                        hits.push(rule.name.clone());
                    }
                }
            }
        }
        hits
    }

    /// 标记规则已注入（repeat=once 的规则自此抑制）。
    pub fn mark_injected(&mut self, name: &str) {
        if self
            .rules
            .iter()
            .any(|r| r.name == name && r.repeat == Repeat::Once)
        {
            self.injected.insert(name.to_string());
        }
    }

    /// 取规则正文（注入渲染用）。
    #[must_use]
    pub fn rule_body(&self, name: &str) -> Option<&str> {
        self.rules
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.body.as_str())
    }

    /// 规则中断模式。
    #[must_use]
    pub fn interrupt_mode(&self, name: &str) -> InterruptMode {
        self.rules
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.interrupt_mode)
            .unwrap_or_default()
    }

    /// 对指定流缓冲执行门控 + 匹配。
    fn match_buffer(&self, stream: &str) -> MatchResult {
        let Some(buf) = self.buffers.get(stream) else {
            return Vec::new();
        };
        let mut hits = Vec::new();
        for rule in &self.rules {
            if !self.can_trigger(rule) {
                continue;
            }
            if !rule.covers_stream(stream) {
                continue;
            }
            // 文本/思考作用域 v1：仅 Always 中断（Never 无流式折叠通道）。
            if rule.interrupt_mode == InterruptMode::Never {
                continue;
            }
            if rule.matches_condition(buf) {
                hits.push(rule.name.clone());
            }
        }
        hits
    }

    /// 重复策略门：repeat=once 且已注入 → 抑制。
    fn can_trigger(&self, rule: &Rule) -> bool {
        rule.repeat == Repeat::Always || !self.injected.contains(&rule.name)
    }
}

/// 从工具调用参数提取「matcher digest」（omp `matcherDigest`）：写/编辑载荷优先，
/// 其余工具回退整参数 JSON。
#[must_use]
pub fn digest_for(tool: &str, arguments: &serde_json::Value) -> String {
    let pick = |key: &str| {
        arguments
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    match tool {
        "write_file" | "write" => pick("content"),
        "apply_hashline" | "edit" => pick("patch"),
        "run_command" | "bash" => pick("command"),
        "grep" | "search" => pick("pattern"),
        _ => None,
    }
    .unwrap_or_else(|| serde_json::to_string(arguments).unwrap_or_default())
}

/// 从工具调用参数提取路径（供 glob 门与 ast 语言推断）。
#[must_use]
pub fn path_of(arguments: &serde_json::Value) -> Option<String> {
    arguments
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// 从 assistant 消息抽取工具调用元组（id, name, args）。
#[must_use]
pub fn tool_calls_of(
    message: &agent_core::AssistantMessage,
) -> Vec<(String, String, serde_json::Value)> {
    message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some((id.clone(), name.clone(), arguments.clone())),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::parse_rule;

    fn manager(rules: Vec<Rule>) -> TtsrManager {
        TtsrManager::new(rules, &[])
    }

    fn text_rule(name: &str, condition: &str) -> Rule {
        parse_rule(
            name,
            &format!("---\ncondition: [{condition:?}]\n---\nbody {name}"),
        )
        .unwrap()
    }

    #[test]
    fn delta_accumulates_and_matches() {
        let mut m = manager(vec![text_rule("leak", r"Box::leak")]);
        m.on_turn_start();
        assert!(m.check_delta("text", "use ").is_empty());
        assert_eq!(m.check_delta("text", "Box::leak"), vec!["leak"]);
        // 缓冲跨 delta 累积：再次匹配仍命中（缓冲未清）。
        assert_eq!(m.check_delta("text", "!"), vec!["leak"]);
    }

    #[test]
    fn thinking_stream_isolated_from_text() {
        let mut m = manager(vec![text_rule("leak", r"Box::leak")]);
        m.on_turn_start();
        assert!(m.check_delta("thinking", "Box::leak").is_empty());
        assert_eq!(m.check_delta("text", "Box::leak"), vec!["leak"]);
    }

    #[test]
    fn once_rule_suppressed_after_injection() {
        let mut m = manager(vec![text_rule("leak", r"Box::leak")]);
        m.on_turn_start();
        assert_eq!(m.check_delta("text", "Box::leak"), vec!["leak"]);
        m.mark_injected("leak");
        m.on_turn_start(); // 新轮
        assert!(m.check_delta("text", "Box::leak").is_empty());
    }

    #[test]
    fn always_rule_never_suppressed() {
        let mut m = manager(vec![
            parse_rule(
                "always-rule",
                "---\nrepeat: always\ncondition: [x]\n---\nbody",
            )
            .unwrap(),
        ]);
        m.on_turn_start();
        assert_eq!(m.check_delta("text", "x"), vec!["always-rule"]);
        m.mark_injected("always-rule");
        assert_eq!(m.check_delta("text", "x"), vec!["always-rule"]);
    }

    #[test]
    fn restore_suppresses_only_once_rules() {
        let mut m = manager(vec![
            text_rule("once-rule", "x"),
            parse_rule("always-rule", "---\nrepeat: always\ncondition: [x]\n---\nb").unwrap(),
        ]);
        m.restore(&["once-rule".into(), "always-rule".into()]);
        m.on_turn_start();
        assert_eq!(m.check_delta("text", "x"), vec!["always-rule"]);
    }

    #[test]
    fn disabled_rules_filtered() {
        let mut m = TtsrManager::new(
            vec![text_rule("a", "x"), text_rule("b", "x")],
            &["a".into()],
        );
        m.on_turn_start();
        assert_eq!(m.check_delta("text", "x"), vec!["b"]);
    }

    #[test]
    fn tool_call_matches_digest_and_path_glob() {
        let r = parse_rule(
            "no-leak",
            "---\nscope: tool:write_file\ncondition: [Box::leak]\n---\nbody",
        )
        .unwrap();
        let mut m = manager(vec![r]);
        m.on_turn_start();
        let args = serde_json::json!({ "path": "src/lib.rs", "content": "let x = Box::leak(y);" });
        assert_eq!(
            m.check_tool_call(
                "write_file",
                Some("src/lib.rs"),
                &digest_for("write_file", &args)
            ),
            vec!["no-leak"]
        );
        // 其他工具不匹配。
        let run = serde_json::json!({ "command": "echo Box::leak" });
        assert!(
            m.check_tool_call("run_command", None, &digest_for("run_command", &run))
                .is_empty()
        );
    }

    #[test]
    fn tool_glob_gates_path() {
        let r = parse_rule(
            "rust-only",
            "---\nscope: tool:write_file(*.rs)\ncondition: [x]\n---\nbody",
        )
        .unwrap();
        let mut m = manager(vec![r]);
        m.on_turn_start();
        let args = serde_json::json!({ "path": "src/lib.rs", "content": "x" });
        assert_eq!(
            m.check_tool_call(
                "write_file",
                Some("src/lib.rs"),
                &digest_for("write_file", &args)
            ),
            vec!["rust-only"]
        );
        let py = serde_json::json!({ "path": "src/lib.py", "content": "x" });
        assert!(
            m.check_tool_call(
                "write_file",
                Some("src/lib.py"),
                &digest_for("write_file", &py)
            )
            .is_empty()
        );
    }

    #[test]
    fn any_tool_scope_matches_every_tool() {
        let r = parse_rule("any", "---\nscope: tool\ncondition: [secret]\n---\nbody").unwrap();
        let mut m = manager(vec![r]);
        m.on_turn_start();
        let args = serde_json::json!({ "path": "a.txt", "content": "secret" });
        assert_eq!(
            m.check_tool_call(
                "write_file",
                Some("a.txt"),
                &digest_for("write_file", &args)
            ),
            vec!["any"]
        );
        let run = serde_json::json!({ "command": "cat secret" });
        assert_eq!(
            m.check_tool_call("run_command", None, &digest_for("run_command", &run)),
            vec!["any"]
        );
    }

    #[test]
    fn ast_condition_matches_structured_pattern() {
        let r = parse_rule(
            "no-clear-timeout-if",
            "---\nscope: tool:write_file\nastCondition: ['if ($X) clearTimeout($X)']\n---\nbody",
        )
        .unwrap();
        let mut m = manager(vec![r]);
        m.on_turn_start();
        let bad = serde_json::json!({
            "path": "src/app.js",
            "content": "if (id) clearTimeout(id);"
        });
        assert_eq!(
            m.check_tool_call(
                "write_file",
                Some("src/app.js"),
                &digest_for("write_file", &bad)
            ),
            vec!["no-clear-timeout-if"]
        );
        // 不满足 metavariable 同一性：不匹配。
        let ok = serde_json::json!({
            "path": "src/app.js",
            "content": "if (a) clearTimeout(b);"
        });
        assert!(
            m.check_tool_call(
                "write_file",
                Some("src/app.js"),
                &digest_for("write_file", &ok)
            )
            .is_empty()
        );
    }

    #[test]
    fn never_interrupt_text_rule_does_not_abort() {
        let r = parse_rule(
            "quiet",
            "---\ninterruptMode: never\ncondition: [x]\n---\nbody",
        )
        .unwrap();
        let mut m = manager(vec![r]);
        m.on_turn_start();
        assert!(m.check_delta("text", "x").is_empty());
    }
}
