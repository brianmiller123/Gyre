//! Token 计数：tiktoken-rs 精确计数（OpenAI BPE），其它 provider 回退到启发式。

use tiktoken_rs::CoreBPE;

use agent_core::{ContentBlock, ProviderMessage, UserContent};

/// token 计数器：按 provider 选用 BPE 词表。
pub struct TokenCounter {
    /// OpenAI 编码器（cl100k_base，gpt-4/gpt-3.5 系列）。
    encoder: Option<CoreBPE>,
}

impl TokenCounter {
    /// 构造 OpenAI 词表计数器。
    ///
    /// # Errors
    /// 词表加载失败时返回错误（已嵌入二进制，通常不会）。
    pub fn openai() -> Result<Self, String> {
        let encoder = tiktoken_rs::cl100k_base().map_err(|e| e.to_string())?;
        Ok(Self { encoder: Some(encoder) })
    }

    /// 回退计数器（启发式 chars/4，用于不支持 BPE 的 provider）。
    #[must_use]
    pub fn heuristic() -> Self {
        Self { encoder: None }
    }

    /// 计算一段文本的 token 数。
    pub fn count_text(&self, text: &str) -> usize {
        match &self.encoder {
            Some(enc) => enc.encode_with_special_tokens(text).len(),
            None => text.chars().count() / 4,
        }
    }

    /// 计算完整上下文（system + messages）的 token 数。
    pub fn count_context(&self, system: &[String], messages: &[ProviderMessage]) -> usize {
        let mut total: usize = 0;
        for s in system {
            // 每条 system 加角色开销 ~4 token
            total += 4 + self.count_text(s);
        }
        for m in messages {
            total += 4; // 角色标记开销
            match m {
                ProviderMessage::System(s) => total += self.count_text(s),
                ProviderMessage::User { content } => {
                    for c in content {
                        match c {
                            UserContent::Text { text } => total += self.count_text(text),
                            UserContent::Image { mime, data } => {
                                total += self.count_text(mime) + data.len() / 4;
                            }
                        }
                    }
                }
                ProviderMessage::Assistant { content } => {
                    for b in content {
                        match b {
                            ContentBlock::Text { text } => total += self.count_text(text),
                            ContentBlock::Thinking { text, .. } => total += self.count_text(text),
                            ContentBlock::ToolCall { name, arguments, .. } => {
                                total += self.count_text(name) + self.count_text(&arguments.to_string());
                            }
                        }
                    }
                }
                ProviderMessage::Tool { content, .. } => total += self.count_text(content),
            }
        }
        // 对话尾部 priming 开销
        total + 3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_counter_runs() {
        let counter = TokenCounter::openai().expect("cl100k 加载");
        let n = counter.count_text("Hello, world!");
        assert!(n >= 3 && n <= 6, "expected ~4 tokens, got {n}");
    }

    #[test]
    fn heuristic_counter_uses_chars() {
        let counter = TokenCounter::heuristic();
        let n = counter.count_text("abcdefgh");
        assert_eq!(n, 2); // 8 chars / 4
    }

    #[test]
    fn count_context_includes_system() {
        let counter = TokenCounter::heuristic();
        let total = counter.count_context(&["hello".into()], &[]);
        assert!(total >= 5);
    }
}
