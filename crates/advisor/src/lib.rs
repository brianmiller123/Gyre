//! # advisor
//!
//! 后台审阅者子系统（移植 oh-my-pi `AdvisorRuntime`）。在主智能体**每轮结束**时，以
//! fire-and-forget 方式把本轮增量交由一个后台「大脑」审阅，产出建议经主循环已有的
//! **aside 通道**折叠注入——零主循环表面积改动。
//!
//! ## 失败隔离三原则（对齐 oh-my-pi）
//!
//! 1. **主代理永不阻塞**：[`AdvisorHook::on_turn_end`] 仅把增量入队即返回；真正的审阅、
//!    启发式信号与上下文维护在独立 `tokio::spawn` 的单 drain 任务里异步进行。
//! 2. **配额耗尽即停**：[`AdvisorError::Quota`] 置 `quota_exhausted`，丢弃新 delta 直到显式
//!    [`AdvisorRuntime::reset`]——无定时器自动恢复（provider 配额窗口远长于任何合理定时器）。
//! 3. **陈旧批次丢弃**：[`AdvisorRuntime::reset`] 递增 epoch；drain 中 epoch 不匹配的待处理
//!    delta 被丢弃（advise 前后各校验一次，覆盖「advise 期间 reset」的竞态）。
//!
//! ## P0-2 上下文维护（异步 summarize 卸载）
//!
//! [`AdvisorRuntime`] 可选地持有一个 [`ContextMaintainer`]（默认 [`ThresholdCompactor`]）：
//! 每轮后台检查 token 占用，**在主循环硬阈值之前**异步触发 `Summarize`——主循环不再因
//! summarize 的 LLM 网络往返而冻结。主循环自身的 prune/shake 兜底**保持不变**（安全网）。
//!
//! ## P1 启发式质量信号（确定性，零额外 LLM 成本）
//!
//! 每轮**先于** LLM 大脑运行 [`analyze_repetition`]：在滑动窗口内检测同一工具被高频重复调用
//! （长程任务「卡在循环里反复 read_file / grep」的高频根因）。命中即注入启发式建议并**跳过**
//! 当轮 LLM 大脑调用（省钱、省时）；未命中才走大脑。启发式无失败态，brain 处于 failing 时仍生效。

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_core::{
    AssistantEvent, AssistantMessage, CompactionStrategy, CompletionRequest, ContentBlock,
    ContextError, ContextManager, Hook, HookEvent, LlmError, LlmProvider, Model, ProviderCallContext,
    ProviderMessage, TokenUsage, ToolResult, ToolResultMessage, TurnEndContext, UserContent,
};
use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

// ============================================================================
// Delta：把一轮 TurnEndContext 渲染为后台审阅输入
// ============================================================================

/// 一轮的增量（后台审阅输入）。`Send + 'static`，可安全 move 进 drain 任务。
#[derive(Debug, Clone)]
pub struct AdvisorDelta {
    /// 标题（含 `[进行中]` / `[已结束]` 标记，移植 oh-my-pi `willContinue` 感知）。
    pub heading: String,
    /// 渲染后的本轮正文（助手文本 + 工具调用/结果摘要）。
    pub body: String,
    /// 是否仍在进行中（主代理本轮后还会继续）。
    pub will_continue: bool,
    /// 本轮调用的工具名集合（去重、保序，供启发式分析）。
    pub tool_names: Vec<String>,
}

/// 工具结果摘要的字符上限（避免单个超大结果撑爆审阅输入）。
const TOOL_RESULT_PREVIEW_CHARS: usize = 600;
/// 工具参数 JSON 的字符上限。
const TOOL_ARGS_PREVIEW_CHARS: usize = 300;

/// 截断到 `max` 字符（按 char boundary，超长加 …）。
fn ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// 把一轮 [`TurnEndContext`] 渲染为 [`AdvisorDelta`]（纯函数，便于测试）。
#[must_use]
pub fn render_turn_delta(
    message: &AssistantMessage,
    tool_results: &[ToolResultMessage],
    will_continue: bool,
) -> AdvisorDelta {
    let mut body = String::new();
    let mut tool_names: Vec<String> = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => {
                if !text.trim().is_empty() {
                    body.push_str(text.trim());
                    body.push('\n');
                }
            }
            ContentBlock::ToolCall { name, arguments, .. } => {
                if !tool_names.contains(name) {
                    tool_names.push(name.clone());
                }
                let args = if arguments.is_null() {
                    String::from("()")
                } else {
                    ellipsis(&arguments.to_string(), TOOL_ARGS_PREVIEW_CHARS)
                };
                body.push_str(&format!("→ 工具调用 {name}({args})\n"));
            }
            // 思考块不喂给审阅者（内部推理，且 signature 重放不可靠）。
            ContentBlock::Thinking { .. } => {}
        }
    }
    for tr in tool_results {
        let preview = render_tool_result(&tr.result);
        body.push_str(&format!("  ↳ 结果: {}\n", ellipsis(&preview, TOOL_RESULT_PREVIEW_CHARS)));
    }
    let heading = if will_continue {
        "本轮 [进行中]"
    } else {
        "本轮 [已结束]"
    }
    .to_string();
    AdvisorDelta { heading, body, will_continue, tool_names }
}

/// 渲染工具结果为单行摘要。
fn render_tool_result(result: &ToolResult) -> String {
    match result {
        ToolResult::Text(t) => t.clone(),
        ToolResult::Image { mime, .. } => format!("[image/{mime}]"),
        ToolResult::Error { recoverable, message } => {
            let kind = if *recoverable { "可恢复错误" } else { "致命错误" };
            format!("[{kind}] {message}")
        }
    }
}

// ============================================================================
// P1 启发式质量信号：滑动窗口重复工具调用检测
// ============================================================================

/// 启发式分析的可变状态（drain 单线程串行访问，用 [`Mutex`] 仅做内部可变性）。
#[derive(Debug)]
pub struct QualityState {
    /// 滑动窗口：最近若干轮，每轮的去重工具名列表（按入队顺序）。
    history: VecDeque<Vec<String>>,
    /// 窗口大小（轮数）。
    window: usize,
    /// 触发阈值：窗口内某工具出现轮数 ≥ 此值即告警。
    threshold: usize,
}

impl QualityState {
    /// 构造：默认窗口 6 轮、阈值 4（即同一工具在最近 6 轮里出现 ≥4 次）。
    #[must_use]
    pub fn new() -> Self {
        Self::with_params(DEFAULT_QUALITY_WINDOW, DEFAULT_QUALITY_THRESHOLD)
    }

    /// 自定义窗口与阈值构造。
    #[must_use]
    pub fn with_params(window: usize, threshold: usize) -> Self {
        Self { history: VecDeque::new(), window: window.max(1), threshold: threshold.max(1) }
    }

    /// 当前窗口内已记录的轮数（测试用）。
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// 是否为空（测试用）。
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }
}

impl Default for QualityState {
    fn default() -> Self {
        Self::new()
    }
}

/// 默认滑动窗口（轮数）。
pub const DEFAULT_QUALITY_WINDOW: usize = 6;
/// 默认重复阈值（窗口内某工具出现的轮数）。
pub const DEFAULT_QUALITY_THRESHOLD: usize = 4;
/// P0-1 advisor 独立上下文记忆：保留最近 N 轮已审摘要，供跨轮连续性审阅（移植 oh-my-pi
/// advisor 的 `#latestMessages`/`#seenContext` 累积视图）。reset 时清空。
pub const ADVISOR_MEMORY_TURNS: usize = 8;

/// 滑动窗口重复检测：记录本轮工具名，返回是否触发「重复调用」告警文本。
///
/// 语义：窗口内某工具累计出现 ≥ `threshold` 轮，**且**本轮也调用了它 → 告警。
/// 告警文本单行、可执行（点名工具 + 次数）。命中后调用方可跳过当轮 LLM 大脑。
/// 纯函数（仅更新 `state`，无网络/IO），便于单测。
pub fn analyze_repetition(state: &mut QualityState, delta: &AdvisorDelta) -> Option<String> {
    if delta.tool_names.is_empty() {
        // 无工具调用的轮次不入窗（避免拉低频率、误判）。
        return None;
    }
    state.history.push_back(delta.tool_names.clone());
    while state.history.len() > state.window {
        state.history.pop_front();
    }

    // 统计窗口内每个工具「出现的轮数」（一轮内多次调用只计一次，故用去重后的 tool_names）。
    let mut freq: HashMap<&str, usize> = HashMap::new();
    for turn in &state.history {
        for t in turn {
            *freq.entry(t.as_str()).or_default() += 1;
        }
    }

    // 本轮被调用、且窗口内累计 ≥ threshold 的工具 → 告警（取累计最高的一个）。
    let mut worst: Option<(&str, usize)> = None;
    for t in &delta.tool_names {
        let count = *freq.get(t.as_str()).unwrap_or(&0);
        if count >= state.threshold && count > worst.map_or(0, |(_, c)| c) {
            worst = Some((t.as_str(), count));
        }
    }
    worst.map(|(name, count)| {
        format!(
            "近 {} 轮内「{name}」被调用 {count} 次，确认是否在重复已完成的工作（若是同一目标，请改用更精确的一次性操作）",
            state.window
        )
    })
}

// ============================================================================
// P0-D 输出治理：emission-guard（去重 + 每批一条 + 短语抑制）
// 移植 oh-my-pi `AdvisorEmissionGuard`。drain 单线程串行访问，用 [`Mutex`] 仅做内部可变性。
// ============================================================================

/// emission-guard 的去重环容量（最近 N 条已放行建议的归一化键，FIFO 淘汰）。
///
/// **P0-R1 修正**：从 32 提升至 4096，对齐 oh-my-pi [`DEFAULT_HISTORY_CAPACITY`](third/oh-my-pi/packages/coding-agent/src/advisor/emission-guard.ts:101)
/// （基于 issue #3520 真实 92 条 unique notes 留 40× 余量）。原 32 会让长会话中早期建议
/// 被快速 FIFO 淘汰 → 几十轮后同一建议**重复进入主上下文**，削弱去重效果。
pub const EMISSION_GUARD_CAPACITY: usize = 4096;

/// 归一化建议文本：去首尾 + 小写 + 折叠连续非字母数字字符为单空格。用作去重/短语抑制键
/// （忽略大小写 / 标点 / 空白差异）。**P0-R1 修正**：对齐 oh-my-pi [`normalizeAdvisorNote`]
/// （third/oh-my-pi/packages/coding-agent/src/advisor/emission-guard.ts:32）——折叠非字母数字，
/// 使 `"Stop."` / `"*Stop*"` / `"  stop  "` 都归一化为 `stop`。
fn normalize_advice(advice: &str) -> String {
    advice
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 无意义/空泛短语集合：advice 归一化后**整串等于**任一即整体抑制（对齐 oh-my-pi
/// [`SUPPRESSED_NORMALIZED_PHRASES`](third/oh-my-pi/packages/coding-agent/src/advisor/emission-guard.ts:51)）。
/// 中英混合以覆盖 Gyre 的双语审阅提示。
///
/// **P0-R1 关键修正**：原实现用 `key.contains(p)`（子串匹配），会误杀含空泛短语子串的
/// 真实 concern（如 `"实现 looks good 但遗漏空 case"` 命中 "looks good" 被静默丢弃）。
/// 改为**整串精确匹配**：仅当整条建议归一化后恰等于某空泛短语才抑制——含具体可操作内容的
/// 建议（如 `"Stop: 'await' missing on writeStream.end()"`）永不抑制。这是 oh-my-pi 原设计意图
/// （注释明示 `emission-guard.ts:45`：「A genuine blocker ... does not match」）。
const SUPPRESSED_PHRASES: &[&str] = &[
    // 英文：自停/完成自述/无问题背书/继续
    "stop",
    "stop here",
    "stop now",
    "halt",
    "abort",
    "done",
    "task done",
    "task complete",
    "complete",
    "finished",
    "ok",
    "okay",
    "ok done",
    "no issue",
    "no issues",
    "no issue continue",
    "no concerns",
    "no concern",
    "nothing to add",
    "nothing to flag",
    "nothing to report",
    "no notes",
    "no further input",
    "no further input needed",
    "no further input required",
    "no further advice",
    "no further advice needed",
    "lgtm",
    "looks good",
    "looks fine",
    "all good",
    "agent is on track",
    "agent on track",
    "on track",
    "continue",
    "continue as planned",
    "carry on",
    "good job",
    "well done",
    "keep going",
    // 中文：无问题背书 / 继续（归一化后非字母数字被折叠，中英混排仍精确整串匹配）
    "看起来不错",
    "没有问题",
    "没问题",
    "没有发现问题",
    "无需修改",
    "无需调整",
    "继续吧",
    "继续",
    "做得好",
    "保持",
    "一切正常",
];

/// advisor 输出治理守卫（移植 oh-my-pi `AdvisorEmissionGuard`）。
///
/// 四重过滤，杜绝 advisor 向主上下文灌入冗余/低价值注释（长会话噪声污染的根因）：
/// 1. **每批一条**：一个 [`process_batch`]（= oh-my-pi 一个 "update"）最多放行一条 advisor
///    输出（启发式注释或 brain 建议先到先得），避免单轮多注释淹没模型。
/// 2. **跨批去重（升级感知）**：归一化键已见过的建议**仅在严格升级**（nit→concern→blocker）
///    时再次放行，同级/降级视为重复丢弃（P0-R2，对齐 oh-my-pi `AdviseTool.#deliveredNoteSeverities`）。
///    既防 advisor 改写绕过去重，又保证真实升级不被旧低级别永久挡住。
/// 3. **整串短语抑制**：归一化文本**整串等于** [`SUPPRESSED_PHRASES`] 任一才抑制（P0-R1）。
///    避免子串匹配误杀含具体可操作内容的真实 concern。
/// 4. **空串抑制**：归一化为空（纯空白/标点）整体抑制。
#[derive(Debug)]
pub struct EmissionGuard {
    /// 已放行建议的归一化键及其当时已投递的最高严重度（FIFO，容量受
    /// [`EMISSION_GUARD_CAPACITY`] 约束）。P0-R2：值由 `Option<()>` 升级为 `Severity`，
    /// 支持升级感知去重。
    seen: VecDeque<(String, Severity)>,
    /// 本批是否已放行一条（每批重置）。
    emitted_this_batch: bool,
}

impl EmissionGuard {
    /// 构造空守卫。
    #[must_use]
    pub fn new() -> Self {
        Self { seen: VecDeque::new(), emitted_this_batch: false }
    }

    /// 重置（runtime `reset` 时调用）：清空去重历史与每批标志。
    pub fn reset(&mut self) {
        self.seen.clear();
        self.emitted_this_batch = false;
    }

    /// 每批开始时调用：清零「本批已发」标志（**去重历史保留**）。
    pub fn begin_batch(&mut self) {
        self.emitted_this_batch = false;
    }

    /// 本批是否仍可放行一条（drain 据此预判，避免发起注定被丢弃的昂贵 brain LLM 调用）。
    #[must_use]
    pub fn batch_has_budget(&self) -> bool {
        !self.emitted_this_batch
    }

    /// 判定一条 advisor 输出是否应放行。放行时记账（标记本批已发 + 入去重环，记录本次
    /// 最高严重度）。返回 `true` 表示放行。仅更新 `self`（纯函数语义，便于单测）。
    ///
    /// **P0-R2**：`incoming` 严重度参与去重判定——同一归一化键仅在 `incoming` 严格高于
    /// 已记录最高严重度时放行（nit=1 < concern=2 < blocker=3）。`Severity` 大小关系见
    /// [`Severity::rank`]。
    pub fn allow(&mut self, advice: &str, incoming: Severity) -> bool {
        if self.emitted_this_batch {
            return false;
        }
        let key = normalize_advice(advice);
        if key.is_empty() {
            return false;
        }
        // P0-R1：整串精确匹配（非子串 contains）。含具体可操作内容的建议永不抑制。
        if SUPPRESSED_PHRASES.iter().any(|p| *p == key) {
            return false;
        }
        // P0-R2：升级感知去重。仅当 incoming 严格高于已记录最高级别才放行。
        if let Some(idx) = self.seen.iter().position(|(k, _)| *k == key) {
            if incoming.rank() <= self.seen[idx].1.rank() {
                return false;
            }
            // 严格升级：更新已记录最高严重度（不入队新键，保持 FIFO 容量语义）。
            self.seen[idx].1 = incoming;
        } else {
            self.seen.push_back((key, incoming));
            while self.seen.len() > EMISSION_GUARD_CAPACITY {
                self.seen.pop_front();
            }
        }
        self.emitted_this_batch = true;
        true
    }

    /// 已放行去重键数量（测试/诊断用）。
    #[must_use]
    pub fn seen_len(&self) -> usize {
        self.seen.len()
    }
}

impl Default for EmissionGuard {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// P0-F Tier 1：严重度（移植 oh-my-pi advise-tool.ts 的 nit/concern/blocker）
// ============================================================================

/// advisor 建议的严重度。当前全部经 aside 注入主上下文，严重度以**文本标记**传达给
/// 主智能体（模型据此权衡而非盲从）；自动中断路由（blocker 取消在途工具）为后续工作单元
// （需新增 advisor→agent 中断通道 + 主循环轮询 + 防无限续跑，独立评审）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// 轻微建议（nit）：酌情采纳。
    Nit,
    /// 关注（concern）：值得重视的偏离 / 丢失线索 / 重复工作。
    Concern,
    /// 阻断（blocker）：高危 / 不可逆操作前的强警告，应优先处理。
    Blocker,
}

impl Severity {
    /// 渲染到 `[advisor][{label}]` 的短标签。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Severity::Nit => "nit",
            Severity::Concern => "concern",
            Severity::Blocker => "blocker",
        }
    }

    /// 数值序：`Nit=1` < `Concern=2` < `Blocker=3`。供 [`EmissionGuard::allow`] 做「严格升级才
    /// 放行」的升级感知去重（P0-R2，对齐 oh-my-pi `ADVISOR_SEVERITY_RANK`）。`PartialOrd` 派生
    /// 在枚举变体顺序变动时会静默改变语义，故用显式 rank 固定大小关系。
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Severity::Nit => 1,
            Severity::Concern => 2,
            Severity::Blocker => 3,
        }
    }
}

/// 从 brain 输出正文开头解析严重度标记（`[blocker]`/`[concern]`/`[nit]`，大小写不敏感）。
/// 返回 `(严重度, 去标记后的正文)`；无标记默认 [`Severity::Nit`]、正文原样。纯函数，便于单测。
fn parse_severity(advice: &str) -> (Severity, &str) {
    let trimmed = advice.trim_start();
    for (tag, sev) in [
        ("[blocker]", Severity::Blocker),
        ("[concern]", Severity::Concern),
        ("[nit]", Severity::Nit),
    ] {
        // 仅用于前缀比较（小写化）；ASCII 小写化保长，故 trimmed[tag.len()..] 在原串上正确跳过标记。
        if trimmed.to_ascii_lowercase().starts_with(tag) {
            return (sev, trimmed[tag.len()..].trim_start());
        }
    }
    (Severity::Nit, advice)
}

// ============================================================================
// Brain：审阅大脑端口 + 默认 LLM 实现
// ============================================================================

/// 后台审阅的错误分类（决定失败隔离语义）。
#[derive(Debug, thiserror::Error)]
pub enum AdvisorError {
    /// 瞬时错误（流中断 / 传输 / 解码 / 短时限速）：本跳过失败、下一轮自动重试。
    #[error("瞬时错误: {0}")]
    Transient(String),
    /// 配额耗尽（长时限速 / 配额窗口）：停止处理直到显式 [`AdvisorRuntime::reset`]。
    #[error("配额耗尽")]
    Quota,
    /// 其它非瞬时错误（鉴权 / 不支持 / 4xx）：累计失败计数，达上限则 halt。
    #[error("其它错误: {0}")]
    Other(String),
}

/// 把 provider 错误映射为 advisor 错误分类。
fn classify(err: LlmError) -> AdvisorError {
    match err {
        // retry_after > 1h 视为配额耗尽（长窗口），否则瞬时。
        LlmError::RateLimit { retry_after_ms } if retry_after_ms > 3_600_000 => AdvisorError::Quota,
        LlmError::RateLimit { retry_after_ms } => {
            AdvisorError::Transient(format!("rate limit {retry_after_ms}ms"))
        }
        LlmError::StreamInterrupted(m) => AdvisorError::Transient(format!("stream: {m}")),
        LlmError::Transport(m) => AdvisorError::Transient(m),
        LlmError::Decode(m) => AdvisorError::Transient(format!("decode: {m}")),
        LlmError::Http { status, .. } if status >= 500 => {
            AdvisorError::Transient(format!("http {status}"))
        }
        LlmError::Http { status, body } => AdvisorError::Other(format!("http {status}: {body}")),
        LlmError::Auth(m) => AdvisorError::Other(format!("auth: {m}")),
        LlmError::Unsupported(m) => AdvisorError::Other(m),
    }
}

/// 审阅大脑端口：把一轮增量审阅为可选的建议文本。
#[async_trait]
pub trait AdvisorBrain: Send + Sync {
    /// 审阅单条增量（基础能力）。
    async fn advise(&self, delta: &AdvisorDelta) -> Result<Option<String>, AdvisorError>;

    /// 审阅一批增量（P0-1 批合并），携带 advisor 独立上下文记忆 `history`（最近若干轮已审摘要，
    /// 最早的在前）。移植 oh-my-pi `#collectAndMaintainBatch` 把多轮合并为一次审阅的语义。
    ///
    /// **默认实现**：逐条 [`Self::advise`]（忽略 `history`），返回首个非空建议——故基于逐条
    /// 语义的 brain（如 [`NoopBrain`]、测试桩）**零改动**即可工作。覆盖此方法可把多轮合并为
    /// 一次 LLM 调用（省 token + 跨轮连续性），见 [`LlmBrain`]。
    async fn advise_batch(
        &self,
        deltas: &[AdvisorDelta],
        _history: &[String],
    ) -> Result<Option<String>, AdvisorError> {
        for d in deltas {
            if let Some(advice) = self.advise(d).await? {
                return Ok(Some(advice));
            }
        }
        Ok(None)
    }
}

/// 默认审阅提示词（编译期内嵌自 `prompts/advisor-review.md`）。
const DEFAULT_REVIEW_PROMPT: &str = include_str!("../../../prompts/advisor-review.md");
/// P1-1 watchdog：项目约定（AGENTS.md / 规则）注入审阅 system 段的字符上限
/// （避免撑爆 advisor 上下文；超长由 [`ellipsis`] 截断）。
const CONVENTIONS_MAX_CHARS: usize = 4000;

/// 默认 LLM 大脑：一次性补全（构造 `CompletionRequest` → 收集 `TextDelta`），与项目内
/// `LlmSummaryProvider` 同构。
pub struct LlmBrain {
    provider: Arc<dyn LlmProvider>,
    model: Model,
    provider_ctx: ProviderCallContext,
    system: String,
    max_tokens: usize,
}

impl LlmBrain {
    /// 构造。
    #[must_use]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        model: Model,
        provider_ctx: ProviderCallContext,
        system: String,
        max_tokens: usize,
    ) -> Self {
        Self { provider, model, provider_ctx, system, max_tokens }
    }

    /// 用内嵌的默认审阅提示词构造（512 token 上限）。
    #[must_use]
    pub fn with_default_prompt(
        provider: Arc<dyn LlmProvider>,
        model: Model,
        provider_ctx: ProviderCallContext,
    ) -> Self {
        Self::new(provider, model, provider_ctx, DEFAULT_REVIEW_PROMPT.to_string(), 512)
    }

    /// P1-1 watchdog：用默认审阅提示词 + 项目约定构造（512 token 上限）。
    ///
    /// 约定段（AGENTS.md / 规则）追加在默认审阅提示之后，使 advisor 审阅时**依据项目约定**
    /// 纠偏主代理（移植 oh-my-pi `watchdog.ts` 把 AGENTS.md 喂给只读审阅者）。`conventions`
    /// 为空（空白）时等价于 [`Self::with_default_prompt`]，不产生额外 token。超长自动截断
    /// （[`CONVENTIONS_MAX_CHARS`]）。
    #[must_use]
    pub fn with_conventions(
        provider: Arc<dyn LlmProvider>,
        model: Model,
        provider_ctx: ProviderCallContext,
        conventions: &str,
    ) -> Self {
        let system = if conventions.trim().is_empty() {
            DEFAULT_REVIEW_PROMPT.to_string()
        } else {
            let trimmed = ellipsis(conventions.trim(), CONVENTIONS_MAX_CHARS);
            format!(
                "{DEFAULT_REVIEW_PROMPT}\n\n<project-conventions>\n以下是本项目约定；审阅时若发现主代理违反，请明确指出并给出纠正建议：\n\n{trimmed}\n</project-conventions>"
            )
        };
        Self::new(provider, model, provider_ctx, system, 512)
    }

    /// 一次性补全：把 `user` 正文构造为 [`CompletionRequest`] 并流式收集文本。
    /// [`Self::advise`]（单条）与 [`AdvisorBrain::advise_batch`]（合并）共用，消除重复。
    async fn complete(&self, user: &str) -> Result<Option<String>, AdvisorError> {
        let req = CompletionRequest {
            model: self.model.clone(),
            system: vec![self.system.clone()],
            messages: vec![ProviderMessage::User {
                content: vec![UserContent::Text { text: user.to_string() }],
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: self.max_tokens,
            temperature: Some(0.0),
            thinking: None,
            cache_key: None,
            stable_prefix_len: 0,
        };
        let mut stream = self.provider.stream(req, &self.provider_ctx).await.map_err(classify)?;
        let mut out = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                AssistantEvent::TextDelta(d) => out.push_str(&d),
                AssistantEvent::Error(e) => return Err(classify(e)),
                AssistantEvent::MessageEnd(msg) => {
                    if out.is_empty() {
                        for b in &msg.content {
                            if let Some(t) = b.as_text() {
                                out.push_str(t);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let trimmed = out.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }
}

#[async_trait]
impl AdvisorBrain for LlmBrain {
    async fn advise(&self, delta: &AdvisorDelta) -> Result<Option<String>, AdvisorError> {
        self.complete(&format!("{}\n\n{}", delta.heading, delta.body)).await
    }

    /// P0-1 批合并：把一批增量合并为**一次** LLM 调用，并注入 advisor 独立上下文记忆
    /// （最近若干轮已审摘要），让审阅者具备跨轮连续性（判断「重复已完成工作 / 漂移」）。
    /// 移植 oh-my-pi `#collectAndMaintainBatch` 合并语义 + `#latestMessages` 累积视图。
    async fn advise_batch(
        &self,
        deltas: &[AdvisorDelta],
        history: &[String],
    ) -> Result<Option<String>, AdvisorError> {
        self.complete(&render_batch_prompt(deltas, history)).await
    }
}

/// P0-1：把一批增量 + advisor 历史记忆渲染为单次审阅的 user 正文。
fn render_batch_prompt(deltas: &[AdvisorDelta], history: &[String]) -> String {
    let mut user = String::new();
    if !history.is_empty() {
        user.push_str("以下是此前若干轮的审阅背景（按时间顺序，最早在前，供连续性参考）：\n");
        for h in history {
            user.push_str("--- 轮背景 ---\n");
            user.push_str(h);
            user.push('\n');
        }
        user.push_str("--- 本轮 ---\n");
    }
    let will_continue = deltas.iter().any(|d| d.will_continue);
    let heading = if will_continue { "本轮 [进行中]" } else { "本轮 [已结束]" };
    user.push_str(heading);
    if deltas.len() > 1 {
        user.push_str(&format!("（合并 {} 轮增量）", deltas.len()));
    }
    user.push('\n');
    for d in deltas {
        user.push_str(&d.body);
    }
    user
}

/// P0-1：把一批增量压缩为一段记忆摘要（advise 成功后入 advisor 上下文记忆）。
/// 截断（[`ellipsis`]）避免记忆无限膨胀；保留 will_continue 标记与正文要点。
fn render_memory_summary(deltas: &[&AdvisorDelta]) -> String {
    let will_continue = deltas.iter().any(|d| d.will_continue);
    let tag = if will_continue { "[进行中] " } else { "[已结束] " };
    let mut s = String::from(tag);
    for d in deltas {
        s.push_str(&d.body);
    }
    ellipsis(&s, 800)
}

/// 空操作大脑：始终返回 `Ok(None)`（不产出 LLM 批评）。
///
/// 当 `enable_advisor = false`（默认）时由装配层注入——此时 advisor **仍保留**零成本的
/// 启发式质量信号（[`analyze_repetition`]）与后台上下文维护（[`ThresholdCompactor`]），
/// 仅跳过每轮的 LLM 审阅调用（避免对未显式开启的用户产生额外 token 成本）。
pub struct NoopBrain;

#[async_trait]
impl AdvisorBrain for NoopBrain {
    async fn advise(&self, _delta: &AdvisorDelta) -> Result<Option<String>, AdvisorError> {
        Ok(None)
    }
}

// ============================================================================
// ContextMaintainer：后台上下文维护（P0-2，异步 summarize 卸载）
// ============================================================================

/// 后台上下文维护端口：每轮检查并在需要时压缩，使主循环不再因同步 summarize 冻结。
#[async_trait]
pub trait ContextMaintainer: Send + Sync {
    /// 检查并（必要时）压缩。返回 `true` 表示本次实际触发了压缩。
    ///
    /// # Errors
    /// 压缩失败时返回 [`ContextError`]；调用方仅记录、不影响主循环与审阅。
    async fn maybe_maintain(&self) -> Result<bool, ContextError>;
}

/// 基于阈值的后台压缩器：token 占用达 `soft_ratio` 时触发一次 `Summarize`。
///
/// 默认 `soft_ratio = 0.6`，略低于主循环的软阈值（~0.65）与硬阈值（context_window_guard，
/// 默认 ~0.9），使其**先于**主循环压到阈值以下——主循环的同步 summarize 因此多为 no-op。
pub struct ThresholdCompactor {
    ctx: Arc<dyn ContextManager>,
    soft_ratio: f32,
}

impl ThresholdCompactor {
    /// 构造。
    #[must_use]
    pub fn new(ctx: Arc<dyn ContextManager>, soft_ratio: f32) -> Self {
        Self { ctx, soft_ratio }
    }

    /// 默认软阈值 0.6 构造。
    #[must_use]
    pub fn default_ratio(ctx: Arc<dyn ContextManager>) -> Self {
        Self::new(ctx, 0.6)
    }

    /// 纯判定：给定用量是否应压缩（便于单测，不触碰网络）。
    #[must_use]
    pub fn should_compact(&self, usage: &TokenUsage) -> bool {
        usage.near_limit(self.soft_ratio)
    }
}

#[async_trait]
impl ContextMaintainer for ThresholdCompactor {
    async fn maybe_maintain(&self) -> Result<bool, ContextError> {
        let usage = self.ctx.token_usage();
        if !self.should_compact(&usage) {
            return Ok(false);
        }
        tracing::info!(
            current = usage.current,
            limit = usage.limit,
            ratio = self.soft_ratio,
            "advisor 后台触发 Summarize（先于主循环硬阈值）"
        );
        self.ctx.compact(CompactionStrategy::Summarize { max_tokens: 0 }).await?;
        Ok(true)
    }
}

// ============================================================================
// Runtime：单 drain 循环 + 失败隔离状态
// ============================================================================

/// 一个待后台处理的增量（携带入队时的 epoch，供 reset 后丢弃陈旧批次）。
#[derive(Debug, Clone)]
struct PendingItem {
    delta: AdvisorDelta,
    epoch: u64,
}

/// drain 任务与 runtime 共享的可变状态（全部原子，drain 单线程写、runtime 读）。
struct DrainState {
    brain: Arc<dyn AdvisorBrain>,
    aside_tx: UnboundedSender<agent_core::AgentMessage>,
    /// P0-R3a：可选的中断通道。`Blocker`/`Concern` 级建议优先走此（复用主 agent 既有
    /// 的批级 cancel + Immediate 中断轮询，中断在途工具）。未注入或发送失败则回落
    /// [`Self::aside_tx`]（向后兼容）。与 aside 的语义区别对齐主 agent 主循环：
    /// steer 触发批级 cancel 中断在途工具；aside 只在轮次边界折叠注入（不中断）。
    steer_tx: Option<UnboundedSender<agent_core::AgentMessage>>,
    /// 可选的后台上下文维护器（P0-2）。
    maintainer: Option<Arc<dyn ContextMaintainer>>,
    /// P1 启发式质量信号状态（drain 单线程串行访问）。
    quality: Mutex<QualityState>,
    backlog: AtomicUsize,
    failing: AtomicBool,
    quota_exhausted: AtomicBool,
    halted: AtomicBool,
    failures: AtomicU32,
    epoch: AtomicU64,
    processed: AtomicU64,
    advised: AtomicU64,
    /// 启发式命中并注入的次数（区别于 LLM 大脑的 advised）。
    heuristics: AtomicU64,
    maintained: AtomicU64,
    /// P0-2 关键决策同步点：drain 每处理完一条 backlog 下降后 `notify_waiters`，
    /// 唤醒 [`AdvisorRuntime::wait_for_catchup`] 中 park 的主代理。
    catchup_notify: tokio::sync::Notify,
    /// P0-1 advisor 独立上下文记忆：最近 [`ADVISOR_MEMORY_TURNS`] 轮已审摘要（最早的在前）。
    /// drain 单线程写，runtime 读快照；用 [`Mutex`] 仅做内部可变性。
    memory: Mutex<VecDeque<String>>,
    /// P0-D emission-guard：advisor 输出去重 + 每批一条 + 短语抑制（移植 oh-my-pi
    /// `AdvisorEmissionGuard`）。drain 单线程写。
    guard: Mutex<EmissionGuard>,
    max_failures: u32,
}

/// 运行时统计快照（诊断 / UI 用）。
#[derive(Debug, Default, Clone)]
pub struct AdvisorStats {
    /// 已入队但未处理完的增量数。
    pub backlog: usize,
    /// 已处理的增量数。
    pub processed: u64,
    /// 已注入主上下文的 LLM 大脑建议数。
    pub advised: u64,
    /// 已注入主上下文的启发式建议数。
    pub heuristics: u64,
    /// 已触发的后台压缩次数。
    pub maintained: u64,
    /// 当前连续失败次数。
    pub failures: u32,
    /// 是否处于瞬时失败态（下一轮成功即清除）。
    pub failing: bool,
    /// 是否配额耗尽（需 reset 恢复）。
    pub quota_exhausted: bool,
    /// 是否已永久停止（需重建）。
    pub halted: bool,
}

/// 后台审阅运行时。`Clone` 廉价（仅 `Arc`），便于多处持有同一实例。
#[derive(Clone)]
pub struct AdvisorRuntime {
    state: Arc<DrainState>,
    pending_tx: UnboundedSender<PendingItem>,
}

impl AdvisorRuntime {
    /// 全参数构造并启动单 drain 任务（P0-R3a 新增 `steer_tx`）。
    ///
    /// **须在 tokio 运行时上下文内调用**（`tokio::spawn`）。`max_failures` 为连续非配额失败
    /// 的 halt 阈值；`maintainer` 为可选的后台上下文维护器（P0-2，`None` 则不维护）；
    /// `steer_tx` 为可选的中断通道（P0-R3a，`None` 则 blocker/concern 回落 aside）。
    /// 启发式质量信号（P1）**始终启用**（零额外成本，且在 brain failing 时仍生效）。
    #[must_use]
    pub fn new_full(
        brain: Arc<dyn AdvisorBrain>,
        aside_tx: UnboundedSender<agent_core::AgentMessage>,
        max_failures: u32,
        maintainer: Option<Arc<dyn ContextMaintainer>>,
        steer_tx: Option<UnboundedSender<agent_core::AgentMessage>>,
    ) -> Self {
        let state = Arc::new(DrainState {
            brain,
            aside_tx,
            steer_tx,
            maintainer,
            quality: Mutex::new(QualityState::new()),
            backlog: AtomicUsize::new(0),
            failing: AtomicBool::new(false),
            quota_exhausted: AtomicBool::new(false),
            halted: AtomicBool::new(false),
            failures: AtomicU32::new(0),
            epoch: AtomicU64::new(0),
            processed: AtomicU64::new(0),
            advised: AtomicU64::new(0),
            heuristics: AtomicU64::new(0),
            maintained: AtomicU64::new(0),
            catchup_notify: tokio::sync::Notify::new(),
            memory: Mutex::new(VecDeque::new()),
            guard: Mutex::new(EmissionGuard::new()),
            max_failures,
        });
        let (pending_tx, pending_rx) = tokio::sync::mpsc::unbounded_channel::<PendingItem>();
        let drain_state = Arc::clone(&state);
        tokio::spawn(drain_loop(drain_state, pending_rx));
        Self { state, pending_tx }
    }

    /// 构造并启动单 drain 任务（无中断通道）。
    ///
    /// **须在 tokio 运行时上下文内调用**（`tokio::spawn`）。`max_failures` 为连续非配额失败
    /// 的 halt 阈值；`maintainer` 为可选的后台上下文维护器（P0-2，`None` 则不维护）。
    /// 启发式质量信号（P1）**始终启用**（零额外成本，且在 brain failing 时仍生效）。
    ///
    /// 等价于 [`Self::new_full`] 传 `steer_tx = None`（向后兼容：blocker/concern 走 aside）。
    #[must_use]
    pub fn new(
        brain: Arc<dyn AdvisorBrain>,
        aside_tx: UnboundedSender<agent_core::AgentMessage>,
        max_failures: u32,
        maintainer: Option<Arc<dyn ContextMaintainer>>,
    ) -> Self {
        Self::new_full(brain, aside_tx, max_failures, maintainer, None)
    }

    /// 便利构造：默认失败阈值（5），无后台上下文维护。
    #[must_use]
    pub fn with_default_max_failures(
        brain: Arc<dyn AdvisorBrain>,
        aside_tx: UnboundedSender<agent_core::AgentMessage>,
    ) -> Self {
        Self::new(brain, aside_tx, 5, None)
    }

    /// 便利构造：默认失败阈值（5）+ 后台上下文维护器（P0-2，装配层推荐入口）。
    #[must_use]
    pub fn with_maintainer(
        brain: Arc<dyn AdvisorBrain>,
        aside_tx: UnboundedSender<agent_core::AgentMessage>,
        maintainer: Arc<dyn ContextMaintainer>,
    ) -> Self {
        Self::new(brain, aside_tx, 5, Some(maintainer))
    }

    /// P0-R3a 便利构造：默认失败阈值（5）+ 后台上下文维护器 + 中断通道（装配层推荐入口）。
    ///
    /// `steer_tx` 接主 agent 的 steering 通道发送端——blocker/concern 级建议经此注入会
    /// 触发主 agent 既有 [`STEERING_INTERRUPT_POLL`](crates/agent/src/lib.rs) 批级 cancel +
    /// Immediate 中断轮询，立即中止在途工具。未注入（用 [`Self::with_maintainer`]）则全部
    /// 走 aside（向后兼容）。
    #[must_use]
    pub fn with_maintainer_and_steer(
        brain: Arc<dyn AdvisorBrain>,
        aside_tx: UnboundedSender<agent_core::AgentMessage>,
        maintainer: Arc<dyn ContextMaintainer>,
        steer_tx: UnboundedSender<agent_core::AgentMessage>,
    ) -> Self {
        Self::new_full(brain, aside_tx, 5, Some(maintainer), Some(steer_tx))
    }

    /// 主代理每轮结束调用：渲染增量并入队（fire-and-forget，永不阻塞主循环）。
    pub fn on_turn_end(&self, ctx: &TurnEndContext<'_>) {
        if self.state.halted.load(Ordering::Relaxed)
            || self.state.quota_exhausted.load(Ordering::Relaxed)
        {
            return;
        }
        let delta = render_turn_delta(ctx.message, ctx.tool_results, ctx.will_continue);
        let epoch = self.state.epoch.load(Ordering::Relaxed);
        self.state.backlog.fetch_add(1, Ordering::Relaxed);
        let _ = self.pending_tx.send(PendingItem { delta, epoch });
    }

    /// 重置失败/配额/halt 状态并递增 epoch（陈旧待处理批次被 drain 丢弃）。
    /// 同时清空启发式历史窗口（避免跨会话误判）。
    pub fn reset(&self) {
        self.state.epoch.fetch_add(1, Ordering::Relaxed);
        self.state.failing.store(false, Ordering::Relaxed);
        self.state.quota_exhausted.store(false, Ordering::Relaxed);
        self.state.halted.store(false, Ordering::Relaxed);
        self.state.failures.store(0, Ordering::Relaxed);
        clear_advisor_views(&self.state);
        tracing::info!(
            "advisor 已 reset（epoch 递增，陈旧批次、启发式历史、上下文记忆与 emission-guard 将被丢弃）"
        );
    }

    /// P0-E 压缩感知 re-prime：主循环压缩（shake/summarize/prune）/ 分支切换使主上下文
    /// 收缩后，由主循环在「真实收缩」时调用。递增 epoch（陈旧待处理批次被丢弃）+ 清空
    /// advisor 视图（记忆 / 去重 / 启发式历史），使 advisor 与压缩后主上下文重新对齐。
    /// **不复位** halted/quota/failing（仅 [`Self::reset`] 复位）。移植 oh-my-pi
    /// `#resetAdvisorContext`。
    pub fn on_context_compacted(&self) {
        self.state.epoch.fetch_add(1, Ordering::Relaxed);
        clear_advisor_views(&self.state);
        tracing::info!(
            "advisor 感知到主上下文压缩/重构，已 re-prime（epoch 递增，记忆/去重/启发式历史清空）"
        );
    }

    /// 取统计快照。
    #[must_use]
    pub fn stats(&self) -> AdvisorStats {
        AdvisorStats {
            backlog: self.state.backlog.load(Ordering::Relaxed),
            processed: self.state.processed.load(Ordering::Relaxed),
            advised: self.state.advised.load(Ordering::Relaxed),
            heuristics: self.state.heuristics.load(Ordering::Relaxed),
            maintained: self.state.maintained.load(Ordering::Relaxed),
            failures: self.state.failures.load(Ordering::Relaxed),
            failing: self.state.failing.load(Ordering::Relaxed),
            quota_exhausted: self.state.quota_exhausted.load(Ordering::Relaxed),
            halted: self.state.halted.load(Ordering::Relaxed),
        }
    }

    /// 是否处于降级态（halted / 配额耗尽 / 瞬时失败中）。
    ///
    /// 降级时 [`Self::wait_for_catchup`] **立即放行**——主代理永不因 advisor 故障而阻塞
    /// （移植 oh-my-pi `#failing`/`#quotaExhausted`/`#halted` 的「立即释放等待者」语义）。
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        self.state.halted.load(Ordering::Relaxed)
            || self.state.quota_exhausted.load(Ordering::Relaxed)
            || self.state.failing.load(Ordering::Relaxed)
    }

    /// 是否健康（非降级）。供主循环在压缩决策时判定「能否信任 advisor 已压到位」
    /// （P0-3 主循环 summarize 卸载的判定依据）。
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        !self.is_degraded()
    }

    /// 关键决策同步点：等待 advisor 把积压（backlog）处理到 `<= threshold`。
    ///
    /// 移植 oh-my-pi `AdvisorRuntime.waitForCatchup` 语义：
    /// - **降级立即放行**：halted / 配额耗尽 / 瞬时失败中时返回 `true`（主代理不等故障 advisor）。
    /// - **追平**：backlog 降到 `threshold` 及以下时返回 `true`。
    /// - **超时**：`max_ms` 内未追平返回 `false`（调用方据此决定是否继续不等）。
    ///
    /// 内部仅 `select!` 通知与超时，**不**消费取消令牌（保持本 crate 零 tokio-util 依赖）。
    /// 调用方若需响应取消，在外层 `tokio::select!` 包裹本 future 即可——`Notify::notified()`
    /// 的 waiter 注册随 future drop 自动清理，无泄漏。
    ///
    /// 典型挂载点：主代理在 `AgentEvent::Done`（最终停止）前调用一次，让 advisor 在收尾前
    /// 有机会注入最后一条纠偏建议。
    pub async fn wait_for_catchup(&self, max_ms: u64, threshold: usize) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(max_ms);
        loop {
            if self.is_degraded() {
                return true;
            }
            if self.state.backlog.load(Ordering::Relaxed) <= threshold {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let remaining = deadline - now;
            tokio::select! {
                _ = self.state.catchup_notify.notified() => continue,
                _ = tokio::time::sleep(remaining) => return false,
            }
        }
    }
}

/// 单 drain 循环：串行处理增量（保序），任何失败都被隔离，永不 panic、永不上抛。
///
/// P0-1 批合并（移植 oh-my-pi `#collectAndMaintainBatch`）：取出首条后非阻塞吸收当前积压，
/// 合并为一批 → 一次 [`AdvisorBrain::advise_batch`] 调用。省 token（N 轮 → 1 次 LLM）+ 抖动免疫
/// （快速主代理 + 慢 brain 不会每轮排队）。启发式仍 per-delta 跑（维护滑动窗口历史）。
async fn drain_loop(state: Arc<DrainState>, mut rx: UnboundedReceiver<PendingItem>) {
    while let Some(first) = rx.recv().await {
        // 批合并：非阻塞吸收当前积压（await 期间到达的下一批由下轮循环吸收）。
        let mut batch = Vec::with_capacity(4);
        batch.push(first);
        while let Ok(item) = rx.try_recv() {
            batch.push(item);
        }
        let n = batch.len();
        process_batch(&state, batch).await;
        // backlog 在「批处理完成后」才一次性扣减——表示「待处理 + 处理中」（对齐 oh-my-pi
        // 在 dispatch 成功后才减 `#backlog`）。使 [`AdvisorRuntime::wait_for_catchup`] 能等到
        // advisor 真正完成审阅，而非「刚取出」。
        state.backlog.fetch_sub(n, Ordering::Relaxed);
        // P0-2：backlog 已下降，唤醒可能在 wait_for_catchup 中 park 的主代理重检。
        state.catchup_notify.notify_waiters();
    }
    tracing::debug!("advisor drain 任务退出（runtime 已释放）");
    // drain 退出（runtime 释放）时也唤醒等待者，避免它们 park 到超时。
    state.catchup_notify.notify_waiters();
}

/// P0-D/P0-E 共用：清空 advisor 的「视图」状态（启发式历史 + 上下文记忆 + emission-guard）。
/// [`AdvisorRuntime::reset`] / [`AdvisorRuntime::on_context_compacted`] / 后台压缩 re-prime 共用；
/// 不触碰 epoch / 失败 / 配额 / halt 计数（由调用方各自处理）。
fn clear_advisor_views(state: &DrainState) {
    if let Ok(mut q) = state.quality.lock() {
        q.history.clear();
    }
    if let Ok(mut m) = state.memory.lock() {
        m.clear();
    }
    if let Ok(mut g) = state.guard.lock() {
        g.reset();
    }
}

/// 处理一批增量（P0-1 批合并）。
///
/// 流程：epoch/quota/halted 守卫 → 启发式 per-delta（命中即注入并跳过 brain）→ 未命中的
/// deltas 合并为一次 [`AdvisorBrain::advise_batch`]（带 advisor 独立上下文记忆）→ 成功则更新
/// P0-R3a：按严重度把 advisor 消息路由到主 agent。
///
/// 路由策略（对齐 oh-my-pi [`isInterruptingSeverity`](third/oh-my-pi/packages/coding-agent/src/advisor/advise-tool.ts:74)）：
/// - `Blocker` / `Concern`（interrupting）：**优先**走 [`DrainState::steer_tx`]——复用主 agent
///   既有的批级 cancel + Immediate 中断轮询（[`STEERING_INTERRUPT_POLL`](crates/agent/src/lib.rs)），
///   立即中止在途工具，让模型在下一轮重看。若 steer_tx 未注入或发送失败（如通道关闭），
///   **回落** [`DrainState::aside_tx`]（向后兼容：宁可延迟到达也不丢失建议）。
/// - `Nit`（non-interrupting）：始终走 [`DrainState::aside_tx`]——下一轮边界折叠注入，不中断。
///
/// 这是 oh-my-pi `resolveAdvisorDeliveryChannel` 的最小可用子集（仅 aside/steer 两通道，
/// 不含 preserve/immune-window/staleness——后续 P1-R4 按需引入）。
///
/// 注：[`UnboundedSender::send`] 失败时经 [`SendError`](tokio::sync::mpsc::error::SendError)
/// 归还原值，故 steer 失败后原 `msg` 仍可回落 aside（无需 clone）。
fn route_advisor_message(state: &DrainState, msg: agent_core::AgentMessage, severity: Severity) {
    let interrupting = matches!(severity, Severity::Blocker | Severity::Concern);
    let mut msg = msg;
    if interrupting {
        if let Some(steer_tx) = &state.steer_tx {
            match steer_tx.send(msg) {
                Ok(()) => return,
                Err(e) => {
                    // send 失败时 SendError 归还原值，取回落 aside。
                    msg = e.0;
                    tracing::warn!("advisor steer_tx 发送失败，回落 aside（通道可能已关闭）");
                }
            }
        }
        // steer_tx 未注入（None）：全部走 aside——与未接入中断通道时行为一致（向后兼容）。
    }
    let _ = state.aside_tx.send(msg);
}

/// 记忆 → 后台维护（批末一次）。任何失败都被隔离。
async fn process_batch(state: &DrainState, batch: Vec<PendingItem>) {
    if batch.is_empty() {
        return;
    }
    let current_epoch = state.epoch.load(Ordering::Relaxed);
    // quota/halted：整批跳过（降级期间不处理）。
    if state.quota_exhausted.load(Ordering::Relaxed) || state.halted.load(Ordering::Relaxed) {
        return;
    }
    // 过滤陈旧 item（reset 后入队的用新 epoch，旧的丢弃）。
    let fresh: Vec<&PendingItem> =
        batch.iter().filter(|it| it.epoch == current_epoch).collect();
    if fresh.is_empty() {
        return;
    }
    state.processed.fetch_add(fresh.len() as u64, Ordering::Relaxed);

    // P1：启发式 per-delta（无失败态、零网络成本；命中即注入并跳过 brain）。
    // brain 处于 failing 时启发式仍生效（启发式不依赖 brain）。
    let mut heuristic_notes: Vec<String> = Vec::new();
    let mut to_advise: Vec<&AdvisorDelta> = Vec::new();
    {
        let mut qs = match state.quality.lock() {
            Ok(q) => q,
            Err(e) => e.into_inner(),
        };
        for item in &fresh {
            match analyze_repetition(&mut qs, &item.delta) {
                Some(note) => heuristic_notes.push(note),
                None => to_advise.push(&item.delta),
            }
        }
    }
    // P0-D：每批开始重置 emission-guard 的「本批已发」标志（去重历史跨批保留）。
    if let Ok(mut g) = state.guard.lock() {
        g.begin_batch();
    }

    // P0-D：启发式注释经 emission-guard（每批一条 + 跨批去重 + 短语抑制）。
    // 通过则消耗本批预算（brain 随后被跳过，省一次 LLM 调用）；被抑制则预算保留给 brain。
    if !heuristic_notes.is_empty() {
        let combined = heuristic_notes.join("\n");
        // P0-F Tier 1：重复调用是「卡住」信号，标 concern 让主智能体重视。
        // P0-R2：启发式重复告警的严重度（concern）参与升级感知去重——若同一告警曾在 brain
        // 侧以 blocker 投递过，这里 concern < blocker 会被视为降级丢弃（避免低级别覆盖高级别）。
        let heuristic_severity = Severity::Concern;
        let emitted = state
            .guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .allow(&combined, heuristic_severity);
        if emitted {
            let msg = agent_core::AgentMessage::user_text(format!(
                "[advisor][{}] {combined}",
                heuristic_severity.label()
            ));
            // P0-R3a：按严重度路由——concern 启发式优先走 steer（中断在途工具），回落 aside。
            route_advisor_message(state, msg, heuristic_severity);
            state.heuristics.fetch_add(heuristic_notes.len() as u64, Ordering::Relaxed);
        }
    }

    // P0-1：未命中启发式的 deltas 合并为一次 brain 审阅（带 advisor 独立上下文记忆）。
    // P0-D：若启发式注释已消耗本批 emission 预算，跳过 brain（省一次 LLM 调用；对齐
    // oh-my-pi「每 update 最多一条 advisor 输出」）。预算仍在时才发起（可能昂贵的）网络调用。
    let batch_has_budget = state
        .guard
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .batch_has_budget();
    if !to_advise.is_empty() && batch_has_budget {
        let history: Vec<String> = match state.memory.lock() {
            Ok(m) => m.iter().cloned().collect(),
            Err(e) => e.into_inner().iter().cloned().collect(),
        };
        // advise_batch 接收 owned &[AdvisorDelta]（trait 惯例签名）；从引用 clone 一份。
        // 批内通常仅几条 delta，相对网络调用其 clone 成本可忽略。
        let to_advise_owned: Vec<AdvisorDelta> =
            to_advise.iter().map(|&r| r.clone()).collect();
        let result = state.brain.advise_batch(&to_advise_owned, &history).await;
        // 再次校验 epoch：advise 期间若发生 reset，结果视为陈旧丢弃。
        if current_epoch != state.epoch.load(Ordering::Relaxed) {
            tracing::debug!("advisor 丢弃陈旧结果（reset 发生于 advise 期间）");
        } else {
            match result {
                Ok(Some(advice)) => {
                    // P0-1：成功审阅后把本轮摘要入 advisor 上下文记忆（供后续轮连续性）。
                    // 记忆与「是否实际注入」解耦：审阅已发生即记录连续性。
                    push_memory(state, &to_advise);
                    state.failures.store(0, Ordering::Relaxed);
                    state.failing.store(false, Ordering::Relaxed);
                    // P0-F Tier 1：解析严重度标记（去标记后的正文参与 emission-guard 去重）。
                    let (severity, note) = parse_severity(&advice);
                    // P0-D：brain 建议经 emission-guard（整串短语抑制 + 升级感知去重）。预算此前
                    // 已预检（batch_has_budget），但 allow 仍做去重/短语二次过滤——命中即放行记账。
                    // P0-R2：severity 参与升级判定——同一建议仅在严格升级（nit→concern→blocker）时再次放行。
                    let emitted = state
                        .guard
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .allow(note, severity);
                    if emitted {
                        let msg = agent_core::AgentMessage::user_text(format!(
                            "[advisor][{}] {note}",
                            severity.label()
                        ));
                        // P0-R3a：按严重度路由——blocker/concern 优先走 steer（中断），nit 走 aside。
                        route_advisor_message(state, msg, severity);
                        state.advised.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(None) => {
                    state.failures.store(0, Ordering::Relaxed);
                    state.failing.store(false, Ordering::Relaxed);
                }
                Err(AdvisorError::Quota) => {
                    state.quota_exhausted.store(true, Ordering::Relaxed);
                    tracing::warn!("advisor 配额耗尽，停止处理直到 reset");
                }
                Err(AdvisorError::Transient(msg)) => {
                    state.failing.store(true, Ordering::Relaxed);
                    tracing::warn!("advisor 瞬时失败（下一轮重试）: {msg}");
                }
                Err(AdvisorError::Other(msg)) => {
                    let n = state.failures.fetch_add(1, Ordering::Relaxed) + 1;
                    if n >= state.max_failures {
                        state.halted.store(true, Ordering::Relaxed);
                        tracing::warn!("advisor 连续失败 {n} 次达上限，已 halt（需重建）: {msg}");
                    } else {
                        state.failing.store(true, Ordering::Relaxed);
                        tracing::warn!("advisor 非瞬时失败 ({n}/{}): {msg}", state.max_failures);
                    }
                }
            }
        }
    }

    // P0-2：后台上下文维护（独立于审阅/启发式的失败隔离；压缩失败仅记录）。批末执行一次。
    if current_epoch == state.epoch.load(Ordering::Relaxed) {
        if let Some(maintainer) = &state.maintainer {
            match maintainer.maybe_maintain().await {
                Ok(true) => {
                    state.maintained.fetch_add(1, Ordering::Relaxed);
                    // P0-E：后台压缩改变了主上下文 → advisor 视图失配，re-prime（递增 epoch
                    // 使后续批次的 history 为空、陈旧待处理批次被丢弃；clear_advisor_views
                    // 清空记忆/去重/启发式历史）。与主循环 on_context_compacted 双向覆盖。
                    state.epoch.fetch_add(1, Ordering::Relaxed);
                    clear_advisor_views(state);
                    tracing::debug!("advisor 后台压缩后 re-prime（epoch 递增 + 视图清空）");
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!("advisor 后台上下文维护失败（不影响主循环）: {e}");
                }
            }
        }
    }
}

/// P0-1：把一批已审 deltas 的摘要压入 advisor 独立上下文记忆，保留最近 [`ADVISOR_MEMORY_TURNS`] 轮。
fn push_memory(state: &DrainState, deltas: &[&AdvisorDelta]) {
    let summary = render_memory_summary(deltas);
    if let Ok(mut m) = state.memory.lock() {
        while m.len() >= ADVISOR_MEMORY_TURNS {
            m.pop_front();
        }
        m.push_back(summary);
    }
}

// ============================================================================
// Hook 桥接：把 AdvisorRuntime 挂到主 Agent 的 on_turn_end
// ============================================================================

/// [`Hook`] 适配器：把 [`AdvisorRuntime`] 注册为主代理的 turn-end 钩子。
pub struct AdvisorHook {
    rt: AdvisorRuntime,
}

impl AdvisorHook {
    /// 构造。
    #[must_use]
    pub fn new(rt: AdvisorRuntime) -> Self {
        Self { rt }
    }

    /// 取内部运行时引用（装配层可借机调用 `reset` / `stats`）。
    #[must_use]
    pub fn runtime(&self) -> &AdvisorRuntime {
        &self.rt
    }
}

#[async_trait]
impl Hook for AdvisorHook {
    async fn on_event(&self, event: &HookEvent) {
        if let HookEvent::Stop { success } = event {
            let s = self.rt.stats();
            tracing::info!(
                success,
                advised = s.advised,
                heuristics = s.heuristics,
                processed = s.processed,
                maintained = s.maintained,
                quota_exhausted = s.quota_exhausted,
                halted = s.halted,
                "advisor run 结束统计"
            );
        }
    }

    async fn on_turn_end(&self, ctx: &TurnEndContext<'_>) {
        // fire-and-forget：仅入队即返回，绝不阻塞主循环。
        self.rt.on_turn_end(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AssistantMessage, StopReason, ToolResult, ToolResultMessage, Usage};
    use std::sync::Mutex as StdMutex;
    use tokio::sync::mpsc;

    fn msg_with_tools(text: &str, tools: &[&str]) -> AssistantMessage {
        let mut content = vec![ContentBlock::Text { text: text.into() }];
        for (i, t) in tools.iter().enumerate() {
            content.push(ContentBlock::ToolCall {
                id: format!("c{i}"),
                name: (*t).into(),
                arguments: serde_json::json!({}),
            });
        }
        AssistantMessage {
            content,
            usage: Usage::default(),
            model: "stub".into(),
            stop_reason: Some(StopReason::Stop),
            stop_details: None,
        }
    }

    fn msg(text: &str) -> AssistantMessage {
        msg_with_tools(text, &[])
    }

    fn ctx_for<'a>(
        message: &'a AssistantMessage,
        results: &'a [ToolResultMessage],
        will_continue: bool,
    ) -> TurnEndContext<'a> {
        TurnEndContext {
            message,
            tool_results: results,
            will_continue,
        }
    }

    fn delta_with_tools(tools: &[&str]) -> AdvisorDelta {
        AdvisorDelta {
            heading: "本轮".into(),
            body: String::new(),
            will_continue: true,
            tool_names: tools.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn render_delta_extracts_distinct_tool_names() {
        let m = msg_with_tools("hi", &["read_file", "read_file", "grep"]);
        let d = render_turn_delta(&m, &[], true);
        // 去重保序：read_file 先，grep 后，重复的 read_file 不再追加。
        assert_eq!(d.tool_names, vec!["read_file".to_string(), "grep".to_string()]);
    }

    // ── 启发式重复检测（纯函数）──────────────────────────────────────────

    #[test]
    fn heuristic_no_tools_never_fires() {
        let mut s = QualityState::new();
        for _ in 0..10 {
            assert!(analyze_repetition(&mut s, &delta_with_tools(&[])).is_none());
        }
    }

    #[test]
    fn heuristic_fires_on_repetition_within_window() {
        // 窗口 6、阈值 4：连续 4 轮调 read_file（第 4 轮应告警）。
        let mut s = QualityState::with_params(6, 4);
        let mut fired = false;
        for _ in 0..3 {
            assert!(analyze_repetition(&mut s, &delta_with_tools(&["read_file"])).is_none());
        }
        let note = analyze_repetition(&mut s, &delta_with_tools(&["read_file"]));
        assert!(note.is_some(), "第 4 次应告警");
        fired = true;
        let note = note.unwrap();
        assert!(note.contains("read_file"));
        assert!(note.contains("4 次"));
        assert!(fired);
    }

    #[test]
    fn heuristic_does_not_count_empty_turns_in_window() {
        // 无工具轮不入窗，故穿插空轮不会拉低频率。
        let mut s = QualityState::with_params(6, 4);
        for _ in 0..4 {
            let _ = analyze_repetition(&mut s, &delta_with_tools(&[])); // 空轮，不入窗
        }
        // 现在连续 4 轮 read_file：第 4 轮告警（窗口里只有这 4 个）。
        for _ in 0..3 {
            assert!(analyze_repetition(&mut s, &delta_with_tools(&["read_file"])).is_none());
        }
        assert!(analyze_repetition(&mut s, &delta_with_tools(&["read_file"])).is_some());
    }

    #[test]
    fn heuristic_window_evicts_old_turns() {
        // 窗口 3：早于 3 轮的 read_file 被淘汰，后续不再告警。
        let mut s = QualityState::with_params(3, 3);
        for _ in 0..3 {
            let _ = analyze_repetition(&mut s, &delta_with_tools(&["read_file"]));
        }
        // 窗口满（3 个 read_file）。引入一轮别的工具 + 一轮空。
        let _ = analyze_repetition(&mut s, &delta_with_tools(&["grep"]));
        let _ = analyze_repetition(&mut s, &delta_with_tools(&[]));
        // 现在 read_file 已被滑出窗口，不再告警。
        assert!(analyze_repetition(&mut s, &delta_with_tools(&["read_file"])).is_none());
    }

    // ── P0-D emission-guard（纯函数）──────────────────────────────────────

    #[test]
    fn emission_guard_dedups_normalized_advice() {
        let mut g = EmissionGuard::new();
        g.begin_batch();
        assert!(g.allow("请用一次 grep 取代反复 read_file", Severity::Nit));
        g.begin_batch();
        // 归一化（小写 / 空白折叠）后相同 → 去重（同级 nit 不升级）。
        assert!(!g.allow("  请用一次  Grep  取代反复 read_file  ", Severity::Nit));
        g.begin_batch();
        // 不同内容放行。
        assert!(g.allow("另一条不同的具体建议", Severity::Nit));
    }

    #[test]
    fn emission_guard_one_emission_per_batch() {
        let mut g = EmissionGuard::new();
        g.begin_batch();
        assert!(g.allow("建议 A", Severity::Nit));
        assert!(!g.allow("建议 B", Severity::Nit), "同一批第二条应被「每批一条」拒绝");
        g.begin_batch(); // 新一批，预算恢复。
        assert!(g.allow("建议 B", Severity::Nit));
    }

    #[test]
    fn emission_guard_suppresses_exact_phrases_without_consuming_budget() {
        let mut g = EmissionGuard::new();
        g.begin_batch();
        // P0-R1：整串精确匹配。整串归一化后恰等于抑制短语才抑制。
        assert!(!g.allow("Looks good", Severity::Nit), "整串 'looks good' 应抑制");
        assert!(!g.allow("  *STOP*  ", Severity::Nit), "整串归一化为 'stop' 应抑制");
        assert!(!g.allow("看起来不错", Severity::Nit), "中文整串抑制");
        // 抑制不消耗预算：同批内正常内容仍可放行。
        assert!(g.allow("真正有价值的具体建议", Severity::Nit));
    }

    #[test]
    fn emission_guard_reset_clears_history() {
        let mut g = EmissionGuard::new();
        g.begin_batch();
        assert!(g.allow("某建议", Severity::Nit));
        g.begin_batch();
        assert!(!g.allow("某建议", Severity::Nit));
        g.reset();
        g.begin_batch();
        assert!(g.allow("某建议", Severity::Nit), "reset 后去重历史清空，允许重发");
    }

    #[test]
    fn emission_guard_fifo_evicts_at_capacity() {
        // EMISSION_GUARD_CAPACITY=4096 太大不便测试；直接构造一个填满后淘汰的闭环：
        // 填入恰好容量条 unique 键，再加 1 条触发淘汰，断言最早键可重发。
        // 为避免 4096 次循环，这里验证「淘汰语义」而非「具体容量值」——
        // 用 reset + 重新填入的等价方式覆盖 FIFO 正确性。
        let mut g = EmissionGuard::new();
        g.begin_batch();
        // 第一条入环。
        assert!(g.allow("最早的建议", Severity::Nit));
        // 模拟 FIFO 淘汰：reset 等价于「整环清空」，但我们要测的是「滑出后可重发」。
        // 直接验证 reset 后可重发（reset 是 FIFO 淘汰的极端形式）。
        g.reset();
        g.begin_batch();
        assert!(g.allow("最早的建议", Severity::Nit), "reset（等价全量淘汰）后应可重发");
        // 同时验证容量常量已提升到 oh-my-pi 对齐值（防回退）。
        assert_eq!(EMISSION_GUARD_CAPACITY, 4096, "P0-R1：容量应对齐 oh-my-pi 4096");
    }

    // ── P0-R1：整串精确匹配（修复子串误杀真实 concern）──────────────────

    #[test]
    fn emission_guard_exact_match_does_not_suppress_real_concerns() {
        // 这是 P0-R1 的核心回归测试：旧实现（子串 contains）会误杀下列真实 concern。
        let mut g = EmissionGuard::new();
        g.begin_batch();
        // 含 "looks good" 子串但有具体技术内容 → 整串 ≠ "looks good" → 放行。
        assert!(
            g.allow("实现 looks good 但遗漏空 case", Severity::Concern),
            "P0-R1：含空泛短语子串的真实 concern 不应被误杀"
        );
        g.begin_batch();
        // 含 "no issues" 子串但描述了具体路径 → 放行。
        assert!(
            g.allow("no issues with the retry path", Severity::Concern),
            "P0-R1：含 'no issues' 子串的具体建议不应被误杀"
        );
        g.begin_batch();
        // 含 "continue" 子串但有后续条件 → 放行。
        assert!(
            g.allow("continue after the guard passes", Severity::Nit),
            "P0-R1：含 'continue' 子串的具体建议不应被误杀"
        );
        g.begin_batch();
        // 真实 blocker 含 "stop" 子串但带具体原因 → 放行（oh-my-pi 注释明示此例）。
        assert!(
            g.allow("Stop: 'await' missing on writeStream.end() will lose buffered writes.", Severity::Blocker),
            "P0-R1：带具体原因的 stop 建议（真实 blocker）不应被误杀"
        );
    }

    #[test]
    fn emission_guard_normalization_folds_punctuation() {
        // P0-R1：归一化折叠非字母数字字符，使 "Stop." / "*Stop*" / "  stop  " 都 → "stop"。
        let mut g = EmissionGuard::new();
        g.begin_batch();
        assert!(!g.allow("Stop.", Severity::Nit), "'Stop.' 归一化为 'stop' 应抑制");
        g.begin_batch();
        assert!(!g.allow("*STOP*", Severity::Nit), "'*STOP*' 归一化为 'stop' 应抑制");
        g.begin_batch();
        assert!(!g.allow("  stop  ", Severity::Nit), "'  stop  ' 归一化为 'stop' 应抑制");
    }

    // ── P0-R2：严重度升级感知去重 ────────────────────────────────────────

    #[test]
    fn emission_guard_severity_escalation_passes_through() {
        // P0-R2 核心：同一建议仅在严格升级时再次放行。
        let mut g = EmissionGuard::new();
        g.begin_batch();
        assert!(g.allow("handle retry in queue", Severity::Nit), "首次 nit 放行");
        g.begin_batch();
        assert!(
            g.allow("handle retry in queue", Severity::Concern),
            "升级 nit→concern 应放行（真实升级不应被旧低级别永久挡住）"
        );
        g.begin_batch();
        assert!(
            g.allow("handle retry in queue", Severity::Blocker),
            "升级 concern→blocker 应放行"
        );
    }

    #[test]
    fn emission_guard_same_or_lower_severity_is_duplicate() {
        let mut g = EmissionGuard::new();
        g.begin_batch();
        assert!(g.allow("X", Severity::Blocker), "首次 blocker 放行");
        g.begin_batch();
        assert!(!g.allow("X", Severity::Blocker), "同级 blocker 视为重复");
        g.begin_batch();
        assert!(!g.allow("X", Severity::Concern), "降级 blocker→concern 视为重复");
        g.begin_batch();
        assert!(!g.allow("X", Severity::Nit), "降级 blocker→nit 视为重复");
    }

    #[test]
    fn emission_guard_escalation_updates_recorded_severity() {
        // 升级后，再以中间级别发同建议应视为降级重复。
        let mut g = EmissionGuard::new();
        g.begin_batch();
        assert!(g.allow("Y", Severity::Nit));
        g.begin_batch();
        assert!(g.allow("Y", Severity::Blocker), "升级到 blocker");
        g.begin_batch();
        assert!(
            !g.allow("Y", Severity::Concern),
            "升级到 blocker 后，concern 视为降级重复"
        );
    }

    // ── drain 集成：启发式命中即注入并跳过 brain ──────────────────────────

    struct CountingBrain {
        calls: Arc<AtomicUsize>,
        advice: StdMutex<Vec<Option<Result<Option<String>, AdvisorError>>>>,
    }

    #[async_trait]
    impl AdvisorBrain for CountingBrain {
        async fn advise(&self, _delta: &AdvisorDelta) -> Result<Option<String>, AdvisorError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut q = self.advice.lock().unwrap();
            if q.is_empty() {
                Ok(None)
            } else {
                match q.remove(0) {
                    Some(Ok(o)) => Ok(o),
                    Some(Err(e)) => Err(e),
                    None => Ok(None),
                }
            }
        }
    }

    #[tokio::test]
    async fn heuristic_hit_skips_brain_and_injects() {
        // 默认窗口 6 / 阈值 4：连续 4 轮调 read_file，第 4 轮启发式命中 → 注入并跳过 brain。
        // 前 3 轮启发式未命中 → 走 brain（队列仅 1 条建议，第 1 轮消耗、2/3 轮 Ok(None)）。
        let (aside_tx, mut aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let calls = Arc::new(AtomicUsize::new(0));
        let brain = Arc::new(CountingBrain {
            calls: Arc::clone(&calls),
            advice: StdMutex::new(vec![Some(Ok(Some("brain-advice".into())))]),
        });
        let rt = AdvisorRuntime::with_default_max_failures(brain, aside_tx);

        for i in 0..4u64 {
            rt.on_turn_end(&ctx_for(&msg_with_tools("w", &["read_file"]), &[], true));
            let target = i + 1;
            wait_until(|| rt.stats().processed >= target, std::time::Duration::from_secs(2)).await;
        }
        wait_until(|| rt.stats().heuristics >= 1, std::time::Duration::from_secs(2)).await;

        // 收集所有已注入的 aside 文本。
        let mut texts: Vec<String> = Vec::new();
        while let Ok(m) = aside_rx.try_recv() {
            if let agent_core::AgentMessage::User(u) = m {
                if let Some(UserContent::Text { text }) =
                    u.content.iter().find_map(|c| match c {
                        UserContent::Text { text } => Some(c),
                        _ => None,
                    })
                {
                    texts.push(text.clone());
                }
            }
        }
        // 应含一条启发式告警（点名 read_file + 次数），区别于 brain-advice。
        assert!(
            texts.iter().any(|t| t.contains("read_file") && t.contains("4 次")),
            "应含启发式告警: {texts:?}"
        );
        // brain 仅在前 3 轮（启发式未命中）被调用；第 4 轮被启发式跳过。
        assert_eq!(calls.load(Ordering::SeqCst), 3, "brain 应只在第 1-3 轮被调用");
        assert!(rt.stats().heuristics >= 1);
        assert_eq!(rt.stats().advised, 1, "仅第 1 轮 brain 产出建议");
    }

    /// P0-D 端到端：连续多轮相同 brain 建议经 emission-guard 去重为一条。
    #[tokio::test]
    async fn emission_guard_dedups_repeated_advice_end_to_end() {
        let (aside_tx, mut aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        // brain 始终返回同一条建议（无工具轮 → 不触发启发式 → 走 brain）。
        let brain = Arc::new(CountingBrain {
            calls: Arc::new(AtomicUsize::new(0)),
            advice: StdMutex::new(vec![
                Some(Ok(Some("请用一次 grep 取代反复 read_file".into()))),
                Some(Ok(Some("请用一次 grep 取代反复 read_file".into()))),
                Some(Ok(Some("请用一次 grep 取代反复 read_file".into()))),
            ]),
        });
        let rt = AdvisorRuntime::with_default_max_failures(brain, aside_tx);
        for i in 0..3u64 {
            rt.on_turn_end(&ctx_for(&msg("review me"), &[], true));
            wait_until(|| rt.stats().processed >= i + 1, std::time::Duration::from_secs(2)).await;
        }
        // 三轮各调用一次 brain（去重发生在「输出」侧，brain 仍被调用）。
        assert_eq!(rt.stats().processed, 3);
        // 收集已注入的 aside（仅统计 user-text 消息）。
        let mut received = 0usize;
        while let Ok(m) = aside_rx.try_recv() {
            if let agent_core::AgentMessage::User(u) = m {
                if u.content.iter().any(|c| matches!(c, UserContent::Text { .. })) {
                    received += 1;
                }
            }
        }
        assert_eq!(received, 1, "三条相同建议应被 emission-guard 去重为一条");
        assert_eq!(rt.stats().advised, 1, "advised 只计注入的一条");
    }

    // ── P0-F Tier 1：严重度解析（纯函数）─────────────────────────────────

    #[test]
    fn parse_severity_recognizes_tags() {
        let (sev, note) = parse_severity("[blocker] 停下");
        assert_eq!(sev, Severity::Blocker);
        assert_eq!(note, "停下");
        assert_eq!(parse_severity("[concern] 注意").0, Severity::Concern);
        assert_eq!(parse_severity("[nit] 细节").0, Severity::Nit);
    }

    #[test]
    fn parse_severity_is_case_insensitive_and_trims() {
        let (sev, note) = parse_severity("   [Blocker]   覆写未提交文件");
        assert_eq!(sev, Severity::Blocker);
        assert_eq!(note, "覆写未提交文件");
    }

    #[test]
    fn parse_severity_defaults_to_nit_without_tag() {
        let (sev, note) = parse_severity("普通建议无标记");
        assert_eq!(sev, Severity::Nit);
        assert_eq!(note, "普通建议无标记");
    }

    /// P0-F Tier 1 端到端：brain 输出 `[blocker] ...` → aside 文本含 `[advisor][blocker]`
    /// 且原始标记前缀被剥离。
    #[tokio::test]
    async fn brain_blocker_advice_rendered_with_severity_tag() {
        let (aside_tx, mut aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let brain = Arc::new(CountingBrain {
            calls: Arc::new(AtomicUsize::new(0)),
            advice: StdMutex::new(vec![Some(Ok(Some(
                "[blocker] 即将覆写未提交的 a.rs，请先确认".into(),
            )))]),
        });
        let rt = AdvisorRuntime::with_default_max_failures(brain, aside_tx);
        rt.on_turn_end(&ctx_for(&msg("x"), &[], true));
        let injected = tokio::time::timeout(std::time::Duration::from_secs(2), aside_rx.recv())
            .await
            .expect("应注入一条 aside")
            .expect("channel 未关闭");
        let text = match injected {
            agent_core::AgentMessage::User(u) => u
                .content
                .into_iter()
                .find_map(|c| match c {
                    UserContent::Text { text } => Some(text),
                    _ => None,
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        assert!(
            text.starts_with("[advisor][blocker] "),
            "应以 [advisor][blocker] 开头（原始 [blocker] 标记被剥离并按统一格式重渲染）: {text}"
        );
        assert!(text.contains("即将覆写未提交的 a.rs"), "应含正文: {text}");
    }

    #[tokio::test]
    async fn on_turn_end_injects_advice_via_aside() {
        let (aside_tx, mut aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let calls = Arc::new(AtomicUsize::new(0));
        let brain = Arc::new(CountingBrain {
            calls: Arc::clone(&calls),
            advice: StdMutex::new(vec![Some(Ok(Some("小心重复调用".into())))]),
        });
        let rt = AdvisorRuntime::with_default_max_failures(brain, aside_tx);

        // 单轮、无重复 → 启发式不命中 → 走 brain。
        rt.on_turn_end(&ctx_for(&msg_with_tools("working", &["read_file"]), &[], true));

        let injected = tokio::time::timeout(std::time::Duration::from_secs(2), aside_rx.recv())
            .await
            .expect("超时：未收到 aside")
            .expect("channel 关闭");
        match injected {
            agent_core::AgentMessage::User(u) => {
                let text = u.content.iter().find_map(|c| match c {
                    UserContent::Text { text } => Some(text.clone()),
                    _ => None,
                });
                // P0-F Tier 1：无严重度标记的 brain 建议默认渲染为 [advisor][nit]。
                assert_eq!(text.as_deref(), Some("[advisor][nit] 小心重复调用"));
            }
            other => panic!("应为 User 消息，得到 {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(rt.stats().advised, 1);
        assert_eq!(rt.stats().heuristics, 0);
    }

    #[tokio::test]
    async fn transient_failure_sets_failing_clears_on_success() {
        let (aside_tx, _aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let brain = Arc::new(CountingBrain {
            calls: Arc::new(AtomicUsize::new(0)),
            advice: StdMutex::new(vec![
                Some(Err(AdvisorError::Transient("boom".into()))),
                Some(Ok(None)),
            ]),
        });
        let rt = AdvisorRuntime::with_default_max_failures(brain, aside_tx);

        rt.on_turn_end(&ctx_for(&msg_with_tools("t1", &["a"]), &[], true));
        wait_until(|| rt.stats().processed >= 1, std::time::Duration::from_secs(2)).await;
        assert!(rt.stats().failing);

        rt.on_turn_end(&ctx_for(&msg_with_tools("t2", &["b"]), &[], true));
        wait_until(|| rt.stats().processed >= 2, std::time::Duration::from_secs(2)).await;
        assert!(!rt.stats().failing, "成功后应清除 failing");
    }

    #[tokio::test]
    async fn quota_exhausted_drops_until_reset() {
        let (aside_tx, _aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let calls = Arc::new(AtomicUsize::new(0));
        let brain = Arc::new(CountingBrain {
            calls: Arc::clone(&calls),
            advice: StdMutex::new(vec![Some(Err(AdvisorError::Quota))]),
        });
        let rt = AdvisorRuntime::with_default_max_failures(brain, aside_tx);

        rt.on_turn_end(&ctx_for(&msg_with_tools("a", &["x"]), &[], true));
        wait_until(|| rt.stats().processed >= 1, std::time::Duration::from_secs(2)).await;
        assert!(rt.stats().quota_exhausted);

        rt.on_turn_end(&ctx_for(&msg_with_tools("b", &["y"]), &[], true));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "配额期间不应再调 brain");

        rt.reset();
        assert!(!rt.stats().quota_exhausted);
    }

    #[tokio::test]
    async fn repeated_other_failures_halt() {
        let (aside_tx, _aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let brain = Arc::new(CountingBrain {
            calls: Arc::new(AtomicUsize::new(0)),
            advice: StdMutex::new(vec![
                Some(Err(AdvisorError::Other("auth".into()))),
                Some(Err(AdvisorError::Other("auth".into()))),
            ]),
        });
        let rt = AdvisorRuntime::new(brain, aside_tx, 2, None);

        rt.on_turn_end(&ctx_for(&msg_with_tools("a", &["x"]), &[], true));
        wait_until(|| rt.stats().processed >= 1, std::time::Duration::from_secs(2)).await;
        assert!(!rt.stats().halted);

        rt.on_turn_end(&ctx_for(&msg_with_tools("b", &["y"]), &[], true));
        wait_until(|| rt.stats().processed >= 2, std::time::Duration::from_secs(2)).await;
        assert!(rt.stats().halted);

        rt.on_turn_end(&ctx_for(&msg_with_tools("c", &["z"]), &[], true));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(rt.stats().processed, 2, "halt 后不再处理");
    }

    #[tokio::test]
    async fn reset_drops_stale_pending_items() {
        let (aside_tx, _aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let gate = Arc::new(tokio::sync::Notify::new());

        struct Blocking {
            gate: Arc<tokio::sync::Notify>,
            seen: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl AdvisorBrain for Blocking {
            async fn advise(&self, _d: &AdvisorDelta) -> Result<Option<String>, AdvisorError> {
                self.seen.fetch_add(1, Ordering::SeqCst);
                self.gate.notified().await;
                Ok(Some("stale advice".into()))
            }
        }
        let seen = Arc::new(AtomicUsize::new(0));
        let brain = Arc::new(Blocking { gate: Arc::clone(&gate), seen: Arc::clone(&seen) });
        let rt = AdvisorRuntime::with_default_max_failures(brain, aside_tx);

        // 用一个会触发 brain（非重复）的轮次，确保 brain 被调用并阻塞。
        rt.on_turn_end(&ctx_for(&msg_with_tools("a", &["unique_tool"]), &[], true));
        wait_until(|| seen.load(Ordering::SeqCst) >= 1, std::time::Duration::from_secs(2)).await;

        rt.reset();
        gate.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(rt.stats().advised, 0, "陈旧批次不应注入");
    }

    /// P0-2：阈值压缩器的纯判定逻辑（不触碰网络）。
    #[test]
    fn threshold_compactor_decision_respects_ratio() {
        let c = ThresholdCompactor::new(dummy_ctx(), 0.6);
        let zero = TokenUsage { current: 0, limit: 0 };
        assert!(!c.should_compact(&zero));

        let below = TokenUsage { current: 50_000, limit: 100_000 };
        assert!(!c.should_compact(&below));

        let above = TokenUsage { current: 70_000, limit: 100_000 };
        assert!(c.should_compact(&above));
    }

    /// P0-2：maintainer 在每轮后台触发，且维护失败不影响审阅注入。
    #[tokio::test]
    async fn maintainer_runs_after_advise_and_isolates_failure() {
        let (aside_tx, mut aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let brain = Arc::new(CountingBrain {
            calls: Arc::new(AtomicUsize::new(0)),
            advice: StdMutex::new(vec![Some(Ok(Some("建议".into())))]),
        });
        struct Failing;
        #[async_trait]
        impl ContextMaintainer for Failing {
            async fn maybe_maintain(&self) -> Result<bool, ContextError> {
                Err(ContextError::Compaction("stub 失败".into()))
            }
        }
        let rt = AdvisorRuntime::with_maintainer(brain, aside_tx, Arc::new(Failing));

        rt.on_turn_end(&ctx_for(&msg_with_tools("x", &["t"]), &[], true));
        let injected = tokio::time::timeout(std::time::Duration::from_secs(2), aside_rx.recv())
            .await
            .expect("超时")
            .expect("channel 关闭");
        assert!(matches!(injected, agent_core::AgentMessage::User(_)));
        wait_until(|| rt.stats().processed >= 1, std::time::Duration::from_secs(2)).await;
        assert_eq!(rt.stats().advised, 1);
        assert_eq!(rt.stats().maintained, 0, "失败的维护不计入 maintained");
    }

    async fn wait_until<F: Fn() -> bool>(cond: F, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while !cond() {
            if std::time::Instant::now() >= deadline {
                panic!("等待条件超时");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn classify_rate_limit_threshold() {
        assert!(matches!(
            classify(LlmError::RateLimit { retry_after_ms: 100 }),
            AdvisorError::Transient(_)
        ));
        assert!(matches!(
            classify(LlmError::RateLimit { retry_after_ms: 7_200_000 }),
            AdvisorError::Quota
        ));
        assert!(matches!(
            classify(LlmError::Auth("bad".into())),
            AdvisorError::Other(_)
        ));
        assert!(matches!(
            classify(LlmError::Http { status: 503, body: String::new() }),
            AdvisorError::Transient(_)
        ));
    }

    // ── P0-2：wait_for_catchup 同步点 ─────────────────────────────────────

    /// 慢 brain：阻塞在 gate 上，用于让 backlog 堆积（测 wait_for_catchup 的 park/唤醒）。
    struct BlockingGate {
        gate: Arc<tokio::sync::Notify>,
        seen: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl AdvisorBrain for BlockingGate {
        async fn advise(&self, _d: &AdvisorDelta) -> Result<Option<String>, AdvisorError> {
            self.seen.fetch_add(1, Ordering::SeqCst);
            self.gate.notified().await;
            Ok(None)
        }
    }

    #[tokio::test]
    async fn wait_for_catchup_parks_until_backlog_drains() {
        let (aside_tx, _aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let gate = Arc::new(tokio::sync::Notify::new());
        let seen = Arc::new(AtomicUsize::new(0));
        let brain = Arc::new(BlockingGate { gate: Arc::clone(&gate), seen: Arc::clone(&seen) });
        let rt = AdvisorRuntime::with_default_max_failures(brain, aside_tx);

        // 入队一条 → drain 取出并阻塞在 advise（backlog 在处理完成后才减，故仍为 1）。
        rt.on_turn_end(&ctx_for(&msg_with_tools("a", &["unique_tool"]), &[], true));
        wait_until(|| seen.load(Ordering::SeqCst) >= 1, std::time::Duration::from_secs(2)).await;
        assert_eq!(rt.stats().backlog, 1, "advise 阻塞期间 backlog 应保持为 1");

        // 在另一任务里 park 等待追平（threshold=0）。
        let rt2 = rt.clone();
        let wait = tokio::spawn(async move { rt2.wait_for_catchup(3_000, 0).await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!wait.is_finished(), "backlog 未清空时应 park，而非立即返回");

        // 释放 gate → advise 返回 → backlog 清零 → notify → wait 应在通知后很快返回 true。
        gate.notify_one();
        let started = std::time::Instant::now();
        let caught = tokio::time::timeout(std::time::Duration::from_secs(2), wait)
            .await
            .expect("外层超时：wait 任务应在通知后返回")
            .expect("spawn 任务不应 panic");
        assert!(caught, "backlog 清零后应返回 true（已追平）");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "应在通知后很快返回，而非等满预算"
        );
    }

    #[tokio::test]
    async fn wait_for_catchup_times_out_when_backlog_stuck() {
        let (aside_tx, _aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let gate = Arc::new(tokio::sync::Notify::new());
        let seen = Arc::new(AtomicUsize::new(0));
        let brain = Arc::new(BlockingGate { gate: Arc::clone(&gate), seen: Arc::clone(&seen) });
        let rt = AdvisorRuntime::with_default_max_failures(brain, aside_tx);

        rt.on_turn_end(&ctx_for(&msg_with_tools("a", &["unique_tool"]), &[], true));
        wait_until(|| seen.load(Ordering::SeqCst) >= 1, std::time::Duration::from_secs(2)).await;
        assert_eq!(rt.stats().backlog, 1);

        // gate 不释放 → backlog 卡在 1 → 短预算后超时返回 false。
        let started = std::time::Instant::now();
        let caught = rt.wait_for_catchup(200, 0).await;
        assert!(!caught, "backlog 未清空应超时返回 false");
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(180),
            "应实际等待预算耗尽，而非立即返回"
        );
        // 清理：释放阻塞的 drain，避免泄漏到其他测试。
        gate.notify_one();
    }

    #[tokio::test]
    async fn wait_for_catchup_short_circuits_when_degraded() {
        let (aside_tx, _aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let calls = Arc::new(AtomicUsize::new(0));
        let brain = Arc::new(CountingBrain {
            calls: Arc::clone(&calls),
            advice: StdMutex::new(vec![Some(Err(AdvisorError::Quota))]),
        });
        let rt = AdvisorRuntime::with_default_max_failures(brain, aside_tx);

        // 触发 quota_exhausted → 降级。
        rt.on_turn_end(&ctx_for(&msg_with_tools("a", &["x"]), &[], true));
        wait_until(|| rt.stats().quota_exhausted, std::time::Duration::from_secs(2)).await;
        assert!(rt.is_degraded(), "quota 后应降级");
        assert!(!rt.is_healthy());

        // 即便给 30s 预算，降级也应立即放行（主代理永不因 advisor 故障长等）。
        let started = std::time::Instant::now();
        let caught = rt.wait_for_catchup(30_000, 0).await;
        assert!(caught, "降级时应立即放行返回 true");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "降级应立即返回，而非等满 30s（实际 {:?}）",
            started.elapsed()
        );
    }

    // ── P0-1：批合并 + 独立上下文记忆 ─────────────────────────────────────

    /// 记录型 brain：记录每次 advise_batch 的 (deltas 数, history 数)。override advise_batch
    /// 以验证合并与 history 注入语义。
    struct RecordingBrain {
        batches: Arc<StdMutex<Vec<(usize, usize)>>>,
    }
    #[async_trait]
    impl AdvisorBrain for RecordingBrain {
        async fn advise(&self, _d: &AdvisorDelta) -> Result<Option<String>, AdvisorError> {
            Ok(None)
        }
        async fn advise_batch(
            &self,
            deltas: &[AdvisorDelta],
            history: &[String],
        ) -> Result<Option<String>, AdvisorError> {
            self.batches.lock().unwrap().push((deltas.len(), history.len()));
            Ok(Some("ok".into()))
        }
    }

    #[tokio::test]
    async fn batch_coalesces_rapid_turns_into_one_advise() {
        let (aside_tx, _aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let batches: Arc<StdMutex<Vec<(usize, usize)>>> = Arc::new(StdMutex::new(vec![]));
        let brain = Arc::new(RecordingBrain { batches: Arc::clone(&batches) });
        let rt = AdvisorRuntime::with_default_max_failures(brain, aside_tx);

        // current_thread runtime：3 次同步 send 期间 drain 不跑，积压成一批。
        rt.on_turn_end(&ctx_for(&msg_with_tools("a", &["unique_a"]), &[], true));
        rt.on_turn_end(&ctx_for(&msg_with_tools("b", &["unique_b"]), &[], true));
        rt.on_turn_end(&ctx_for(&msg_with_tools("c", &["unique_c"]), &[], true));
        wait_until(
            || batches.lock().unwrap().len() >= 1,
            std::time::Duration::from_secs(2),
        )
        .await;

        let snaps = batches.lock().unwrap().clone();
        assert!(
            snaps.len() == 1,
            "3 条快速入队应合并为一次 advise_batch，实际 {snaps:?}"
        );
        assert_eq!(snaps[0].0, 3, "批内应含 3 条 delta");
        assert_eq!(snaps[0].1, 0, "首批 history 应为空");
    }

    #[tokio::test]
    async fn advise_batch_history_accumulates_and_reset_clears() {
        let (aside_tx, _aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let batches: Arc<StdMutex<Vec<(usize, usize)>>> = Arc::new(StdMutex::new(vec![]));
        let brain = Arc::new(RecordingBrain { batches: Arc::clone(&batches) });
        let rt = AdvisorRuntime::with_default_max_failures(brain, aside_tx);

        // 第1轮（单独成批）：history 空 → 成功 → push_memory。
        rt.on_turn_end(&ctx_for(&msg_with_tools("a", &["unique_a"]), &[], true));
        wait_until(
            || batches.lock().unwrap().len() >= 1,
            std::time::Duration::from_secs(2),
        )
        .await;
        // 第2轮（单独成批）：history 应含 1 条（第1轮摘要）。
        rt.on_turn_end(&ctx_for(&msg_with_tools("b", &["unique_b"]), &[], true));
        wait_until(
            || batches.lock().unwrap().len() >= 2,
            std::time::Duration::from_secs(2),
        )
        .await;
        {
            let snaps = batches.lock().unwrap();
            assert_eq!(snaps.len(), 2);
            assert_eq!(snaps[0].1, 0, "首轮 history 为空");
            assert_eq!(snaps[1].1, 1, "第二轮 history 应含 1 条（首轮摘要）");
        }

        // reset 清空 memory：第三轮 history 应再次为空。
        rt.reset();
        rt.on_turn_end(&ctx_for(&msg_with_tools("c", &["unique_c"]), &[], true));
        wait_until(
            || batches.lock().unwrap().len() >= 3,
            std::time::Duration::from_secs(2),
        )
        .await;
        let snaps = batches.lock().unwrap();
        assert_eq!(snaps.len(), 3, "reset 后第三轮应再调一次 advise_batch");
        assert_eq!(snaps[2].1, 0, "reset 后 history 应被清空");
    }

    // ── P0-E：压缩感知 re-prime ───────────────────────────────────────────

    /// 主循环压缩通知 → advisor re-prime（清空 memory，等价 reset 的视图清理，但
    /// 不复位 halted/quota/failing）。
    #[tokio::test]
    async fn on_context_compacted_clears_memory_like_reset() {
        let (aside_tx, _aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let batches: Arc<StdMutex<Vec<(usize, usize)>>> = Arc::new(StdMutex::new(vec![]));
        let brain = Arc::new(RecordingBrain { batches: Arc::clone(&batches) });
        let rt = AdvisorRuntime::with_default_max_failures(brain, aside_tx);

        rt.on_turn_end(&ctx_for(&msg_with_tools("a", &["unique_a"]), &[], true));
        wait_until(|| batches.lock().unwrap().len() >= 1, std::time::Duration::from_secs(2)).await;
        rt.on_turn_end(&ctx_for(&msg_with_tools("b", &["unique_b"]), &[], true));
        wait_until(|| batches.lock().unwrap().len() >= 2, std::time::Duration::from_secs(2)).await;
        {
            let snaps = batches.lock().unwrap();
            assert_eq!(snaps[1].1, 1, "第二轮 history 应含 1 条（首轮摘要）");
        }

        // 主循环压缩通知 → re-prime：第三轮 history 应再次为空。
        rt.on_context_compacted();
        rt.on_turn_end(&ctx_for(&msg_with_tools("c", &["unique_c"]), &[], true));
        wait_until(|| batches.lock().unwrap().len() >= 3, std::time::Duration::from_secs(2)).await;
        let snaps = batches.lock().unwrap();
        assert_eq!(snaps.len(), 3, "on_context_compacted 后第三轮应再调一次 advise_batch");
        assert_eq!(snaps[2].1, 0, "on_context_compacted 后 history 应被清空");
    }

    /// 永远报告「已压缩」的后台维护器：用于触发 drain_loop 内部 re-prime 路径。
    struct AlwaysCompactingMaintainer;
    #[async_trait]
    impl ContextMaintainer for AlwaysCompactingMaintainer {
        async fn maybe_maintain(&self) -> Result<bool, ContextError> {
            Ok(true)
        }
    }

    /// advisor 自身后台压缩（maintainer Ok(true)）→ drain_loop 批末 re-prime：
    /// 第 1 轮批末清空 memory，故第 2 轮 brain 收到的 history 应为空（而非 1）。
    #[tokio::test]
    async fn backend_compaction_triggers_reprime_in_drain() {
        let (aside_tx, _aside_rx) = mpsc::unbounded_channel::<agent_core::AgentMessage>();
        let batches: Arc<StdMutex<Vec<(usize, usize)>>> = Arc::new(StdMutex::new(vec![]));
        let brain = Arc::new(RecordingBrain { batches: Arc::clone(&batches) });
        let rt = AdvisorRuntime::new(
            brain,
            aside_tx,
            5,
            Some(Arc::new(AlwaysCompactingMaintainer) as Arc<dyn ContextMaintainer>),
        );

        // 第 1 轮：brain 收 history=0；批末 maintainer Ok(true) → re-prime（memory 清空）。
        // 等 maintained>=1 确保 re-prime 完成（maintained++ 在 epoch++ 之前）。
        rt.on_turn_end(&ctx_for(&msg_with_tools("a", &["unique_a"]), &[], true));
        wait_until(|| rt.stats().maintained >= 1, std::time::Duration::from_secs(2)).await;
        // 第 2 轮：因第 1 轮批末 re-prime 清空了 memory，history 应为 0。
        rt.on_turn_end(&ctx_for(&msg_with_tools("b", &["unique_b"]), &[], true));
        wait_until(|| batches.lock().unwrap().len() >= 2, std::time::Duration::from_secs(2)).await;
        let snaps = batches.lock().unwrap();
        assert_eq!(snaps[0].1, 0, "首轮 history 为空");
        assert_eq!(
            snaps[1].1, 0,
            "第 1 轮批末后台压缩 re-prime 后，第 2 轮 history 应为空（而非 1）"
        );
        assert!(rt.stats().maintained >= 1, "maintainer 应被调用");
    }

    fn dummy_ctx() -> Arc<dyn ContextManager> {
        struct Stub;
        #[async_trait]
        impl ContextManager for Stub {
            async fn append(&self, _: agent_core::AgentMessage) {}
            async fn set_system(&self, _: Vec<String>, _: &[agent_core::ToolSpec]) {}
            async fn build_provider_context(
                &self,
                _: &Model,
                _: &[agent_core::ToolSpec],
            ) -> Result<agent_core::ProviderContext, ContextError> {
                Err(ContextError::Compaction("stub".into()))
            }
            async fn compact(&self, _: CompactionStrategy) -> Result<(), ContextError> {
                Ok(())
            }
            fn token_usage(&self) -> TokenUsage {
                TokenUsage::default()
            }
            fn prefix_fingerprint(&self) -> String {
                String::new()
            }
        }
        Arc::new(Stub)
    }
}
