//! 上下文压缩：生产级 summarize / shake / prune。
//!
//! - **Summarize**：将旧对话折叠为一份 handoff 摘要（移植 oh-my-pi compaction-summary），
//!   替换尾部，大幅降低 token。需要外部摘要生成器（LLM 调用）注入。
//! - **Shake**：机械去冗余（连续重复文本、空助手消息）+ 外科手术式归档（大围栏/XML 块、
//!   重型 ToolResult 替换为占位符并落盘到 artifact，可经 `read_file artifact://<id>` 回读），
//!   保护最近 token 窗口、节省阈值门控。移植 oh-my-pi `shake`。
//! - **Prune**：保留最近 N 条（tool-protection：工具结果不被裁剪）。

use std::collections::{HashMap, HashSet};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{
    AgentMessage, AssistantEvent, CompletionRequest, ContentBlock, LlmProvider, Model,
    ProviderCallContext, ProviderMessage, StatusKind, ToolResult, UserContent,
};
use futures::StreamExt;

/// 压缩器：纯函数，不直接调 LLM；summarize 的摘要由外部 `SummaryProvider` 注入。
pub struct Compactor;

/// 摘要提供器：对一批旧消息产出一份摘要文本（由 agent/llm 调用实现）。
pub trait SummaryProvider: Send + Sync {
    /// 生成摘要。`old` 为待折叠的消息（已转文本）。
    ///
    /// # Errors
    /// 生成失败时返回错误字符串。
    fn summarize(&self, old: &[String]) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>;
}

/// 用 LLM 生成摘要的 [`SummaryProvider`] 实现（handoff 摘要）。
///
/// 装配层（cli/server）构造后注入 `InMemoryContext::set_summarizer`。
pub struct LlmSummaryProvider {
    provider: Arc<dyn LlmProvider>,
    model: Model,
    provider_ctx: ProviderCallContext,
}

impl LlmSummaryProvider {
    /// 构造。
    #[must_use]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        model: Model,
        provider_ctx: ProviderCallContext,
    ) -> Self {
        Self {
            provider,
            model,
            provider_ctx,
        }
    }
}

/// 结构化 handoff 摘要指令模板（编译期内嵌自 `prompts/compaction-summary.md`）。
///
/// 移植 oh-my-pi `compaction-summary.md`：用固定段落（目标 / 约束 / 进展[已完成·进行中·受阻] /
/// 关键决策 / 下一步 / 关键上下文）+ 强约束（保留精确路径 / 函数名 / 错误信息 / 未答问题 / 仓库状态）
/// 取代此前一句式的自由要点，显著提升 summarize 压缩与分支切换 handoff 的接续质量——
/// 长会话压缩后模型「丢线索」「重复已完成工作」的根因即提示词过于简陋。
const COMPACTION_SUMMARY_PROMPT: &str = include_str!("../../../prompts/compaction-summary.md");

/// 构造结构化 handoff 摘要请求的用户提示词（指令模板 + 对话历史）。纯函数，便于测试。
fn summary_user_prompt(old: &[String]) -> String {
    format!(
        "{COMPACTION_SUMMARY_PROMPT}\n\n---\n\n以下是需要总结的对话历史：\n\n{}",
        old.join("\n---\n")
    )
}

impl SummaryProvider for LlmSummaryProvider {
    fn summarize(
        &self,
        old: &[String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>
    {
        let prompt = summary_user_prompt(old);
        Box::pin(async move {
            let req = CompletionRequest {
                model: self.model.clone(),
                system: vec![
                    "你是上下文摘要助手，严格按给定的 Markdown 结构输出交接摘要，不要输出任何额外文本。"
                        .to_string(),
                ],
                messages: vec![ProviderMessage::User {
                    content: vec![UserContent::Text { text: prompt }],
                }],
                tools: vec![],
                tool_choice: None,
                max_tokens: 1024,
                temperature: Some(0.0),
                thinking: None,
                cache_key: None,
            };
            let mut stream = self
                .provider
                .stream(req, &self.provider_ctx)
                .await
                .map_err(|e| e.to_string())?;
            let mut out = String::new();
            while let Some(ev) = stream.next().await {
                if let AssistantEvent::TextDelta(d) = ev {
                    out.push_str(&d);
                }
            }
            Ok(out)
        })
    }
}

// ============================================================================
// Shake：外科手术式上下文压缩（移植 oh-my-pi `shake`）
// ============================================================================

/// Shake 配置。
#[derive(Debug, Clone)]
pub struct ShakeConfig {
    /// 保护最近这么多 token 不被归档（默认 16_000）。
    pub protect_tokens: usize,
    /// 估算节省量低于此值则整体跳过（默认 4_000）。
    pub min_savings: usize,
    /// 围栏/XML 块的最小 token 门槛（默认 400）。
    pub fence_min_tokens: usize,
    /// 整体 ToolResult 的最小 token 门槛（默认 400）。
    pub tool_result_min_tokens: usize,
}

impl Default for ShakeConfig {
    fn default() -> Self {
        Self {
            protect_tokens: 16_000,
            min_savings: 4_000,
            fence_min_tokens: 400,
            tool_result_min_tokens: 400,
        }
    }
}

/// Shake 统计（观测用）。
#[derive(Debug, Clone, Default)]
pub struct ShakeStats {
    /// 归档前估算 token。
    pub tokens_before: usize,
    /// 归档后估算 token。
    pub tokens_after: usize,
    /// 估算节省 token。
    pub saved: usize,
    /// 被归档的 ToolResult 数。
    pub tool_results_elided: usize,
    /// 被归档的代码/XML 块数。
    pub blocks_elided: usize,
}

/// 一条可归档区域的定位（内部用）。
#[derive(Debug, Clone)]
enum ShakeRegion {
    /// 整条 ToolResult（按日志索引定位）。
    ToolResult {
        index: usize,
        tokens: usize,
        label: String,
    },
    /// 文本块内的一个字节区间 `[start, end)`。
    Block {
        index: usize,
        slot: BlockSlot,
        start: usize,
        end: usize,
        tokens: usize,
        text: String,
    },
}

impl ShakeRegion {
    fn tokens(&self) -> usize {
        match self {
            Self::ToolResult { tokens, .. } | Self::Block { tokens, .. } => *tokens,
        }
    }
}

/// 文本块定位：哪条消息的哪个文本内容块。
#[derive(Debug, Clone, Copy)]
enum BlockSlot {
    Assistant { block: usize },
    User { block: usize },
}

/// Shake 落盘槽：把被归档内容写入 artifact，返回可用于占位符的 id。
///
/// 实现应保证「相同内容 → 相同 id」（内容哈希去重）。无落盘能力时用 [`NullSink`]，
/// 占位符仍生成但内容不可回读（降级模式）。
pub trait ShakeSink: Send + Sync {
    /// 落盘；返回 artifact id。
    ///
    /// # Errors
    /// 落盘失败时返回错误字符串（调用方据此跳过本次 shake）。
    fn offload(&self, content: &str, label: &str) -> Result<String, String>;
}

/// 不落盘的 sink：以内容哈希作为 id，占位符仍生成但内容不可回读（降级 / 测试用）。
#[derive(Debug, Default, Clone)]
pub struct NullSink;

impl ShakeSink for NullSink {
    fn offload(&self, content: &str, _label: &str) -> Result<String, String> {
        Ok(content_hash(content))
    }
}

/// 目录落盘 sink：写入 `<dir>/<id>`，id 用内容哈希去重。
#[derive(Debug, Clone)]
pub struct DirSink {
    dir: PathBuf,
}

impl DirSink {
    /// 构造。
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// 目录路径。
    #[must_use]
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }
}

impl ShakeSink for DirSink {
    fn offload(&self, content: &str, _label: &str) -> Result<String, String> {
        let id = content_hash(content);
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let path = self.dir.join(&id);
        if !path.exists() {
            std::fs::write(&path, content).map_err(|e| e.to_string())?;
        }
        Ok(id)
    }
}

/// 内容 → 16 位十六进制哈希（用作 artifact id，天然去重）。
fn content_hash(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// 占位符的粗略 token 成本（仅用于节省量估算的门控）。
const PLACEHOLDER_TOKEN_ESTIMATE: usize = 16;

impl Compactor {
    /// Prune：保留最近 `keep_recent` 条；`skill://` read 的工具结果受保护（即便在窗口外也保留），
    /// 避免按需加载的 skill 内容在压缩中丢失（移植 oh-my-pi tool-protection 思想）。
    ///
    /// 工具配对完整性：任一被保留的 `ToolResult`，其发起 `ToolCall` 的助手消息也一并保留，
    /// 避免压缩后留下孤立 tool 消息（OpenAI 要求每条 tool 消息前必有对应的 tool_calls）。
    #[must_use]
    pub fn prune(log: &[AgentMessage], keep_recent: usize) -> Vec<AgentMessage> {
        // 先 supersede：移除被后续同文件 read 取代的旧结果（内容已过期），释放上下文。
        let deduped = supersede_read_results(log);
        if deduped.len() <= keep_recent {
            return deduped;
        }
        let protected = skill_read_call_ids(&deduped);
        let split = deduped.len() - keep_recent;
        // 会被保留的 ToolResult（窗口内 或 skill 受保护）所依赖的 tool_call_id：
        // 这些 ToolCall 所在的助手消息也必须保留，否则产生孤立 tool 消息。
        let needed_call_ids: HashSet<String> = deduped
            .iter()
            .enumerate()
            .filter(|(i, m)| *i >= split || is_protected_skill_message(m, &protected))
            .filter_map(|(_, m)| match m {
                AgentMessage::ToolResult(t) => Some(t.tool_call_id.clone()),
                _ => None,
            })
            .collect();
        deduped
            .iter()
            .enumerate()
            .filter(|(i, m)| {
                *i >= split
                    || is_protected_skill_message(m, &protected)
                    || assistant_has_any_call(m, &needed_call_ids)
            })
            .map(|(_, m)| m.clone())
            .collect()
    }

    /// Shake（机械级）：去连续重复 Status、删空助手消息。纯同步、不落盘。
    ///
    /// 保留为无 sink 的默认入口与既有测试使用；大块归档见 [`Compactor::shake_with`]。
    #[must_use]
    pub fn shake(log: &[AgentMessage]) -> Vec<AgentMessage> {
        shake_mechanical(log)
    }

    /// Shake（外科手术式压缩，移植 oh-my-pi `shake`）：在机械去冗余之上，把超阈值的大块
    /// （围栏代码块 / 顶层 XML）与重型 ToolResult 替换为占位符并落盘到 artifact（可经
    /// `read_file artifact://<id>` 回读），保护最近 `protect_tokens` token 不动，
    /// 估算节省低于 `min_savings` 则整体跳过。
    ///
    /// - `counter` 用于 token 估算（保护窗口 / 节省量）。
    /// - `sink` 用于落盘（[`NullSink`] 不落盘、内容不可回读；[`DirSink`] 落盘可回读）。
    /// - `skill://` read 的工具结果受保护，不归档（按需加载内容不应丢失）。
    ///
    /// # Errors
    /// 仅在 sink 落盘失败时返回错误字符串（调用方据此回退、保留原日志）。
    pub fn shake_with(
        log: &[AgentMessage],
        config: &ShakeConfig,
        counter: &crate::token::TokenCounter,
        sink: &dyn ShakeSink,
    ) -> Result<(Vec<AgentMessage>, ShakeStats), String> {
        shake_with_estimator(log, config, &|s| counter.count_text(s), sink)
    }

    /// Summarize：将旧消息折叠为单条 handoff 摘要用户消息。
    /// 返回 (保留前缀消息数, 新日志)。旧消息被替换为一条摘要。
    ///
    /// # Errors
    /// 通过 `provider` 生成摘要失败时返回错误。
    pub async fn summarize(
        log: Vec<AgentMessage>,
        keep_recent: usize,
        provider: &dyn SummaryProvider,
    ) -> Result<Vec<AgentMessage>, String> {
        if log.len() <= keep_recent {
            return Ok(log);
        }
        // 不要让 recent 窗口起点落在 ToolResult 上：其发起 ToolCall 的助手消息在 old（被摘要）
        // 侧，会留下孤立 tool 消息。向左扩展直到起点非 ToolResult（最近一次工具交换整体保留）。
        let mut split = log.len() - keep_recent;
        while split > 0 && matches!(log.get(split), Some(AgentMessage::ToolResult(_))) {
            split -= 1;
        }
        let old = &log[..split];
        let recent = log[split..].to_vec();

        // 将旧消息渲染为文本行
        let old_text: Vec<String> = old.iter().map(message_to_summary_line).collect();
        let summary = provider.summarize(&old_text).await?;

        let mut result = Vec::with_capacity(recent.len() + 1);
        result.push(AgentMessage::user_text(format!(
            "[上下文摘要] 此前的对话已被压缩为以下要点，请据此继续：\n\n{summary}"
        )));
        result.extend(recent);
        Ok(result)
    }
}

// ============================================================================
// Shake 实现细节（纯函数 + sink 落盘）
// ============================================================================

/// 机械级 shake：去连续重复 Status、删空助手消息。纯函数，不归档。
fn shake_mechanical(log: &[AgentMessage]) -> Vec<AgentMessage> {
    let mut out: Vec<AgentMessage> = Vec::with_capacity(log.len());
    let mut last_status: Option<String> = None;
    for m in log {
        match m {
            AgentMessage::Status(s) => {
                // 去除连续重复状态（如重复的 thinking 进度）。
                if s.kind == StatusKind::Info && Some(&s.text) == last_status.as_ref() {
                    continue;
                }
                last_status = Some(s.text.clone());
                out.push(m.clone());
            }
            AgentMessage::Assistant(a) => {
                // 移除纯空文本的助手消息（无文本、无工具调用）。
                let is_empty = a.content.iter().all(|b| match b {
                    ContentBlock::Text { text } => text.trim().is_empty(),
                    ContentBlock::ToolCall { .. } => false,
                    ContentBlock::Thinking { text, .. } => text.trim().is_empty(),
                }) && !a.has_tool_calls();
                if !is_empty {
                    out.push(m.clone());
                }
            }
            _ => out.push(m.clone()),
        }
    }
    out
}

/// 估算驱动的 shake 本体（供 [`Compactor::shake_with`] 与测试复用）。
fn shake_with_estimator(
    log: &[AgentMessage],
    config: &ShakeConfig,
    estimate: &dyn Fn(&str) -> usize,
    sink: &dyn ShakeSink,
) -> Result<(Vec<AgentMessage>, ShakeStats), String> {
    let deduped = shake_mechanical(log);
    let n = deduped.len();
    let mut stats = ShakeStats::default();
    if n == 0 {
        return Ok((deduped, stats));
    }

    let protected = skill_read_call_ids(&deduped);

    // 累计「严格在该条之后」的 token，用于保护窗口判定。
    let mut acc: usize = 0;
    let mut after = vec![0usize; n];
    for i in (0..n).rev() {
        after[i] = acc;
        acc = acc.saturating_add(message_tokens(&deduped[i], estimate));
    }
    let total_before = acc;

    let mut regions: Vec<ShakeRegion> = Vec::new();
    for (i, m) in deduped.iter().enumerate() {
        // 保护窗口：该条之后仍不足 protect_tokens → 不动近期上下文。
        if after[i] < config.protect_tokens {
            continue;
        }
        match m {
            AgentMessage::ToolResult(t) => {
                if protected.contains(&t.tool_call_id) {
                    continue;
                }
                let text = t.result.to_llm_text();
                let tokens = estimate(&text);
                if tokens < config.tool_result_min_tokens {
                    continue;
                }
                regions.push(ShakeRegion::ToolResult {
                    index: i,
                    tokens,
                    label: t.tool_call_id.clone(),
                });
            }
            AgentMessage::Assistant(a) => {
                for (bi, block) in a.content.iter().enumerate() {
                    if let ContentBlock::Text { text } = block {
                        push_block_regions(
                            i,
                            BlockSlot::Assistant { block: bi },
                            text,
                            config,
                            estimate,
                            &mut regions,
                        );
                    }
                }
            }
            AgentMessage::User(u) => {
                for (bi, c) in u.content.iter().enumerate() {
                    if let UserContent::Text { text } = c {
                        push_block_regions(
                            i,
                            BlockSlot::User { block: bi },
                            text,
                            config,
                            estimate,
                            &mut regions,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    // 节省阈值门控：估算总节省（每个区域 tokens - 占位符成本）。
    let mut savings = 0usize;
    for r in &regions {
        savings = savings.saturating_add(r.tokens().saturating_sub(PLACEHOLDER_TOKEN_ESTIMATE));
    }
    stats.tokens_before = total_before;
    stats.tokens_after = total_before;
    stats.saved = savings;

    if savings < config.min_savings || regions.is_empty() {
        // 不归档：返回机械去冗余结果（可能已省下重复 status / 空消息）。
        return Ok((deduped, stats));
    }

    let mut out = deduped;

    // 1. ToolResult 整体归档：保留 tool_call_id（配对完整），仅替换 result 内容为占位符。
    for r in &regions {
        if let ShakeRegion::ToolResult { index, tokens, label } = r {
            let Some(AgentMessage::ToolResult(t)) = out.get_mut(*index) else {
                continue;
            };
            let text = t.result.to_llm_text();
            let id = sink.offload(&text, label)?;
            t.result = ToolResult::Text(tool_result_placeholder(*tokens, &id));
            stats.tool_results_elided += 1;
        }
    }

    // 2. Block 归档：同一文本块内多区域按 start 降序应用，保证偏移始终有效。
    let mut blocks: Vec<&ShakeRegion> = regions
        .iter()
        .filter(|r| matches!(r, ShakeRegion::Block { .. }))
        .collect();
    blocks.sort_by(|a, b| block_sort_key(a).cmp(&block_sort_key(b)).reverse());
    for r in blocks {
        if let ShakeRegion::Block {
            index,
            slot,
            start,
            end,
            tokens,
            text,
        } = r
        {
            let id = sink.offload(text, "block")?;
            block_splice(&mut out, *index, *slot, *start, *end, &block_placeholder(*tokens, &id));
            stats.blocks_elided += 1;
        }
    }

    stats.tokens_after = total_before.saturating_sub(savings);
    Ok((out, stats))
}

/// 单条消息的 token 估算（保护窗口累计用）。
fn message_tokens(m: &AgentMessage, estimate: &dyn Fn(&str) -> usize) -> usize {
    match m {
        AgentMessage::User(u) => u
            .content
            .iter()
            .map(|c| match c {
                UserContent::Text { text } => estimate(text),
                UserContent::Image { data, .. } => data.len() / 4,
            })
            .sum(),
        AgentMessage::Assistant(a) => a
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => estimate(text),
                ContentBlock::Thinking { text, .. } => estimate(text),
                ContentBlock::ToolCall {
                    name, arguments, ..
                } => estimate(name) + estimate(&arguments.to_string()),
            })
            .sum(),
        AgentMessage::ToolResult(t) => estimate(&t.result.to_llm_text()),
        AgentMessage::Status(s) => estimate(&s.text),
        AgentMessage::Ask(a) => estimate(&a.prompt),
        AgentMessage::SoftRequirement(r) => estimate(&r.tool_name) + estimate(&r.reminder),
    }
}

/// 收集一段文本内超阈值的围栏/XML 块为归档区域。
fn push_block_regions(
    index: usize,
    slot: BlockSlot,
    text: &str,
    config: &ShakeConfig,
    estimate: &dyn Fn(&str) -> usize,
    out: &mut Vec<ShakeRegion>,
) {
    for range in scan_block_ranges(text) {
        if range.end <= range.start || range.end > text.len() {
            continue;
        }
        let slice = &text[range.start..range.end];
        if slice.is_empty() {
            continue;
        }
        let tokens = estimate(slice);
        if tokens < config.fence_min_tokens {
            continue;
        }
        out.push(ShakeRegion::Block {
            index,
            slot,
            start: range.start,
            end: range.end,
            tokens,
            text: slice.to_string(),
        });
    }
}

/// Block 区域排序键：(index, slot 类型, 文本块下标, start)。升序，调用方反转为降序。
fn block_sort_key(r: &ShakeRegion) -> (usize, usize, usize, usize) {
    match r {
        ShakeRegion::Block {
            index,
            slot,
            start,
            ..
        } => {
            let (slot_rank, block) = match slot {
                BlockSlot::Assistant { block } => (0, *block),
                BlockSlot::User { block } => (1, *block),
            };
            (*index, slot_rank, block, *start)
        }
        _ => (0, 0, 0, 0),
    }
}

/// 把 `replacement` 拼接进指定消息文本块的 `[start, end)` 区间。
fn block_splice(
    out: &mut [AgentMessage],
    index: usize,
    slot: BlockSlot,
    start: usize,
    end: usize,
    replacement: &str,
) {
    let Some(m) = out.get_mut(index) else {
        return;
    };
    match slot {
        BlockSlot::Assistant { block } => {
            let AgentMessage::Assistant(a) = m else {
                return;
            };
            let Some(ContentBlock::Text { text }) = a.content.get_mut(block) else {
                return;
            };
            splice_in_place(text, start, end, replacement);
        }
        BlockSlot::User { block } => {
            let AgentMessage::User(u) = m else {
                return;
            };
            let Some(UserContent::Text { text }) = u.content.get_mut(block) else {
                return;
            };
            splice_in_place(text, start, end, replacement);
        }
    }
}

/// 原地拼接 `text[start..end)` 为 `replacement`。`start`/`end` 为字节偏移，须在字符边界。
fn splice_in_place(text: &mut String, start: usize, end: usize, replacement: &str) {
    if end > text.len() || start > end || !text.is_char_boundary(start) || !text.is_char_boundary(end)
    {
        return;
    }
    let mut buf = String::with_capacity(text.len() + replacement.len());
    buf.push_str(&text[..start]);
    buf.push_str(replacement);
    buf.push_str(&text[end..]);
    *text = buf;
}

fn tool_result_placeholder(tokens: usize, id: &str) -> String {
    format!(
        "[已归档：工具结果（约 {tokens} token），用 read_file artifact://{id} 回读原始内容]"
    )
}

fn block_placeholder(tokens: usize, id: &str) -> String {
    format!(
        "[已归档：代码/XML 片段（约 {tokens} token），用 read_file artifact://{id} 回读]"
    )
}

/// 扫描文本中的围栏代码块（``` / ~~~）与顶层 XML 元素，返回字节区间 `[start, end)`
/// （含首尾围栏/标签行，不含尾随换行）。保守策略：未闭合的围栏/标签不产出区间；
/// 围栏内抑制 XML 检测。移植 oh-my-pi `scanTextForBlockRanges`。
fn scan_block_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut in_fence = false;
    let mut fence_start: Option<usize> = None;
    let mut tag_stack: Vec<String> = Vec::new();
    let mut xml_start: Option<usize> = None;

    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut line_start = 0usize;
    let mut i = 0usize;
    while i <= n {
        let at_boundary = i == n || bytes[i] == b'\n';
        if !at_boundary {
            i += 1;
            continue;
        }
        // 当前行 = [line_start, i)（不含换行）。
        let line = text.get(line_start..i).unwrap_or("");
        let trimmed_start = line.trim_start();
        let is_fence_line =
            trimmed_start.starts_with("```") || trimmed_start.starts_with("~~~");
        if is_fence_line {
            if !in_fence {
                in_fence = true;
                fence_start = Some(line_start);
            } else {
                in_fence = false;
                if let Some(start) = fence_start.take() {
                    ranges.push(start..i);
                }
            }
        } else if !in_fence {
            // 仅列首（无前导空白）的整行标签算顶层 XML。
            let at_col0 = trimmed_start.len() == line.len();
            if at_col0 {
                if let Some(name) = parse_opening_xml(line) {
                    if tag_stack.is_empty() {
                        xml_start = Some(line_start);
                    }
                    tag_stack.push(name.to_string());
                } else if let Some(name) = parse_closing_xml(line) {
                    if tag_stack.last().map(String::as_str) == Some(name) {
                        tag_stack.pop();
                        if tag_stack.is_empty() {
                            if let Some(start) = xml_start.take() {
                                ranges.push(start..i);
                            }
                        }
                    }
                }
            }
        }
        if i == n {
            break;
        }
        i += 1; // 跳过换行
        line_start = i;
    }
    merge_ranges(ranges)
}

/// 解析列首整行开标签 `<tag>` / `<tag attrs>`，返回标签名。不匹配返回 `None`。
fn parse_opening_xml(line: &str) -> Option<&str> {
    let l = line.strip_suffix('\r').unwrap_or(line);
    if !l.starts_with('<') || !l.ends_with('>') || l.len() < 3 {
        return None;
    }
    let inner = &l[1..l.len() - 1];
    let name_end = inner
        .bytes()
        .position(|b| !(b.is_ascii_lowercase() || b == b'_' || b == b'-'))
        .unwrap_or(inner.len());
    if name_end == 0 {
        return None;
    }
    let name = &inner[..name_end];
    let rest = &inner[name_end..];
    // 名后须为空，或以空白起头（属性段）。
    if rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t') {
        if name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_' || b == b'-')
        {
            return Some(name);
        }
    }
    None
}

/// 解析列首整行闭标签 `</tag>`，返回标签名。
fn parse_closing_xml(line: &str) -> Option<&str> {
    let l = line.strip_suffix('\r').unwrap_or(line);
    if !l.starts_with("</") || !l.ends_with('>') || l.len() < 4 {
        return None;
    }
    let name = &l[2..l.len() - 1];
    if !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_' || b == b'-')
    {
        Some(name)
    } else {
        None
    }
}

/// 合并重叠/嵌套区间，按 start 升序保留最外层（移植 oh-my-pi `mergeRanges`）。
fn merge_ranges(ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    if ranges.len() <= 1 {
        return ranges;
    }
    let mut sorted = ranges;
    sorted.sort_by_key(|r| r.start);
    let mut kept = Vec::new();
    let mut last_end: Option<usize> = None;
    for r in sorted {
        if last_end.is_some_and(|e| r.start < e) {
            continue;
        }
        last_end = Some(r.end);
        kept.push(r);
    }
    kept
}

/// 移除被后续同文件 read 取代的旧 `read_file` 工具结果（A10 supersede）。
///
/// 同一文件多次读取时仅保留最后一次结果；旧结果内容已过期，裁剪以释放上下文。
fn supersede_read_results(log: &[AgentMessage]) -> Vec<AgentMessage> {
    let mut reads_by_path: HashMap<String, Vec<String>> = HashMap::new();
    for m in log {
        let AgentMessage::Assistant(a) = m else {
            continue;
        };
        for block in &a.content {
            if let ContentBlock::ToolCall { id, name, arguments } = block {
                if name == "read_file" {
                    if let Some(path) = arguments.get("path").and_then(serde_json::Value::as_str) {
                        reads_by_path.entry(path.to_string()).or_default().push(id.clone());
                    }
                }
            }
        }
    }
    let mut remove: HashSet<String> = HashSet::new();
    for ids in reads_by_path.values() {
        if ids.len() > 1 {
            for id in &ids[..ids.len() - 1] {
                remove.insert(id.clone());
            }
        }
    }
    if remove.is_empty() {
        return log.to_vec();
    }
    log.iter()
        .filter(|m| !matches!(m, AgentMessage::ToolResult(t) if remove.contains(&t.tool_call_id)))
        .cloned()
        .collect()
}

/// 收集日志中 `read_file skill://...` 调用的 tool_call_id（受保护，prune 时不裁剪）。
fn skill_read_call_ids(log: &[AgentMessage]) -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();
    for m in log {
        let AgentMessage::Assistant(a) = m else {
            continue;
        };
        for block in &a.content {
            if let ContentBlock::ToolCall { id, name, arguments } = block {
                if name == "read_file"
                    && arguments
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|p| p.starts_with("skill://"))
                {
                    ids.insert(id.clone());
                }
            }
        }
    }
    ids
}

/// 消息是否为受保护的 skill read 结果。
fn is_protected_skill_message(m: &AgentMessage, ids: &std::collections::HashSet<String>) -> bool {
    matches!(m, AgentMessage::ToolResult(t) if ids.contains(&t.tool_call_id))
}

/// 助手消息是否含任一指定 tool_call_id（用于保留「被保留 ToolResult」的发起消息，避免孤立）。
fn assistant_has_any_call(m: &AgentMessage, ids: &HashSet<String>) -> bool {
    let AgentMessage::Assistant(a) = m else {
        return false;
    };
    a.content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolCall { id, .. } if ids.contains(id)))
}

/// 单条消息渲染为摘要行。
pub(crate) fn message_to_summary_line(m: &AgentMessage) -> String {
    match m {
        AgentMessage::User(u) => {
            let t: String = u
                .content
                .iter()
                .filter_map(|c| match c {
                    agent_core::UserContent::Text { text } => Some(text.as_str()),
                    agent_core::UserContent::Image { .. } => Some("[image]"),
                })
                .collect();
            format!("用户: {t}")
        }
        AgentMessage::Assistant(a) => format!("助手: {}", a.text()),
        AgentMessage::ToolResult(t) => format!("工具结果: {}", t.result.to_llm_text()),
        AgentMessage::Status(s) => format!("({}) {}", s.kind_text(), s.text),
        AgentMessage::Ask(a) => format!("[询问] {}", a.prompt),
        AgentMessage::SoftRequirement(r) => format!("[软需求: {}]", r.tool_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AssistantMessage, StatusMessage, ToolResult, ToolResultMessage, Usage};

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
        })
    }

    fn status(text: &str) -> AgentMessage {
        AgentMessage::Status(StatusMessage {
            text: text.into(),
            kind: StatusKind::Info,
        })
    }

    #[test]
    fn prune_keeps_recent() {
        let log = vec![
            AgentMessage::user_text("a"),
            AgentMessage::user_text("b"),
            AgentMessage::user_text("c"),
        ];
        let out = Compactor::prune(&log, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1], AgentMessage::user_text("c"));
    }

    #[test]
    fn prune_supersedes_old_read_results() {
        use serde_json::json;
        let call = |id: &str| {
            AgentMessage::Assistant(AssistantMessage {
                content: vec![ContentBlock::ToolCall {
                    id: id.into(),
                    name: "read_file".into(),
                    arguments: json!({ "path": "a.txt" }),
                }],
                usage: Usage::default(),
                model: "m".into(),
                stop_reason: None,
            })
        };
        let res = |id: &str, text: &str| {
            AgentMessage::ToolResult(ToolResultMessage {
                tool_call_id: id.into(),
                result: ToolResult::text(text),
            })
        };
        // 同文件读两次：旧结果应被取代裁剪，新结果保留
        let log = vec![call("r1"), res("r1", "old"), call("r2"), res("r2", "new")];
        let out = Compactor::prune(&log, 10); // keep 大，仅 supersede 生效
        assert!(
            !out.iter()
                .any(|m| matches!(m, AgentMessage::ToolResult(t) if t.tool_call_id == "r1")),
            "旧 read 应被取代裁剪"
        );
        assert!(
            out.iter()
                .any(|m| matches!(m, AgentMessage::ToolResult(t) if t.tool_call_id == "r2")),
            "新 read 应保留"
        );
    }

    #[test]
    fn prune_protects_skill_read_results() {
        use serde_json::json;
        let call = AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: json!({ "path": "skill://pdf" }),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
        });
        let result = AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "call-1".into(),
            result: ToolResult::text("PDF skill body"),
        });
        let recent = AgentMessage::user_text("recent");
        let log = vec![call, result, recent];
        // keep_recent=1：仅尾部 recent + 受保护的 skill 结果
        let out = Compactor::prune(&log, 1);
        assert!(out
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(t) if t.tool_call_id == "call-1")));
        assert!(out.iter().any(|m| matches!(m, AgentMessage::User(_))));
    }

    /// 回归：受保护的 skill 工具结果保留时，其发起 ToolCall 的助手消息也必须保留，
    /// 否则 convert_to_llm 会得到孤立 tool 消息。
    #[test]
    fn prune_keeps_call_for_protected_skill_result() {
        use serde_json::json;
        let call = AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: json!({ "path": "skill://pdf" }),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
        });
        let result = AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "call-1".into(),
            result: ToolResult::text("PDF skill body"),
        });
        let recent = AgentMessage::user_text("recent");
        let log = vec![call, result, recent];
        let out = Compactor::prune(&log, 1);
        // 受保护的 skill 结果保留，且其发起 ToolCall 的助手消息一并保留（不孤立）。
        assert!(out
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(t) if t.tool_call_id == "call-1")));
        assert!(out.iter().any(|m| matches!(
            m,
            AgentMessage::Assistant(a) if a
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall { id, .. } if id == "call-1"))
        )));
    }

    #[test]
    fn shake_drops_duplicate_status() {
        let log = vec![status("思考中"), status("思考中"), assistant("hi")];
        let out = Compactor::shake(&log);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn shake_drops_empty_assistant() {
        let empty = AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: "   ".into() }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
        });
        let log = vec![empty, assistant("real")];
        let out = Compactor::shake(&log);
        assert_eq!(out.len(), 1);
    }

    struct StaticSummary;
    impl SummaryProvider for StaticSummary {
        fn summarize(&self, _old: &[String]) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>> {
            Box::pin(async { Ok("已总结 3 条".into()) })
        }
    }

    #[tokio::test]
    async fn summarize_replaces_old_with_handoff() {
        let log = vec![
            AgentMessage::user_text("q1"),
            assistant("a1"),
            AgentMessage::user_text("q2"),
            assistant("a2"),
            AgentMessage::user_text("q3"),
        ];
        let out = Compactor::summarize(log, 2, &StaticSummary).await.unwrap();
        // 1 摘要 + 2 保留
        assert_eq!(out.len(), 3);
        assert!(out[0].text_unchecked().contains("上下文摘要"));
    }

    // P0-G：结构化 handoff 摘要提示词（移植 oh-my-pi compaction-summary 模板）。
    // 验证：指令含固定结构段落 + 强约束（路径/未答问题），且对话历史被内嵌。
    #[test]
    fn summary_prompt_is_structured_and_embeds_history() {
        let prompt = summary_user_prompt(&["用户问 X".to_string(), "助手答 Y".to_string()]);
        // 结构化段落（移植 oh-my-pi compaction-summary 模板）。
        assert!(prompt.contains("## 目标"), "缺目标段: {prompt}");
        assert!(prompt.contains("## 进展"), "缺进展段: {prompt}");
        assert!(prompt.contains("### 已完成"), "缺已完成段: {prompt}");
        assert!(prompt.contains("### 受阻"), "缺受阻段: {prompt}");
        assert!(prompt.contains("## 下一步"), "缺下一步段: {prompt}");
        assert!(prompt.contains("## 关键上下文"), "缺关键上下文段: {prompt}");
        // 强约束：保留精确路径/函数名 + 未答问题。
        assert!(prompt.contains("精确的文件路径"), "缺路径保留约束: {prompt}");
        assert!(prompt.contains("尚未回答的问题"), "缺未答问题保留约束: {prompt}");
        // 对话历史被内嵌。
        assert!(
            prompt.contains("用户问 X") && prompt.contains("助手答 Y"),
            "对话历史未内嵌: {prompt}"
        );
    }

    // 测试辅助：取消息文本（简化）
    trait MsgText {
        fn text_unchecked(&self) -> String;
    }
    impl MsgText for AgentMessage {
        fn text_unchecked(&self) -> String {
            match self {
                AgentMessage::User(u) => u
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        agent_core::UserContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect(),
                _ => String::new(),
            }
        }
    }

    // 静默未用类型
    #[allow(dead_code)]
    fn _force_tool_result_use() -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "x".into(),
            result: ToolResult::text("y"),
        })
    }

    // ── Shake：块检测 ──────────────────────────────────────────────────────

    #[test]
    fn scan_detects_fenced_code_block() {
        let text = "intro\n```rust\nfn main() {}\n```\ntail";
        let ranges = scan_block_ranges(text);
        assert_eq!(ranges.len(), 1);
        let slice = &text[ranges[0].start..ranges[0].end];
        assert!(slice.starts_with("```"), "区间应含开围栏: {slice:?}");
        assert!(slice.ends_with("```"), "区间应含闭围栏: {slice:?}");
    }

    #[test]
    fn scan_detects_top_level_xml() {
        let text = "before\n<note>\nsome\n</note>\nafter";
        let ranges = scan_block_ranges(text);
        assert_eq!(ranges.len(), 1);
        let slice = &text[ranges[0].start..ranges[0].end];
        assert!(slice.starts_with("<note>"), "{slice:?}");
        assert!(slice.ends_with("</note>"), "{slice:?}");
    }

    #[test]
    fn scan_ignores_xml_inside_fence() {
        // 围栏内的 <notag> 不单独产出区间；仅识别到围栏整体。
        let text = "```\n<notag>\n```";
        let ranges = scan_block_ranges(text);
        assert_eq!(ranges.len(), 1);
    }

    #[test]
    fn scan_drops_unclosed_fence() {
        // 未闭合围栏：保守不产出区间。
        let text = "```\nfn main() {}";
        assert!(scan_block_ranges(text).is_empty());
    }

    // ── Shake：归档压缩 ───────────────────────────────────────────────────

    fn heuristic_counter() -> crate::token::TokenCounter {
        crate::token::TokenCounter::heuristic()
    }

    #[test]
    fn shake_with_elides_large_tool_result() {
        let big = "x".repeat(4000); // 启发式 ≈ 1000 token
        let log = vec![
            AgentMessage::user_text("问题"),
            AgentMessage::ToolResult(ToolResultMessage {
                tool_call_id: "c1".into(),
                result: ToolResult::text(big),
            }),
        ];
        let cfg = ShakeConfig {
            protect_tokens: 0,
            min_savings: 0,
            fence_min_tokens: 400,
            tool_result_min_tokens: 10,
        };
        let (out, stats) = Compactor::shake_with(&log, &cfg, &heuristic_counter(), &NullSink).unwrap();
        assert_eq!(stats.tool_results_elided, 1);
        assert!(stats.saved > 0);
        let AgentMessage::ToolResult(t) = &out[1] else {
            panic!("应为工具结果消息");
        };
        assert!(
            t.result.to_llm_text().contains("artifact://"),
            "应替换为占位符: {}",
            t.result.to_llm_text()
        );
    }

    #[test]
    fn shake_with_protects_recent_window() {
        let big = "x".repeat(4000);
        let log = vec![AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "c1".into(),
            result: ToolResult::text(big.clone()),
        })];
        // protect_tokens 远超日志总量 → 全部受保护，不归档。
        let cfg = ShakeConfig {
            protect_tokens: 10_000,
            min_savings: 0,
            fence_min_tokens: 400,
            tool_result_min_tokens: 10,
        };
        let (out, stats) = Compactor::shake_with(&log, &cfg, &heuristic_counter(), &NullSink).unwrap();
        assert_eq!(stats.tool_results_elided, 0);
        let AgentMessage::ToolResult(t) = &out[0] else {
            panic!()
        };
        assert_eq!(t.result.to_llm_text(), big, "保护窗口内应原样保留");
    }

    #[test]
    fn shake_with_min_savings_gate() {
        let big = "x".repeat(4000); // 节省约 984 token
        let log = vec![AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "c1".into(),
            result: ToolResult::text(big.clone()),
        })];
        // min_savings 高到不可能达到 → 整体跳过。
        let cfg = ShakeConfig {
            protect_tokens: 0,
            min_savings: 10_000,
            fence_min_tokens: 400,
            tool_result_min_tokens: 10,
        };
        let (out, stats) = Compactor::shake_with(&log, &cfg, &heuristic_counter(), &NullSink).unwrap();
        assert_eq!(stats.tool_results_elided, 0);
        let AgentMessage::ToolResult(t) = &out[0] else {
            panic!()
        };
        assert_eq!(t.result.to_llm_text(), big, "阈值未达应原样保留");
    }

    #[test]
    fn shake_with_elides_block_in_assistant() {
        let body = "let x = 1;\n".repeat(200);
        let code = format!("```rust\n{body}```");
        let log = vec![assistant(&code)];
        let cfg = ShakeConfig {
            protect_tokens: 0,
            min_savings: 0,
            fence_min_tokens: 10,
            tool_result_min_tokens: 400,
        };
        let (out, stats) = Compactor::shake_with(&log, &cfg, &heuristic_counter(), &NullSink).unwrap();
        assert_eq!(stats.blocks_elided, 1);
        let AgentMessage::Assistant(a) = &out[0] else {
            panic!()
        };
        let text = a.text();
        assert!(text.contains("artifact://"), "应为占位符: {text}");
        assert!(!text.contains(&body), "原始大块应已被替换");
    }

    #[test]
    fn shake_with_preserves_tool_call_pairing() {
        // ToolResult 占位化仅替换 result，保留 tool_call_id（不产生孤立消息）。
        use serde_json::json;
        let call = AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: json!({ "path": "a.txt" }),
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
        });
        let big = "x".repeat(4000);
        let res = AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "c1".into(),
            result: ToolResult::text(big),
        });
        let log = vec![call, res];
        let cfg = ShakeConfig {
            protect_tokens: 0,
            min_savings: 0,
            fence_min_tokens: 400,
            tool_result_min_tokens: 10,
        };
        let (out, stats) = Compactor::shake_with(&log, &cfg, &heuristic_counter(), &NullSink).unwrap();
        assert_eq!(stats.tool_results_elided, 1);
        assert_eq!(out.len(), 2, "消息数不变（仅替换内容）");
        let AgentMessage::ToolResult(t) = &out[1] else {
            panic!()
        };
        assert_eq!(t.tool_call_id, "c1", "tool_call_id 保留");
    }

    // ── DirSink 落盘 ──────────────────────────────────────────────────────

    #[test]
    fn dir_sink_offload_roundtrip() {
        let dir = std::env::temp_dir().join(format!("gyre-shake-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sink = DirSink::new(&dir);
        let id = sink.offload("hello world", "block").unwrap();
        let written = std::fs::read_to_string(dir.join(&id)).unwrap();
        assert_eq!(written, "hello world");
        // 同内容 → 同 id（去重），不产生第二份文件。
        let id2 = sink.offload("hello world", "block").unwrap();
        assert_eq!(id, id2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shake_with_dir_sink_persists_recoverable_content() {
        let dir = std::env::temp_dir().join(format!("gyre-shake-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let big = "y".repeat(4000);
        let log = vec![AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "c1".into(),
            result: ToolResult::text(big.clone()),
        })];
        let cfg = ShakeConfig {
            protect_tokens: 0,
            min_savings: 0,
            fence_min_tokens: 400,
            tool_result_min_tokens: 10,
        };
        let (out, stats) =
            Compactor::shake_with(&log, &cfg, &heuristic_counter(), &DirSink::new(&dir)).unwrap();
        assert_eq!(stats.tool_results_elided, 1);
        // 归档目录恰好一份文件，内容 == 原始大文本（可回读）。
        let mut entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(entries.len(), 1, "应落盘一份 artifact");
        let content =
            std::fs::read_to_string(entries.pop().unwrap().unwrap().path()).unwrap();
        assert_eq!(content, big);
        // 占位符引用 artifact://<id>。
        let AgentMessage::ToolResult(t) = &out[0] else {
            panic!()
        };
        assert!(t.result.to_llm_text().contains("artifact://"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
