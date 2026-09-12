//! 内置规则集：随二进制分发的语言约定规则（移植 oh-my-pi `discovery/builtin-rules/`）。
//!
//! 来源与许可：内容取自 oh-my-pi（<https://github.com/can1357/oh-my-pi>）
//! `packages/coding-agent/src/discovery/builtin-rules/*.md`，
//! MIT License，Copyright (c) 2025 Mario Zechner / 2025-2026 Can Bölük / 2026 Stencil Labs, Inc.
//! 规则正文按原样内嵌（`include_str!`），仅在加载时做**工具名映射**（见下）。
//!
//! 27 条规则覆盖 Go（8）/ Rust（6）/ TypeScript（13），全部 `interruptMode: never`：
//! 命中后作为**工具结果前导提醒**折叠，不打断模型流（不会造成「编辑被拒」的意外中断）。
//!
//! 装配语义（与 omp `builtin-defaults` provider 一致）：
//! - **最低优先级**：用户/项目规则（`.gyre/rules/*.md`）中**同名**者覆盖内置副本；
//! - `[ttsr] builtin_rules = false` 关闭整套；`[ttsr] disabled_rules = ["rs-box-leak"]`
//!   按名剔除单条；
//! - 规则零上下文成本：未命中不注入任何 token。
//!
//! 工具名映射（omp 工具名 → Gyre 工具名，其余不变）：
//! - `tool:edit(GLOB)` → `tool:apply_hashline(GLOB)`（Gyre 的编辑面是 hashline 补丁）
//! - `tool:write(GLOB)` → `tool:write_file(GLOB)`
//!
//! 映射在解析后对 [`crate::RuleScope::Tool`] 的 `name` 就地改写；未识别的工具名保持原样
//! （宁可不命中，也不误映射到无关工具）。

use crate::rule::{Rule, RuleScope, parse_rule};

/// 内置规则源：`(name, markdown)`。按名升序（确定性）。
pub const BUILTIN_RULE_SOURCES: &[(&str, &str)] = &[
    ("go-add-cleanup", include_str!("../rules/go-add-cleanup.md")),
    ("go-bench-loop", include_str!("../rules/go-bench-loop.md")),
    (
        "go-exp-promoted",
        include_str!("../rules/go-exp-promoted.md"),
    ),
    ("go-ioutil", include_str!("../rules/go-ioutil.md")),
    (
        "go-join-hostport",
        include_str!("../rules/go-join-hostport.md"),
    ),
    ("go-new-expr", include_str!("../rules/go-new-expr.md")),
    ("go-rand-v2", include_str!("../rules/go-rand-v2.md")),
    ("go-range-int", include_str!("../rules/go-range-int.md")),
    ("rs-box-leak", include_str!("../rules/rs-box-leak.md")),
    (
        "rs-future-prelude",
        include_str!("../rules/rs-future-prelude.md"),
    ),
    ("rs-lazylock", include_str!("../rules/rs-lazylock.md")),
    (
        "rs-match-ergonomics",
        include_str!("../rules/rs-match-ergonomics.md"),
    ),
    ("rs-parking-lot", include_str!("../rules/rs-parking-lot.md")),
    ("rs-result-type", include_str!("../rules/rs-result-type.md")),
    ("ts-bare-catch", include_str!("../rules/ts-bare-catch.md")),
    ("ts-import-type", include_str!("../rules/ts-import-type.md")),
    ("ts-no-any", include_str!("../rules/ts-no-any.md")),
    (
        "ts-no-deprecated-leftovers",
        include_str!("../rules/ts-no-deprecated-leftovers.md"),
    ),
    (
        "ts-no-dynamic-import",
        include_str!("../rules/ts-no-dynamic-import.md"),
    ),
    (
        "ts-no-inline-cast-access",
        include_str!("../rules/ts-no-inline-cast-access.md"),
    ),
    (
        "ts-no-local-is-record",
        include_str!("../rules/ts-no-local-is-record.md"),
    ),
    (
        "ts-no-return-type",
        include_str!("../rules/ts-no-return-type.md"),
    ),
    (
        "ts-no-test-timers",
        include_str!("../rules/ts-no-test-timers.md"),
    ),
    (
        "ts-no-tiny-functions",
        include_str!("../rules/ts-no-tiny-functions.md"),
    ),
    (
        "ts-promise-with-resolvers",
        include_str!("../rules/ts-promise-with-resolvers.md"),
    ),
    (
        "ts-redundant-clear-guard",
        include_str!("../rules/ts-redundant-clear-guard.md"),
    ),
    ("ts-set-map", include_str!("../rules/ts-set-map.md")),
];

/// 解析全部内置规则（失败的单条只告警并跳过，不影响其余规则）。
///
/// 解析结果不缓存：调用方（`load_rules`）在进程启动时聚合一次即可。
#[must_use]
pub fn builtin_rules() -> Vec<Rule> {
    let mut out = Vec::with_capacity(BUILTIN_RULE_SOURCES.len());
    for (name, content) in BUILTIN_RULE_SOURCES {
        match parse_rule(name, content) {
            Ok(rule) => out.push(map_tool_names(rule)),
            Err(e) => tracing::warn!("ttsr: 内置规则 {name} 解析失败（已跳过）: {e}"),
        }
    }
    out
}

/// omp 工具名 → Gyre 工具名（见模块文档）。
#[must_use]
pub fn map_tool_name(name: &str) -> &str {
    match name {
        "edit" => "apply_hashline",
        "write" => "write_file",
        other => other,
    }
}

/// 就地改写规则的工具作用域名。
fn map_tool_names(mut rule: Rule) -> Rule {
    for scope in &mut rule.scopes {
        if let RuleScope::Tool { name, .. } = scope {
            let mapped = map_tool_name(name).to_string();
            *name = mapped;
        }
    }
    rule
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::InterruptMode;
    use std::collections::HashSet;

    /// 全部内置规则必须可解析（正则/glob/scope 全部合法）——回归护栏：
    /// 上游规则正文或 frontmatter 变化导致解析失败时立刻可见。
    #[test]
    fn all_builtin_rules_parse_and_are_non_interrupting() {
        let rules = builtin_rules();
        assert_eq!(rules.len(), BUILTIN_RULE_SOURCES.len(), "不应有解析失败");
        assert_eq!(rules.len(), 27, "内置规则条数（Go 8 / Rust 6 / TS 13）");
        for r in &rules {
            assert!(r.is_matchable(), "{} 至少应有一个条件", r.name);
            assert_eq!(
                r.interrupt_mode,
                InterruptMode::Never,
                "{} 必须是非中断规则（折叠为提醒）",
                r.name
            );
        }
        // 名字唯一（同名覆盖语义的前提）。
        let names: HashSet<&str> = rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names.len(), rules.len(), "内置规则名不得重复");
    }

    /// 工具名映射：omp `edit`/`write` → Gyre `apply_hashline`/`write_file`，
    /// 且映射后的工具名全部落在 Gyre 真实工具面上（避免静默失效）。
    #[test]
    fn tool_names_map_to_gyre_tools() {
        let known: HashSet<&str> = [
            "read_file",
            "write_file",
            "apply_hashline",
            "replace_block",
            "ast_rewrite",
            "run_command",
        ]
        .into_iter()
        .collect();
        let mut saw_mapped = false;
        for r in builtin_rules() {
            for scope in &r.scopes {
                if let RuleScope::Tool { name, .. } = scope {
                    assert!(
                        known.contains(name.as_str()),
                        "{} 的作用域工具 `{name}` 不是 Gyre 工具名",
                        r.name
                    );
                    if name == "apply_hashline" || name == "write_file" {
                        saw_mapped = true;
                    }
                }
            }
        }
        assert!(saw_mapped, "至少应有一条规则命中编辑/写入工具");
        assert_eq!(map_tool_name("edit"), "apply_hashline");
        assert_eq!(map_tool_name("write"), "write_file");
        assert_eq!(map_tool_name("read"), "read");
    }

    /// 作用域保留原始路径 glob（映射不得丢失 glob 门控）。
    #[test]
    fn scope_globs_survive_mapping() {
        let rules = builtin_rules();
        let rs = rules
            .iter()
            .find(|r| r.name == "rs-result-type")
            .expect("rs-result-type 应在内置集中");
        let globs: Vec<&str> = rs
            .scopes
            .iter()
            .filter_map(|s| match s {
                RuleScope::Tool { globs, .. } => Some(globs.as_slice()),
                _ => None,
            })
            .flatten()
            .map(String::as_str)
            .collect();
        assert!(globs.contains(&"*.rs"), "{globs:?}");
    }
}
