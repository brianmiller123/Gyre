//! 工具调用配对保护（移植 oh-my-pi `compaction/tool-protection.ts`）。
//!
//! OpenAI 兼容协议要求每条 `tool` 角色消息前必有对应的 `tool_calls`，反之亦然——配对断裂
//! 会触发 400（`Assistant message has ... with 'tool_calls'` / `no corresponding tool_calls`）。
//! 压缩（prune 的 supersede 取代、跨窗口裁剪）、恢复、分支编辑都可能引入孤立配对。本模块
//! 集中该防护（替代此前散落在 [`crate::compaction`] 各分支的内联逻辑）：
//!
//! - [`sanitize_agent_messages`]：内部 [`AgentMessage`] 序列的配对强制器（**统一入口**），
//!   剥离无结果的 `ToolCall`、丢弃无调用的 `ToolResult`、丢弃因此变空的助手消息。在
//!   [`crate::compaction::Compactor::prune`] 出口作安全网调用，使压缩后的**持久化**日志与
//!   provider 视图同样洁净——此前仅 `sanitize_provider_messages` 在 build 时清理 provider 视图，
//!   持久化日志残留孤立 `ToolCall`，resume 后才在 build 时被剥离。
//! - skill read 保护辅助（[`skill_read_call_ids`] 等）：识别 `read_file skill://...` 的受保护
//!   结果，prune 时不裁（按需加载的 skill 内容不在压缩中丢失）。
//!
//! `compaction/tool-protection.ts`:
//! <https://github.com/can1357/oh-my-pi/blob/master/packages/agent/src/compaction/tool-protection.ts>

use std::collections::HashSet;

use agent_core::{AgentMessage, ContentBlock};

/// 工具保护规则：按（工具名 + 参数）判定某次工具调用是否受压缩保护（移植 oh-my-pi
/// `ProtectedToolMatcher = string | (ctx) => boolean`）。
///
/// 装配层可注入自定义规则（如保护 `plans://` read、特定工具结果）；内置 [`SkillReadRule`]
/// 保护 `read_file skill://...`（按需加载的 skill 内容不在压缩中丢失）。压缩时，任一规则
/// 命中即视为受保护——其 `ToolResult` 与发起 `ToolCall` 均不被裁剪。
pub trait ToolProtectionRule: Send + Sync {
    /// `tool_name` 为工具名（如 `read_file`），`args` 为本次调用参数。
    fn matches(&self, tool_name: &str, args: &serde_json::Value) -> bool;
}

/// 内置规则：保护 `read_file skill://...`（按需加载的 skill 内容不在压缩中丢失）。
#[derive(Debug, Default, Clone, Copy)]
pub struct SkillReadRule;

impl ToolProtectionRule for SkillReadRule {
    fn matches(&self, tool_name: &str, args: &serde_json::Value) -> bool {
        tool_name == "read_file"
            && args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|p| p.starts_with("skill://"))
    }
}

/// 收集日志中受任一规则匹配的工具调用 tool_call_id（压缩时不裁剪）。规则为空时返回空集
/// （无保护）；调用方通常至少包含 [`SkillReadRule`]（compact 时由 host 决定是否额外注入）。
pub fn protected_call_ids(
    log: &[AgentMessage],
    rules: &[Box<dyn ToolProtectionRule>],
) -> HashSet<String> {
    let mut ids = HashSet::new();
    if rules.is_empty() {
        return ids;
    }
    for m in log {
        let AgentMessage::Assistant(a) = m else {
            continue;
        };
        for block in &a.content {
            if let ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } = block
            {
                if rules.iter().any(|r| r.matches(name, arguments)) {
                    ids.insert(id.clone());
                }
            }
        }
    }
    ids
}

/// 收集日志中 `read_file skill://...` 调用的 tool_call_id（受保护，prune 时不裁剪）。
/// 等价于 [`protected_call_ids`] 仅含 [`SkillReadRule`]；保留以兼容既有调用与测试。
pub(crate) fn skill_read_call_ids(log: &[AgentMessage]) -> HashSet<String> {
    protected_call_ids(log, &[Box::new(SkillReadRule)])
}

/// 消息是否为受保护的 skill read 结果。
pub(crate) fn is_protected_skill_message(m: &AgentMessage, ids: &HashSet<String>) -> bool {
    matches!(m, AgentMessage::ToolResult(t) if ids.contains(&t.tool_call_id))
}

/// 助手消息是否含任一指定 tool_call_id（用于保留「被保留 ToolResult」的发起消息，避免孤立）。
pub(crate) fn assistant_has_any_call(m: &AgentMessage, ids: &HashSet<String>) -> bool {
    let AgentMessage::Assistant(a) = m else {
        return false;
    };
    a.content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolCall { id, .. } if ids.contains(id)))
}

/// 强制 `AgentMessage` 序列满足 tool_use / tool_result 配对约束（压缩安全网，统一入口）。
///
/// 镜像 `sanitize_provider_messages`（后者作用于 provider 线协议、在 build 时调用）；本函数
/// 作用于内部富消息、在压缩出口调用，使持久化日志同样洁净：
/// - **悬空 ToolCall**：assistant 的某 `ToolCall` 在序列中找不到匹配的 `ToolResult`
///   （如 supersede 取代旧结果、跨压缩窗口裁剪）→ 剥离该块；若助手消息因此变空则整体丢弃。
/// - **孤立 ToolResult**：`ToolResult` 的发起 `ToolCall` 不在任一保留的助手消息中 → 丢弃
///   （防御异常序列；正常 prune 不会产生）。
///
/// 假设助手消息位于其 `ToolResult` 之前（良构日志的天然顺序，与 `sanitize_provider_messages`
/// 一致）。已配对的序列为恒等操作（no-op）。
pub(crate) fn sanitize_agent_messages(msgs: Vec<AgentMessage>) -> Vec<AgentMessage> {
    // 仍在序列中存在 ToolResult 的 tool_call_id 全集：用于判断 ToolCall 是否悬空。
    let ids_with_result: HashSet<String> = msgs
        .iter()
        .filter_map(|m| match m {
            AgentMessage::ToolResult(t) => Some(t.tool_call_id.clone()),
            _ => None,
        })
        .collect();

    let mut out: Vec<AgentMessage> = Vec::with_capacity(msgs.len());
    // 已被「保留的 assistant 消息」声明的 tool_call_id：用于放行 ToolResult。
    let mut declared: HashSet<String> = HashSet::new();
    for m in msgs {
        match m {
            AgentMessage::Assistant(mut a) => {
                a.content
                    .retain(|b| match b {
                        ContentBlock::ToolCall { id, .. } => ids_with_result.contains(id),
                        _ => true,
                    });
                if a.content.is_empty() {
                    // 纯 tool-call 助手消息且其调用全部悬空：丢弃，避免空助手消息。
                    continue;
                }
                for b in &a.content {
                    if let ContentBlock::ToolCall { id, .. } = b {
                        declared.insert(id.clone());
                    }
                }
                out.push(AgentMessage::Assistant(a));
            }
            AgentMessage::ToolResult(t) => {
                if declared.contains(&t.tool_call_id) {
                    out.push(AgentMessage::ToolResult(t));
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AssistantMessage, ToolResult, ToolResultMessage, Usage};
    use serde_json::json;

    fn call(id: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: id.into(),
                name: "read_file".into(),
                arguments: json!({ "path": "a.txt" }),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
            stop_details: None,
        })
    }

    fn call_with_text(id: &str, text: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Text { text: text.into() },
                ContentBlock::ToolCall {
                    id: id.into(),
                    name: "read_file".into(),
                    arguments: json!({ "path": "a.txt" }),
                },
            ],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
            stop_details: None,
        })
    }

    fn multi_call(id1: &str, id2: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::ToolCall {
                    id: id1.into(),
                    name: "read_file".into(),
                    arguments: json!({ "path": "a.txt" }),
                },
                ContentBlock::ToolCall {
                    id: id2.into(),
                    name: "read_file".into(),
                    arguments: json!({ "path": "b.txt" }),
                },
            ],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
            stop_details: None,
        })
    }

    fn res(id: &str, text: &str) -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            result: ToolResult::text(text),
        })
    }

    /// 收集输出中保留的 ToolCall id（顺序敏感）。
    fn call_ids(msgs: &[AgentMessage]) -> Vec<String> {
        let mut v = Vec::new();
        for m in msgs {
            if let AgentMessage::Assistant(a) = m {
                for b in &a.content {
                    if let ContentBlock::ToolCall { id, .. } = b {
                        v.push(id.clone());
                    }
                }
            }
        }
        v
    }

    #[test]
    fn sanitize_is_noop_when_fully_paired() {
        // 配对完整 → 原样保留（数量与 tool_call 不变）。
        let log = vec![call("c1"), res("c1", "x"), AgentMessage::user_text("u")];
        let out = sanitize_agent_messages(log.clone());
        assert_eq!(out.len(), log.len());
        assert_eq!(call_ids(&out), vec!["c1".to_string()]);
    }

    #[test]
    fn sanitize_strips_orphan_toolcall_and_keeps_text() {
        // 助手含 text + tool_call，但 tool_call 无结果 → 剥离 tool_call，保留 text。
        let log = vec![call_with_text("c1", "hi"), AgentMessage::user_text("u")];
        let out = sanitize_agent_messages(log);
        assert_eq!(out.len(), 2, "助手（仅 text）+ user 应保留");
        assert!(call_ids(&out).is_empty(), "无结果的 tool_call 应被剥离");
        let AgentMessage::Assistant(a) = &out[0] else {
            panic!("助手应保留");
        };
        assert!(a
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "hi")));
    }

    #[test]
    fn sanitize_drops_assistant_becoming_empty() {
        // 纯 tool_call 助手且无结果 → 剥离后变空 → 整体丢弃。
        let log = vec![call("c1"), AgentMessage::user_text("u")];
        let out = sanitize_agent_messages(log);
        assert_eq!(out.len(), 1, "纯悬空 tool_call 助手应被丢弃");
        assert!(matches!(out[0], AgentMessage::User(_)));
    }

    #[test]
    fn sanitize_strips_only_orphan_among_consecutive_tool_calls() {
        // 同一助手两个连续 tool_call，仅一个有结果 → 剥离悬空的，保留配对的。
        let log = vec![
            multi_call("c1", "c2"),
            res("c2", "y"),
            AgentMessage::user_text("u"),
        ];
        let out = sanitize_agent_messages(log);
        assert_eq!(call_ids(&out), vec!["c2".to_string()], "仅 c2（有结果）应保留");
        assert!(
            out.iter()
                .any(|m| matches!(m, AgentMessage::ToolResult(t) if t.tool_call_id == "c2")),
            "c2 结果应保留"
        );
    }

    #[test]
    fn sanitize_drops_orphan_tool_result_whose_call_absent() {
        // ToolResult 的发起助手不在序列（异常）→ 丢弃该结果（防御）。
        let log = vec![res("c1", "x"), AgentMessage::user_text("u")];
        let out = sanitize_agent_messages(log);
        assert!(
            !out.iter()
                .any(|m| matches!(m, AgentMessage::ToolResult(_))),
            "无发起助手的孤立 ToolResult 应丢弃"
        );
    }

    #[test]
    fn skill_read_call_ids_collects_only_skill_paths() {
        let skill_call = AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "s1".into(),
                name: "read_file".into(),
                arguments: json!({ "path": "skill://pdf" }),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
            stop_details: None,
        });
        let ids = skill_read_call_ids(&[skill_call, call("r1")]);
        assert!(ids.contains("s1"), "skill:// 路径应受保护");
        assert!(!ids.contains("r1"), "普通 read 不受保护");
    }

    #[test]
    fn assistant_has_any_call_and_is_protected_helpers() {
        let ids: HashSet<String> = ["c1".to_string()].into_iter().collect();
        assert!(assistant_has_any_call(&call("c1"), &ids));
        assert!(!assistant_has_any_call(&call("c2"), &ids));
        assert!(!assistant_has_any_call(&AgentMessage::user_text("u"), &ids));
        assert!(is_protected_skill_message(&res("c1", "x"), &ids));
        assert!(!is_protected_skill_message(&res("c2", "x"), &ids));
    }

    #[test]
    fn protected_call_ids_supports_custom_rules() {
        // P3：自定义规则保护非 skill 工具调用（验证 matcher 泛化对内置 SkillReadRule 之外生效）。
        struct SpecialRule;
        impl ToolProtectionRule for SpecialRule {
            fn matches(&self, tool_name: &str, _args: &serde_json::Value) -> bool {
                tool_name == "special"
            }
        }
        let special_call = AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "sp".into(),
                name: "special".into(),
                arguments: json!({}),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
            stop_details: None,
        });
        let skill_call = AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "sk".into(),
                name: "read_file".into(),
                arguments: json!({ "path": "skill://x" }),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
            stop_details: None,
        });
        // 内置 SkillReadRule + 自定义 SpecialRule 同时生效。
        let rules: Vec<Box<dyn ToolProtectionRule>> =
            vec![Box::new(SkillReadRule), Box::new(SpecialRule)];
        let ids = protected_call_ids(&[special_call, skill_call, call("plain")], &rules);
        assert!(ids.contains("sp"), "自定义规则应保护 special 工具调用");
        assert!(ids.contains("sk"), "内置 SkillReadRule 仍保护 skill://");
        assert!(!ids.contains("plain"), "普通工具调用不受任一规则保护");
    }
}
