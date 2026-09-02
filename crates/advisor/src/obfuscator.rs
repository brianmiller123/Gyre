//! Secret 脱敏：评审快照喂给 advisor 模型前掩码常见凭据形态。
//!
//! 移植 oh-my-pi `SecretObfuscator` 的简化版：内置高频 pattern（API key / token /
//! AWS / GitHub PAT / Bearer），额外支持调用方追加 pattern。替换为 `<redacted>`。

use regex::Regex;

/// 凭据掩码器（线程安全）。
pub struct SecretObfuscator {
    patterns: Vec<Regex>,
}

impl Default for SecretObfuscator {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretObfuscator {
    /// 内置 pattern 构造器。
    #[must_use]
    pub fn new() -> Self {
        let builtin = [
            // 形如 `key = value` / `key: value` / `key=value`（引号可选，值 ≥ 8 字符）。
            r#"(?i)(api[_-]?key|access[_-]?key|secret|password|passwd|token)\s*[:=]\s*["']?[A-Za-z0-9_\-./+]{8,}"#,
            // OpenAI / Anthropic 风格。
            r"sk-[A-Za-z0-9_\-]{16,}",
            r"sk-ant-[A-Za-z0-9_\-]{16,}",
            // GitHub。
            r"ghp_[A-Za-z0-9]{20,}",
            r"github_pat_[A-Za-z0-9_]{20,}",
            // AWS access key。
            r"AKIA[0-9A-Z]{16}",
            // Bearer 头。
            r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{12,}",
            // 常见环境变量导出。
            r#"(?i)(export\s+)?(OPENAI|ANTHROPIC|GEMINI|GITHUB|HUGGINGFACE|GOOGLE|AZURE)_?(API|ACCESS|SECRET|TOKEN|KEY)["']?=["']?[A-Za-z0-9_\-./+]{8,}"#,
        ];
        let patterns = builtin
            .iter()
            .map(|p| Regex::new(p).expect("内置脱敏 pattern 必须合法"))
            .collect();
        Self { patterns }
    }

    /// 追加自定义 pattern（用户/仓库级）。
    pub fn add_pattern(&mut self, re: Regex) {
        self.patterns.push(re);
    }

    /// 掩码文本中的凭据。
    #[must_use]
    pub fn obfuscate(&self, text: &str) -> String {
        let mut out = text.to_string();
        for p in &self.patterns {
            out = p.replace_all(&out, "<redacted>").into_owned();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_common_keys() {
        let o = SecretObfuscator::new();
        let text = "api_key=sk-1234567890abcdefgh set it";
        let out = o.obfuscate(text);
        assert!(!out.contains("sk-1234567890abcdefgh"), "{out}");
        assert!(out.contains("<redacted>"), "{out}");

        let text2 = "token: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef";
        let out2 = o.obfuscate(text2);
        assert!(
            out2.contains("<redacted>") && !out2.contains("ghp_"),
            "{out2}"
        );
    }

    #[test]
    fn masks_bearer_and_env() {
        let o = SecretObfuscator::new();
        let out = o.obfuscate("Authorization: Bearer abc.def.ghi-jkl_mno");
        assert!(
            out.contains("<redacted>") && !out.contains("abc.def"),
            "{out}"
        );

        let out2 = o.obfuscate("export OPENAI_API_KEY=sk-xxxxxxxxxxxxxxxx");
        assert!(
            out2.contains("<redacted>") && !out2.contains("sk-xxx"),
            "{out2}"
        );
    }

    #[test]
    fn leaves_plain_text() {
        let o = SecretObfuscator::new();
        let text = "the quick brown fox jumps over the lazy dog";
        assert_eq!(o.obfuscate(text), text);
    }

    #[test]
    fn custom_pattern_appended() {
        let mut o = SecretObfuscator::new();
        o.add_pattern(Regex::new(r"MYCORP-[A-Za-z0-9]{10,}").unwrap());
        let out = o.obfuscate("key MYCORP-abc123xyz999 here");
        assert!(
            out.contains("<redacted>") && !out.contains("MYCORP"),
            "{out}"
        );
    }
}
