//! H25：内置模型目录（离线近似）。
//!
//! oh-my-pi 用 `packages/catalog/src/models.json`（生成自上游目录）驱动上下文窗口 /
//! 输出上限 / 思考能力 / 路由策略。Gyre 不引入生成管线，改为维护一张**精简内置目录**：
//!
//! - **只做补缺**：仅当 `[[models]]` 未显式给出 `max_input_tokens` / `max_output_tokens`
//!   时，用目录里的近似值覆盖内置兜底（128_000 / 4096）；显式配置永远优先。
//! - **未知模型零影响**：pattern 不匹配 → 行为与引入目录前完全一致（既有配置与测试不受影响）。
//! - **思考能力**：目录声明 `supports_thinking = false` 的模型即便全局 `enable_thinking`
//!   打开也不会带思考配置（例如 gpt-4o / deepseek-chat）；目录未覆盖的模型沿用全局开关。
//! - 数值是**公开文档的近似值**，用于压缩触发与预算估算，不保证与上游变更同步；需要精确
//!   控制就在配置里显式写窗口大小。
//!
//! 运行时发现（`agent models list` 拉取 provider `/models`）与目录互补：目录负责「开箱即用
//! 的合理默认」，发现负责「当前账号真实可用的清单」。

use crate::config::wildcard_match;
use agent_core::Api;

/// 一条目录记录（pattern 支持 `*`/`?` 通配）。
#[derive(Debug, Clone, Copy)]
pub struct CatalogEntry {
    /// 模型 id 匹配模式（如 `gpt-4o*`、`claude-sonnet-4*`）。
    pub pattern: &'static str,
    /// 上下文窗口（最大输入 token，近似）。
    pub max_input_tokens: usize,
    /// 最大输出 token（近似）。
    pub max_output_tokens: usize,
    /// 是否支持思考（extended thinking / reasoning）。
    pub supports_thinking: bool,
    /// 建议线协议（仅供展示；实际以配置 `api` 为准）。
    pub api: Api,
}

/// 内置目录（顺序即匹配优先级：更具体的模式在前）。
pub const CATALOG: &[CatalogEntry] = &[
    // ── OpenAI ──
    CatalogEntry {
        pattern: "gpt-5*",
        max_input_tokens: 400_000,
        max_output_tokens: 128_000,
        supports_thinking: true,
        api: Api::OpenAiResponses,
    },
    CatalogEntry {
        pattern: "gpt-4.1*",
        max_input_tokens: 1_000_000,
        max_output_tokens: 32_768,
        supports_thinking: false,
        api: Api::OpenAiCompletions,
    },
    CatalogEntry {
        pattern: "gpt-4o*",
        max_input_tokens: 128_000,
        max_output_tokens: 16_384,
        supports_thinking: false,
        api: Api::OpenAiCompletions,
    },
    CatalogEntry {
        pattern: "o1*",
        max_input_tokens: 200_000,
        max_output_tokens: 100_000,
        supports_thinking: true,
        api: Api::OpenAiResponses,
    },
    CatalogEntry {
        pattern: "o3*",
        max_input_tokens: 200_000,
        max_output_tokens: 100_000,
        supports_thinking: true,
        api: Api::OpenAiResponses,
    },
    CatalogEntry {
        pattern: "o4-mini*",
        max_input_tokens: 200_000,
        max_output_tokens: 100_000,
        supports_thinking: true,
        api: Api::OpenAiResponses,
    },
    // ── Anthropic ──
    CatalogEntry {
        pattern: "claude-sonnet-4*",
        max_input_tokens: 200_000,
        max_output_tokens: 64_000,
        supports_thinking: true,
        api: Api::AnthropicMessages,
    },
    CatalogEntry {
        pattern: "claude-opus-4*",
        max_input_tokens: 200_000,
        max_output_tokens: 32_000,
        supports_thinking: true,
        api: Api::AnthropicMessages,
    },
    CatalogEntry {
        pattern: "claude-haiku-4*",
        max_input_tokens: 200_000,
        max_output_tokens: 64_000,
        supports_thinking: true,
        api: Api::AnthropicMessages,
    },
    CatalogEntry {
        pattern: "claude-3-7-sonnet*",
        max_input_tokens: 200_000,
        max_output_tokens: 64_000,
        supports_thinking: true,
        api: Api::AnthropicMessages,
    },
    CatalogEntry {
        pattern: "claude-3-5-sonnet*",
        max_input_tokens: 200_000,
        max_output_tokens: 8_192,
        supports_thinking: true,
        api: Api::AnthropicMessages,
    },
    CatalogEntry {
        pattern: "claude-3-5-haiku*",
        max_input_tokens: 200_000,
        max_output_tokens: 8_192,
        supports_thinking: false,
        api: Api::AnthropicMessages,
    },
    // ── DeepSeek ──
    CatalogEntry {
        pattern: "deepseek-reasoner*",
        max_input_tokens: 128_000,
        max_output_tokens: 64_000,
        supports_thinking: true,
        api: Api::DeepSeek,
    },
    CatalogEntry {
        pattern: "deepseek*",
        max_input_tokens: 128_000,
        max_output_tokens: 8_192,
        supports_thinking: false,
        api: Api::DeepSeek,
    },
    // ── Google ──
    CatalogEntry {
        pattern: "gemini-2.5*",
        max_input_tokens: 1_000_000,
        max_output_tokens: 65_536,
        supports_thinking: true,
        api: Api::GoogleGenerativeAi,
    },
    CatalogEntry {
        pattern: "gemini-2.0*",
        max_input_tokens: 1_000_000,
        max_output_tokens: 8_192,
        supports_thinking: true,
        api: Api::GoogleGenerativeAi,
    },
    CatalogEntry {
        pattern: "gemini-1.5*",
        max_input_tokens: 1_000_000,
        max_output_tokens: 8_192,
        supports_thinking: false,
        api: Api::GoogleGenerativeAi,
    },
    // ── 智谱 GLM ──
    CatalogEntry {
        pattern: "glm-4.5*",
        max_input_tokens: 128_000,
        max_output_tokens: 16_384,
        supports_thinking: true,
        api: Api::Zai,
    },
    CatalogEntry {
        pattern: "glm-4*",
        max_input_tokens: 128_000,
        max_output_tokens: 16_384,
        supports_thinking: false,
        api: Api::Zai,
    },
    // ── 其它常见开源/网关模型 ──
    CatalogEntry {
        pattern: "qwen3*",
        max_input_tokens: 128_000,
        max_output_tokens: 32_768,
        supports_thinking: true,
        api: Api::OpenAiCompletions,
    },
    CatalogEntry {
        pattern: "qwen*",
        max_input_tokens: 128_000,
        max_output_tokens: 8_192,
        supports_thinking: false,
        api: Api::OpenAiCompletions,
    },
    CatalogEntry {
        pattern: "kimi-k2*",
        max_input_tokens: 128_000,
        max_output_tokens: 16_384,
        supports_thinking: false,
        api: Api::OpenAiCompletions,
    },
    CatalogEntry {
        pattern: "grok-4*",
        max_input_tokens: 256_000,
        max_output_tokens: 32_768,
        supports_thinking: true,
        api: Api::OpenAiCompletions,
    },
    CatalogEntry {
        pattern: "grok-3*",
        max_input_tokens: 131_072,
        max_output_tokens: 16_384,
        supports_thinking: false,
        api: Api::OpenAiCompletions,
    },
    CatalogEntry {
        pattern: "llama-3*",
        max_input_tokens: 128_000,
        max_output_tokens: 8_192,
        supports_thinking: false,
        api: Api::OpenAiCompletions,
    },
    CatalogEntry {
        pattern: "mistral-large*",
        max_input_tokens: 128_000,
        max_output_tokens: 8_192,
        supports_thinking: false,
        api: Api::OpenAiCompletions,
    },
];

/// 按模型 id 查目录（首个匹配胜出；无匹配 → `None`）。
#[must_use]
pub fn lookup(model_id: &str) -> Option<&'static CatalogEntry> {
    let id = model_id.trim().to_ascii_lowercase();
    if id.is_empty() {
        return None;
    }
    CATALOG
        .iter()
        .find(|e| wildcard_match(e.pattern, id.as_str()))
}

/// 过滤目录（`agent models catalog <filter>`；大小写不敏感，匹配 id 或 api 名）。
#[must_use]
pub fn search(filter: &str) -> Vec<&'static CatalogEntry> {
    let f = filter.trim().to_ascii_lowercase();
    CATALOG
        .iter()
        .filter(|e| {
            f.is_empty()
                || e.pattern.to_ascii_lowercase().contains(&f)
                || e.api.as_str().contains(&f)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_matches_known_families_and_reports_each_field() {
        let e = lookup("gpt-4o-mini").expect("gpt-4o* 应命中");
        assert_eq!(e.max_input_tokens, 128_000);
        assert_eq!(e.max_output_tokens, 16_384);
        assert!(!e.supports_thinking, "gpt-4o 系列默认不带思考");
        assert_eq!(e.api, Api::OpenAiCompletions);

        let claude = lookup("claude-sonnet-4-20250514").expect("claude-sonnet-4* 应命中");
        assert_eq!(claude.api, Api::AnthropicMessages);
        assert!(claude.supports_thinking);

        // 大小写与空白不敏感。
        assert!(lookup("  GPT-5  ").is_some());
        // 更具体的模式优先于宽模式（deepseek-reasoner 先于 deepseek*）。
        let r = lookup("deepseek-reasoner").unwrap();
        assert!(r.supports_thinking && r.max_output_tokens == 64_000);
        let chat = lookup("deepseek-chat").unwrap();
        assert!(!chat.supports_thinking);
    }

    #[test]
    fn unknown_models_and_empty_ids_do_not_match() {
        // 未知模型必须返回 None（配置里未列出的自建/网关模型行为不变）。
        assert!(lookup("my-local-model").is_none());
        assert!(lookup("").is_none());
        assert!(lookup("   ").is_none());
    }

    #[test]
    fn catalog_entries_are_well_formed() {
        for e in CATALOG {
            assert!(!e.pattern.is_empty(), "pattern 不得为空");
            assert!(
                e.max_input_tokens > 0 && e.max_output_tokens > 0,
                "{} 的窗口/输出必须为正",
                e.pattern
            );
            assert!(
                e.max_output_tokens < e.max_input_tokens,
                "{} 输出上限应小于上下文窗口",
                e.pattern
            );
            // pattern 必须能匹配至少一个「自身」形态的 id（去掉通配符后加后缀）。
            let probe = e.pattern.trim_end_matches('*').to_string() + "x";
            assert!(
                wildcard_match(e.pattern, &probe),
                "pattern {} 无法匹配示例 id {probe}",
                e.pattern
            );
        }
    }

    #[test]
    fn search_filters_by_id_and_api() {
        let all = search("");
        assert_eq!(all.len(), CATALOG.len());
        let anthropic = search("anthropic");
        assert!(
            anthropic.iter().all(|e| e.api == Api::AnthropicMessages),
            "按 api 过滤"
        );
        assert!(!anthropic.is_empty());
        let ds = search("deepseek");
        assert!(ds.len() >= 2);
        assert!(search("nonexistent-model").is_empty());
    }
}
