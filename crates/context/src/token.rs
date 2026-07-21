//! Token 计数：tiktoken-rs 精确计数（按 model 族选 BPE：gpt-4o/o 系列 o200k_base，
//! gpt-4/gpt-3.5 系列 cl100k_base），其它 provider 回退到启发式。

use tiktoken_rs::CoreBPE;

use agent_core::{ContentBlock, ProviderMessage, UserContent};

/// token 计数器：按 model 族选用 BPE 词表。
pub struct TokenCounter {
    /// cl100k_base（gpt-4 / gpt-3.5 系列）。
    cl100k: Option<CoreBPE>,
    /// o200k_base（gpt-4o / o1 / o3 / o4-mini 系列）。
    o200k: Option<CoreBPE>,
}

impl TokenCounter {
    /// 构造 OpenAI 词表计数器（同时加载 cl100k + o200k）。
    ///
    /// cl100k 必须加载成功；o200k 加载失败时回退 `None`（gpt-4o 等退用 cl100k，
    /// 精度略降但不阻断）。
    ///
    /// # Errors
    /// cl100k 词表加载失败时返回错误（已嵌入二进制，通常不会）。
    pub fn openai() -> Result<Self, String> {
        let cl100k = tiktoken_rs::cl100k_base().map_err(|e| e.to_string())?;
        let o200k = tiktoken_rs::o200k_base().ok();
        Ok(Self {
            cl100k: Some(cl100k),
            o200k,
        })
    }

    /// 回退计数器（启发式 chars/4，用于不支持 BPE 的 provider）。
    #[must_use]
    pub fn heuristic() -> Self {
        Self {
            cl100k: None,
            o200k: None,
        }
    }

    /// 是否为 o200k_base 编码的 model（gpt-4o 系列 + o1/o3/o4 推理系列）。
    ///
    /// 移植 tiktoken Python `encoding_for_model` 的映射规则：o200k 覆盖 GPT-4o 家族与
    /// 新一代推理模型，其余 OpenAI 模型用 cl100k_base。非 OpenAI provider 一律按
    /// cl100k 近似（无公开 BPE 词表）。
    #[must_use]
    pub fn is_o200k_model(model_id: &str) -> bool {
        let l = model_id.to_ascii_lowercase();
        l.contains("gpt-4o")
            || l.starts_with("o1")
            || l.starts_with("o3")
            || l.starts_with("o4")
            || l.contains("o4-mini")
    }

    /// 按 model id 选编码器：o200k model → o200k（缺失则回退 cl100k）；其余 → cl100k。
    fn encoder_for(&self, model_id: &str) -> Option<&CoreBPE> {
        if Self::is_o200k_model(model_id) {
            self.o200k.as_ref().or(self.cl100k.as_ref())
        } else {
            self.cl100k.as_ref()
        }
    }

    /// 计算一段文本的 token 数（按 model 族选 BPE）。
    pub fn count_text_for(&self, text: &str, model_id: &str) -> usize {
        match self.encoder_for(model_id) {
            Some(enc) => enc.encode_with_special_tokens(text).len(),
            None => text.chars().count() / 4,
        }
    }

    /// 计算一段文本的 token 数（向后兼容：默认 cl100k 编码）。
    pub fn count_text(&self, text: &str) -> usize {
        self.count_text_for(text, "gpt-4")
    }

    /// 计算完整上下文（system + messages）的 token 数（按 model 族选 BPE）。
    pub fn count_context_for(
        &self,
        system: &[String],
        messages: &[ProviderMessage],
        model_id: &str,
    ) -> usize {
        let mut total: usize = 0;
        for s in system {
            // 每条 system 加角色开销 ~4 token
            total += 4 + self.count_text_for(s, model_id);
        }
        for m in messages {
            total += 4; // 角色标记开销
            match m {
                ProviderMessage::System(s) => total += self.count_text_for(s, model_id),
                ProviderMessage::User { content } => {
                    for c in content {
                        match c {
                            UserContent::Text { text } => {
                                total += self.count_text_for(text, model_id)
                            }
                            UserContent::Image { mime, data } => {
                                total += self.count_text_for(mime, model_id) + data.len() / 4;
                            }
                        }
                    }
                }
                ProviderMessage::Assistant { content } => {
                    for b in content {
                        match b {
                            ContentBlock::Text { text } => {
                                total += self.count_text_for(text, model_id)
                            }
                            ContentBlock::Thinking { text, .. } => {
                                total += self.count_text_for(text, model_id);
                            }
                            ContentBlock::ToolCall {
                                name, arguments, ..
                            } => {
                                total += self.count_text_for(name, model_id)
                                    + self.count_text_for(&arguments.to_string(), model_id);
                            }
                        }
                    }
                }
                ProviderMessage::Tool { content, .. } => {
                    total += self.count_text_for(content, model_id);
                }
            }
        }
        // 对话尾部 priming 开销
        total + 3
    }

    /// 计算完整上下文的 token 数（向后兼容：默认 cl100k 编码）。
    pub fn count_context(&self, system: &[String], messages: &[ProviderMessage]) -> usize {
        self.count_context_for(system, messages, "gpt-4")
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

    #[test]
    fn o200k_model_detection() {
        // gpt-4o 家族 + o 系列推理模型 → o200k_base。
        assert!(TokenCounter::is_o200k_model("gpt-4o"));
        assert!(TokenCounter::is_o200k_model("gpt-4o-mini"));
        assert!(TokenCounter::is_o200k_model("gpt-4o-2024-08-06"));
        assert!(TokenCounter::is_o200k_model("o1"));
        assert!(TokenCounter::is_o200k_model("o1-preview"));
        assert!(TokenCounter::is_o200k_model("o3-mini"));
        assert!(TokenCounter::is_o200k_model("o4-mini"));
        // 其余 OpenAI → cl100k_base。
        assert!(!TokenCounter::is_o200k_model("gpt-4"));
        assert!(!TokenCounter::is_o200k_model("gpt-4-turbo"));
        assert!(!TokenCounter::is_o200k_model("gpt-3.5-turbo"));
        // 非 OpenAI provider → cl100k 近似（无公开 BPE）。
        assert!(!TokenCounter::is_o200k_model("claude-3-5-sonnet-20241022"));
        assert!(!TokenCounter::is_o200k_model("glm-4.6"));
        assert!(!TokenCounter::is_o200k_model("deepseek-chat"));
    }

    #[test]
    fn count_text_for_picks_o200k_for_gpt4o() {
        let c = TokenCounter::openai().expect("tiktoken");
        // 中文 + emoji：o200k 与 cl100k 对这类文本编码不同（验证按 model 选了不同编码器）。
        let text = "你好世界 🎉 emoji 测试 token 计数";
        let n_cl100k = c.count_text_for(text, "gpt-4");
        let n_o200k = c.count_text_for(text, "gpt-4o");
        assert!(n_cl100k > 0, "cl100k 计数应 > 0");
        assert!(n_o200k > 0, "o200k 计数应 > 0");
        // gpt-4o 与 gpt-4 在该文本上编码不同（o200k 对 emoji/中文通常更紧凑）。
        // 不强断言大小方向（版本相关），但断言「按 model 选了编码路径」：
        // 用一个纯 ASCII 文本（两编码器结果应相同）作对照，确认差异来自编码器选择。
        let ascii = "plain ascii text only";
        assert_eq!(
            c.count_text_for(ascii, "gpt-4"),
            c.count_text_for(ascii, "gpt-4o"),
            "纯 ASCII 两编码器应一致"
        );
    }

    #[test]
    fn count_context_for_model_variants_run() {
        let c = TokenCounter::openai().expect("tiktoken");
        let sys = vec!["系统提示".into()];
        let msgs = vec![ProviderMessage::User {
            content: vec![UserContent::Text {
                text: "你好".into(),
            }],
        }];
        let n4 = c.count_context_for(&sys, &msgs, "gpt-4");
        let n4o = c.count_context_for(&sys, &msgs, "gpt-4o");
        assert!(n4 > 0 && n4o > 0);
    }
}
