//! [`LlmProvider`] 端口 trait 与相关类型。
//!
//! 这是「让模型说话」的唯一抽象。具体适配器（OpenAI/Anthropic）在 `crates/llm` 实现。

use std::pin::Pin;

use futures::stream::Stream;
use serde::{Deserialize, Serialize};

use crate::error::LlmError;
use crate::message::{AssistantMessage, ProviderMessage, Usage};
use crate::model::Model;
use crate::tool::{ToolChoiceDirective, ToolSpec};

/// 流式补全返回的异步事件流。
pub type AssistantEventStream = Pin<Box<dyn Stream<Item = AssistantEvent> + Send>>;

/// 思考配置（reasoning budget）。
#[derive(Debug, Clone)]
pub struct ThinkingConfig {
    /// 思考 token 预算。
    pub budget_tokens: usize,
}

impl ThinkingConfig {
    /// 构造思考配置。
    #[must_use]
    pub const fn new(budget_tokens: usize) -> Self {
        Self { budget_tokens }
    }
}

/// 推理强度档位（移植 oh-my-pi `Effort`）。
///
/// 用于 [`ThinkingPolicy::Auto`]：分类器把 prompt 难度映射到档位，
/// 再经 [`Effort::default_budget`] 转为 `budget_tokens`，钳到模型范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// 极简（仅做格式 / 单步操作）。
    Minimal,
    /// 低（简单问答、列表）。
    Low,
    /// 中（常规编码、解释）。
    Medium,
    /// 高（重构、调试、设计）。
    High,
    /// 超高（跨文件架构改动、复杂推理）。
    XHigh,
}

impl Effort {
    /// 档位 → 默认 `budget_tokens`。
    ///
    /// 反向对齐 [`crate::LlmProvider`] openai 适配器的 budget→effort 阈值
    ///（`>=32_000 → high`、`>=12_000 → medium`、否则 `low`），使 effort↔budget 往返一致。
    #[must_use]
    pub const fn default_budget(self) -> usize {
        match self {
            Self::Minimal => 1_000,
            Self::Low => 4_000,
            Self::Medium => 12_000,
            Self::High => 32_000,
            Self::XHigh => 64_000,
        }
    }

    /// 档位 → 钳到 `[min, max]` 的 `budget_tokens`。
    #[must_use]
    pub fn budget_clamped(self, min: usize, max: usize) -> usize {
        self.default_budget().clamp(min, max)
    }

    /// 从模型输出文本解析档位（小写、容错；未识别 → None）。
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim().to_lowercase();
        Some(match t.as_str() {
            "minimal" | "trivial" => Self::Minimal,
            "low" | "moderate" => Self::Low,
            "medium" => Self::Medium,
            "high" | "hard" => Self::High,
            "xhigh" | "x-high" | "extra_high" | "extra-high" => Self::XHigh,
            _ => return None,
        })
    }
}

/// 思考难度分类器（移植 oh-my-pi [`auto-thinking/classifier.ts`](https://github.com/can1357/oh-my-pi/blob/master/packages/coding-agent/src/auto-thinking/classifier.ts)）。
///
/// 用 tiny/smol 模型把 prompt 分到 [`Effort`] 档。失败返回 `None`（调用方回退 fallback，
/// 不阻断 turn）。具体实现（在线 smol 模型 / 本地 on-device）在 `crates/llm` 提供。
#[async_trait::async_trait]
pub trait ThinkingClassifier: Send + Sync {
    /// 分类 `prompt` 难度。`model` 为本轮目标模型（供钳位参考）。
    ///
    /// 返回 `None` 的情形：无可用分类模型、网络失败、输出不可解析、被 abort/timeout。
    async fn classify(&self, prompt: &str, model: &Model) -> Option<Effort>;
}

/// 思考策略：静态预算或自适应（per-prompt 分类）。
///
/// - [`ThinkingPolicy::Static`]：整个 run 用同一 [`ThinkingConfig`]（当前默认行为）。
/// - [`ThinkingPolicy::Auto`]：每轮 prompt 前经 [`ThinkingClassifier`] 决定 [`Effort`] →
///   `budget_tokens`，钳到 `[min_budget, max_budget]`。模型不支持思考（`supports_thinking=false`）
///   → 本轮不思考；分类失败 → 用 `fallback`。
#[derive(Clone)]
pub enum ThinkingPolicy {
    /// 静态预算。
    Static(ThinkingConfig),
    /// 自适应预算。
    Auto {
        /// 难度分类器。
        classifier: std::sync::Arc<dyn ThinkingClassifier>,
        /// 分类失败时的兜底配置。
        fallback: ThinkingConfig,
        /// budget 钳位下限（默认 `1_000`）。
        min_budget: usize,
        /// budget 钳位上限（默认 `64_000`）。
        max_budget: usize,
    },
}

impl ThinkingPolicy {
    /// 默认钳位下限。
    pub const DEFAULT_MIN_BUDGET: usize = 1_000;
    /// 默认钳位上限。
    pub const DEFAULT_MAX_BUDGET: usize = 64_000;

    /// 构造自适应策略（使用默认钳位范围）。
    #[must_use]
    pub fn auto(
        classifier: std::sync::Arc<dyn ThinkingClassifier>,
        fallback: ThinkingConfig,
    ) -> Self {
        Self::Auto {
            classifier,
            fallback,
            min_budget: Self::DEFAULT_MIN_BUDGET,
            max_budget: Self::DEFAULT_MAX_BUDGET,
        }
    }

    /// 解析本轮思考配置。
    ///
    /// - `Static` → 直接返回内部配置。
    /// - `Auto` + 模型不支持思考 → `None`（本轮不思考）。
    /// - `Auto` + 分类成功 → 钳位后的 [`ThinkingConfig`]。
    /// - `Auto` + 分类失败 → `fallback`。
    ///
    /// `prompt` 为本轮用户输入（分类输入）；`model` 为目标模型。
    pub async fn resolve(&self, prompt: &str, model: &Model) -> Option<ThinkingConfig> {
        match self {
            Self::Static(cfg) => Some(cfg.clone()),
            Self::Auto {
                classifier,
                fallback,
                min_budget,
                max_budget,
            } => {
                if !model.supports_thinking {
                    return None;
                }
                // 截断保护（移植 classifyDifficulty MAX_INPUT_CHARS）：分类器输入过长会
                // 浪费 token 且降低准确度。head 4000 + tail 2000 = 6000 上限。
                let input = truncate_classifier_input(prompt);
                match classifier.classify(&input, model).await {
                    Some(effort) => {
                        let budget = effort.budget_clamped(*min_budget, *max_budget);
                        Some(ThinkingConfig::new(budget))
                    }
                    None => Some(fallback.clone()),
                }
            }
        }
    }
}

/// 截断分类器输入：超长文本取 head 4000 + tail 2000（移植 oh-my-pi `HEAD_CHARS/TAIL_CHARS`）。
#[must_use]
fn truncate_classifier_input(s: &str) -> String {
    const MAX: usize = 6_000;
    const HEAD: usize = 4_000;
    const TAIL: usize = 2_000;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(HEAD).collect();
    let tail: String = s
        .chars()
        .rev()
        .take(TAIL)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n…[截断]…\n{tail}")
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    /// H24：`auth` 的三种取值（含 omp 的 camelCase 写法）。
    #[test]
    fn auth_mode_parses_omp_and_gyre_spellings() {
        #[derive(serde::Deserialize)]
        struct Holder {
            auth: AuthMode,
        }
        let parse = |v: &str| -> AuthMode {
            serde_json::from_str::<Holder>(&format!("{{\"auth\":\"{v}\"}}"))
                .map(|h| h.auth)
                .unwrap()
        };
        assert_eq!(parse("api_key"), AuthMode::ApiKey);
        assert_eq!(parse("apiKey"), AuthMode::ApiKey, "omp 写法（camelCase）");
        assert_eq!(parse("none"), AuthMode::None);
        assert_eq!(parse("oauth"), AuthMode::OAuth);
        assert_eq!(AuthMode::default(), AuthMode::ApiKey);
    }

    /// H24：`auth = none` 或 key 为空时不发鉴权头（`builtin_api_key() == None`）。
    #[test]
    fn builtin_api_key_respects_auth_mode_and_empty_key() {
        let ctx = |auth: AuthMode, key: Option<&str>| ProviderCallContext {
            api_key: key.map(str::to_string),
            auth,
            ..Default::default()
        };
        assert_eq!(
            ctx(AuthMode::ApiKey, Some("sk-x")).builtin_api_key(),
            Some("sk-x")
        );
        assert_eq!(ctx(AuthMode::ApiKey, Some("")).builtin_api_key(), None);
        assert_eq!(ctx(AuthMode::ApiKey, None).builtin_api_key(), None);
        assert_eq!(
            ctx(AuthMode::None, Some("sk-x")).builtin_api_key(),
            None,
            "auth = none 时即使配了 key 也不发"
        );
        assert_eq!(
            ctx(AuthMode::OAuth, Some("tok")).builtin_api_key(),
            Some("tok")
        );
    }

    /// H24：`max_tokens_field` 的 wire 名称。
    #[test]
    fn max_tokens_field_names() {
        assert_eq!(MaxTokensField::default(), MaxTokensField::MaxTokens);
        assert_eq!(MaxTokensField::MaxTokens.as_str(), "max_tokens");
        assert_eq!(
            MaxTokensField::MaxCompletionTokens.as_str(),
            "max_completion_tokens"
        );
    }
}

#[cfg(test)]
mod effort_tests {
    use super::*;

    #[test]
    fn effort_default_budget_monotonic() {
        let budgets = [
            Effort::Minimal.default_budget(),
            Effort::Low.default_budget(),
            Effort::Medium.default_budget(),
            Effort::High.default_budget(),
            Effort::XHigh.default_budget(),
        ];
        // 档位越高预算越大。
        assert!(budgets.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn effort_budget_clamped_respects_bounds() {
        assert_eq!(Effort::Minimal.budget_clamped(4_000, 64_000), 4_000);
        assert_eq!(Effort::XHigh.budget_clamped(1_000, 32_000), 32_000);
        assert_eq!(Effort::Medium.budget_clamped(1_000, 64_000), 12_000);
    }

    #[test]
    fn effort_parse_recognizes_aliases() {
        assert_eq!(Effort::parse("Low"), Some(Effort::Low));
        assert_eq!(Effort::parse("  hard\n"), Some(Effort::High));
        assert_eq!(Effort::parse("trivial"), Some(Effort::Minimal));
        assert_eq!(Effort::parse("xhigh"), Some(Effort::XHigh));
        assert_eq!(Effort::parse("nonsense"), None);
    }

    #[test]
    fn truncate_classifier_input_keeps_short_unchanged() {
        assert_eq!(truncate_classifier_input("hello"), "hello");
    }

    #[test]
    fn truncate_classifier_input_head_tail_for_long() {
        let s: String = "a".repeat(10_000);
        let t = truncate_classifier_input(&s);
        assert!(t.contains("[截断]"));
        // head 4000 + 标记 + tail 2000，远短于原文。
        assert!(t.chars().count() < 7_000);
    }
}

/// 流式助手事件（增量优先，对齐 oh-my-pi `AssistantMessageEvent`）。
#[derive(Debug, Clone)]
pub enum AssistantEvent {
    /// 文本增量。
    TextDelta(String),
    /// 思考增量。
    ThinkingDelta(String),
    /// 工具调用开始。
    ToolCallStart {
        /// 调用 ID。
        id: String,
        /// 工具名。
        name: String,
    },
    /// 工具调用参数增量（部分 JSON）。
    ToolCallDelta {
        /// 调用 ID。
        id: String,
        /// 部分 JSON 字符串。
        partial_json: String,
    },
    /// 工具调用结束。
    ToolCallEnd {
        /// 调用 ID。
        id: String,
    },
    /// 消息开始。
    MessageStart,
    /// 消息增量快照（partial，供 UI 渲染）。
    MessageUpdate(AssistantMessage),
    /// 消息结束（完整消息）。
    MessageEnd(AssistantMessage),
    /// 用量更新。
    Usage(Usage),
    /// 错误。
    Error(LlmError),
}

/// 一次流式补全请求。
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// 目标模型。
    pub model: Model,
    /// 稳定前缀（已被 `StablePrefix` 冻结的 system prompts）。
    pub system: Vec<String>,
    /// 已转换的 Provider 线消息。
    pub messages: Vec<ProviderMessage>,
    /// 可用工具规格。
    pub tools: Vec<ToolSpec>,
    /// 工具选择指令。
    pub tool_choice: Option<ToolChoiceDirective>,
    /// 最大输出 token。
    pub max_tokens: usize,
    /// 温度。
    pub temperature: Option<f32>,
    /// 思考配置。
    pub thinking: Option<ThinkingConfig>,
    /// 前缀缓存 key（sessionId 回退）。
    pub cache_key: Option<String>,
    /// 与上次 [`crate::ContextManager::build_provider_context`] 相比，前缀字节稳定的消息数。
    ///
    /// `0` 表示无稳定前缀（首次构建 / 压缩 / 分支切换 / system 变更后）。provider 据此
    /// **精确**放置 `cache_control` breakpoint：命中此索引之前的消息可复用 KV 缓存，
    /// 避免每轮全量 re-prefill。移植 oh-my-pi `AppendOnlyContextManager.longestStablePrefix`。
    ///
    /// 例如 Anthropic 适配器把消息级 breakpoint 放在 `min(stable_prefix_len, len) - 1`
    ///（且不超过倒数第二条），而非固定的「倒数第二条」启发式——压缩/分支切换后稳定前缀
    /// 缩短，breakpoint 随之前移，避免把 cache 配额浪费在已失效的位置。
    pub stable_prefix_len: usize,
}

/// 内置鉴权模式（H24，对齐 oh-my-pi provider `auth`）。
///
/// - [`AuthMode::ApiKey`]（默认）：按 `config → oauth.toml → auth.toml → env` 分层解析；
/// - [`AuthMode::None`]：**不发送**内置鉴权头（本地网关 / 免鉴权端点）；
/// - [`AuthMode::OAuth`]：只使用 OAuth 凭据存储中的访问令牌（缺失即报错，不回退 env）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AuthMode {
    /// 使用 API key（默认）。
    #[default]
    #[serde(rename = "api_key", alias = "apiKey", alias = "apikey")]
    ApiKey,
    /// 不发送内置鉴权头。
    #[serde(rename = "none")]
    None,
    /// 使用 OAuth 凭据存储中的访问令牌。
    #[serde(rename = "oauth", alias = "OAuth")]
    OAuth,
}

/// `max_tokens` 字段名（H24，对齐 oh-my-pi `compat.maxTokensField`）。
///
/// 部分 OpenAI 兼容网关只认 `max_completion_tokens`（或反之），默认保持 Gyre 既有 wire
/// （`max_tokens`），显式配置才切换。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MaxTokensField {
    /// `max_tokens`（默认）。
    #[default]
    #[serde(rename = "max_tokens")]
    MaxTokens,
    /// `max_completion_tokens`。
    #[serde(rename = "max_completion_tokens")]
    MaxCompletionTokens,
}

impl MaxTokensField {
    /// wire 字段名。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

/// 兼容开关（H24）：OpenAI 兼容网关常见的「参数不被接受」问题。
///
/// 全部默认关闭（保持标准 wire）；仅当目标网关明确拒绝某参数时才开启——避免为兼容
/// 而默认削弱请求语义。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderQuirks {
    /// 省略 `temperature`（部分推理模型/gateway 拒绝该参数）。
    pub omit_temperature: bool,
    /// 省略 `stream_options.include_usage`（部分兼容网关不认该字段并报 400）。
    pub omit_stream_options: bool,
    /// 省略 `tool_choice`（强制选择会让部分网关 400）。
    pub omit_tool_choice: bool,
    /// 省略 `reasoning_effort` / `reasoning`（非 OpenAI 官方推理模型常见拒绝）。
    pub omit_reasoning: bool,
    /// `max_tokens` 的 wire 字段名（`None` = 默认 `max_tokens`）。
    pub max_tokens_field: Option<MaxTokensField>,
}

impl ProviderQuirks {
    /// 是否存在任一开关（省去调用方逐项判断）。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !(self.omit_temperature
            || self.omit_stream_options
            || self.omit_tool_choice
            || self.omit_reasoning)
            && self.max_tokens_field.is_none()
    }
}

/// 单次 Provider 调用的运行时上下文（鉴权、并发限流等）。
#[derive(Debug, Clone, Default)]
pub struct ProviderCallContext {
    /// API key（已 ${ENV} 展开）。
    pub api_key: Option<String>,
    /// base URL（自定义网关/本地 vLLM 可改）。
    pub base_url: Option<String>,
    /// per-provider 并发上限（in-flight 限流，移植 oh-my-pi lease）。
    pub max_in_flight: Option<usize>,
    /// 自定义请求头（H24）：模型 profile 的 `headers` 表，`${ENV}` 已展开。
    ///
    /// 适配器在**内置鉴权头之后**应用，因此同名头以用户配置为准（网关需要非 Bearer
    /// 方案时可在 `headers` 里直接给 `Authorization`）。
    pub headers: Vec<(String, String)>,
    /// 兼容开关（H24）：网关/本地模型的参数兼容性微调，键见
    /// [`ProviderQuirks`](crate::ProviderQuirks)。
    pub quirks: crate::ProviderQuirks,
    /// 内置鉴权模式（H24）：`none` 时适配器不发送内置鉴权头。
    pub auth: AuthMode,
}

impl ProviderCallContext {
    /// 内置鉴权键：`auth = none` 或键为空时返回 `None`（适配器此时**不加**鉴权头，
    /// 而不是发送 `Authorization: Bearer `）。
    #[must_use]
    pub fn builtin_api_key(&self) -> Option<&str> {
        if self.auth == AuthMode::None {
            return None;
        }
        self.api_key.as_deref().filter(|key| !key.is_empty())
    }
}

/// LLM Provider 端口：具体实现是适配器（OpenAI/Anthropic/...）。
///
/// 由 `ProviderRegistry`（`crates/llm`）按 [`Model::api`](crate::Model::api) 路由。
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    /// 唯一标识，用于 registry 与日志。
    fn id(&self) -> &'static str;

    /// 该适配器支持的线协议族（决定路由）。
    fn supports(&self) -> &[crate::model::Api];

    /// 流式补全 —— 唯一的「让模型说话」入口。
    ///
    /// # Errors
    /// 网络、鉴权、解析错误时返回 [`LlmError`]。
    async fn stream(
        &self,
        request: CompletionRequest,
        ctx: &ProviderCallContext,
    ) -> Result<AssistantEventStream, LlmError>;
}

/// 一个总是返回错误的桩 Provider（测试/未配置场景）。
pub struct UnconfiguredProvider;

#[async_trait::async_trait]
impl LlmProvider for UnconfiguredProvider {
    fn id(&self) -> &'static str {
        "unconfigured"
    }
    fn supports(&self) -> &[crate::model::Api] {
        &[]
    }
    async fn stream(
        &self,
        _request: CompletionRequest,
        _ctx: &ProviderCallContext,
    ) -> Result<AssistantEventStream, LlmError> {
        Err(LlmError::Unsupported("未配置任何 LLM Provider".into()))
    }
}

/// 将单个事件包成单元素流（供同步/桩实现复用）。
#[must_use]
pub fn once(event: AssistantEvent) -> AssistantEventStream {
    Box::pin(futures::stream::iter(vec![event]))
}
