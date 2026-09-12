//! # agent-ttsr
//!
//! 时间旅行流规则（Time-Traveling Stream Rules）：规则不写进 system prompt（零上下文
//! 成本），而是对模型**流式输出**实时匹配——正则匹配文本/思考增量、ast-grep 匹配
//! 写/编辑载荷。命中即中断流，注入规则为隐藏消息后从同一轮重试（"时间旅行"）。
//!
//! 移植自 oh-my-pi `export/ttsr.ts` + `session/ttsr-coordinator.ts`（设计详见
//! `docs/oh-my-pi-feature-analysis.md` §1）。与 oh-my-pi 的关键对齐：
//! - 规则文件为 Markdown（frontmatter + 正文），目录 `<cwd>/.gyre/rules/*.md`；
//! - 注入抑制状态以 `[ttsr-injection:…]` 标记消息持久化，压缩/恢复后不重复注入；
//! - 文本/思考作用域 v1 仅支持中断（`interruptMode: always`）；工具作用域同时支持
//!   `always`（丢弃重试）与 `never`（折叠进工具结果提醒）；
//! - Rust `regex` crate 线性时间匹配，无 JS 版 `ReDoS` 风险。
//!
//! ```toml
//! # .gyre/rules/no-box-leak.md
//! ---
//! name: no-box-leak
//! condition: ["(?i)Box::leak"]
//! scope: [text, tool:write_file]
//! ---
//! 禁止在生产代码路径使用 `Box::leak`；请改用 `Arc<str>`。
//! ```

pub mod builtin;
pub mod coordinator;
pub mod frontmatter;
pub mod matcher;
pub mod rule;

pub use builtin::{BUILTIN_RULE_SOURCES, builtin_rules};
pub use coordinator::{INJECTION_MARKER, ToolOutcome, TtsrConfig, TtsrCoordinator, parse_marker};
pub use matcher::{TtsrManager, digest_for, path_of, tool_calls_of};
pub use rule::{InterruptMode, Repeat, Rule, RuleScope, discover_rules, parse_rule};

use std::path::Path;

/// 汇总规则集：内置规则（可选） + `<dir>` 下的用户/项目规则。
///
/// 遮蔽语义（与 omp `builtin-defaults` provider 一致）：**同名以用户/项目规则为准**——
/// 内置副本被替换；不同名则追加。`disabled_rules` 的按名过滤在
/// [`TtsrCoordinator::new`] 内统一执行，覆盖内置与用户规则。
#[must_use]
pub fn load_rules(dir: &Path, builtin_enabled: bool) -> Vec<Rule> {
    let mut rules = if builtin_enabled {
        builtin::builtin_rules()
    } else {
        Vec::new()
    };
    for discovered in discover_rules(dir) {
        match rules.iter().position(|r| r.name == discovered.name) {
            Some(pos) => {
                tracing::info!(
                    "ttsr: 规则 {} 覆盖同名内置规则（{}）",
                    discovered.name,
                    discovered.name
                );
                rules[pos] = discovered;
            }
            None => rules.push(discovered),
        }
    }
    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-ttsr-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// H44：内置规则随装配进入规则集，用户规则追加；同名用户规则**覆盖**内置副本。
    #[test]
    fn load_rules_merges_builtin_and_project_with_name_override() {
        let dir = tmpdir("load");
        let builtin_count = builtin_rules().len();
        assert!(builtin_count > 0);

        // 项目规则：一条与内置同名（rs-box-leak）、一条新名。
        std::fs::write(
            dir.join("rs-box-leak.md"),
            "---\ncondition: ['(?i)Box::leak']\nscope: [text]\n---\n项目版：禁止 Box::leak（覆盖内置）。",
        )
        .unwrap();
        std::fs::write(
            dir.join("project-only.md"),
            "---\ncondition: ['(?i)project_marker']\nscope: [text]\n---\n项目独有规则。",
        )
        .unwrap();

        let rules = load_rules(&dir, true);
        assert_eq!(
            rules.len(),
            builtin_count + 1,
            "同名覆盖不增条目，新名追加一条"
        );
        let overridden = rules
            .iter()
            .find(|r| r.name == "rs-box-leak")
            .expect("内置同名项仍在（被告覆盖）");
        assert!(
            overridden.body.contains("项目版"),
            "同名应以项目规则为准: {}",
            overridden.body
        );
        assert!(rules.iter().any(|r| r.name == "project-only"));

        // builtin_rules = false → 只要项目规则。
        let only_project = load_rules(&dir, false);
        assert_eq!(only_project.len(), 2, "{only_project:?}");
        assert!(only_project.iter().all(|r| r.name != "ts-no-any"));

        // 无项目目录 → 纯内置集。
        let empty = tmpdir("empty");
        let builtin_only = load_rules(&empty, true);
        assert_eq!(builtin_only.len(), builtin_count);
    }

    fn tool_call(name: &str) -> agent_core::AssistantMessage {
        agent_core::AssistantMessage {
            content: vec![agent_core::ContentBlock::ToolCall {
                id: "t1".into(),
                name: name.into(),
                arguments: serde_json::json!({ "path": "src/a.rs", "input": "Box::leak(x)" }),
                signature: None,
            }],
            usage: agent_core::Usage::default(),
            model: "m".into(),
            stop_reason: Some(agent_core::StopReason::ToolUse),
            stop_details: None,
        }
    }

    /// H44：`disabled_rules` 与 `builtin_rules = false` 对内置规则同样生效
    /// （内置 `rs-box-leak` 命中 Rust 编辑 → 折叠为工具结果提醒）。
    #[test]
    fn disabled_rules_filter_builtins_through_coordinator() {
        let dir = tmpdir("disabled");
        let enabled = TtsrCoordinator::new(TtsrConfig::default(), load_rules(&dir, true));
        enabled.on_turn_start();
        assert_eq!(
            enabled.check_tool_calls(&tool_call("apply_hashline")),
            ToolOutcome::Reminders,
            "内置规则应命中 apply_hashline(*.rs)"
        );

        let disabled = TtsrCoordinator::new(
            TtsrConfig {
                disabled_rules: vec!["rs-box-leak".into()],
                ..Default::default()
            },
            load_rules(&dir, true),
        );
        disabled.on_turn_start();
        assert_eq!(
            disabled.check_tool_calls(&tool_call("apply_hashline")),
            ToolOutcome::None,
            "被禁用的内置规则不应命中"
        );

        let off = TtsrCoordinator::new(TtsrConfig::default(), load_rules(&dir, false));
        off.on_turn_start();
        assert_eq!(
            off.check_tool_calls(&tool_call("apply_hashline")),
            ToolOutcome::None,
            "builtin_rules = false 时整套内置规则关闭"
        );
    }
}
