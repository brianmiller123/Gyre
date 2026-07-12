//! 上下文压缩：生产级 summarize / shake / prune。
//!
//! - **Summarize**：将旧对话折叠为一份 handoff 摘要（移植 oh-my-pi compaction-summary），
//!   替换尾部，大幅降低 token。需要外部摘要生成器（LLM 调用）注入。
//! - **Shake**：移除冗余——连续重复文本、空输出助手消息、可折叠的连续状态消息。
//! - **Prune**：保留最近 N 条（tool-protection：工具结果不被裁剪）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agent_core::{
    AgentMessage, AssistantEvent, CompletionRequest, ContentBlock, LlmProvider, Model,
    ProviderCallContext, ProviderMessage, StatusKind, UserContent,
};
use futures::StreamExt;

/// 压缩器：纯函数，不直接调 LLM；summarize 的摘要由外部 `SummaryProvider` 注入。
pub struct Compactor;

/// 摘要提供器：对一批旧消息产出一份摘要文本（由 agent/llm 调用实现）。
pub trait SummaryProvider: Send + Sync {
    /// 生成摘要。`old` 为待折叠的消息（已转文本）。
    ///
    /// # Errors
    /// 生成失败时返回错误字符串。
    fn summarize(&self, old: &[String]) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>;
}

/// 用 LLM 生成摘要的 [`SummaryProvider`] 实现（handoff 摘要）。
///
/// 装配层（cli/server）构造后注入 `InMemoryContext::set_summarizer`。
pub struct LlmSummaryProvider {
    provider: Arc<dyn LlmProvider>,
    model: Model,
    provider_ctx: ProviderCallContext,
}

impl LlmSummaryProvider {
    /// 构造。
    #[must_use]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        model: Model,
        provider_ctx: ProviderCallContext,
    ) -> Self {
        Self {
            provider,
            model,
            provider_ctx,
        }
    }
}

impl SummaryProvider for LlmSummaryProvider {
    fn summarize(
        &self,
        old: &[String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>
    {
        let prompt = format!(
            "将以下已发生的对话历史压缩为简洁要点摘要，保留关键决策、文件改动与未决事项：\n\n{}",
            old.join("\n---\n")
        );
        Box::pin(async move {
            let req = CompletionRequest {
                model: self.model.clone(),
                system: vec!["你是上下文摘要助手，仅输出简洁要点。".to_string()],
                messages: vec![ProviderMessage::User {
                    content: vec![UserContent::Text { text: prompt }],
                }],
                tools: vec![],
                tool_choice: None,
                max_tokens: 1024,
                temperature: Some(0.0),
                thinking: None,
                cache_key: None,
            };
            let mut stream = self
                .provider
                .stream(req, &self.provider_ctx)
                .await
                .map_err(|e| e.to_string())?;
            let mut out = String::new();
            while let Some(ev) = stream.next().await {
                if let AssistantEvent::TextDelta(d) = ev {
                    out.push_str(&d);
                }
            }
            Ok(out)
        })
    }
}

impl Compactor {
    /// Prune：保留最近 `keep_recent` 条；`skill://` read 的工具结果受保护（即便在窗口外也保留），
    /// 避免按需加载的 skill 内容在压缩中丢失（移植 oh-my-pi tool-protection 思想）。
    ///
    /// 工具配对完整性：任一被保留的 `ToolResult`，其发起 `ToolCall` 的助手消息也一并保留，
    /// 避免压缩后留下孤立 tool 消息（OpenAI 要求每条 tool 消息前必有对应的 tool_calls）。
    #[must_use]
    pub fn prune(log: &[AgentMessage], keep_recent: usize) -> Vec<AgentMessage> {
        // 先 supersede：移除被后续同文件 read 取代的旧结果（内容已过期），释放上下文。
        let deduped = supersede_read_results(log);
        if deduped.len() <= keep_recent {
            return deduped;
        }
        let protected = skill_read_call_ids(&deduped);
        let split = deduped.len() - keep_recent;
        // 会被保留的 ToolResult（窗口内 或 skill 受保护）所依赖的 tool_call_id：
        // 这些 ToolCall 所在的助手消息也必须保留，否则产生孤立 tool 消息。
        let needed_call_ids: HashSet<String> = deduped
            .iter()
            .enumerate()
            .filter(|(i, m)| *i >= split || is_protected_skill_message(m, &protected))
            .filter_map(|(_, m)| match m {
                AgentMessage::ToolResult(t) => Some(t.tool_call_id.clone()),
                _ => None,
            })
            .collect();
        deduped
            .iter()
            .enumerate()
            .filter(|(i, m)| {
                *i >= split
                    || is_protected_skill_message(m, &protected)
                    || assistant_has_any_call(m, &needed_call_ids)
            })
            .map(|(_, m)| m.clone())
            .collect()
    }

    /// Shake：移除冗余/空消息，保留语义。
    /// - 连续相同的 Status 文本去重
    /// - 空文本助手消息移除
    /// - 连续 ToolResult 合并不做（保留配对完整性）
    #[must_use]
    pub fn shake(log: &[AgentMessage]) -> Vec<AgentMessage> {
        let mut out: Vec<AgentMessage> = Vec::with_capacity(log.len());
        let mut last_status: Option<String> = None;
        for m in log {
            match m {
                AgentMessage::Status(s) => {
                    // 去除连续重复状态（如重复的 thinking 进度）
                    if s.kind == StatusKind::Info && Some(&s.text) == last_status.as_ref() {
                        continue;
                    }
                    last_status = Some(s.text.clone());
                    out.push(m.clone());
                }
                AgentMessage::Assistant(a) => {
                    // 移除纯空文本的助手消息（无文本、无工具调用）
                    let is_empty = a.content.iter().all(|b| match b {
                        ContentBlock::Text { text } => text.trim().is_empty(),
                        ContentBlock::ToolCall { .. } => false,
                        ContentBlock::Thinking { text, .. } => text.trim().is_empty(),
                    }) && !a.has_tool_calls();
                    if !is_empty {
                        out.push(m.clone());
                    }
                }
                _ => out.push(m.clone()),
            }
        }
        out
    }

    /// Summarize：将旧消息折叠为单条 handoff 摘要用户消息。
    /// 返回 (保留前缀消息数, 新日志)。旧消息被替换为一条摘要。
    ///
    /// # Errors
    /// 通过 `provider` 生成摘要失败时返回错误。
    pub async fn summarize(
        log: Vec<AgentMessage>,
        keep_recent: usize,
        provider: &dyn SummaryProvider,
    ) -> Result<Vec<AgentMessage>, String> {
        if log.len() <= keep_recent {
            return Ok(log);
        }
        // 不要让 recent 窗口起点落在 ToolResult 上：其发起 ToolCall 的助手消息在 old（被摘要）
        // 侧，会留下孤立 tool 消息。向左扩展直到起点非 ToolResult（最近一次工具交换整体保留）。
        let mut split = log.len() - keep_recent;
        while split > 0 && matches!(log.get(split), Some(AgentMessage::ToolResult(_))) {
            split -= 1;
        }
        let old = &log[..split];
        let recent = log[split..].to_vec();

        // 将旧消息渲染为文本行
        let old_text: Vec<String> = old.iter().map(message_to_summary_line).collect();
        let summary = provider.summarize(&old_text).await?;

        let mut result = Vec::with_capacity(recent.len() + 1);
        result.push(AgentMessage::user_text(format!(
            "[上下文摘要] 此前的对话已被压缩为以下要点，请据此继续：\n\n{summary}"
        )));
        result.extend(recent);
        Ok(result)
    }
}

/// 移除被后续同文件 read 取代的旧 `read_file` 工具结果（A10 supersede）。
///
/// 同一文件多次读取时仅保留最后一次结果；旧结果内容已过期，裁剪以释放上下文。
fn supersede_read_results(log: &[AgentMessage]) -> Vec<AgentMessage> {
    let mut reads_by_path: HashMap<String, Vec<String>> = HashMap::new();
    for m in log {
        let AgentMessage::Assistant(a) = m else {
            continue;
        };
        for block in &a.content {
            if let ContentBlock::ToolCall { id, name, arguments } = block {
                if name == "read_file" {
                    if let Some(path) = arguments.get("path").and_then(serde_json::Value::as_str) {
                        reads_by_path.entry(path.to_string()).or_default().push(id.clone());
                    }
                }
            }
        }
    }
    let mut remove: HashSet<String> = HashSet::new();
    for ids in reads_by_path.values() {
        if ids.len() > 1 {
            for id in &ids[..ids.len() - 1] {
                remove.insert(id.clone());
            }
        }
    }
    if remove.is_empty() {
        return log.to_vec();
    }
    log.iter()
        .filter(|m| !matches!(m, AgentMessage::ToolResult(t) if remove.contains(&t.tool_call_id)))
        .cloned()
        .collect()
}

/// 收集日志中 `read_file skill://...` 调用的 tool_call_id（受保护，prune 时不裁剪）。
fn skill_read_call_ids(log: &[AgentMessage]) -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();
    for m in log {
        let AgentMessage::Assistant(a) = m else {
            continue;
        };
        for block in &a.content {
            if let ContentBlock::ToolCall { id, name, arguments } = block {
                if name == "read_file"
                    && arguments
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|p| p.starts_with("skill://"))
                {
                    ids.insert(id.clone());
                }
            }
        }
    }
    ids
}

/// 消息是否为受保护的 skill read 结果。
fn is_protected_skill_message(m: &AgentMessage, ids: &std::collections::HashSet<String>) -> bool {
    matches!(m, AgentMessage::ToolResult(t) if ids.contains(&t.tool_call_id))
}

/// 助手消息是否含任一指定 tool_call_id（用于保留「被保留 ToolResult」的发起消息，避免孤立）。
fn assistant_has_any_call(m: &AgentMessage, ids: &HashSet<String>) -> bool {
    let AgentMessage::Assistant(a) = m else {
        return false;
    };
    a.content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolCall { id, .. } if ids.contains(id)))
}

/// 单条消息渲染为摘要行。
fn message_to_summary_line(m: &AgentMessage) -> String {
    match m {
        AgentMessage::User(u) => {
            let t: String = u
                .content
                .iter()
                .filter_map(|c| match c {
                    agent_core::UserContent::Text { text } => Some(text.as_str()),
                    agent_core::UserContent::Image { .. } => Some("[image]"),
                })
                .collect();
            format!("用户: {t}")
        }
        AgentMessage::Assistant(a) => format!("助手: {}", a.text()),
        AgentMessage::ToolResult(t) => format!("工具结果: {}", t.result.to_llm_text()),
        AgentMessage::Status(s) => format!("({}) {}", s.kind_text(), s.text),
        AgentMessage::Ask(a) => format!("[询问] {}", a.prompt),
        AgentMessage::SoftRequirement(r) => format!("[软需求: {}]", r.tool_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AssistantMessage, StatusMessage, ToolResult, ToolResultMessage, Usage};

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
        })
    }

    fn status(text: &str) -> AgentMessage {
        AgentMessage::Status(StatusMessage {
            text: text.into(),
            kind: StatusKind::Info,
        })
    }

    #[test]
    fn prune_keeps_recent() {
        let log = vec![
            AgentMessage::user_text("a"),
            AgentMessage::user_text("b"),
            AgentMessage::user_text("c"),
        ];
        let out = Compactor::prune(&log, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1], AgentMessage::user_text("c"));
    }

    #[test]
    fn prune_supersedes_old_read_results() {
        use serde_json::json;
        let call = |id: &str| {
            AgentMessage::Assistant(AssistantMessage {
                content: vec![ContentBlock::ToolCall {
                    id: id.into(),
                    name: "read_file".into(),
                    arguments: json!({ "path": "a.txt" }),
                }],
                usage: Usage::default(),
                model: "m".into(),
                stop_reason: None,
            })
        };
        let res = |id: &str, text: &str| {
            AgentMessage::ToolResult(ToolResultMessage {
                tool_call_id: id.into(),
                result: ToolResult::text(text),
            })
        };
        // 同文件读两次：旧结果应被取代裁剪，新结果保留
        let log = vec![call("r1"), res("r1", "old"), call("r2"), res("r2", "new")];
        let out = Compactor::prune(&log, 10); // keep 大，仅 supersede 生效
        assert!(
            !out.iter()
                .any(|m| matches!(m, AgentMessage::ToolResult(t) if t.tool_call_id == "r1")),
            "旧 read 应被取代裁剪"
        );
        assert!(
            out.iter()
                .any(|m| matches!(m, AgentMessage::ToolResult(t) if t.tool_call_id == "r2")),
            "新 read 应保留"
        );
    }

    #[test]
    fn prune_protects_skill_read_results() {
        use serde_json::json;
        let call = AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: json!({ "path": "skill://pdf" }),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
        });
        let result = AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "call-1".into(),
            result: ToolResult::text("PDF skill body"),
        });
        let recent = AgentMessage::user_text("recent");
        let log = vec![call, result, recent];
        // keep_recent=1：仅尾部 recent + 受保护的 skill 结果
        let out = Compactor::prune(&log, 1);
        assert!(out
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(t) if t.tool_call_id == "call-1")));
        assert!(out.iter().any(|m| matches!(m, AgentMessage::User(_))));
    }

    /// 回归：受保护的 skill 工具结果保留时，其发起 ToolCall 的助手消息也必须保留，
    /// 否则 convert_to_llm 会得到孤立 tool 消息。
    #[test]
    fn prune_keeps_call_for_protected_skill_result() {
        use serde_json::json;
        let call = AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: json!({ "path": "skill://pdf" }),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
        });
        let result = AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "call-1".into(),
            result: ToolResult::text("PDF skill body"),
        });
        let recent = AgentMessage::user_text("recent");
        let log = vec![call, result, recent];
        let out = Compactor::prune(&log, 1);
        // 受保护的 skill 结果保留，且其发起 ToolCall 的助手消息一并保留（不孤立）。
        assert!(out
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(t) if t.tool_call_id == "call-1")));
        assert!(out.iter().any(|m| matches!(
            m,
            AgentMessage::Assistant(a) if a
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall { id, .. } if id == "call-1"))
        )));
    }

    #[test]
    fn shake_drops_duplicate_status() {
        let log = vec![status("思考中"), status("思考中"), assistant("hi")];
        let out = Compactor::shake(&log);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn shake_drops_empty_assistant() {
        let empty = AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: "   ".into() }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
        });
        let log = vec![empty, assistant("real")];
        let out = Compactor::shake(&log);
        assert_eq!(out.len(), 1);
    }

    struct StaticSummary;
    impl SummaryProvider for StaticSummary {
        fn summarize(&self, _old: &[String]) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>> {
            Box::pin(async { Ok("已总结 3 条".into()) })
        }
    }

    #[tokio::test]
    async fn summarize_replaces_old_with_handoff() {
        let log = vec![
            AgentMessage::user_text("q1"),
            assistant("a1"),
            AgentMessage::user_text("q2"),
            assistant("a2"),
            AgentMessage::user_text("q3"),
        ];
        let out = Compactor::summarize(log, 2, &StaticSummary).await.unwrap();
        // 1 摘要 + 2 保留
        assert_eq!(out.len(), 3);
        assert!(out[0].text_unchecked().contains("上下文摘要"));
    }

    // 测试辅助：取消息文本（简化）
    trait MsgText {
        fn text_unchecked(&self) -> String;
    }
    impl MsgText for AgentMessage {
        fn text_unchecked(&self) -> String {
            match self {
                AgentMessage::User(u) => u
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        agent_core::UserContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect(),
                _ => String::new(),
            }
        }
    }

    // 静默未用类型
    #[allow(dead_code)]
    fn _force_tool_result_use() -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "x".into(),
            result: ToolResult::text("y"),
        })
    }
}
