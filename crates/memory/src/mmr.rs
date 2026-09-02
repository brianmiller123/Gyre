//! P1 检索质量：MMR 多样性重排（直译 mnemopi `core/mmr.ts` 的纯 TS 循环版）。
//!
//! 最大化边界相关性：`λ·relevance − (1−λ)·maxSimilarity(与已选集合)`。
//! 首元素恒为原始相关性第一名，因此不改变“最相关唯一”的既有断言。
//! 相似度用 Gyre 的 [`super::structured::tokenize`] 分词（对中文友好，
//! mnemopi 的 whitespace 切分对无空格文本退化为整串）。

/// 两个文本的 Jaccard 相似度（按词集合；任一为空则 0）。
///
/// # Panics
/// 无。
#[must_use]
pub fn jaccard_similarity(tokens_a: &[String], tokens_b: &[String]) -> f64 {
    if tokens_a.is_empty() || tokens_b.is_empty() {
        return 0.0;
    }
    let set_b: std::collections::HashSet<&str> = tokens_b.iter().map(String::as_str).collect();
    let mut intersection = 0usize;
    let mut seen_a: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for token in tokens_a {
        if seen_a.insert(token.as_str()) && set_b.contains(token.as_str()) {
            intersection += 1;
        }
    }
    let union = seen_a.len() + set_b.len() - intersection;
    if union == 0 {
        0.0
    } else {
        // 集合大小有界（token 数），usize→f64 精度损失无实际影响
        #[allow(clippy::cast_precision_loss)]
        let inter = intersection as f64;
        #[allow(clippy::cast_precision_loss)]
        let uni = union as f64;
        inter / uni
    }
}

/// 中文高频虚词单字（MMR 相似度/近重复判定专用停用字）。
///
/// 单字级 tokenize 下“的/了/在”等虚词为所有中文记录共享，抬高 Jaccard 与
/// containment 的假交集；过滤后相似度只反映实词重叠。注意：**不参与 BM25 检索**
/// （检索停用词另见 [`super::synonyms::STOP_WORDS`]，默认未启用）。
const CJK_STOP_CHARS: &str = "的了在是与和及或等对为从到于之其被把让给也都还很更最就又并而但且只才已曾正将这那我你他她它它们不有无上下中内外前后间时后年里来出过个些样种";

/// 过滤中文停用单字（MMR 相似度输入专用）：保留拉丁/数字 token 与中文实词单字。
///
/// # Panics
/// 无。
#[must_use]
pub fn strip_cjk_stop_chars(tokens: &[String]) -> Vec<String> {
    tokens
        .iter()
        .filter(|t| {
            let mut chars = t.chars();
            // 仅过滤「单字 CJK 虚词」；空/多字 token（拉丁词、数字串、复字词）不过滤
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return true;
            };
            !(CJK_STOP_CHARS.contains(c) && ('\u{3400}'..='\u{9FFF}').contains(&c))
        })
        .cloned()
        .collect()
}

/// 两个文本的 containment 相似度 `|A∩B| / min(|A|,|B|)`（任一为空则 0）。
///
/// 对“追加型重复”（短文本是长文本的子集，如 auto-retain 两次沉淀近同内容），
/// Jaccard 因追加部分稀释并集而低估（0.85 级），containment 给到 1.0——
/// 与 mnemopi 语义级向量相似度（同源记录 cosine≈1）对齐，是中文近重复判定的
/// 正确度量。
///
/// # Panics
/// 无。
#[must_use]
pub fn containment_similarity(tokens_a: &[String], tokens_b: &[String]) -> f64 {
    if tokens_a.is_empty() || tokens_b.is_empty() {
        return 0.0;
    }
    let set_b: std::collections::HashSet<&str> = tokens_b.iter().map(String::as_str).collect();
    let mut intersection = 0usize;
    let mut seen_a: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for token in tokens_a {
        if seen_a.insert(token.as_str()) && set_b.contains(token.as_str()) {
            intersection += 1;
        }
    }
    let min_len = seen_a.len().min(set_b.len());
    if min_len == 0 {
        0.0
    } else {
        // 集合大小有界（token 数），usize→f64 精度损失无实际影响
        #[allow(clippy::cast_precision_loss)]
        let inter = intersection as f64;
        #[allow(clippy::cast_precision_loss)]
        let mn = min_len as f64;
        inter / mn
    }
}

/// MMR 重排：返回按多样性重排后的原始索引序列（长度 = min(limit, items)）。
///
/// - `items`：预排序（相关性降序）的候选；首元素必为索引 0。
/// - `similarity(i, j)`：候选 i 与 j 的相似度（0..=1）。
#[must_use]
pub fn mmr_rerank_indices(
    count: usize,
    scores: &[f64],
    similarity: impl Fn(usize, usize) -> f64,
    lambda: f64,
    limit: usize,
) -> Vec<usize> {
    let limit = limit.min(count);
    if limit <= 1 || count == 0 {
        return (0..limit).collect();
    }
    let mut selected: Vec<usize> = vec![0];
    let mut remaining: Vec<usize> = (1..count).collect();
    while !remaining.is_empty() && selected.len() < limit {
        let mut best_idx = 0usize;
        let mut best_score = f64::NEG_INFINITY;
        for (pos, &candidate) in remaining.iter().enumerate() {
            let mut max_similarity = 0.0f64;
            for &picked in &selected {
                let sim = similarity(candidate, picked);
                if sim > max_similarity {
                    max_similarity = sim;
                }
            }
            let relevance = scores[candidate];
            // λ·relevance − (1−λ)·maxSimilarity（mul_add 保精度）
            let mmr_score = (1.0 - lambda).mul_add(-max_similarity, lambda * relevance);
            if mmr_score > best_score {
                best_score = mmr_score;
                best_idx = pos;
            }
        }
        selected.push(remaining.swap_remove(best_idx));
    }
    if selected.len() < limit {
        selected.extend(&remaining[..(limit - selected.len()).min(remaining.len())]);
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(s: &str) -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    }

    #[test]
    fn jaccard_basic() {
        approx_eq(jaccard_similarity(&tokens("a b c"), &tokens("a b d")), 0.5);
        approx_eq(jaccard_similarity(&tokens("a b"), &tokens("a b")), 1.0);
        approx_eq(jaccard_similarity(&[], &tokens("a")), 0.0);
    }

    #[test]
    fn strip_cjk_stop_chars_filters_only_single_cjk_virtual_chars() {
        // 单字中文虚词被过滤；实词单字、拉丁词、多字串保留。
        let t = strip_cjk_stop_chars(&[
            "了".into(),
            "的".into(),
            "修复".into(),
            "登录".into(),
            "rust".into(),
            "2024".into(),
        ]);
        assert_eq!(t, vec!["修复", "登录", "rust", "2024"]);
        // 空输入安全
        assert!(strip_cjk_stop_chars(&[]).is_empty());
    }

    #[test]
    fn containment_rewards_subset_duplicates() {
        // 追加型重复：短文本是长文本子集 → containment = 1.0（Jaccard 只有 0.846）
        let a = strip_cjk_stop_chars(&chars_of("修复了登录页的认证问题"));
        let b = strip_cjk_stop_chars(&chars_of("修复了登录页的认证问题请尽快处理"));
        approx_eq(containment_similarity(&a, &b), 1.0);
        // 部分重叠：共享虚词在过滤后为 0
        let c = strip_cjk_stop_chars(&chars_of("部署了新的支付网关"));
        approx_eq(containment_similarity(&a, &c), 0.0);
        // 空输入安全
        approx_eq(containment_similarity(&[], &a), 0.0);
        approx_eq(containment_similarity(&a, &a), 1.0);
    }

    /// 逐字切分（模拟 structured::tokenize 的 CJK 单字路径）。
    fn chars_of(s: &str) -> Vec<String> {
        s.chars().map(|c| c.to_string()).collect()
    }

    #[test]
    fn mmr_keeps_top_and_diversifies() {
        // 候选 0/1 高度相似（重复内容），候选 2 独立但分稍低。
        let contents = [
            tokens("aaa bbb ccc"),
            tokens("aaa bbb ccc ddd"),
            tokens("xxx yyy zzz"),
        ];
        let scores = [1.0, 0.98, 0.8];
        let sim = |a: usize, b: usize| jaccard_similarity(&contents[a], &contents[b]);
        let idx = mmr_rerank_indices(3, &scores, sim, 0.7, 3);
        assert_eq!(idx[0], 0, "首元素恒为相关性第一");
        assert_eq!(idx[1], 2, "重复内容被多样性压后，独立候选上位");
        assert_eq!(idx[2], 1);
    }

    #[test]
    fn mmr_single_and_limit() {
        assert_eq!(mmr_rerank_indices(1, &[1.0], |_, _| 0.0, 0.7, 5), vec![0]);
        assert_eq!(
            mmr_rerank_indices(0, &[], |_, _| 0.0, 0.7, 5),
            Vec::<usize>::new()
        );
        let idx = mmr_rerank_indices(4, &[1.0; 4], |_, _| 0.0, 0.7, 2);
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn lambda_zero_prefers_diversity() {
        let contents = [tokens("a b c"), tokens("a b c d"), tokens("x y z")];
        let scores = [1.0, 0.9, 0.1];
        let sim = |a: usize, b: usize| jaccard_similarity(&contents[a], &contents[b]);
        // λ=0：纯多样性，首个仍为 0，随后选与集合最不相似的
        let idx = mmr_rerank_indices(3, &scores, sim, 0.0, 3);
        assert_eq!(idx[1], 2);
    }

    /// 浮点断言：绝对差 < 1e-12。
    fn approx_eq(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-12, "{actual} ≈ {expected}");
    }
}
