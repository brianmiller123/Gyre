//! P1 检索质量：查询意图分类与权重偏置（直译 mnemopi `core/query-intent.ts`）。
//!
//! 查询命中不同语义类别（时间/事实/实体/偏好/流程）时，按类别 bias 调整
//! fts/importance/temporal/vec 四维权重：偏好类查询更重视 importance，流程类
//! 更重视词法命中。未命中（general）bias 全 1.0，行为与关闭时一致。

use std::sync::OnceLock;

use regex::Regex;

/// 查询意图类别（对齐 mnemopi `QueryIntentCategory`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentCategory {
    Temporal,
    Factual,
    Entity,
    Preference,
    Procedural,
    General,
}

/// 类别权重偏置（对齐 mnemopi `INTENT_WEIGHTS`：vec/fts/importance 三维 +
/// 本实现扩展的 temporal 维——时间类查询加大时间信号，偏好/实体类削弱之）。
#[derive(Debug, Clone, Copy)]
pub struct IntentWeights {
    pub vec_bias: f64,
    pub fts_bias: f64,
    pub importance_bias: f64,
    pub temporal_bias: f64,
}

pub const INTENT_WEIGHTS: [(IntentCategory, IntentWeights); 6] = [
    (
        IntentCategory::Temporal,
        IntentWeights {
            vec_bias: 0.6,
            fts_bias: 1.5,
            importance_bias: 0.8,
            temporal_bias: 1.3,
        },
    ),
    (
        IntentCategory::Factual,
        IntentWeights {
            vec_bias: 1.0,
            fts_bias: 1.2,
            importance_bias: 0.9,
            temporal_bias: 1.0,
        },
    ),
    (
        IntentCategory::Entity,
        IntentWeights {
            vec_bias: 1.1,
            fts_bias: 1.0,
            importance_bias: 1.3,
            temporal_bias: 0.8,
        },
    ),
    (
        IntentCategory::Preference,
        IntentWeights {
            vec_bias: 0.9,
            fts_bias: 0.8,
            importance_bias: 1.5,
            temporal_bias: 0.6,
        },
    ),
    (
        IntentCategory::Procedural,
        IntentWeights {
            vec_bias: 1.3,
            fts_bias: 0.9,
            importance_bias: 0.7,
            temporal_bias: 1.0,
        },
    ),
    (
        IntentCategory::General,
        IntentWeights {
            vec_bias: 1.0,
            fts_bias: 1.0,
            importance_bias: 1.0,
            temporal_bias: 1.0,
        },
    ),
];

/// 分类结果。
#[derive(Debug, Clone)]
pub struct QueryIntent {
    pub category: IntentCategory,
    /// 命中强度（0.3 + 0.15 × 命中数，封顶 1.0）。
    pub confidence: f64,
    /// 命中过的类别（含非最优）。
    pub signals: Vec<IntentCategory>,
    pub weights: IntentWeights,
}

fn intent_patterns() -> &'static Vec<(IntentCategory, Vec<Regex>)> {
    static PATTERNS: OnceLock<Vec<(IntentCategory, Vec<Regex>)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let groups: &[(IntentCategory, &[&str])] = &[
            (
                IntentCategory::Temporal,
                &[
                    r"\b(when|last|yesterday|today|tomorrow|ago|before|after|since|until|during|recently|lately)\b",
                    r"\b(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b",
                    r"\b(january|february|march|april|may|june|july|august|september|october|november|december)\b",
                    r"\b\d{4}-\d{2}-\d{2}\b",
                    r"\b\d{1,2}[/-]\d{1,2}[/-]\d{2,4}\b",
                    r"\b(this|next|last)\s+(week|month|year|monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b",
                    r"\b\d+\s+(day|week|month|year|hour|minute)s?\s+(ago|from now|later|earlier)\b",
                ],
            ),
            (
                IntentCategory::Factual,
                &[
                    r"\bwhat\s+is\b",
                    r"\bwho\s+is\b",
                    r"\bwhere\s+is\b",
                    r"\b(definition|define|explain|meaning)\b",
                    r"\bhow\s+(many|much|long|far)\b",
                ],
            ),
            (
                IntentCategory::Entity,
                &[
                    r"\b(tell\s+me\s+about|what\s+do\s+you\s+know\s+about)\b",
                    r"\b(who\s+is|what\s+does)\s+[a-z]+\b",
                    r"\b(about|regarding|concerning)\s+[a-z]+\b",
                ],
            ),
            (
                IntentCategory::Preference,
                &[
                    r"\b(prefer|like|dislike|want|hate|love|enjoy|favorite|best|worst)\b",
                    r"\b(should\s+i|would\s+you|do\s+you\s+recommend)\b",
                    r"\b(choose|pick|select|option|choice|decide)\b",
                ],
            ),
            (
                IntentCategory::Procedural,
                &[
                    r"\bhow\s+(to|do|can|should|would)\b",
                    r"\b(step|process|procedure|workflow|guide|tutorial)\b",
                    r"\b(setup|install|configure|build|deploy|run|execute|start|stop)\b",
                ],
            ),
        ];
        groups
            .iter()
            .map(|(category, patterns)| {
                (
                    *category,
                    patterns
                        .iter()
                        .map(|p| Regex::new(p).expect("静态正则合法"))
                        .collect(),
                )
            })
            .collect()
    })
}

/// 分类查询意图：逐类别累计命中数，score = min(0.3 + 0.15×命中, 1.0)，取最高。
///
/// # Panics
/// 内建正则编译失败时（静态模式，正常不会发生）。
#[must_use]
pub fn classify_intent(query: &str) -> QueryIntent {
    let query_lower = query.to_ascii_lowercase();
    let mut best_category = IntentCategory::General;
    let mut best_score = 0.0f64;
    let mut signals = Vec::new();
    for (category, patterns) in intent_patterns() {
        let mut matches = 0usize;
        for pattern in patterns {
            if pattern.is_match(&query_lower) {
                matches += 1;
                signals.push(*category);
            }
        }
        if matches > 0 {
            #[allow(clippy::cast_precision_loss)] // 命中数 ≤ 模式数（个位数）
            let score = (matches as f64).mul_add(0.15, 0.3).min(1.0);
            if score > best_score {
                best_score = score;
                best_category = *category;
            }
        }
    }
    let weights = INTENT_WEIGHTS
        .iter()
        .find(|(c, _)| *c == best_category)
        .map(|(_, w)| *w)
        .expect("类别必在权重表");
    QueryIntent {
        category: best_category,
        confidence: best_score,
        signals,
        weights,
    }
}

/// 调整四维权重（不归一化——Gyre 权重是加性结构，同除总量不影响排序）。
#[must_use]
pub fn adjust_weights(
    fts: f64,
    importance: f64,
    temporal: f64,
    vec: f64,
    intent: &QueryIntent,
) -> (f64, f64, f64, f64) {
    (
        fts * intent.weights.fts_bias,
        importance * intent.weights.importance_bias,
        temporal * intent.weights.temporal_bias,
        vec * intent.weights.vec_bias,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // 字面量 1.5 二进制精确表示，严格比较安全
    #[allow(clippy::float_cmp)]
    fn preference_query_classified() {
        let intent = classify_intent("which is the best approach for this?");
        assert_eq!(intent.category, IntentCategory::Preference);
        assert!(intent.confidence > 0.0);
        assert_eq!(intent.weights.importance_bias, 1.5);
    }

    #[test]
    // 字面量 1.5 二进制精确表示，严格比较安全
    #[allow(clippy::float_cmp)]
    fn temporal_query_classified() {
        let intent = classify_intent("when did we last deploy?");
        assert_eq!(intent.category, IntentCategory::Temporal);
        assert_eq!(intent.weights.fts_bias, 1.5);
    }

    #[test]
    fn procedural_query_classified() {
        let intent = classify_intent("how to configure the server?");
        assert_eq!(intent.category, IntentCategory::Procedural);
    }

    #[test]
    fn general_query_identity_weights() {
        let intent = classify_intent("奇奇怪怪的杂项");
        assert_eq!(intent.category, IntentCategory::General);
        approx(intent.weights.vec_bias, 1.0);
        approx(intent.weights.fts_bias, 1.0);
        approx(intent.weights.importance_bias, 1.0);
        // general 不调整：adjust_weights 恒等
        let (f, i, t, v) = adjust_weights(1.0, 0.5, 0.3, 0.5, &intent);
        approx(f, 1.0);
        approx(i, 0.5);
        approx(t, 0.3);
        approx(v, 0.5);
    }

    #[test]
    fn preference_boosts_importance() {
        let intent = classify_intent("do you recommend fast storage?");
        let (f, i, t, _v) = adjust_weights(1.0, 0.5, 0.3, 0.5, &intent);
        approx(f, 0.8);
        approx(i, 0.75);
        approx(t, 0.18);
    }

    fn approx(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-12, "{actual} ≈ {expected}");
    }
}
