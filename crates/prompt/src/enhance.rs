//! # Prompt enhancement (ported from Roo-Code)
//!
//! 直接移植 Roo-Code 的 `ENHANCE` support-prompt（`src/shared/support-prompt.ts`）：
//! 把草稿作为**用户消息**交给模型，要求只输出增强后的 prompt——无解释、无前言、
//! 无列表、无占位符、无包裹引号。Web 的 ✨ 按钮与 CLI 的 `/enhance` 共用同一模板
//! 与同一 provider 调用，行为与 Roo-Code 完全一致。
//!
//! 与此前「多预设 + 脚手架」方案的差异：Roo-Code 没有 preset，也不拼 system prompt，
//! 而是把整段指令作为用户消息一次性送出——模型据此产出干净、自然的增强 prompt，
//! 而非结构化模板。这正是其效果更好的关键。

#![allow(clippy::module_name_repetitions)]

use agent_core::{
    AssistantEvent, CompletionRequest, LlmError, LlmProvider, Model, ProviderCallContext,
    ProviderMessage, UserContent,
};
use futures::StreamExt;

/// Roo-Code `ENHANCE` 模板（逐字移植）。`{draft}` 处填入用户草稿。
///
/// 原文（Roo-Code `supportPromptConfigs.ENHANCE.template`）：
/// > Generate an enhanced version of this prompt (reply with only the enhanced prompt - no
/// > conversation, explanations, lead-in, bullet points, placeholders, or surrounding quotes):
///
/// 后接空行与用户输入。Roo 把整段作为**用户消息**送出（无 system prompt）。
const ENHANCE_TEMPLATE: &str = "Generate an enhanced version of this prompt (reply with only \
the enhanced prompt - no conversation, explanations, lead-in, bullet points, placeholders, or \
surrounding quotes):\n\n{draft}";

/// 构造 Roo-Code 风格的增强请求：模板作为用户消息，无 system prompt、无工具。
///
/// 调用方负责 `provider.stream(req, ctx)` 并只取 [`AssistantEvent::TextDelta`]；
/// 或直接用便捷函数 [`enhance_collect`]。
#[must_use]
pub fn build_enhance_request(draft: &str, model: Model) -> CompletionRequest {
    let user_text = ENHANCE_TEMPLATE.replace("{draft}", draft);
    CompletionRequest {
        model,
        // 与 Roo-Code 一致：不设 system prompt，整段指令作为用户消息。
        system: vec![],
        messages: vec![ProviderMessage::User {
            content: vec![UserContent::Text { text: user_text }],
        }],
        tools: vec![],
        tool_choice: None,
        max_tokens: 4096,
        // 不显式设温度：用 provider 默认（Roo-Code enhance 不覆盖温度）。
        temperature: None,
        thinking: None,
        cache_key: Some("gyre-enhance".to_string()),
        stable_prefix_len: 0,
    }
}

/// 便捷：驱动 provider 流并收集为完整字符串（CLI / HTTP 端点用）。
///
/// # Errors
/// Provider 返回 [`AssistantEvent::Error`] 或流错误时透传。
pub async fn enhance_collect(
    draft: &str,
    provider: &dyn LlmProvider,
    ctx: &ProviderCallContext,
    model: &Model,
) -> Result<String, LlmError> {
    let req = build_enhance_request(draft, model.clone());
    let mut stream = provider.stream(req, ctx).await?;
    let mut out = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            AssistantEvent::TextDelta(d) => out.push_str(&d),
            AssistantEvent::Error(e) => return Err(e),
            AssistantEvent::MessageEnd(_) => break,
            _ => {}
        }
    }
    Ok(out.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_uses_roo_template_as_user_message() {
        let m = Model::with_defaults("x", "p", agent_core::Api::OpenAiCompletions);
        let req = build_enhance_request("fix the login bug", m);
        // 无 system prompt（与 Roo-Code 一致）。
        assert!(req.system.is_empty());
        // 无工具。
        assert!(req.tools.is_empty());
        // 单条用户消息。
        assert_eq!(req.messages.len(), 1);
        let ProviderMessage::User { content } = &req.messages[0] else {
            panic!("应为用户消息");
        };
        let text = match &content[0] {
            UserContent::Text { text } => text.as_str(),
            _ => panic!("应为文本块"),
        };
        // Roo 模板的关键指令逐字存在。
        assert!(text.contains("Generate an enhanced version of this prompt"));
        assert!(text.contains("reply with only the enhanced prompt"));
        assert!(text.contains("no conversation, explanations, lead-in"));
        // 草稿被拼接在模板末尾（空行之后）。
        assert!(text.ends_with("fix the login bug"));
        assert!(text.contains("\n\nfix the login bug"));
    }

    #[test]
    fn template_matches_roo_code_verbatim() {
        // 守护模板不被无意改动：与 Roo-Code ENHANCE 模板逐字一致。
        let rendered = ENHANCE_TEMPLATE.replace("{draft}", "X");
        assert_eq!(
            rendered,
            "Generate an enhanced version of this prompt (reply with only the enhanced prompt - \
no conversation, explanations, lead-in, bullet points, placeholders, or surrounding quotes):\n\nX"
        );
    }
}
