//! [`ContextManager`] 端口 trait 与上下文压缩策略。

use serde::{Deserialize, Serialize};

use crate::error::ContextError;
use crate::message::{AgentMessage, ProviderMessage, Usage};
use crate::model::Model;
use crate::tool::ToolSpec;

/// 会话节点 id（字符串，进程内全局唯一；持久化后跨进程亦唯一）。
pub type NodeId = String;

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
    /// 与上次 [`ContextManager::build_provider_context`] 相比，前缀字节稳定的消息数
    ///（provider 前缀缓存可命中到此索引）。
    ///
    /// `0` 表示无稳定前缀（首次构建 / 压缩 / 分支切换 / system 变更后）。移植 oh-my-pi
    /// `AppendOnlyContextManager` 的 `longestStablePrefix`：provider 端可据此精确放置
    /// `cache_control` breakpoint，最大化 KV 缓存命中、避免每轮全量 re-prefill。
    pub stable_prefix_len: usize,
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

/// 会话树节点：把 [`AgentMessage`] 包上 `id` + `parent_id`，构成可分支的森林。
///
/// 设计移植 oh-my-pi `SessionEntry`：每条消息是一个节点，`parent_id` 指向对话中的
/// 上一条消息（根节点为 `None`）。多个节点共享同一 `parent_id` 即构成分支（fork），
/// 支持「撤销某步重试」「探索两条方案」等会话树导航。
///
/// 持久化为 JSONL：每行一个 `SessionNode`。旧版线性日志（每行一个裸 `AgentMessage`）
/// 在加载时被无损迁移为单链树（见 `agent_context` 装配层）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionNode {
    /// 节点 id（全局唯一）。
    pub id: NodeId,
    /// 父节点 id（根节点为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<NodeId>,
    /// 该节点承载的内部富消息。
    pub message: AgentMessage,
}

impl SessionNode {
    /// 构造一个根节点（无父）。
    #[must_use]
    pub fn root(id: NodeId, message: AgentMessage) -> Self {
        Self {
            id,
            parent_id: None,
            message,
        }
    }

    /// 构造一个带父节点的子节点。
    #[must_use]
    pub fn child(id: NodeId, parent_id: NodeId, message: AgentMessage) -> Self {
        Self {
            id,
            parent_id: Some(parent_id),
            message,
        }
    }
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

    /// 删除指定日志索引（0-based）的消息，并连带清理其孤立工具结果。
    ///
    /// 返回实际移除的日志条目数（含被删除消息本身及其孤立 tool 结果）；
    /// 索引越界返回 `Ok(0)`。默认实现返回错误（不支持删除）。
    ///
    /// # Errors
    /// 不支持删除或内部错误时返回 [`ContextError`]。
    async fn delete_message_at(&self, _index: usize) -> Result<usize, ContextError> {
        Err(ContextError::Compaction("此上下文不支持删除消息".into()))
    }

    /// 当前 token 用量。
    fn token_usage(&self) -> TokenUsage;

    /// 当前活跃分支累计的 token 用量（input/output/cache/cost）。
    ///
    /// 默认返回空用量（线性桩上下文 / 不支持统计的实现）。供 UI 在重连 / 切换会话时
    /// 恢复用量显示——前端累计态被清零后，以此基线重建，后续单次增量帧叠加其上，
    /// 避免切换会话后历史用量丢失。
    fn accumulated_usage(&self) -> Usage {
        Usage::default()
    }

    /// StablePrefix 指纹（缓存命中判断）。
    fn prefix_fingerprint(&self) -> String;

    // ── 会话树 / 分支导航（P1-3）─────────────────────────────────────────
    //
    // 以下方法均带默认实现（返回空 / 否定），仅树形上下文（[`agent_context::InMemoryContext`]
    // / [`agent_context::PersistentContext`]）覆写。线性桩上下文无需关心。

    /// 当前活跃叶子节点 id（树形上下文）；线性上下文返回 `None`。
    async fn active_leaf(&self) -> Option<NodeId> {
        None
    }

    /// 切换活跃叶子（分支切换）：仅移动「续写点」，不注入 handoff。
    ///
    /// 返回 `true` 表示目标节点存在且已切换；`false` 表示不支持或节点不存在。
    /// 切换后追加的消息将从目标节点分叉，形成新分支。
    async fn set_active_leaf(&self, _id: &NodeId) -> bool {
        false
    }

    /// 切换到目标叶子并把「被离开分支」的独有后缀折叠为 handoff 摘要注入新分支。
    ///
    /// 语义：从当前叶子 `old_leaf` 回溯到与 `new_leaf` 的最近公共祖先，收集这条独有
    /// 后缀交由 [`SummaryProvider`](agent_context::compaction::SummaryProvider) 生成摘要，
    /// 随后切换到 `new_leaf` 并追加摘要为用户消息（续写点落在摘要节点）。
    ///
    /// 返回 `true` 表示切换成功；`false` 表示目标不存在或不支持。
    ///
    /// # Errors
    /// 摘要生成失败时返回 [`ContextError`]。
    async fn switch_branch_with_handoff(&self, _new_leaf: &NodeId) -> Result<bool, ContextError> {
        Ok(false)
    }

    /// 取会话森林全部节点的快照（插入顺序）。
    ///
    /// 默认返回空（线性上下文无树概念）。供 UI 渲染分支树。
    async fn snapshot_nodes(&self) -> Vec<SessionNode> {
        Vec::new()
    }

    /// 全部叶子节点 id（无子节点的节点）；默认返回空。
    async fn list_leaves(&self) -> Vec<NodeId> {
        Vec::new()
    }

    /// 某节点的直接子节点 id 列表；默认返回空。
    async fn children_of(&self, _id: &NodeId) -> Vec<NodeId> {
        Vec::new()
    }
}
