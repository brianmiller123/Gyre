//! [`ContextManager`] 端口 trait 与上下文压缩策略。

use crate::error::ContextError;
use crate::message::{AgentMessage, ProviderMessage};
use crate::model::Model;
use crate::tool::ToolSpec;

/// token 用量统计。
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    /// 当前上下文估算 token 数。
    pub current: usize,
    /// 模型上限。
    pub limit: usize,
}

impl TokenUsage {
    /// 是否接近上限（默认 80%，由 `context_window_guard` 传入）。
    #[must_use]
    pub fn near_limit(&self, guard: f32) -> bool {
        if self.limit == 0 {
            return false;
        }
        #[allow(clippy::cast_precision_loss)]
        let ratio = self.current as f32 / self.limit as f32;
        ratio >= guard
    }
}

/// 组装好的 Provider 上下文（发送给 LLM 前的稳定形态）。
#[derive(Debug, Clone)]
pub struct ProviderContext {
    /// 稳定前缀指纹（缓存命中判断）。
    pub fingerprint: String,
    /// system prompts（已冻结）。
    pub system: Vec<String>,
    /// Provider 线消息。
    pub messages: Vec<ProviderMessage>,
    /// token 估算。
    pub tokens: TokenUsage,
}

/// 压缩策略（移植 oh-my-pi compaction）。
#[derive(Debug, Clone)]
pub enum CompactionStrategy {
    /// 摘要：旧对话折叠为 handoff 摘要。
    Summarize {
        /// 目标 token 上限。
        max_tokens: usize,
    },
    /// 裁剪：丢弃最旧的非保护消息（tool-protection 保留工具结果）。
    Prune {
        /// 保留最近 N 条。
        keep_recent: usize,
    },
    /// 抖动：移除冗余/重复内容（shake）。
    Shake,
}

/// 上下文管理端口。
///
/// 实现要点（移植 oh-my-pi `append-only-context`）：
/// - [`ContextManager::append`] 是日常唯一变异路径（AppendOnlyLog 只追加）。
/// - [`ContextManager::build_provider_context`] 经 transformContext → convertToLlm →
///   StablePrefix 冻结，产出稳定字节序列以最大化 provider 前缀缓存命中。
/// - [`ContextManager::compact`] 是除 append 外唯一可 replaceTail 的合法路径。
#[async_trait::async_trait]
pub trait ContextManager: Send + Sync {
    /// 仅追加一条消息。
    async fn append(&self, message: AgentMessage);

    /// 更新稳定前缀（system + tool spec），用于模式切换 / MCP 重连 / 启动初始化。
    async fn set_system(&self, system: Vec<String>, tools: &[ToolSpec]);

    /// 组装 Provider 上下文。
    ///
    /// # Errors
    /// 超出窗口或内部错误时返回 [`ContextError`]。
    async fn build_provider_context(
        &self,
        model: &Model,
        tools: &[ToolSpec],
    ) -> Result<ProviderContext, ContextError>;

    /// 触发压缩。
    ///
    /// # Errors
    /// 压缩失败时返回 [`ContextError`]。
    async fn compact(&self, strategy: CompactionStrategy) -> Result<(), ContextError>;

    /// 当前 token 用量。
    fn token_usage(&self) -> TokenUsage;

    /// StablePrefix 指纹（缓存命中判断）。
    fn prefix_fingerprint(&self) -> String;
}
