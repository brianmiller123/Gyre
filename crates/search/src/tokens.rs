//! Token 度量（tiktoken-rs，OpenAI BPE 近似）。
//!
//! 对 GPT-3.5/4 系（cl100k_base）精确；对其它 provider（Anthropic/Gemini…）为合理近似，
//! 供搜索结果展示「这批命中约 N tokens」、上下文预算估算等场景使用。

use std::sync::OnceLock;

use tiktoken_rs::cl100k_base;

/// 近似 token 计数（cl100k_base，含特殊 token 计数）。
///
/// # Errors
/// BPE 词表加载失败时返回错误。
pub fn count_tokens(text: &str) -> Result<usize, String> {
    static BPE: OnceLock<Result<tiktoken_rs::CoreBPE, String>> = OnceLock::new();
    let bpe = BPE.get_or_init(|| cl100k_base().map_err(|e| e.to_string()));
    match bpe {
        Ok(b) => Ok(b.encode_with_special_tokens(text).len()),
        Err(e) => Err(e.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_nonzero_for_english() {
        let n = count_tokens("hello world from rust").unwrap();
        assert!(n >= 3);
    }

    #[test]
    fn empty_is_zero() {
        assert_eq!(count_tokens("").unwrap(), 0);
    }

    #[test]
    fn chinese_counts_reasonably() {
        // 中文按 BPE 切分，每字约 1~2 token
        let n = count_tokens("你好世界").unwrap();
        assert!(n >= 2);
    }
}
