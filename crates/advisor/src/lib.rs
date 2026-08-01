//! # agent-advisor
//!
//! 独立只读评审 agent（P1-L）：快照主 transcript（脱敏后）→ 独立 provider 评审 →
//! 产出 `nit | concern | blocker` 三级建议，经 [`EmissionGuard`] 去重/filler 过滤。
//!
//! 移植 oh-my-pi `coding-agent/src/advisor/`（runtime + emission-guard + watchdog）。
//! 与主 agent 解耦：本 crate 只依赖 `agent-core`，不引入循环依赖；评审用独立
//! provider/model 调用（一次流式补全），宿主负责触发节奏与建议注入。

pub mod guard;
pub mod obfuscator;
pub mod watchdog;

use std::sync::Arc;

use agent_core::{
    AgentMessage, CompletionRequest, LlmError, LlmProvider, Model, ProviderCallContext,
};

pub use guard::EmissionGuard;
pub use obfuscator::SecretObfuscator;
pub use watchdog::{discover_watchdog, render_watchdog};

/// 建议级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// 轻微（风格/优化建议）。
    Nit,
    /// 关注（可能偏离目标）。
    Concern,
    /// 阻断（当前方向必然失败/违反验收）。
    Blocker,
}

impl Severity {
    /// 解析（容错大小写与变体）。
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim().to_lowercase();
        Some(match t.as_str() {
            "nit" | "nits" | "nitpick" | "nitpick:" => Self::Nit,
            "concern" | "concerns" | "warning" => Self::Concern,
            "blocker" | "blockers" | "error" | "critical" => Self::Blocker,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Nit => "nit",
            Self::Concern => "concern",
            Self::Blocker => "blocker",
        }
    }
}

/// 一条评审建议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advice {
    pub severity: Severity,
    pub note: String,
}

/// 评审 advisor：快照 → 脱敏 → 独立 provider 评审 → guard 过滤。
pub struct Advisor {
    provider: Arc<dyn LlmProvider>,
    model: Model,
    guard: EmissionGuard,
    obfuscator: SecretObfuscator,
    watchdog: String,
    /// 快照每消息最大字符数（工具结果/长文本截断）。
    max_message_chars: usize,
    /// 快照最大总字符数（超限丢弃最旧轮次）。
    max_snapshot_chars: usize,
}

impl Advisor {
    /// 用独立 provider/model 构造（guard/watchdog 空 → 不注入准则）。
    #[must_use]
    pub fn new(provider: Arc<dyn LlmProvider>, model: Model) -> Self {
        Self {
            provider,
            model,
            guard: EmissionGuard::new(),
            obfuscator: SecretObfuscator::new(),
            watchdog: String::new(),
            max_message_chars: 1200,
            max_snapshot_chars: 24_000,
        }
    }

    /// 注入 WATCHDOG.md 评审准则（拼接文本）。
    #[must_use]
    pub fn with_watchdog(mut self, watchdog: String) -> Self {
        self.watchdog = watchdog;
        self
    }

    /// 自定义快照上限（测试用）。
    #[must_use]
    pub fn with_limits(mut self, max_message_chars: usize, max_snapshot_chars: usize) -> Self {
        self.max_message_chars = max_message_chars;
        self.max_snapshot_chars = max_snapshot_chars;
        self
    }

    /// 追加脱敏 pattern（仓库级凭据形态）。
    pub fn add_secret_pattern(&mut self, re: regex::Regex) {
        self.obfuscator.add_pattern(re);
    }

    /// 评审一次快照：返回经 guard 过滤后的建议（可能为空）。
    ///
    /// # Errors
    /// provider 调用失败时返回 [`LlmError`]。
    pub async fn review(
        &self,
        messages: &[AgentMessage],
        call_ctx: &ProviderCallContext,
    ) -> Result<Vec<Advice>, LlmError> {
        let snapshot = self.render_snapshot(messages);
        let mut system = String::from(
            "你是独立评审 agent：只读观察主 agent 的对话，找出它自己看不见的问题。\
\n输出 0-3 条建议，每条严格一行，格式：`[severity] note`（severity ∈ nit|concern|blocker）。\
\n不输出任何其他内容。没有值得说的就输出空行。",
        );
        if !self.watchdog.is_empty() {
            system.push('\n');
            system.push_str(&self.watchdog);
        }
        let prompt = format!(
            "以下是主 agent 的对话快照（工具输出已截断，凭据已脱敏）：\n\n{snapshot}\n\n你的建议："
        );
        let request = CompletionRequest {
            model: self.model.clone(),
            system: vec![system],
            messages: vec![agent_core::ProviderMessage::User {
                content: vec![agent_core::UserContent::Text { text: prompt }],
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: 1024,
            temperature: Some(0.2),
            thinking: None, // advisor 评审不需思考预算（快、省）。
            cache_key: None,
            stable_prefix_len: 0,
        };
        use futures_util::StreamExt;
        let mut stream = self.provider.stream(request, call_ctx).await?;
        let mut response = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                agent_core::AssistantEvent::TextDelta(text) => response.push_str(&text),
                agent_core::AssistantEvent::MessageEnd { .. } => break,
                _ => {}
            }
        }
        let response = self.obfuscator.obfuscate(&response);
        Ok(self.parse_response(&response))
    }

    /// 快照渲染：逐消息单段（角色 + 截断内容；工具结果折叠）。
    #[must_use]
    pub fn render_snapshot(&self, messages: &[AgentMessage]) -> String {
        let mut out = String::new();
        let mut total = 0usize;
        // 从新到旧收集（保留最近轮次），再反转回时间序。
        let mut recent: Vec<String> = Vec::new();
        for m in messages.iter().rev() {
            let rendered = match m {
                AgentMessage::User(u) => {
                    let text = u
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            agent_core::UserContent::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("用户: {}", truncate_chars(&text, self.max_message_chars))
                }
                AgentMessage::Assistant(a) => {
                    let mut parts = Vec::new();
                    for block in &a.content {
                        match block {
                            agent_core::ContentBlock::Text { text } => parts.push(truncate_chars(text, self.max_message_chars)),
                            agent_core::ContentBlock::Thinking { text, .. } => parts.push(format!("(思考) {}", truncate_chars(text, 400))),
                            agent_core::ContentBlock::ToolCall { name, .. } => parts.push(format!("工具调用: {name}")),
                        }
                    }
                    if parts.is_empty() {
                        "助手: (空)".to_string()
                    } else {
                        format!("助手: {}", parts.join("\n"))
                    }
                }
                AgentMessage::ToolResult(r) => {
                    let body = match &r.result {
                        agent_core::ToolResult::Text(t) => truncate_chars(t, 400),
                        agent_core::ToolResult::Error { message, .. } => {
                            format!("(错误) {}", truncate_chars(message, 200))
                        }
                        agent_core::ToolResult::Image { .. } => "(图像)".into(),
                    };
                    format!("工具 {} 结果: {body}", r.tool_call_id)
                }
                _ => continue,
            };
            let line = format!("{}\n", self.obfuscator.obfuscate(&rendered));
            total += line.len();
            if total > self.max_snapshot_chars {
                break;
            }
            recent.push(line);
        }
        for line in recent.into_iter().rev() {
            out.push_str(&line);
        }
        out
    }

    /// 解析评审输出：`[severity] note` 或 `severity: note` 行。
    #[must_use]
    pub fn parse_response(&self, response: &str) -> Vec<Advice> {
        let mut out = Vec::new();
        for line in response.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // 形态 1：`[concern] note`；形态 2：`concern: note`；形态 3：`concern - note`。
            let parsed = if let Some(rest) = line.strip_prefix('[') {
                rest.split_once(']')
            } else {
                line.split_once(':')
                    .or_else(|| line.split_once(" - "))
                    .or_else(|| line.split_once('：'))
            };
            let (sev_s, note) = match parsed {
                Some((s, n)) => (s, n),
                None => continue,
            };
            let note = note.trim();
            if note.is_empty() {
                continue;
            }
            let Some(severity) = Severity::parse(sev_s) else {
                continue;
            };
            if self.guard.emit(severity, note) {
                out.push(Advice {
                    severity,
                    note: note.to_string(),
                });
            }
        }
        out
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str(&format!("…(共 {count} 字符)"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response_extracts_advice() {
        let a = Advisor::new(
            Arc::new(agent_core::llm::UnconfiguredProvider),
            Model::with_defaults("t", "t", agent_core::Api::OpenAiCompletions),
        );
        let adv = a.parse_response(
            "[concern] The error is silently swallowed in read_file.\n\
             blocker: Acceptance criteria changed.\n\
             不是建议的行\n\
             [nit] - leading dash note\n",
        );
        assert_eq!(adv.len(), 3, "{adv:?}");
        assert_eq!(adv[0].severity, Severity::Concern);
        assert_eq!(adv[1].severity, Severity::Blocker);
        assert_eq!(adv[1].note, "Acceptance criteria changed.");
        assert_eq!(adv[2].severity, Severity::Nit);
    }

    #[test]
    fn parse_response_dedupes() {
        let a = Advisor::new(
            Arc::new(agent_core::llm::UnconfiguredProvider),
            Model::with_defaults("t", "t", agent_core::Api::OpenAiCompletions),
        );
        let adv = a.parse_response(
            "[concern] Same observation twice.\n[concern] same OBSERVATION twice!!\n[blocker] Different blocker.\n",
        );
        assert_eq!(adv.len(), 2, "{adv:?}");
    }

    #[test]
    fn parse_response_skips_filler() {
        let a = Advisor::new(
            Arc::new(agent_core::llm::UnconfiguredProvider),
            Model::with_defaults("t", "t", agent_core::Api::OpenAiCompletions),
        );
        let adv = a.parse_response("[nit] Stop.\n[nit] ok\n[nit] ！！！\n");
        assert!(adv.is_empty());
    }

    #[test]
    fn snapshot_renders_and_truncates() {
        let a = Advisor::new(
            Arc::new(agent_core::llm::UnconfiguredProvider),
            Model::with_defaults("t", "t", agent_core::Api::OpenAiCompletions),
        );
        let long = "x".repeat(5000);
        let msgs = vec![
            AgentMessage::user_text("hello"),
            AgentMessage::Assistant(agent_core::AssistantMessage {
                content: vec![agent_core::ContentBlock::Text { text: long.clone() }],
                usage: agent_core::Usage::default(),
                model: "test".into(),
                stop_reason: None,
                stop_details: None,
            }),
        ];
        let snap = a.render_snapshot(&msgs);
        assert!(snap.contains("hello"));
        assert!(snap.contains("助手:"));
        assert!(snap.contains("共 5000 字符"));
    }

    #[test]
    fn snapshot_obfuscates_secrets() {
        let a = Advisor::new(
            Arc::new(agent_core::llm::UnconfiguredProvider),
            Model::with_defaults("t", "t", agent_core::Api::OpenAiCompletions),
        );
        let msgs = vec![AgentMessage::user_text("use api_key=sk-abcdef1234567890 now")];
        let snap = a.render_snapshot(&msgs);
        assert!(!snap.contains("sk-abcdef1234567890"), "{snap}");
        assert!(snap.contains("<redacted>"), "{snap}");
    }
}
