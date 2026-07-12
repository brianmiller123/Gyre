//! 模型身份与线协议族。
//!
//! `Model` 决定 [`LlmProvider`](crate::LlmProvider) 的路由（通过 `api` 字段）与能力开关。

use serde::{Deserialize, Serialize};

/// 线协议族 —— 唯一决定「请求如何序列化 / 响应如何解析」的判别字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Api {
    /// Anthropic Messages API（Claude）。
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    /// OpenAI Responses API。
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    /// OpenAI Chat Completions API（兼容网关/本地 vLLM 最广）。
    #[serde(rename = "openai-completions")]
    OpenAiCompletions,
    /// DeepSeek API（基于 OpenAI 兼容，含 reasoning_content / R1 格式 / thinking 模式特化）。
    #[serde(rename = "deepseek")]
    DeepSeek,
    /// GLM（智谱 / Z.ai）官方 API（OpenAI 兼容 + thinking 开关 + reasoning_content / preserveReasoning 特化）。
    #[serde(rename = "zai")]
    Zai,
    /// Google Generative AI（Gemini）。
    #[serde(rename = "google-generative-ai")]
    GoogleGenerativeAi,
    /// Ollama Chat。
    #[serde(rename = "ollama-chat")]
    OllamaChat,
}

impl Api {
    /// 线协议的人类可读标识。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic-messages",
            Self::OpenAiResponses => "openai-responses",
            Self::OpenAiCompletions => "openai-completions",
            Self::DeepSeek => "deepseek",
            Self::Zai => "zai",
            Self::GoogleGenerativeAi => "google-generative-ai",
            Self::OllamaChat => "ollama-chat",
        }
    }
}

impl std::fmt::Display for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Api {
    type Err = crate::ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "anthropic-messages" => Ok(Self::AnthropicMessages),
            "openai-responses" => Ok(Self::OpenAiResponses),
            "openai-completions" => Ok(Self::OpenAiCompletions),
            "deepseek" => Ok(Self::DeepSeek),
            "zai" | "glm" => Ok(Self::Zai),
            "google-generative-ai" => Ok(Self::GoogleGenerativeAi),
            "ollama-chat" => Ok(Self::OllamaChat),
            other => Err(crate::ConfigError::Invalid(format!("未知 Api 线协议: '{other}'"))),
        }
    }
}

/// 模型身份 —— 决定 Provider 路由与能力开关。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    /// 模型 ID（如 `gpt-4o`）。
    pub id: String,
    /// Provider 标识（`openai` / `anthropic` / `google` / ...）。
    pub provider: String,
    /// 线协议族，决定 [`LlmProvider`](crate::LlmProvider) 路由目标。
    pub api: Api,
    /// 最大输入 token。
    pub max_input_tokens: usize,
    /// 最大输出 token。
    pub max_output_tokens: usize,
    /// 是否支持工具调用。
    pub supports_tools: bool,
    /// 是否支持流式。
    pub supports_streaming: bool,
    /// 是否支持思考（thinking/reasoning）。
    pub supports_thinking: bool,
    /// 额外请求体字段（per-model 配置），会在 Provider 构建请求体时合并到顶层。
    ///
    /// 用于传递 Provider 特有的非标准参数，如 vLLM 的 `chat_template_kwargs`：
    /// ```toml
    /// extra_body = { chat_template_kwargs = { thinking = true } }
    /// ```
    #[serde(default)]
    pub extra_body: Option<serde_json::Value>,
}

impl Model {
    /// 合理的默认能力（供 config 构造时回退）。
    #[must_use]
    pub fn with_defaults(id: impl Into<String>, provider: impl Into<String>, api: Api) -> Self {
        Self {
            id: id.into(),
            provider: provider.into(),
            api,
            max_input_tokens: 128_000,
            max_output_tokens: 8192,
            supports_tools: true,
            supports_streaming: true,
            supports_thinking: false,
            extra_body: None,
        }
    }
}
