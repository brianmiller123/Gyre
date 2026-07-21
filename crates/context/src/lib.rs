//! # agent-context
//!
//! 生产级上下文记忆：
//! - [`InMemoryContext`]：会话树（节点森林 + `active_leaf`）+ StablePrefix 指纹
//!   （最大化 provider 前缀缓存命中）。支持 fork / 分支切换 / handoff 摘要。
//! - [`TokenCounter`](crate::token::TokenCounter)：tiktoken-rs 精确计数（OpenAI BPE），其它 provider 回退启发式。
//! - [`Compactor`](crate::compaction::Compactor)：summarize（handoff 摘要）/ shake（去冗余）/ prune（窗口裁剪）。
//! - [`PersistentContext`]：JSONL 落盘 + 恢复（断点续跑）；会话树原生持久化 + 旧线性日志无损迁移。
//!
//! 移植 oh-my-pi `append-only-context` 的「只追加 + 稳定前缀」理念与 `compaction/` 子系统，
//! 以及会话树（`SessionEntry` parentId 模型，见 [`tree`] 模块）。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

pub mod compaction;
pub mod persistence;
pub mod token;
pub mod tree;

pub use persistence::delete_message_in_file;
pub use persistence::{SessionInfo, SessionStore};
pub use tree::{
    branch_messages, branch_path_ids, branch_path_nodes, children_of,
    collect_entries_for_branch_summary, common_ancestor, leaves, new_node_id, node_index,
    path_to_root, wrap_linear_as_nodes,
};

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use agent_core::{
    AgentMessage, CompactionStrategy, ContentBlock, ContextError, ContextManager, Model, NodeId,
    ProviderContext, ProviderMessage, SessionNode, TokenUsage, ToolSpec, Usage,
};

pub use persistence::PersistentContext;

/// 内存上下文：会话树（节点森林 + 活跃叶子）+ 稳定前缀 + 精确 token 计数。
///
/// 内部以 `Vec<SessionNode>` 森林存储**全部历史**（含所有分支），`active_leaf` 标记当前
/// 续写点。`append` 在活跃叶子下挂新节点；`set_active_leaf` 移动续写点（之后 append 即 fork）；
/// `build_provider_context` 从活跃叶子向根回溯组装消息序列。压缩（shake/summarize/prune）
/// 作用于活跃路径，分支数据以「重新挂载到新活跃叶子」的方式保全（不丢失）。
pub struct InMemoryContext {
    inner: tokio::sync::Mutex<Inner>,
    counter: token::TokenCounter,
}

struct Inner {
    system: Vec<String>,
    /// 会话森林（插入顺序）：含全部分支节点。
    nodes: Vec<SessionNode>,
    /// `id → nodes 下标` 索引（与 `nodes` 同步重建）。
    index: std::collections::HashMap<NodeId, usize>,
    /// 当前活跃叶子（续写点）。
    active_leaf: Option<NodeId>,
    fingerprint: String,
    /// 注入的摘要提供器（summarize / branch handoff 用）。
    summarizer: Option<Box<dyn compaction::SummaryProvider>>,
    /// Shake 归档配置（保护窗口 / 节省阈值 / 块门槛）。
    shake_config: compaction::ShakeConfig,
    /// Shake 落盘槽（`None` → 用 [`compaction::NullSink`]，占位符仍生成但不可回读）。
    shake_sink: Option<Arc<dyn compaction::ShakeSink>>,
    /// 缓存的模型上下文窗口上限（由 `build_provider_context` 更新）。
    /// 供 `token_usage()` 同步返回正确的 limit，避免 Web/ACP 客户端收到 limit=0。
    model_limit: usize,
    /// 最近一次 `build_provider_context` 的 model id（供 `token_usage()` 同步选对 BPE 编码）。
    last_model_id: String,
    /// StablePrefix 追踪：上次 `build_provider_context` 发送的 ProviderMessage digest 序列。
    ///
    /// 下次 build 时据此计算最长字节稳定前缀（[`stable_prefix_len`](agent_core::ProviderContext::stable_prefix_len)），
    /// 供 provider 精确放置 `cache_control` breakpoint、最大化前缀缓存命中（避免每轮全量
    /// re-prefill）。移植 oh-my-pi `AppendOnlyContextManager.#messageDigests`：压缩 /
    /// 分支切换 / system 变更后清空（整体前缀失效，从 0 重新积累）。
    prefix_digests: Vec<u64>,
}

impl Inner {
    fn rebuild_index(&mut self) {
        self.index = tree::node_index(&self.nodes);
    }

    /// 活跃路径（根→叶）的消息克隆序列。
    fn active_path_messages(&self) -> Vec<AgentMessage> {
        let Some(leaf) = self.active_leaf.as_ref() else {
            return Vec::new();
        };
        tree::branch_path_nodes(&self.nodes, leaf)
            .into_iter()
            .map(|n| n.message.clone())
            .collect()
    }

    /// 用压缩后的线性消息序列**重建活跃路径**，同时**保全分支**：
    ///
    /// - 贪心匹配 `new_messages` 与旧活跃路径节点（按消息相等），命中的节点**保留原 id**
    ///   （从而保留挂在其上的分支）；未命中的消息成为全新 id 节点（如 summarize 的摘要）。
    /// - 新活跃链按 `new_messages` 顺序重新 parent 链接，链首为根（`parent = None`）。
    /// - 旧活跃路径节点中，**被其它分支（off-path）依赖为祖先的**予以保留（原 parent 不动），
    ///   成为「游离前缀子树」，使依赖分支的根→叶路径完整不变；**无依赖的前缀节点**删除
    ///   （常见无分支压缩后不留痕迹，不污染叶子列表）。
    /// - off-path 分支节点原样保留。
    ///
    /// 结果：压缩只影响 LLM 看到的活跃路径，**分支数据零丢失**，挂载点不上移。
    fn compact_active_path(&mut self, old_leaf: &NodeId, new_messages: Vec<AgentMessage>) {
        // 压缩改变活跃路径内容 → 稳定前缀失效（移植 oh-my-pi `syncMessages` 压缩分支 clear）。
        self.prefix_digests.clear();
        // 旧活跃路径（根→叶）：(id, 原 parent, msg)。
        let active: Vec<(NodeId, Option<NodeId>, AgentMessage)> = {
            let mut v: Vec<(NodeId, Option<NodeId>, AgentMessage)> =
                tree::branch_path_nodes(&self.nodes, old_leaf)
                    .into_iter()
                    .map(|n| (n.id.clone(), n.parent_id.clone(), n.message.clone()))
                    .collect();
            // branch_path_nodes 已是根→叶顺序。
            let _ = &mut v;
            v
        };
        let active_id_set: HashSet<NodeId> = active.iter().map(|(id, _, _)| id.clone()).collect();

        // 贪心匹配：为每条 new_messages 找下一条相等的活跃消息，命中则复用其 id。
        let mut mi = 0usize;
        let mut new_chain: Vec<SessionNode> = Vec::with_capacity(new_messages.len());
        let mut preserved_ids: HashSet<NodeId> = HashSet::new();
        for msg in &new_messages {
            let mut reused: Option<(usize, NodeId)> = None;
            for j in mi..active.len() {
                if &active[j].2 == msg {
                    reused = Some((j, active[j].0.clone()));
                    break;
                }
            }
            let id = match reused {
                Some((j, id)) => {
                    mi = j + 1;
                    preserved_ids.insert(id.clone());
                    id
                }
                None => tree::new_node_id(),
            };
            new_chain.push(SessionNode {
                id,
                parent_id: None,
                message: msg.clone(),
            });
        }
        // 新链顺序 parent 链接。
        for k in 1..new_chain.len() {
            let prev_id = new_chain[k - 1].id.clone();
            new_chain[k].parent_id = Some(prev_id);
        }
        let new_leaf = new_chain.last().map(|n| n.id.clone());

        // needed = 复用 id ∪ 「off-path 分支的活跃祖先」。
        let mut needed: HashSet<NodeId> = HashSet::new();
        for id in &preserved_ids {
            needed.insert(id.clone());
        }
        for n in &self.nodes {
            if active_id_set.contains(&n.id) {
                continue;
            }
            // 该 off-path 节点的根→叶链上所有活跃节点均需保留（保路径完整）。
            for cid in tree::path_to_root(&self.nodes, &n.id) {
                if active_id_set.contains(&cid) {
                    needed.insert(cid);
                }
            }
        }

        // 重建森林：新活跃链 + needed 的非复用活跃节点（原 parent 保留）+ off-path 节点（原样）。
        let mut out: Vec<SessionNode> = Vec::with_capacity(self.nodes.len());
        out.extend(new_chain);
        for (id, parent, msg) in &active {
            if preserved_ids.contains(id) {
                continue; // 已在新链
            }
            if needed.contains(id) {
                out.push(SessionNode {
                    id: id.clone(),
                    parent_id: parent.clone(),
                    message: msg.clone(),
                });
            }
            // 其余（无依赖的旧前缀）：丢弃。
        }
        for n in &self.nodes {
            if !active_id_set.contains(&n.id) {
                out.push(n.clone());
            }
        }

        self.nodes = out;
        self.rebuild_index();
        self.active_leaf = new_leaf;
    }
}

impl InMemoryContext {
    /// 构造：初始 system + OpenAI 精确计数器。
    ///
    /// # Panics
    /// tiktoken 词表加载失败时 panic（通常嵌入二进制不会）。
    #[must_use]
    pub fn new(system: Vec<String>) -> Self {
        let counter = token::TokenCounter::openai().unwrap_or_else(|e| {
            tracing::warn!("tiktoken 加载失败，回退启发式计数: {e}");
            token::TokenCounter::heuristic()
        });
        let fingerprint = fingerprint_of(&system, &[]);
        Self {
            inner: tokio::sync::Mutex::new(Inner {
                system,
                nodes: Vec::new(),
                index: std::collections::HashMap::new(),
                active_leaf: None,
                fingerprint,
                summarizer: None,
                shake_config: compaction::ShakeConfig::default(),
                shake_sink: None,
                model_limit: 0,
                last_model_id: String::new(),
                prefix_digests: Vec::new(),
            }),
            counter,
        }
    }

    /// 指定 token 计数器构造（测试用）。
    #[must_use]
    pub fn with_counter(system: Vec<String>, counter: token::TokenCounter) -> Self {
        let fingerprint = fingerprint_of(&system, &[]);
        Self {
            inner: tokio::sync::Mutex::new(Inner {
                system,
                nodes: Vec::new(),
                index: std::collections::HashMap::new(),
                active_leaf: None,
                fingerprint,
                summarizer: None,
                shake_config: compaction::ShakeConfig::default(),
                shake_sink: None,
                model_limit: 0,
                last_model_id: String::new(),
                prefix_digests: Vec::new(),
            }),
            counter,
        }
    }

    /// 注入摘要提供器（启用生产级 summarize / branch handoff）。
    pub async fn set_summarizer(&self, provider: Box<dyn compaction::SummaryProvider>) {
        self.inner.lock().await.summarizer = Some(provider);
    }

    /// 注入 Shake 落盘目录（启用大块归档 + `read_file artifact://` 回读）。
    /// 建议传入 [`compaction::DirSink`]，指向 `<workspace>/.gyre/artifacts`。
    pub async fn set_shake_sink(&self, sink: Arc<dyn compaction::ShakeSink>) {
        self.inner.lock().await.shake_sink = Some(sink);
    }

    /// 覆盖 Shake 配置（保护窗口 / 节省阈值 / 块门槛）。默认见 [`compaction::ShakeConfig::default`]。
    pub async fn set_shake_config(&self, config: compaction::ShakeConfig) {
        self.inner.lock().await.shake_config = config;
    }

    /// 取当前**活跃路径**消息快照（根→叶；向后兼容旧调用方）。
    pub async fn snapshot(&self) -> Vec<AgentMessage> {
        self.inner.lock().await.active_path_messages()
    }

    /// 用线性消息序列恢复（按单链树装载；向后兼容旧调用方）。
    pub async fn restore(&self, log: Vec<AgentMessage>) {
        let mut inner = self.inner.lock().await;
        let nodes = tree::wrap_linear_as_nodes(&log);
        inner.active_leaf = nodes.last().map(|n| n.id.clone());
        inner.nodes = nodes;
        inner.rebuild_index();
    }

    /// 取会话森林全部节点快照（持久化 / UI 分支树用）。
    pub async fn snapshot_nodes(&self) -> Vec<SessionNode> {
        self.inner.lock().await.nodes.clone()
    }

    /// 用节点森林恢复（重置内部状态；活跃叶子取末节点，缺失时为 `None`）。
    pub async fn restore_nodes(&self, nodes: Vec<SessionNode>) {
        let mut inner = self.inner.lock().await;
        let leaf = inner.active_leaf.clone();
        inner.nodes = nodes;
        inner.rebuild_index();
        // 活跃叶子缺失（如首次加载）→ 默认末节点。
        if leaf.as_ref().map_or(true, |l| !inner.index.contains_key(l)) {
            inner.active_leaf = inner.nodes.last().map(|n| n.id.clone());
        }
    }

    /// 追加一条消息并返回新建节点（父 = 当前活跃叶子；追加后活跃叶子前移到新节点）。
    pub async fn append_node(&self, message: AgentMessage) -> SessionNode {
        let mut inner = self.inner.lock().await;
        let parent = inner.active_leaf.clone();
        let id = tree::new_node_id();
        let node = SessionNode {
            id: id.clone(),
            parent_id: parent,
            message,
        };
        let pos = inner.nodes.len();
        inner.nodes.push(node.clone());
        inner.index.insert(id.clone(), pos);
        inner.active_leaf = Some(id);
        node
    }

    /// 当前活跃叶子 id。
    pub async fn active_leaf(&self) -> Option<NodeId> {
        self.inner.lock().await.active_leaf.clone()
    }

    /// 切换活跃叶子（分支切换，纯移动续写点）。
    /// 返回 `true` 表示目标存在且已切换。
    pub async fn set_active_leaf(&self, id: &str) -> bool {
        let mut inner = self.inner.lock().await;
        if inner.index.contains_key(id) {
            inner.active_leaf = Some(id.to_string());
            // 分支切换改变活跃路径 → 稳定前缀失效。
            inner.prefix_digests.clear();
            true
        } else {
            false
        }
    }

    /// 切换到 `new_leaf` 并把「被离开分支」的独有后缀折叠为 handoff 摘要注入新分支
    /// （移植 oh-my-pi `collectEntriesForBranchSummary`）。
    ///
    /// 流程：收集 old_leaf→公共祖先 的独有节点 → 经 [`compaction::SummaryProvider`]
    /// 生成摘要 → 切到 new_leaf 并追加摘要为用户消息（续写点落在摘要节点）。无摘要器
    /// 或无独有后缀时退化为纯切换（不报错）。
    ///
    /// # Errors
    /// 摘要生成失败返回 [`ContextError::Compaction`]。
    pub async fn switch_branch_with_handoff(&self, new_leaf: &str) -> Result<bool, ContextError> {
        // 阶段一（锁内）：校验 + 收集独有后缀 + 取出 summarizer。
        let (entries, summarizer) = {
            let mut inner = self.inner.lock().await;
            if !inner.index.contains_key(new_leaf) {
                return Ok(false);
            }
            let old = inner.active_leaf.clone();
            let (entries, _anc) =
                tree::collect_entries_for_branch_summary(&inner.nodes, old.as_deref(), new_leaf);
            let summarizer = std::mem::take(&mut inner.summarizer);
            (entries, summarizer)
        };
        // 阶段二（锁外）：生成摘要（LLM 调用，可能耗时）。
        let summary = if entries.is_empty() || summarizer.is_none() {
            None
        } else {
            let summarizer = summarizer.unwrap();
            let lines: Vec<String> = entries
                .iter()
                .map(|n| compaction::message_to_summary_line(&n.message))
                .collect();
            let result = summarizer.summarize(&lines).await;
            // 阶段三前先把 summarizer 放回（无论成功失败）。
            let mut inner = self.inner.lock().await;
            inner.summarizer = Some(summarizer);
            drop(inner);
            Some(result.map_err(ContextError::Compaction)?)
        };
        // 阶段三（锁内）：切换 + 追加 handoff 消息。
        let mut inner = self.inner.lock().await;
        if !inner.index.contains_key(new_leaf) {
            return Ok(false);
        }
        // 分支切换改变活跃路径 → 稳定前缀失效。
        inner.prefix_digests.clear();
        if let Some(summary) = summary {
            let id = tree::new_node_id();
            let pos = inner.nodes.len();
            inner.nodes.push(SessionNode {
                id: id.clone(),
                parent_id: Some(new_leaf.to_string()),
                message: AgentMessage::user_text(format!(
                    "[分支切换 handoff] 此前另一条分支的进展摘要：\n\n{summary}"
                )),
            });
            inner.index.insert(id.clone(), pos);
            inner.active_leaf = Some(id);
        } else {
            inner.active_leaf = Some(new_leaf.to_string());
        }
        Ok(true)
    }

    /// 全部叶子节点 id。
    pub async fn list_leaves(&self) -> Vec<NodeId> {
        let inner = self.inner.lock().await;
        tree::leaves(&inner.nodes)
    }

    /// 某节点的直接子节点 id。
    pub async fn children_of(&self, id: &str) -> Vec<NodeId> {
        let inner = self.inner.lock().await;
        tree::children_of(&inner.nodes, id)
    }

    /// 删除节点 vec 第 `index` 个节点，并连带清理其孤立工具结果后代；其余子树
    /// （分支）重新挂到被删节点的父节点上。返回实际移除的节点数。
    pub async fn delete_at(&self, index: usize) -> usize {
        let mut inner = self.inner.lock().await;
        let original = inner.nodes.len();
        let Some(new_nodes) = tree::remove_node_with_orphans(&inner.nodes, index) else {
            return 0;
        };
        let removed = original - new_nodes.len();
        inner.nodes = new_nodes;
        inner.rebuild_index();
        // 活跃叶子被删时回退到末节点。
        if let Some(leaf) = inner.active_leaf.clone() {
            if !inner.index.contains_key(&leaf) {
                inner.active_leaf = inner.nodes.last().map(|n| n.id.clone());
            }
        }
        removed
    }
}

#[async_trait::async_trait]
impl ContextManager for InMemoryContext {
    async fn append(&self, message: AgentMessage) {
        // append_node 已更新索引与活跃叶子。
        let _ = self.append_node(message).await;
    }

    async fn set_system(&self, system: Vec<String>, tools: &[ToolSpec]) {
        let mut inner = self.inner.lock().await;
        inner.system = system;
        inner.fingerprint = fingerprint_of(&inner.system, tools);
        // system 变更使整体前缀（system + messages）失效 → 稳定前缀归零重新积累。
        inner.prefix_digests.clear();
    }

    async fn build_provider_context(
        &self,
        model: &Model,
        tools: &[ToolSpec],
    ) -> Result<ProviderContext, ContextError> {
        let mut inner = self.inner.lock().await;
        let messages = convert_to_llm(&inner.active_path_messages());
        inner.fingerprint = fingerprint_of(&inner.system, tools);
        // 缓存模型窗口上限 + model id，供 token_usage() 同步返回正确值与 BPE 编码。
        inner.model_limit = model.max_input_tokens;
        inner.last_model_id = model.id.clone();
        // P2-F：按 model 族选 BPE（gpt-4o/o 系列 o200k_base，其余 cl100k_base），
        // 精确计数直接影响压缩触发时机（near_limit）与 shake 保护窗口。
        let current = self
            .counter
            .count_context_for(&inner.system, &messages, &model.id);

        // P0-A：计算与上次发送序列的最长字节稳定前缀。digest 命中 ⇒ ProviderMessage
        // 逻辑内容相同 ⇒ provider 端序列化字节相同 ⇒ 前缀缓存可命中到此索引。移植 oh-my-pi
        // `AppendOnlyContextManager.longestStablePrefix`：provider 据此精确放置 cache_control
        // breakpoint，避免每轮全量 re-prefill。
        let new_digests: Vec<u64> = messages.iter().map(digest_message).collect();
        let stable_prefix_len = longest_stable_prefix(&new_digests, &inner.prefix_digests);
        inner.prefix_digests = new_digests;

        Ok(ProviderContext {
            fingerprint: inner.fingerprint.clone(),
            system: inner.system.clone(),
            messages,
            tokens: TokenUsage {
                current,
                limit: model.max_input_tokens,
            },
            stable_prefix_len,
        })
    }

    async fn compact(&self, strategy: CompactionStrategy) -> Result<(), ContextError> {
        // 各分支自行管理加锁：Prune/Shake 为纯计算 + 微量文件写入（持锁可接受）；
        // Summarize 涉及 LLM 网络调用，须释放锁再 await，避免持锁数十秒阻塞其他操作。
        match strategy {
            CompactionStrategy::Prune { keep_recent } => {
                let mut inner = self.inner.lock().await;
                let Some(old_leaf) = inner.active_leaf.clone() else {
                    return Ok(());
                };
                let path = inner.active_path_messages();
                let new_log = compaction::Compactor::prune(&path, keep_recent);
                tracing::info!(
                    before = path.len(),
                    after = new_log.len(),
                    "已裁剪上下文（活跃分支）"
                );
                inner.compact_active_path(&old_leaf, new_log);
            }
            CompactionStrategy::Shake => {
                let mut inner = self.inner.lock().await;
                let Some(old_leaf) = inner.active_leaf.clone() else {
                    return Ok(());
                };
                let path = inner.active_path_messages();
                let config = inner.shake_config.clone();
                let sink: Arc<dyn compaction::ShakeSink> = inner
                    .shake_sink
                    .clone()
                    .unwrap_or_else(|| Arc::new(compaction::NullSink));
                match compaction::Compactor::shake_with(
                    &path,
                    &config,
                    &self.counter,
                    sink.as_ref(),
                ) {
                    Ok((new_log, stats)) => {
                        if stats.saved > 0 {
                            tracing::info!(
                                saved = stats.saved,
                                tool_results = stats.tool_results_elided,
                                blocks = stats.blocks_elided,
                                "已 shake 归档压缩（活跃分支）"
                            );
                            inner.compact_active_path(&old_leaf, new_log);
                        } else {
                            tracing::info!(kept = path.len(), "shake 无可归档内容（仅机械去冗余）");
                            // 机械去冗余（去重复 Status / 删空助手）仍需落盘：以 shake_mechanical 结果重建。
                            let mech = compaction::Compactor::shake(&path);
                            if mech.len() != path.len() {
                                inner.compact_active_path(&old_leaf, mech);
                            }
                        }
                    }
                    Err(e) => tracing::warn!("shake 落盘失败，保留原日志: {e}"),
                }
            }
            CompactionStrategy::Summarize { .. } => {
                // 阶段一（锁内）：取活跃路径 + summarizer + old_leaf。
                let mut inner = self.inner.lock().await;
                let Some(old_leaf) = inner.active_leaf.clone() else {
                    return Ok(());
                };
                let path = inner.active_path_messages();
                let keep = path.len().min(6);
                let Some(summarizer) = std::mem::take(&mut inner.summarizer) else {
                    tracing::warn!("summarize 需注入 SummaryProvider，跳过");
                    return Ok(());
                };
                let original = path.clone();
                // 阶段二（锁外）：释放锁后执行 LLM summarize。
                drop(inner);
                let outcome =
                    compaction::Compactor::summarize(path, keep, summarizer.as_ref()).await;
                let (new_log, result) = match outcome {
                    Ok(new_log) => (new_log, Ok::<(), ContextError>(())),
                    Err(e) => (original, Err(ContextError::Compaction(e))),
                };
                // 阶段三（锁内）：写回。压缩窗口期间并发 append 的后代节点会被
                // compact_active_path 的「off-path 保留」逻辑纳入新森林（保枝不丢）。
                let kept = {
                    let mut inner = self.inner.lock().await;
                    inner.summarizer = Some(summarizer);
                    inner.compact_active_path(&old_leaf, new_log);
                    inner.active_path_messages().len()
                };
                if result.is_ok() {
                    tracing::info!(kept, "已 summarize 压缩（活跃分支）");
                }
                return result;
            }
        }
        Ok(())
    }

    async fn delete_message_at(&self, index: usize) -> Result<usize, ContextError> {
        Ok(self.delete_at(index).await)
    }

    fn token_usage(&self) -> TokenUsage {
        match self.inner.try_lock() {
            Ok(inner) => TokenUsage {
                current: self.counter.count_context_for(
                    &inner.system,
                    &convert_to_llm(&inner.active_path_messages()),
                    &inner.last_model_id,
                ),
                limit: inner.model_limit,
            },
            Err(_) => TokenUsage::default(),
        }
    }

    fn accumulated_usage(&self) -> Usage {
        // 活跃分支（根→叶）累计用量：汇总各 assistant 消息的 usage。供 UI 在重连 /
        // 切换会话时恢复用量显示——前端累计态清零后以此基线重建，后续增量帧叠加其上。
        match self.inner.try_lock() {
            Ok(inner) => {
                let mut total = Usage::default();
                for msg in inner.active_path_messages() {
                    if let AgentMessage::Assistant(a) = msg {
                        total.add(&a.usage);
                    }
                }
                total
            }
            Err(_) => Usage::default(),
        }
    }

    fn prefix_fingerprint(&self) -> String {
        match self.inner.try_lock() {
            Ok(inner) => inner.fingerprint.clone(),
            Err(_) => "<locked>".into(),
        }
    }

    // ── 会话树 / 分支导航 trait 方法（覆写默认实现）──
    async fn active_leaf(&self) -> Option<NodeId> {
        InMemoryContext::active_leaf(self).await
    }

    async fn set_active_leaf(&self, id: &NodeId) -> bool {
        InMemoryContext::set_active_leaf(self, id).await
    }

    async fn switch_branch_with_handoff(&self, new_leaf: &NodeId) -> Result<bool, ContextError> {
        InMemoryContext::switch_branch_with_handoff(self, new_leaf).await
    }

    async fn snapshot_nodes(&self) -> Vec<SessionNode> {
        InMemoryContext::snapshot_nodes(self).await
    }

    async fn list_leaves(&self) -> Vec<NodeId> {
        InMemoryContext::list_leaves(self).await
    }

    async fn children_of(&self, id: &NodeId) -> Vec<NodeId> {
        InMemoryContext::children_of(self, id).await
    }
}

/// convertToLlm 边界：过滤 UI 消息，仅保留 User/Assistant/ToolResult。
///
/// 经 [`sanitize_provider_messages`] 处理后保证 OpenAI 角色顺序不变量：
/// 每条 `tool` 消息前必有声明其 `tool_call_id` 的 assistant 消息。
fn convert_to_llm(log: &[AgentMessage]) -> Vec<ProviderMessage> {
    let raw: Vec<ProviderMessage> = log
        .iter()
        .filter_map(|msg| match msg {
            AgentMessage::User(u) => Some(ProviderMessage::User {
                content: u.content.clone(),
            }),
            AgentMessage::Assistant(a) => {
                // P2-P：API 级 refusal 不重放（移植 replay-policy.ts filterProviderReplayMessages）。
                // refusal assistant 消息是终态错误（content_filter / sensitive），重放会把 refusal
                // 文本反复喂回模型；过滤后任何紧跟的孤立 tool 消息由 sanitize_provider_messages 清理。
                if a.is_provider_refusal() {
                    None
                } else {
                    Some(ProviderMessage::Assistant {
                        content: a.content.clone(),
                    })
                }
            }
            AgentMessage::ToolResult(t) => {
                // 多模态工具结果：把 ToolResult::Image 编码为 base64 ToolImage，
                // 让支持多模态的 provider（Anthropic）真正"看到"图像。
                let images = match &t.result {
                    agent_core::ToolResult::Image { mime, data } => {
                        use base64::Engine as _;
                        vec![agent_core::ToolImage {
                            mime: mime.clone(),
                            data: base64::engine::general_purpose::STANDARD.encode(data),
                        }]
                    }
                    _ => Vec::new(),
                };
                // 兜底空 content：部分兼容 provider（GLM/Z.ai 等）拒绝 content 为空的 tool
                // 消息，直接返回 400。静默命令（mkdir/touch）、读空文件、无匹配搜索等会产出
                // 空文本，触发「工具调用后偶发 400」。此处统一填充占位，保证请求体始终合法。
                let content = t.result.to_llm_text();
                let content = if content.is_empty() {
                    "(无输出)".to_string()
                } else {
                    content
                };
                Some(ProviderMessage::Tool {
                    tool_call_id: t.tool_call_id.clone(),
                    content,
                    is_error: matches!(t.result, agent_core::ToolResult::Error { .. }),
                    images,
                })
            }
            AgentMessage::Status(_) | AgentMessage::Ask(_) | AgentMessage::SoftRequirement(_) => {
                None
            }
        })
        .collect();
    sanitize_provider_messages(raw)
}

/// 净化 provider 消息流，强制满足 OpenAI tool 调用配对约束（防御压缩/恢复后残留的孤立配对）：
///
/// - **孤立 tool 结果**：某 `Tool` 消息的 `tool_call_id` 未被任何**保留下来**的 assistant
///   消息声明（其发起消息被 prune/summarize 裁掉）→ 丢弃该 tool 消息。
///   （修复 `Messages with role 'tool' must be a response to a preceding message
///   with 'tool_calls'` 的 400 错误。）
/// - **悬空 tool 调用**：某 assistant `ToolCall` 块在最终序列中找不到匹配的 `Tool` 结果
///   （如 `supersede_read_results` 取代旧结果后残留的调用）→ 剥离该 ToolCall 块；
///   若剥离后 assistant 消息变空则整体丢弃。
fn sanitize_provider_messages(msgs: Vec<ProviderMessage>) -> Vec<ProviderMessage> {
    // 仍在序列中存在（待保留）Tool 结果的 tool_call_id 全集：用于判断 ToolCall 是否悬空。
    let mut ids_with_result: HashSet<String> = HashSet::new();
    for m in &msgs {
        if let ProviderMessage::Tool { tool_call_id, .. } = m {
            ids_with_result.insert(tool_call_id.clone());
        }
    }

    let mut out: Vec<ProviderMessage> = Vec::with_capacity(msgs.len());
    // 已被「保留的 assistant 消息」声明的 tool_call_id：用于放行 tool 消息。
    let mut declared: HashSet<String> = HashSet::new();
    for m in msgs {
        match m {
            ProviderMessage::Assistant { content } => {
                let filtered: Vec<ContentBlock> = content
                    .into_iter()
                    .filter(|b| match b {
                        ContentBlock::ToolCall { id, .. } => ids_with_result.contains(id),
                        _ => true,
                    })
                    .collect();
                for b in &filtered {
                    if let ContentBlock::ToolCall { id, .. } = b {
                        declared.insert(id.clone());
                    }
                }
                if filtered.is_empty() {
                    // 纯 tool-call 助手消息且其调用全部悬空：丢弃，避免空助手消息。
                    continue;
                }
                out.push(ProviderMessage::Assistant { content: filtered });
            }
            other => {
                // 仅当存在前置 assistant 声明时才保留 Tool 消息，否则为孤立 tool，丢弃。
                if let ProviderMessage::Tool { tool_call_id, .. } = &other {
                    if !declared.contains(tool_call_id) {
                        continue;
                    }
                }
                out.push(other);
            }
        }
    }
    out
}

/// StablePrefix 指纹：system + tool spec 的字节哈希。
fn fingerprint_of(system: &[String], tools: &[ToolSpec]) -> String {
    let mut hasher = DefaultHasher::new();
    for s in system {
        s.hash(&mut hasher);
    }
    for t in tools {
        t.name.hash(&mut hasher);
        t.description.hash(&mut hasher);
    }
    format!("fp:{:016x}", hasher.finish())
}

/// 单条 ProviderMessage 的确定性 digest（移植 oh-my-pi `#messageDigest`）。
///
/// 基于 [`Debug`] 格式化（Rust enum/struct 的 Debug 输出按定义顺序确定，对相同逻辑内容
/// 产生相同字节），覆盖 role / content / tool_call_id / is_error / images 全字段。
/// 用于 [`longest_stable_prefix`] 判断前缀字节是否稳定。
#[must_use]
fn digest_message(msg: &ProviderMessage) -> u64 {
    let mut hasher = DefaultHasher::new();
    let buf = format!("{msg:?}");
    buf.hash(&mut hasher);
    hasher.finish()
}

/// 新旧 digest 序列的最长公共前缀长度（移植 oh-my-pi `#longestStablePrefix`）。
///
/// 返回首个 digest 不一致的索引；全部一致时返回较短序列长度。用于计算
/// [`ProviderContext::stable_prefix_len`]：provider 前缀缓存可命中到此。
#[must_use]
fn longest_stable_prefix(new_digests: &[u64], old_digests: &[u64]) -> usize {
    let bound = new_digests.len().min(old_digests.len());
    for i in 0..bound {
        if new_digests[i] != old_digests[i] {
            return i;
        }
    }
    bound
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AssistantMessage, Usage};

    #[tokio::test]
    async fn append_and_build_filters_status() {
        let ctx = InMemoryContext::new(vec!["sys".into()]);
        ctx.append(AgentMessage::user_text("hello")).await;
        ctx.append(AgentMessage::Status(agent_core::StatusMessage {
            text: "thinking...".into(),
            kind: agent_core::StatusKind::Info,
        }))
        .await;
        ctx.append(AgentMessage::Assistant(AssistantMessage {
            content: vec![agent_core::ContentBlock::Text {
                text: "hi back".into(),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
            stop_details: None,
        }))
        .await;

        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        let built = ctx.build_provider_context(&model, &[]).await.unwrap();
        assert_eq!(built.messages.len(), 2);
        assert!(!built.fingerprint.is_empty());
        assert!(built.tokens.current > 0);
    }

    /// P2-P：API 级 refusal assistant 消息不重放（移植 replay-policy.ts filterProviderReplayMessages）。
    /// refusal（Error + sensitive 详情）从 provider 上下文过滤；普通 Error（无 refusal 详情）保留。
    #[tokio::test]
    async fn build_filters_provider_refusal_assistant() {
        let ctx = InMemoryContext::new(vec!["sys".into()]);
        ctx.append(AgentMessage::user_text("q1")).await;
        // refusal 消息：Error + sensitive → 应被过滤。
        ctx.append(AgentMessage::Assistant(AssistantMessage {
            content: vec![agent_core::ContentBlock::Text {
                text: "I can't help with that".into(),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: Some(agent_core::StopReason::Error),
            stop_details: Some(agent_core::StopDetails::new("sensitive")),
        }))
        .await;
        ctx.append(AgentMessage::user_text("q2")).await;
        // 普通 Error（无 refusal 详情）→ 应保留。
        ctx.append(AgentMessage::Assistant(AssistantMessage {
            content: vec![agent_core::ContentBlock::Text {
                text: "partial".into(),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: Some(agent_core::StopReason::Error),
            stop_details: None,
        }))
        .await;

        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        let built = ctx.build_provider_context(&model, &[]).await.unwrap();
        // 期望：user(q1) 被过滤的 refusal user(q2) 普通Error assistant = 3 条
        // （refusal assistant 不出现在 provider 消息中）。
        // 逐条提取首文本块：user 取 UserContent::Text，assistant 取 ContentBlock::Text。
        let texts: Vec<String> = built
            .messages
            .iter()
            .filter_map(|m| match m {
                agent_core::ProviderMessage::Assistant { content } => {
                    content.iter().find_map(|b| b.as_text().map(String::from))
                }
                agent_core::ProviderMessage::User { content } => content.iter().find_map(|c| {
                    if let agent_core::UserContent::Text { text } = c {
                        Some(text.clone())
                    } else {
                        None
                    }
                }),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["q1".to_string(), "q2".to_string(), "partial".to_string()]
        );
        assert!(
            !texts.iter().any(|t| t.contains("can't help")),
            "refusal 文本不应重放"
        );
    }

    /// P0-A：连续 append 时，第二次 build 的稳定前缀应覆盖上次全部消息
    ///（字节未变 → provider 前缀缓存可命中到此）。移植 oh-my-pi `longestStablePrefix`。
    #[tokio::test]
    async fn build_tracks_stable_prefix_on_append() {
        let ctx = InMemoryContext::new(vec!["sys".into()]);
        ctx.append(AgentMessage::user_text("first")).await;
        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);

        let b1 = ctx.build_provider_context(&model, &[]).await.unwrap();
        // 首次构建：无前次记录，稳定前缀为 0。
        assert_eq!(b1.stable_prefix_len, 0);
        let first_len = b1.messages.len();
        assert_eq!(first_len, 1);

        ctx.append(AgentMessage::user_text("second")).await;
        let b2 = ctx.build_provider_context(&model, &[]).await.unwrap();
        // 追加后：前 first_len 条与上次字节相同 → 全命中。
        assert_eq!(
            b2.stable_prefix_len, first_len,
            "追加后稳定前缀应等于上次消息数"
        );
        assert_eq!(b2.messages.len(), first_len + 1);
    }

    /// P0-A：删除中间消息后，首条 digest 命中、分歧点之后重发（stable_prefix_len 反映分歧点）。
    #[tokio::test]
    async fn stable_prefix_partial_hit_when_middle_deleted() {
        let ctx = InMemoryContext::new(vec!["sys".into()]);
        ctx.append(AgentMessage::user_text("a")).await;
        ctx.append(AgentMessage::user_text("b")).await;
        ctx.append(AgentMessage::user_text("c")).await;
        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        let b1 = ctx.build_provider_context(&model, &[]).await.unwrap();
        assert_eq!(b1.messages.len(), 3);

        // 删除中间消息（索引 1 = "b"）。
        let removed = ctx.delete_message_at(1).await.unwrap();
        assert_eq!(removed, 1);
        let b2 = ctx.build_provider_context(&model, &[]).await.unwrap();
        // "a" digest 命中；"b" 消失 → 从索引 1 起分歧。
        assert_eq!(b2.messages.len(), 2, "删一条后应剩 2 条");
        assert_eq!(b2.stable_prefix_len, 1, "中间删除：首条命中，其后重发");
    }

    /// P0-A：system 变更后整体前缀缓存失效，stable_prefix_len 应归 0（即使 messages 未变）。
    #[tokio::test]
    async fn set_system_invalidates_stable_prefix() {
        let ctx = InMemoryContext::new(vec!["sys".into()]);
        ctx.append(AgentMessage::user_text("a")).await;
        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        let _ = ctx.build_provider_context(&model, &[]).await.unwrap();

        ctx.append(AgentMessage::user_text("b")).await;
        let b2 = ctx.build_provider_context(&model, &[]).await.unwrap();
        assert_eq!(b2.stable_prefix_len, 1, "追加后首条命中");

        // system 变更：整体前缀缓存失效，稳定前缀应归 0。
        ctx.set_system(vec!["new-sys".into()], &[]).await;
        let b3 = ctx.build_provider_context(&model, &[]).await.unwrap();
        assert_eq!(b3.stable_prefix_len, 0, "system 变更后应 invalidate");
        assert_eq!(b3.messages.len(), 2, "messages 不应因 set_system 改变");
    }

    /// P0-A：压缩后整体前缀失效，stable_prefix_len 应归 0（全量重放）。
    /// 移植 oh-my-pi `syncMessages` 的「数组变短 → clear」分支。
    #[tokio::test]
    async fn compact_clears_stable_prefix() {
        let ctx = InMemoryContext::new(vec!["sys".into()]);
        ctx.append(AgentMessage::user_text("a")).await;
        ctx.append(AgentMessage::user_text("b")).await;
        ctx.append(AgentMessage::user_text("c")).await;
        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        let b1 = ctx.build_provider_context(&model, &[]).await.unwrap();
        assert_eq!(b1.messages.len(), 3);
        assert_eq!(b1.stable_prefix_len, 0);

        // 第二次 build（无变化）→ 全命中，建立稳定前缀。
        let b2 = ctx.build_provider_context(&model, &[]).await.unwrap();
        assert_eq!(b2.stable_prefix_len, 3);

        // 压缩（prune keep_recent=1）：compact_active_path 清空 prefix_digests。
        ctx.compact(agent_core::CompactionStrategy::Prune { keep_recent: 1 })
            .await
            .unwrap();
        let b3 = ctx.build_provider_context(&model, &[]).await.unwrap();
        assert_eq!(b3.stable_prefix_len, 0, "压缩后应全量重放，稳定前缀归零");
    }

    /// 回归：空 content 的 ToolResult（静默命令 mkdir/touch、读空文件、无匹配搜索等）
    /// 序列化为 provider tool 消息后必须非空，否则 GLM/Z.ai 等兼容 provider 拒绝空 tool
    /// content 并返回 400——这正是「工具调用后偶发 400」的根因。
    #[tokio::test]
    async fn empty_tool_result_content_gets_placeholder() {
        let ctx = InMemoryContext::new(vec!["sys".into()]);
        ctx.append(AgentMessage::Assistant(AssistantMessage {
            content: vec![agent_core::ContentBlock::ToolCall {
                id: "call_1".into(),
                name: "run_command".into(),
                arguments: serde_json::json!({"command":"mkdir x"}),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: Some(agent_core::StopReason::ToolUse),
            stop_details: None,
        }))
        .await;
        ctx.append(AgentMessage::ToolResult(agent_core::ToolResultMessage {
            tool_call_id: "call_1".into(),
            result: agent_core::ToolResult::text(""),
        }))
        .await;

        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        let built = ctx.build_provider_context(&model, &[]).await.unwrap();

        let tool_contents: Vec<&str> = built
            .messages
            .iter()
            .filter_map(|m| match m {
                agent_core::ProviderMessage::Tool { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            tool_contents.iter().all(|c| !c.is_empty()),
            "tool 消息 content 不应为空（兼容 provider 会返回 400），实际: {tool_contents:?}"
        );
    }

    #[tokio::test]
    async fn token_usage_returns_cached_limit_after_build() {
        // 回归：build_provider_context 后 token_usage() 应返回正确的 limit（非 0）。
        let ctx = InMemoryContext::with_counter(vec![], token::TokenCounter::heuristic());
        ctx.append(AgentMessage::user_text("test")).await;
        // build 前 limit 为 0（尚未知道模型窗口）
        assert_eq!(ctx.token_usage().limit, 0);
        let model = Model {
            id: "m".into(),
            provider: "openai".into(),
            api: agent_core::Api::OpenAiCompletions,
            max_input_tokens: 128_000,
            max_output_tokens: 4096,
            supports_tools: true,
            supports_streaming: true,
            supports_thinking: false,
            extra_body: None,
        };
        ctx.build_provider_context(&model, &[]).await.unwrap();
        // build 后 limit 应反映模型窗口
        let usage = ctx.token_usage();
        assert_eq!(usage.limit, 128_000, "limit 应为缓存的模型窗口大小");
        assert!(usage.current > 0, "current 应非零");
    }

    #[tokio::test]
    async fn accumulated_usage_sums_active_path_assistants() {
        // 回归：切换会话 / 重连时服务端据 accumulated_usage() 回放用量基线，
        // 须汇总活跃分支全部 assistant 消息的 usage（user/tool 消息不计）。
        let ctx = InMemoryContext::new(vec!["sys".into()]);
        ctx.append(AgentMessage::user_text("hi")).await;
        ctx.append(AgentMessage::Assistant(AssistantMessage {
            content: vec![agent_core::ContentBlock::Text {
                text: "hello".into(),
            }],
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 10,
                cache_write_tokens: 5,
                cost_usd: 0.001,
            },
            model: "m".into(),
            stop_reason: Some(agent_core::StopReason::Stop),
            stop_details: None,
        }))
        .await;
        ctx.append(AgentMessage::user_text("again")).await;
        ctx.append(AgentMessage::Assistant(AssistantMessage {
            content: vec![agent_core::ContentBlock::Text {
                text: "world".into(),
            }],
            usage: Usage {
                input_tokens: 200,
                output_tokens: 30,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.002,
            },
            model: "m".into(),
            stop_reason: Some(agent_core::StopReason::Stop),
            stop_details: None,
        }))
        .await;
        let acc = ctx.accumulated_usage();
        assert_eq!(acc.input_tokens, 300, "input 应为两次 assistant 之和");
        assert_eq!(acc.output_tokens, 80, "output 应为两次 assistant 之和");
        assert_eq!(acc.cache_read_tokens, 10);
        assert_eq!(acc.cache_write_tokens, 5);
        assert!(
            (acc.cost_usd - 0.003).abs() < 1e-9,
            "cost 应为两次 assistant 之和"
        );
    }

    #[tokio::test]
    async fn prune_and_shake_compact() {
        let ctx = InMemoryContext::with_counter(vec![], token::TokenCounter::heuristic());
        for i in 0..5 {
            ctx.append(AgentMessage::user_text(format!("msg{i}"))).await;
        }
        ctx.compact(CompactionStrategy::Prune { keep_recent: 2 })
            .await
            .unwrap();
        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        let built = ctx.build_provider_context(&model, &[]).await.unwrap();
        assert_eq!(built.messages.len(), 2);
    }

    #[tokio::test]
    async fn snapshot_and_restore_roundtrip() {
        let ctx = InMemoryContext::with_counter(vec![], token::TokenCounter::heuristic());
        ctx.append(AgentMessage::user_text("persist me")).await;
        let snap = ctx.snapshot().await;
        let ctx2 = InMemoryContext::with_counter(vec![], token::TokenCounter::heuristic());
        ctx2.restore(snap).await;
        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        let built = ctx2.build_provider_context(&model, &[]).await.unwrap();
        assert_eq!(built.messages.len(), 1);
    }

    /// 回归：压缩（prune/summarize）后绝不能向 provider 发出孤立 tool 消息。
    /// 复现 web 端 HTTP 400「Messages with role 'tool' must be a response to a
    /// preceding message with 'tool_calls'」。
    #[tokio::test]
    async fn compact_never_leaves_orphan_tool_message() {
        use serde_json::json;
        // assistant(tool_call c1) → tool_result(c1) → user(recent)
        let call = AgentMessage::Assistant(AssistantMessage {
            content: vec![agent_core::ContentBlock::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: json!({ "path": "x.txt" }),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
            stop_details: None,
        });
        let res = AgentMessage::ToolResult(agent_core::ToolResultMessage {
            tool_call_id: "c1".into(),
            result: agent_core::ToolResult::text("file body"),
        });
        let recent = AgentMessage::user_text("recent turn");
        let ctx = InMemoryContext::with_counter(vec![], token::TokenCounter::heuristic());
        ctx.append(call).await;
        ctx.append(res).await;
        ctx.append(recent).await;
        // keep_recent=1：原本会把 c1 配对裁散，sanitize 必须保证无孤立 tool 消息。
        ctx.compact(CompactionStrategy::Prune { keep_recent: 1 })
            .await
            .unwrap();
        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        let built = ctx.build_provider_context(&model, &[]).await.unwrap();
        // 不变量：每条 Tool 消息前必有声明其 tool_call_id 的 Assistant 消息。
        let mut declared: std::collections::HashSet<String> = std::collections::HashSet::new();
        for m in &built.messages {
            match m {
                ProviderMessage::Assistant { content } => {
                    for b in content {
                        if let agent_core::ContentBlock::ToolCall { id, .. } = b {
                            declared.insert(id.clone());
                        }
                    }
                }
                ProviderMessage::Tool { tool_call_id, .. } => {
                    assert!(
                        declared.contains(tool_call_id),
                        "孤立 tool 消息：{tool_call_id} 无前置 tool_calls"
                    );
                }
                _ => {}
            }
        }
    }

    /// 回归：summarize 压缩期间必须释放锁，使 token_usage()（try_lock）仍能返回
    /// 缓存的非零值。修复前持锁 await LLM（数十秒），token_usage() 因 try_lock 失败
    /// 返回全零 default，且阻塞所有 append/build。
    #[tokio::test]
    async fn summarize_releases_lock_during_llm_call() {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::Arc;
        use tokio::sync::Notify;

        /// 模拟慢 LLM：进入 summarize 后通知测试线程并阻塞等待放行信号。
        struct BlockingSummary {
            started: Arc<Notify>,
            proceed: Arc<Notify>,
        }
        impl compaction::SummaryProvider for BlockingSummary {
            fn summarize(
                &self,
                _old: &[String],
            ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
                let started = self.started.clone();
                let proceed = self.proceed.clone();
                Box::pin(async move {
                    started.notify_one();
                    proceed.notified().await;
                    Ok("handoff summary".into())
                })
            }
        }

        let ctx = Arc::new(InMemoryContext::with_counter(
            vec![],
            token::TokenCounter::heuristic(),
        ));
        // 足够多的消息以触发实际 summarize（log.len() > keep）。
        for i in 0..8 {
            ctx.append(AgentMessage::user_text(format!("msg{i}"))).await;
        }
        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        // 先 build 缓存 model_limit，使 token_usage() 在锁空闲时返回非零 limit。
        ctx.build_provider_context(&model, &[]).await.unwrap();

        let started = Arc::new(Notify::new());
        let proceed = Arc::new(Notify::new());
        ctx.set_summarizer(Box::new(BlockingSummary {
            started: started.clone(),
            proceed: proceed.clone(),
        }))
        .await;

        // 后台启动 summarize 压缩。
        let ctx_for_task = ctx.clone();
        let handle = tokio::spawn(async move {
            ctx_for_task
                .compact(CompactionStrategy::Summarize { max_tokens: 0 })
                .await
        });

        // 等到 summarize 进入 LLM 阶段（此刻锁应已释放）。
        started.notified().await;

        // 关键断言：summarize 进行中，token_usage() 必须返回缓存非零值而非全零 default。
        // 修复前持锁 await，try_lock 失败 → TokenUsage::default()（limit=0, current=0）。
        let usage = ctx.token_usage();
        assert_ne!(usage.limit, 0, "锁应已释放，limit 不应为 0");
        assert!(usage.current > 0, "锁应已释放，current 应非零");

        // 放行 summarize 完成。
        proceed.notify_one();
        handle.await.unwrap().unwrap();

        // 压缩后日志应被折叠（消息数减少）。
        let built = ctx.build_provider_context(&model, &[]).await.unwrap();
        assert!(
            built.messages.len() < 8,
            "压缩后消息数应减少，实际 {}",
            built.messages.len()
        );
    }

    /// `delete_message_at`（trait，经 InMemoryContext）：删除后日志与 token 计数同步更新。
    #[tokio::test]
    async fn delete_message_at_updates_inmemory() {
        use agent_core::ContextManager;
        let ctx = InMemoryContext::with_counter(vec![], token::TokenCounter::heuristic());
        ctx.append(AgentMessage::user_text("a")).await;
        ctx.append(AgentMessage::user_text("b")).await;
        ctx.append(AgentMessage::user_text("c")).await;
        // 删除索引 1（"b"）。
        let removed = ctx.delete_message_at(1).await.unwrap();
        assert_eq!(removed, 1);
        let snap = ctx.snapshot().await;
        let texts: Vec<String> = snap
            .iter()
            .filter_map(|m| match m {
                AgentMessage::User(u) => u.content.first().and_then(|c| match c {
                    agent_core::UserContent::Text { text } => Some(text.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["a".to_string(), "c".to_string()]);
        // 越界返回 0。
        assert_eq!(ctx.delete_message_at(99).await.unwrap(), 0);
        let _ = ctx.token_usage(); // 确保未 panic
    }

    // ── 会话树（P1-3）：fork / 切换 / 路径 / 压缩保枝 ───────────────────────

    /// 在节点 A 处 fork 出 B、C：切换活跃叶子时上下文正确反映各自路径。
    #[tokio::test]
    async fn fork_and_switch_reflects_each_path() {
        let ctx = InMemoryContext::with_counter(vec![], token::TokenCounter::heuristic());
        // 公共前缀：a → b。
        ctx.append(AgentMessage::user_text("a")).await;
        ctx.append(AgentMessage::user_text("b")).await;
        let leaf_b = ctx.active_leaf().await.unwrap();

        // 分支 B：从 b 续写 b1。
        ctx.append(AgentMessage::user_text("b1")).await;
        let leaf_b1 = ctx.active_leaf().await.unwrap();

        // 切回 b，再续写 c1（形成分支 C）。
        assert!(ctx.set_active_leaf(&leaf_b).await);
        ctx.append(AgentMessage::user_text("c1")).await;
        let leaf_c1 = ctx.active_leaf().await.unwrap();

        // 现在有两个叶子：b1 与 c1，且 b1 != c1。
        assert_ne!(leaf_b1, leaf_c1);
        let mut leaves = ctx.list_leaves().await;
        leaves.sort();
        assert_eq!(leaves, vec![leaf_b1.clone(), leaf_c1.clone()]);

        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);

        // 切到 B：上下文应含 a, b, b1。
        ctx.set_active_leaf(&leaf_b1).await;
        let b_texts = user_texts(&ctx, &model).await;
        assert_eq!(b_texts, vec!["a", "b", "b1"]);

        // 切到 C：上下文应含 a, b, c1。
        ctx.set_active_leaf(&leaf_c1).await;
        let c_texts = user_texts(&ctx, &model).await;
        assert_eq!(c_texts, vec!["a", "b", "c1"]);
    }

    /// fork 后继续 append：新消息挂到当前活跃叶子下，不污染兄弟分支。
    #[tokio::test]
    async fn append_after_switch_creates_branch() {
        let ctx = InMemoryContext::with_counter(vec![], token::TokenCounter::heuristic());
        ctx.append(AgentMessage::user_text("root")).await;
        let n0 = ctx.active_leaf().await.unwrap();
        ctx.append(AgentMessage::user_text("main")).await;
        let n_main = ctx.active_leaf().await.unwrap();

        // 切回 root 续写一条 alt（fork）。
        ctx.set_active_leaf(&n0).await;
        ctx.append(AgentMessage::user_text("alt")).await;
        let n_alt = ctx.active_leaf().await.unwrap();

        // children_of(root) 应同时含 main 与 alt。
        let mut ch = ctx.children_of(&n0).await;
        ch.sort();
        assert_eq!(ch.len(), 2);

        // 切回 main 继续：新消息在 main 分支下，不影响 alt。
        ctx.set_active_leaf(&n_main).await;
        ctx.append(AgentMessage::user_text("main2")).await;
        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        assert_eq!(
            user_texts(&ctx, &model).await,
            vec!["root", "main", "main2"]
        );

        // alt 分支仅 root, alt。
        ctx.set_active_leaf(&n_alt).await;
        assert_eq!(user_texts(&ctx, &model).await, vec!["root", "alt"]);
    }

    /// 压缩（prune）作用于活跃分支，但**分支数据不丢失**：另一分支的节点保留。
    #[tokio::test]
    async fn prune_preserves_other_branch() {
        let ctx = InMemoryContext::with_counter(vec![], token::TokenCounter::heuristic());
        // 主分支：m0..m4。
        for i in 0..5 {
            ctx.append(AgentMessage::user_text(format!("m{i}"))).await;
        }
        let m1 = ctx.snapshot_nodes().await[1].id.clone();
        // 从 m1 fork 一条分支：切回 m1，续写 x。
        ctx.set_active_leaf(&m1).await;
        ctx.append(AgentMessage::user_text("branch-x")).await;
        let leaf_x = ctx.active_leaf().await.unwrap();
        // 回主分支末梢 m4。
        let m4 = {
            let nodes = ctx.snapshot_nodes().await;
            nodes
                .iter()
                .find(|n| matches!(&n.message, AgentMessage::User(u) if user_text_of(u)=="m4"))
                .unwrap()
                .id
                .clone()
        };
        ctx.set_active_leaf(&m4).await;
        // 主分支 prune keep_recent=2：m0..m2 被裁。
        ctx.compact(CompactionStrategy::Prune { keep_recent: 2 })
            .await
            .unwrap();
        // 主分支上下文只剩 m3,m4。
        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        assert_eq!(user_texts(&ctx, &model).await, vec!["m3", "m4"]);
        // branch-x 分支仍可切换且内容正确（m0,m1,branch-x）。
        assert!(ctx.set_active_leaf(&leaf_x).await, "分支叶子应仍存在");
        assert_eq!(user_texts(&ctx, &model).await, vec!["m0", "m1", "branch-x"]);
    }

    /// 分支 handoff：从 B 切到 C 时，把 B 的独有后缀摘要注入 C 分支（用桩摘要器）。
    #[tokio::test]
    async fn switch_branch_with_handoff_injects_summary() {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::Arc;
        use std::sync::Mutex;

        struct StaticSummary {
            seen: Arc<Mutex<Vec<String>>>,
        }
        impl compaction::SummaryProvider for StaticSummary {
            fn summarize(
                &self,
                old: &[String],
            ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
                let seen = self.seen.clone();
                let joined = old.join("||");
                Box::pin(async move {
                    seen.lock().unwrap().push(joined.clone());
                    Ok(format!("SUM[{joined}]"))
                })
            }
        }

        let ctx = InMemoryContext::with_counter(vec![], token::TokenCounter::heuristic());
        // 公共：a → b。
        ctx.append(AgentMessage::user_text("a")).await;
        ctx.append(AgentMessage::user_text("b")).await;
        let b = ctx.active_leaf().await.unwrap();
        // B 独有：b1。
        ctx.append(AgentMessage::user_text("b1")).await;
        let leaf_b1 = ctx.active_leaf().await.unwrap();
        // C：从 b fork c1。
        ctx.set_active_leaf(&b).await;
        ctx.append(AgentMessage::user_text("c1")).await;
        let leaf_c1 = ctx.active_leaf().await.unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        ctx.set_summarizer(Box::new(StaticSummary { seen: seen.clone() }))
            .await;

        // 当前在 C（c1）；切到 B（b1）并要求 handoff：被离开的是 C 的独有后缀 c1。
        // 先回到 B，再从 B 切到 C 触发 B 独有后缀 b1 的摘要。这里测：从 c1 切到 b1。
        // 为确定性，显式先 set 到 c1，再 handoff 到 b1。
        ctx.set_active_leaf(&leaf_c1).await;
        let ok = ctx.switch_branch_with_handoff(&leaf_b1).await.unwrap();
        assert!(ok);
        // 摘要器被调用一次，输入含 c1。
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "handoff 应调用一次摘要器");
        assert!(
            calls[0].contains("c1"),
            "摘要输入应含被离开分支的 c1，实际: {}",
            calls[0]
        );

        // 活跃路径：a → b → b1 → [handoff]。handoff 消息在末尾。
        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        let texts = user_texts(&ctx, &model).await;
        assert!(texts.len() >= 3);
        assert_eq!(&texts[..2], &["a", "b"]);
        // 最后一条是 handoff 注入消息。
        assert!(
            texts.last().unwrap().contains("SUM["),
            "末尾应为 handoff 摘要: {:?}",
            texts
        );
    }

    /// `snapshot_nodes` / `restore_nodes` 往返：分支结构完整保留。
    #[tokio::test]
    async fn snapshot_restore_nodes_preserves_tree() {
        let ctx = InMemoryContext::with_counter(vec![], token::TokenCounter::heuristic());
        ctx.append(AgentMessage::user_text("a")).await;
        let a = ctx.active_leaf().await.unwrap();
        ctx.append(AgentMessage::user_text("b1")).await;
        let leaf_b1 = ctx.active_leaf().await.unwrap();
        ctx.set_active_leaf(&a).await;
        ctx.append(AgentMessage::user_text("c1")).await;
        let leaf_c1 = ctx.active_leaf().await.unwrap();

        let snap = ctx.snapshot_nodes().await;
        let ctx2 = InMemoryContext::with_counter(vec![], token::TokenCounter::heuristic());
        ctx2.restore_nodes(snap).await;
        // 两叶子皆在，且各自路径正确。
        let mut leaves = ctx2.list_leaves().await;
        leaves.sort();
        assert_eq!(leaves.len(), 2);
        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        ctx2.set_active_leaf(&leaf_b1).await;
        assert_eq!(user_texts(&ctx2, &model).await, vec!["a", "b1"]);
        ctx2.set_active_leaf(&leaf_c1).await;
        assert_eq!(user_texts(&ctx2, &model).await, vec!["a", "c1"]);
    }

    /// 异步提取辅助：取活跃路径上的 user 文本。
    async fn user_texts(ctx: &InMemoryContext, model: &Model) -> Vec<String> {
        let built = ctx.build_provider_context(model, &[]).await.unwrap();
        built
            .messages
            .iter()
            .filter_map(|m| match m {
                ProviderMessage::User { content } => content
                    .iter()
                    .filter_map(|c| match c {
                        agent_core::UserContent::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .next(),
                _ => None,
            })
            .collect()
    }

    fn user_text_of(u: &agent_core::UserMessage) -> String {
        u.content
            .iter()
            .filter_map(|c| match c {
                agent_core::UserContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}
