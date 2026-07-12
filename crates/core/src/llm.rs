//! [`LlmProvider`] 端口 trait 与相关类型。
//!
//! 这是「让模型说话」的唯一抽象。具体适配器（OpenAI/Anthropic）在 `crates/llm` 实现。

use std::pin::Pin;

use futures::stream::Stream;

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
    pub fn new(budget_tokens: usize) -> Self {
        Self { budget_tokens }
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
    /// 稳定前缀（已被 StablePrefix 冻结的 system prompts）。
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
