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

    /// 是否达到绝对 token 阈值（H26：`CompactionPolicy` 解析出的阈值）。
    ///
    /// `threshold == 0` 表示「不限制」（窗口未知/为 0），恒 `false`；否则用 `>=`
    /// 与 [`Self::near_limit`] 保持同一比较语义（阈值取整后不应比百分比更晚触发）。
    #[must_use]
    pub const fn reaches(&self, threshold: usize) -> bool {
        threshold > 0 && self.current >= threshold
    }
}

/// 自动压缩触发策略（H26，移植 oh-my-pi `resolveThresholdTokens`）。
///
/// 三级优先级：
/// 1. **绝对阈值** [`threshold_tokens`](Self::threshold_tokens)：设了正数就用它（钳到
///    `[1, window-1]`），适合「窗口很大但要留固定余量」的场景；
/// 2. **预留余量** [`reserve_tokens`](Self::reserve_tokens)：阈值 = `window - reserve`
///    （钳到 `[1, window-1]`）；配置的余量 ≥ 窗口时按窗口 15% 兜底（对齐 omp
///    `resolveBudgetReserveTokens` 的「不可能默认值」回退），避免阈值退化到 0/负；
/// 3. **百分比** [`guard`](Self::guard)：阈值 = `floor(window * guard)`，钳到
///    `[1, window-1]`——Gyre 既有默认（`0.8`），保持向后兼容。
///
/// `window == 0`（未知窗口）→ 阈值 `0` = 不触发压缩。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionPolicy {
    /// 绝对 token 阈值（最高优先级；`None`/0 = 未设置）。
    pub threshold_tokens: Option<usize>,
    /// 预留余量（次优先级；`None` = 未设置）。
    pub reserve_tokens: Option<usize>,
    /// 百分比阈值（兜底；Gyre 既有 `context_window_guard`）。
    pub guard: f32,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            threshold_tokens: None,
            reserve_tokens: None,
            guard: 0.8,
        }
    }
}

impl CompactionPolicy {
    /// 仅百分比（既有行为）。
    #[must_use]
    pub const fn percent(guard: f32) -> Self {
        Self {
            threshold_tokens: None,
            reserve_tokens: None,
            guard,
        }
    }

    /// 解析出给定上下文窗口的**触发阈值**（token；`0` = 不限制）。
    #[must_use]
    pub fn threshold_for(&self, context_window: usize) -> usize {
        if context_window == 0 {
            return 0;
        }
        let upper = context_window.saturating_sub(1).max(1);
        if let Some(t) = self.threshold_tokens.filter(|t| *t > 0) {
            return t.clamp(1, upper);
        }
        if let Some(r) = self.reserve_tokens {
            // 余量 ≥ 窗口：配置不可能成立 → 按窗口 15% 兜底（omp 同款回退）。
            let reserve = if r >= context_window {
                (context_window * 15 / 100).max(1)
            } else {
                r
            };
            return context_window.saturating_sub(reserve).clamp(1, upper);
        }
        // 百分比先取整到 [1, 99]（`f32 0.01` 转 `f64` 会略小于 0.01，直接乘会少 1 token）。
        let percent = f64::from((self.guard.clamp(0.01, 0.99) * 100.0).round());
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let t = (context_window as f64 * percent / 100.0).floor() as usize;
        t.clamp(1, upper)
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
    /// 图像化压缩（P2）：旧对话渲染为 PNG 帧（本地、无 LLM 调用），
    /// 视觉模型读图回放；非视觉模型应使用 [`CompactionStrategy::Summarize`]。
    Snapcompact {
        /// 帧数预算（超出丢中间帧）。
        max_frames: usize,
    },
    /// 裁剪：丢弃最旧的非保护消息（tool-protection 保留工具结果）。
    Prune {
        /// 保留最近 N 条。
        keep_recent: usize,
    },
    /// 抖动：移除冗余/重复内容（shake）。
    Shake,
}

/// 压缩后端选择（装配期从配置解析，注入 `AgentBuilder`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionBackend {
    /// 本地 LLM handoff 摘要（默认，向后兼容）。
    Summarize,
    /// 本地 PNG 帧渲染（snapcompact；要求视觉模型）。
    Snapcompact,
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
    pub const fn root(id: NodeId, message: AgentMessage) -> Self {
        Self {
            id,
            parent_id: None,
            message,
        }
    }

    /// 构造一个带父节点的子节点。
    #[must_use]
    pub const fn child(id: NodeId, parent_id: NodeId, message: AgentMessage) -> Self {
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
///   `StablePrefix` 冻结，产出稳定字节序列以最大化 provider 前缀缓存命中。
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

    /// `StablePrefix` 指纹（缓存命中判断）。
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

#[cfg(test)]
mod compaction_policy_tests {
    use super::{CompactionPolicy, TokenUsage};

    #[test]
    fn absolute_threshold_wins_and_is_clamped_into_window() {
        let p = CompactionPolicy {
            threshold_tokens: Some(1000),
            reserve_tokens: Some(10),
            guard: 0.5,
        };
        assert_eq!(p.threshold_for(100_000), 1000, "绝对阈值优先于余量/百分比");
        // 超过窗口 → 钳到 window-1（永远留 1 token 余量）。
        assert_eq!(p.threshold_for(500), 499);
        // 0 视为未设置 → 落到下一优先级（余量）。
        let zero = CompactionPolicy {
            threshold_tokens: Some(0),
            reserve_tokens: Some(1000),
            guard: 0.5,
        };
        assert_eq!(zero.threshold_for(10_000), 9000);
        // 窗口未知 → 0 = 不限制。
        assert_eq!(p.threshold_for(0), 0);
    }

    #[test]
    fn reserve_threshold_subtracts_and_falls_back_when_impossible() {
        let p = CompactionPolicy {
            threshold_tokens: None,
            reserve_tokens: Some(3000),
            guard: 0.8,
        };
        assert_eq!(p.threshold_for(10_000), 7000, "window - reserve");
        // 余量 ≥ 窗口：按窗口 15% 兜底（不可能配置不应把阈值压到 0/负）。
        let tight = CompactionPolicy {
            threshold_tokens: None,
            reserve_tokens: Some(20_000),
            guard: 0.8,
        };
        assert_eq!(tight.threshold_for(10_000), 8500, "15% 兜底余量");
        // 余量 = 0 → 阈值贴到 window-1。
        let no_reserve = CompactionPolicy {
            threshold_tokens: None,
            reserve_tokens: Some(0),
            guard: 0.8,
        };
        assert_eq!(no_reserve.threshold_for(100), 99);
    }

    #[test]
    fn percent_is_the_default_and_never_disables_compaction() {
        let p = CompactionPolicy::default();
        assert_eq!(p.threshold_for(100_000), 80_000);
        // 极小窗口：floor(0.8*1)=0 需被钳到 1（否则等于关闭压缩）。
        assert_eq!(p.threshold_for(1), 1);
        // 越界百分比被钳到 [1%, 99%]。
        let wild = CompactionPolicy::percent(5.0);
        assert_eq!(wild.threshold_for(1000), 990);
        let tiny = CompactionPolicy::percent(0.0);
        assert_eq!(tiny.threshold_for(1000), 10);
    }

    #[test]
    fn reaches_uses_inclusive_comparison_and_zero_means_unlimited() {
        let usage = TokenUsage {
            current: 100,
            limit: 200,
        };
        assert!(
            usage.reaches(100),
            "达到阈值即触发（>=，与 near_limit 同语义）"
        );
        assert!(!usage.reaches(101));
        assert!(!usage.reaches(0), "0 = 不限制");
        assert!(usage.near_limit(0.5));
        assert!(!usage.near_limit(0.51));
    }
}
