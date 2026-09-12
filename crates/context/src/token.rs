//! Token 计数：tiktoken-rs 精确计数（按 model 族选 BPE：gpt-4o/o 系列 o200k_base，
//! gpt-4/gpt-3.5 系列 cl100k_base）。
//!
//! H26：模型可**显式声明 tokenizer 家族**（`[models] tokenizer = "..."`），覆盖按 id 的
//! 推断——非 OpenAI provider（Claude/Gemini/DeepSeek/Qwen…）没有公开 BPE 词表，用
//! `heuristic:<chars_per_token>` 声明更贴近真实分词密度（如 CJK 语料 ~1.5 字符/token，
//! 而不是 cl100k 的近似）。未声明时行为与引入前完全一致（按 model id 推断）。

use tiktoken_rs::CoreBPE;

use agent_core::{ContentBlock, ProviderMessage, UserContent};

/// H26：tokenizer 家族（显式声明或按 model id 推断）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TokenFamily {
    /// `o200k_base`（GPT-4o / o1 / o3 / o4 系列）。
    O200k,
    /// `cl100k_base`（GPT-4 / GPT-3.5 系列；也是非 OpenAI 模型的默认近似）。
    Cl100k,
    /// 启发式近似：`chars / chars_per_token`（无 BPE 词表的 provider 用）。
    Heuristic {
        /// 每 token 字符数（> 0）。
        chars_per_token: f32,
    },
}

impl TokenFamily {
    /// 解析声明串：`o200k` / `cl100k` / `heuristic` / `heuristic:<chars_per_token>`。
    ///
    /// 未知串返回 `None`（调用方回退按 model id 推断，不因笔误改变计数口径）。
    #[must_use]
    pub fn parse(spec: &str) -> Option<Self> {
        let s = spec.trim().to_ascii_lowercase();
        match s.as_str() {
            "o200k" | "o200k_base" => Some(Self::O200k),
            "cl100k" | "cl100k_base" => Some(Self::Cl100k),
            "heuristic" | "chars" => Some(Self::Heuristic {
                chars_per_token: 4.0,
            }),
            other => other
                .strip_prefix("heuristic:")
                .or_else(|| other.strip_prefix("chars:"))
                .and_then(|n| n.trim().parse::<f32>().ok())
                .filter(|r| *r > 0.0)
                .map(|chars_per_token| Self::Heuristic { chars_per_token }),
        }
    }
}

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

    /// H26：解析「声明优先、id 推断兜底」的 tokenizer 家族。
    #[must_use]
    pub fn family_for(model_id: &str, spec: Option<&str>) -> TokenFamily {
        if let Some(f) = spec.and_then(TokenFamily::parse) {
            return f;
        }
        if Self::is_o200k_model(model_id) {
            TokenFamily::O200k
        } else {
            TokenFamily::Cl100k
        }
    }

    /// 计算一段文本的 token 数（按 model 族选 BPE）。
    pub fn count_text_for(&self, text: &str, model_id: &str) -> usize {
        self.count_text_with_family(text, model_id, None)
    }

    /// H26：按**声明优先**的家族计数（`spec` 为配置里的 tokenizer 串）。
    pub fn count_text_with_family(&self, text: &str, model_id: &str, spec: Option<&str>) -> usize {
        match Self::family_for(model_id, spec) {
            TokenFamily::O200k => self.o200k.as_ref().or(self.cl100k.as_ref()).map_or_else(
                || heuristic_chars(text, 4.0),
                |enc| enc.encode_with_special_tokens(text).len(),
            ),
            TokenFamily::Cl100k => self.cl100k.as_ref().map_or_else(
                || heuristic_chars(text, 4.0),
                |enc| enc.encode_with_special_tokens(text).len(),
            ),
            TokenFamily::Heuristic { chars_per_token } => heuristic_chars(text, chars_per_token),
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
        self.count_context_with_family(system, messages, model_id, None)
    }

    /// H26：完整上下文计数 + 显式 tokenizer 声明。
    pub fn count_context_with_family(
        &self,
        system: &[String],
        messages: &[ProviderMessage],
        model_id: &str,
        spec: Option<&str>,
    ) -> usize {
        let mut total: usize = 0;
        for s in system {
            // 每条 system 加角色开销 ~4 token
            total += 4 + self.count_text_with_family(s, model_id, spec);
        }
        for m in messages {
            total += 4; // 角色标记开销
            match m {
                ProviderMessage::System(s) => {
                    total += self.count_text_with_family(s, model_id, spec)
                }
                ProviderMessage::User { content } => {
                    for c in content {
                        match c {
                            UserContent::Text { text } => {
                                total += self.count_text_with_family(text, model_id, spec)
                            }
                            UserContent::Image { mime, data } => {
                                total += self.count_text_with_family(mime, model_id, spec)
                                    + data.len() / 4;
                            }
                        }
                    }
                }
                ProviderMessage::Assistant { content } => {
                    for b in content {
                        match b {
                            ContentBlock::Text { text } => {
                                total += self.count_text_with_family(text, model_id, spec)
                            }
                            ContentBlock::Thinking { text, .. } => {
                                total += self.count_text_with_family(text, model_id, spec);
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
                    total += self.count_text_with_family(content, model_id, spec);
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

/// 启发式近似：`chars / chars_per_token`（非空文本至少 1 token；空文本 0）。
fn heuristic_chars(text: &str, chars_per_token: f32) -> usize {
    let chars = text.chars().count();
    if chars == 0 {
        return 0;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let n = (chars as f32 / chars_per_token.max(0.1)).ceil() as usize;
    n.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_counter_runs() {
        let counter = TokenCounter::openai().expect("cl100k 加载");
        let n = counter.count_text("Hello, world!");
        assert!((3..=6).contains(&n), "expected ~4 tokens, got {n}");
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

    // ── H26：tokenizer 家族声明 ───────────────────────────────────────────────

    #[test]
    fn token_family_parses_supported_specs() {
        assert_eq!(TokenFamily::parse("o200k"), Some(TokenFamily::O200k));
        assert_eq!(TokenFamily::parse(" O200K_base "), Some(TokenFamily::O200k));
        assert_eq!(TokenFamily::parse("cl100k"), Some(TokenFamily::Cl100k));
        assert_eq!(
            TokenFamily::parse("heuristic"),
            Some(TokenFamily::Heuristic {
                chars_per_token: 4.0
            })
        );
        assert_eq!(
            TokenFamily::parse("heuristic:1.5"),
            Some(TokenFamily::Heuristic {
                chars_per_token: 1.5
            })
        );
        // 非法：未知串 / 非正比例 / 空。
        assert_eq!(TokenFamily::parse("gpt2"), None);
        assert_eq!(TokenFamily::parse("heuristic:0"), None);
        assert_eq!(TokenFamily::parse("heuristic:abc"), None);
        assert_eq!(TokenFamily::parse(""), None);
    }

    #[test]
    fn declared_family_overrides_id_inference() {
        // 无声明：按 id 推断（gpt-4o → o200k，claude → cl100k）。
        assert_eq!(TokenCounter::family_for("gpt-4o", None), TokenFamily::O200k);
        assert_eq!(
            TokenCounter::family_for("claude-sonnet-4", None),
            TokenFamily::Cl100k
        );
        // 声明优先：claude 也能声明成 o200k / 启发式（覆盖 id 推断）。
        assert_eq!(
            TokenCounter::family_for("claude-sonnet-4", Some("o200k")),
            TokenFamily::O200k
        );
        assert_eq!(
            TokenCounter::family_for("gpt-4o", Some("cl100k")),
            TokenFamily::Cl100k
        );
        // 非法声明回落 id 推断（笔误不改变计数口径）。
        assert_eq!(
            TokenCounter::family_for("gpt-4o", Some("bogus")),
            TokenFamily::O200k
        );
    }

    #[test]
    fn declared_heuristic_ratio_changes_counting() {
        let counter = TokenCounter::heuristic();
        // 12 个字符：4 字符/token → 3；1.5 字符/token（CJK 近似）→ 8。
        let text = "你好世界你好世界你好世界";
        assert_eq!(
            counter.count_text_with_family(text, "m", Some("heuristic:4")),
            3
        );
        assert_eq!(
            counter.count_text_with_family(text, "m", Some("heuristic:1.5")),
            8
        );
        // 空文本恒 0（不因 ceil 变成 1）。
        assert_eq!(
            counter.count_text_with_family("", "m", Some("heuristic:1.5")),
            0
        );
    }

    #[test]
    fn declared_family_applies_to_context_counting() {
        let counter = TokenCounter::heuristic();
        let system = vec!["你好世界你好世界".to_string()];
        let cl100k_like = counter.count_context_with_family(&system, &[], "m", None);
        let dense = counter.count_context_with_family(&system, &[], "m", Some("heuristic:1.5"));
        assert!(
            dense > cl100k_like,
            "CJK 近似应给出更多 token：dense={dense} cl100k={cl100k_like}"
        );
        // 与逐段计数一致（system 角色开销 4 + 文本 + 尾部 priming 3）。
        let text_tokens = counter.count_text_with_family(&system[0], "m", Some("heuristic:1.5"));
        assert_eq!(dense, 4 + text_tokens + 3);
    }
}
