//! # agent-context
//!
//! 生产级上下文记忆：
//! - [`InMemoryContext`]：AppendOnlyLog + StablePrefix 指纹（最大化 provider 前缀缓存命中）。
//! - [`TokenCounter`](crate::token::TokenCounter)：tiktoken-rs 精确计数（OpenAI BPE），其它 provider 回退启发式。
//! - [`Compactor`](crate::compaction::Compactor)：summarize（handoff 摘要）/ shake（去冗余）/ prune（窗口裁剪）。
//! - [`PersistentContext`]：JSONL 落盘 + 恢复（断点续跑）。
//!
//! 移植 oh-my-pi `append-only-context` 的「只追加 + 稳定前缀」理念与 `compaction/` 子系统。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

pub mod compaction;
pub mod persistence;
pub mod token;

pub use persistence::{SessionInfo, SessionStore};

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use agent_core::{
    AgentMessage, CompactionStrategy, ContentBlock, ContextError, ContextManager, Model,
    ProviderContext, ProviderMessage, TokenUsage, ToolSpec,
};

pub use persistence::PersistentContext;

/// 内存上下文：AppendOnlyLog + 稳定前缀 + 精确 token 计数。
pub struct InMemoryContext {
    inner: tokio::sync::Mutex<Inner>,
    counter: token::TokenCounter,
}

struct Inner {
    system: Vec<String>,
    log: Vec<AgentMessage>,
    fingerprint: String,
    /// 注入的摘要提供器（summarize 用）。
    summarizer: Option<Box<dyn compaction::SummaryProvider>>,
    /// 缓存的模型上下文窗口上限（由 `build_provider_context` 更新）。
    /// 供 `token_usage()` 同步返回正确的 limit，避免 Web/ACP 客户端收到 limit=0。
    model_limit: usize,
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
                log: Vec::new(),
                fingerprint,
                summarizer: None,
                model_limit: 0,
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
                log: Vec::new(),
                fingerprint,
                summarizer: None,
                model_limit: 0,
            }),
            counter,
        }
    }

    /// 注入摘要提供器（启用生产级 summarize）。
    pub async fn set_summarizer(&self, provider: Box<dyn compaction::SummaryProvider>) {
        self.inner.lock().await.summarizer = Some(provider);
    }

    /// 取当前日志快照（持久化用）。
    pub async fn snapshot(&self) -> Vec<AgentMessage> {
        self.inner.lock().await.log.clone()
    }

    /// 替换日志（恢复用）。
    pub async fn restore(&self, log: Vec<AgentMessage>) {
        self.inner.lock().await.log = log;
    }
}

#[async_trait::async_trait]
impl ContextManager for InMemoryContext {
    async fn append(&self, message: AgentMessage) {
        self.inner.lock().await.log.push(message);
    }

    async fn set_system(&self, system: Vec<String>, tools: &[ToolSpec]) {
        let mut inner = self.inner.lock().await;
        inner.system = system;
        inner.fingerprint = fingerprint_of(&inner.system, tools);
    }

    async fn build_provider_context(
        &self,
        model: &Model,
        tools: &[ToolSpec],
    ) -> Result<ProviderContext, ContextError> {
        let mut inner = self.inner.lock().await;
        let messages = convert_to_llm(&inner.log);
        inner.fingerprint = fingerprint_of(&inner.system, tools);
        // 缓存模型窗口上限，供 token_usage() 同步返回正确值。
        inner.model_limit = model.max_input_tokens;
        let current = self.counter.count_context(&inner.system, &messages);
        Ok(ProviderContext {
            fingerprint: inner.fingerprint.clone(),
            system: inner.system.clone(),
            messages,
            tokens: TokenUsage {
                current,
                limit: model.max_input_tokens,
            },
        })
    }

    async fn compact(&self, strategy: CompactionStrategy) -> Result<(), ContextError> {
        // 各分支自行管理加锁：Prune/Shake 为纯计算（快速，全程持锁无妨）；
        // Summarize 涉及 LLM 网络调用，必须释放锁再 await，避免持锁数十秒阻塞
        // append/build/token_usage 等所有操作，并使 token_usage() 返回全零。
        match strategy {
            CompactionStrategy::Prune { keep_recent } => {
                let mut inner = self.inner.lock().await;
                inner.log = compaction::Compactor::prune(&inner.log, keep_recent);
                tracing::info!(kept = inner.log.len(), "已裁剪上下文");
            }
            CompactionStrategy::Shake => {
                let mut inner = self.inner.lock().await;
                inner.log = compaction::Compactor::shake(&inner.log);
                tracing::info!(kept = inner.log.len(), "已 shake 去冗余");
            }
            CompactionStrategy::Summarize { .. } => {
                // ── 阶段一（锁内）：取出 log 与 summarizer，确定压缩参数。──
                // summarizer 是 Box<dyn Trait> 无法 clone，只能整体 take 出锁，
                // 否则锁外的 summarize await 无法借用锁内对象。
                let mut inner = self.inner.lock().await;
                let log = std::mem::take(&mut inner.log);
                let keep = log.len().min(6);
                let Some(summarizer) = std::mem::take(&mut inner.summarizer) else {
                    // 无摘要器：回退日志，跳过压缩。
                    inner.log = log;
                    tracing::warn!("summarize 需注入 SummaryProvider，跳过");
                    return Ok(());
                };
                // 保留原始日志副本：summarize 失败时回退，避免上下文数据丢失。
                let original = log.clone();
                // ── 阶段二（锁外）：释放锁后执行 LLM summarize（网络调用，可能耗时数十秒）。──
                drop(inner);
                let outcome =
                    compaction::Compactor::summarize(log, keep, summarizer.as_ref()).await;
                // ── 阶段三（锁内）：重新加锁写回结果。──
                // 先用不含锁的 match 决定写回内容（成功用压缩结果，失败回退原始日志），
                // 再用一把紧致锁完成写回 + 合并窗口期间追加的消息，使锁仅覆盖必要区间。
                let (mut new_log, result) = match outcome {
                    Ok(new_log) => (new_log, Ok::<(), ContextError>(())),
                    Err(e) => (original, Err(ContextError::Compaction(e))),
                };
                let kept = {
                    let mut inner = self.inner.lock().await;
                    inner.summarizer = Some(summarizer);
                    // 合并 summarize 窗口（锁已释放）期间追加的消息，避免覆盖丢失：
                    // take 期间 inner.log 为空，并发 append 写入的新消息需追加到结果之后。
                    new_log.extend(std::mem::take(&mut inner.log));
                    inner.log = new_log;
                    inner.log.len()
                };
                if result.is_ok() {
                    tracing::info!(kept, "已 summarize 压缩");
                }
                return result;
            }
        }
        Ok(())
    }

    fn token_usage(&self) -> TokenUsage {
        match self.inner.try_lock() {
            Ok(inner) => TokenUsage {
                current: self.counter.count_context(&inner.system, &convert_to_llm(&inner.log)),
                limit: inner.model_limit,
            },
            Err(_) => TokenUsage::default(),
        }
    }

    fn prefix_fingerprint(&self) -> String {
        match self.inner.try_lock() {
            Ok(inner) => inner.fingerprint.clone(),
            Err(_) => "<locked>".into(),
        }
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
            AgentMessage::Assistant(a) => Some(ProviderMessage::Assistant {
                content: a.content.clone(),
            }),
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
                Some(ProviderMessage::Tool {
                    tool_call_id: t.tool_call_id.clone(),
                    content: t.result.to_llm_text(),
                    is_error: matches!(t.result, agent_core::ToolResult::Error { .. }),
                    images,
                })
            }
            AgentMessage::Status(_) | AgentMessage::Ask(_) | AgentMessage::SoftRequirement(_) => None,
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
            content: vec![agent_core::ContentBlock::Text { text: "hi back".into() }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
        }))
        .await;

        let model = Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        let built = ctx.build_provider_context(&model, &[]).await.unwrap();
        assert_eq!(built.messages.len(), 2);
        assert!(!built.fingerprint.is_empty());
        assert!(built.tokens.current > 0);
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
}
