//! GPT-5 Harmony-header 泄漏检测与恢复（移植 oh-my-pi `harmony-leak`）。
//!
//! Codex / GPT-5 Responses API 在某些情况下会把内部 Harmony 协议 token
//!（`<|start|>` / `<|call|>` / `<|return|>` 等）或 function marker（`to=functions.X`）
//! 泄漏到可见输出，甚至在 in-band 工具调用里「自造」工具结果。本模块实现：
//! - 多信号融合检测（H / M / C / G / S / B / R / T），跳过 code fence
//! - tool_arg surface 的截断恢复（truncate-resume）
//! - 隐私安全的审计事件
//!
//! 详见 oh-my-pi `docs/ERRATA-GPT5-HARMONY.md` §3。Gyre 适配点：`Thinking` 块字段名
//! 为 `text`（非 `thinking`）；`ToolCall.arguments` 为 [`serde_json::Value`]。

use std::sync::LazyLock;

use agent_core::{Api, AssistantMessage, ContentBlock, Model, StopReason};
use regex::Regex;
use sha2::{Digest, Sha256};

// 单一 marker 模式真相源（errata 中的 `M`）。
static MARKER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bto=functions\.[A-Za-z_]\w*").unwrap());
// Harmony 协议控制 token（`H`）。
static HARMONY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<\|(start|end|channel|message|call|return)\|>").unwrap());
// Channel-word 邻接（`C`）：marker 紧跟在 channel/role 名后。
static CHANNEL_WORD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:analysis|commentary|assistant|user|system|developer|tool)\s+to=functions\.")
        .unwrap()
});
// Glitch-token 邻接（`G`）。`\x4a` = J，避免本源码自检命中。
static GLITCH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:changedFiles|RTlu|Jsii(?:_commentary)?|\x4aapgolly)\b").unwrap()
});
// Body-channel 级联（`B`）：marker 后跟 ` code`，200 字符内又出现 marker。
static BODY_CASCADE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"to=functions\.\w+\s+code\b[\s\S]{0,200}?to=functions\.").unwrap()
});
// 伪造结果框架（`R`）：marker 后 80 字符内出现 `code_output\nCell N:`。
static FAKE_RESULT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"to=functions\.\w+[\s\S]{0,80}?code_output\s*\nCell\s+\d+:").unwrap()
});
// 围栏行（``` / ~~~）。
static FENCE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:```+|~~~+)").unwrap());

// 语料中出现的非拉丁文字：CJK + ext、Cyrillic、Thai、Georgian、Armenian、Kannada、
// Telugu、Devanagari、Arabic、Malayalam。
const SCRIPT_CLASS: &str = concat!(
    "\u{3400}-\u{4DBF}\u{4E00}-\u{9FFF}\u{F900}-\u{FAFF}",
    "\u{0400}-\u{04FF}\u{0E00}-\u{0E7F}\u{10A0}-\u{10FF}\u{0530}-\u{058F}",
    "\u{0C80}-\u{0CFF}\u{0C00}-\u{0C7F}\u{0900}-\u{097F}\u{0600}-\u{06FF}\u{0D00}-\u{0D7F}",
);
static SCRIPT_RUN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("[{SCRIPT_CLASS}]{{2,}}")).unwrap());

/// 信号类别排序（`H` 单独成类；其余按此序）。
const SIGNAL_ORDER: &[char] = &['M', 'C', 'G', 'S', 'B', 'R', 'T'];

/// 一条泄漏信号的类别集合（`H` 或 `M` + 共生信号）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarmonySignal {
    /// 信号类别（按 [`SIGNAL_ORDER`] 排序；`H` 单独）。
    pub classes: Vec<char>,
    /// 起始字节偏移。
    pub start: usize,
    /// 结束字节偏移（不含）。
    pub end: usize,
    /// 命中文本。
    pub text: String,
}

/// 泄漏出现的表面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarmonySurface {
    /// assistant 文本块。
    AssistantText,
    /// assistant 思考块。
    AssistantThinking,
    /// 工具调用参数。
    ToolArg,
}

/// 一次泄漏检测的结果。
#[derive(Debug, Clone)]
pub struct HarmonyDetection {
    /// 泄漏表面。
    pub surface: HarmonySurface,
    /// 命中的 content 块索引（供恢复/审计定位）。
    pub content_index: Option<usize>,
    /// 工具名（仅 tool_arg）。
    pub tool_name: Option<String>,
    /// 工具调用 id（仅 tool_arg）。
    pub tool_call_id: Option<String>,
    /// 命中信号列表（按位置排序）。
    pub signals: Vec<HarmonySignal>,
}

/// 审计动作（对应 oh-my-pi 双计数器的三条出路）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarmonyAuditAction {
    /// 截断恢复成功（truncate-resume 计数器）。
    TruncateResume,
    /// 放弃当前回复重试（abort-retry 计数器）。
    AbortRetry,
    /// 计数器超限，升级为错误。
    Escalated,
}

/// 隐私安全的审计事件（removed 仅保留 marker/非拉丁字符，源码/密钥以 `·` 脱敏）。
#[derive(Debug, Clone)]
pub struct HarmonyAuditEvent {
    /// 采取的动作。
    pub action: HarmonyAuditAction,
    /// 泄漏表面。
    pub surface: HarmonySurface,
    /// 信号类别标签（如 `M+C`、`H`）。
    pub signal: String,
    /// 当前重试轮次。
    pub retry_n: usize,
    /// 模型 id。
    pub model: String,
    /// provider。
    pub provider: String,
    /// 工具名（仅 tool_arg）。
    pub tool_name: Option<String>,
    /// 移除文本长度。
    pub removed_len: usize,
    /// 移除文本前 8 hex（sha256）。
    pub removed_sha8: String,
    /// 脱敏后的移除文本预览（≤64 字符）。
    pub removed_preview: String,
}

/// 截断恢复产物。
#[derive(Debug, Clone)]
pub struct HarmonyRecoveredToolCall {
    /// 清理后的 assistant 消息（仅含截断后的 tool call）。
    pub message: AssistantMessage,
    /// 被移除的泄漏文本。
    pub removed: String,
}

/// 是否对该模型的回复运行泄漏检测。
///
/// Gyre 判据：[`Api::OpenAiResponses`]（Harmony 是 OpenAI Responses API 的内部协议；
/// Chat Completions / Anthropic / GLM 等均无 Harmony）。默认对整个协议族开启而非枚举
/// 模型 id，避免未来的 gpt-5.6 静默绕过——检测本身廉价，漏检代价不廉价。
#[must_use]
pub fn is_harmony_leak_target(model: &Model) -> bool {
    model.api == Api::OpenAiResponses
}

/// 把信号列表渲染为人类可读标签（如 `H,M+C`）。
#[must_use]
pub fn signal_list_label(signals: &[HarmonySignal]) -> String {
    let mut seen: Vec<String> = Vec::new();
    for s in signals {
        let label = s
            .classes
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join("+");
        if !seen.contains(&label) {
            seen.push(label);
        }
    }
    if seen.is_empty() {
        "none".to_string()
    } else {
        seen.join(",")
    }
}

/// 检测 `text` 中的 Harmony 泄漏。干净返回 `None`。
///
/// 触发规则：`H` 单独触发；`M` 需配至少一个共生信号（`C`/`G`/`S`/`B`/`R`/`T`）。
/// 裸 `M` 不触发——本文档、其测试与 bug 报告 legitimately 携带 marker。
///
/// `tool_arg` surface 更严格：工具参数是任意文件/数据内容，可 legitimately 携带 marker、
/// channel word、harmony token 或非拉丁文字（编辑这些 fixture 就是如此）。唯一可靠的
/// 泄漏信号是「结构合法解析之后的尾随内容」，故 tool_arg 检测**额外要求** `T` 共生信号。
/// 无 `parsed_end` 边界时 `T` 永不置位，tool_arg 扫描保持惰性——绝不硬中止合法工具调用。
#[must_use]
pub fn detect_harmony_leak(
    text: &str,
    surface: HarmonySurface,
    opts: &DetectOpts,
) -> Option<HarmonyDetection> {
    let fences = compute_fence_ranges(text);
    let mut signals: Vec<HarmonySignal> = Vec::new();

    // H：harmony 控制token。
    for m in HARMONY_RE.find_iter(text) {
        if is_inside_fence(&fences, m.start()) {
            continue;
        }
        signals.push(make_signal(&['H'], m.start(), m.end(), m.as_str()));
    }

    // M + 共生信号。
    for m in MARKER_RE.find_iter(text) {
        let start = m.start();
        if is_inside_fence(&fences, start) {
            continue;
        }
        let end = m.end();
        let mut classes = vec!['M'];

        let adjacent_start = start.saturating_sub(64);
        let adjacent_end = (end + 16).min(text.len());
        let adjacent = &text[adjacent_start..adjacent_end];

        let near_start = start.saturating_sub(16);
        let near_end = (end + 16).min(text.len());
        let near = &text[near_start..near_end];

        let forward_end = (start + 240).min(text.len());
        let forward = &text[start..forward_end];

        if CHANNEL_WORD_RE.is_match(adjacent) {
            classes.push('C');
        }
        if GLITCH_RE.is_match(near) {
            classes.push('G');
        }
        if has_script_mismatch_near(text, start, end) {
            classes.push('S');
        }
        if BODY_CASCADE_RE.is_match(forward) {
            classes.push('B');
        }
        if FAKE_RESULT_RE.is_match(forward) {
            classes.push('R');
        }
        if let Some(parsed_end) = opts.parsed_end {
            if start >= parsed_end {
                classes.push('T');
            }
        }

        // M 单独永不触发。
        if classes.len() > 1 {
            signals.push(make_signal(&classes, start, end, m.as_str()));
        }
    }

    if signals.is_empty() {
        return None;
    }
    // tool_arg 无 T 不触发（惰性安全默认）。
    if surface == HarmonySurface::ToolArg && !signals.iter().any(|s| s.classes.contains(&'T')) {
        return None;
    }
    signals.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
    Some(HarmonyDetection {
        surface,
        content_index: opts.content_index,
        tool_name: opts.tool_name.map(ToOwned::to_owned),
        tool_call_id: opts.tool_call_id.map(ToOwned::to_owned),
        signals,
    })
}

/// 扫描 assistant 消息的所有 content 块，返回首个检测。
///
/// 不接受 `parsed_end` 回调：流式后处理无法可靠解析工具 DSL 边界，故 tool_arg surface
/// 保持惰性（安全默认）。assistant_text / assistant_thinking 仍按基础规则检测。
#[must_use]
pub fn detect_in_message(message: &AssistantMessage) -> Option<HarmonyDetection> {
    for (i, block) in message.content.iter().enumerate() {
        let d = match block {
            ContentBlock::Text { text } => detect_harmony_leak(
                text,
                HarmonySurface::AssistantText,
                &DetectOpts {
                    content_index: Some(i),
                    ..DetectOpts::default()
                },
            ),
            ContentBlock::Thinking { text, .. } => detect_harmony_leak(
                text,
                HarmonySurface::AssistantThinking,
                &DetectOpts {
                    content_index: Some(i),
                    ..DetectOpts::default()
                },
            ),
            ContentBlock::ToolCall {
                name,
                id,
                arguments,
                ..
            } => {
                let Some(arg_text) = get_tool_argument_text(arguments) else {
                    continue;
                };
                // tool_name/tool_call_id 借自 message（非 'static），不能经 DetectOpts
                //（&'static str）传递；检测返回后用 owned clone 补设。
                detect_harmony_leak(
                    &arg_text,
                    HarmonySurface::ToolArg,
                    &DetectOpts {
                        content_index: Some(i),
                        ..DetectOpts::default()
                    },
                )
                .map(|mut d| {
                    d.tool_name = Some(name.clone());
                    d.tool_call_id = Some(id.clone());
                    d
                })
            }
        };
        if d.is_some() {
            return d;
        }
    }
    None
}

/// 在污染点所在行首截断工具调用参数，并追加恢复 sentinel。
///
/// 返回清理后的 assistant 消息（仅含截断后的 tool call）与被移除的文本。当工具不可恢复
/// 或截断后会什么都不剩时返回 `None`。`accepts` 判据：`input` 以 `@` 开头（hashline DSL）。
#[must_use]
pub fn recover_tool_call(
    message: &AssistantMessage,
    detection: &HarmonyDetection,
) -> Option<HarmonyRecoveredToolCall> {
    if detection.surface != HarmonySurface::ToolArg {
        return None;
    }
    let idx = detection.content_index?;
    let block = message.content.get(idx)?;
    let ContentBlock::ToolCall {
        id,
        name,
        arguments,
    } = block
    else {
        return None;
    };

    let input = arguments.get("input").and_then(serde_json::Value::as_str)?;
    // hashline DSL 以 @ 开头才可恢复（apply_patch 信封 / JSON 变体不可恢复）。
    if !input.trim_start().starts_with('@') {
        return None;
    }

    let offset = detection.signals.first()?.start;
    const SENTINEL: &str = "\n*** Abort\n";
    let truncated = truncate_at_line_and_append_sentinel(input, offset, SENTINEL)?;

    let mut new_args = arguments.clone();
    new_args["input"] = serde_json::Value::String(truncated.clean);
    let clean_block = ContentBlock::ToolCall {
        id: id.clone(),
        name: name.clone(),
        arguments: new_args,
    };
    let mut clean_message = message.clone();
    clean_message.content = vec![clean_block];
    clean_message.stop_reason = Some(StopReason::ToolUse);
    Some(HarmonyRecoveredToolCall {
        message: clean_message,
        removed: truncated.removed,
    })
}

/// 返回污染子串（审计用，abort 路径）。从首个信号到块末尾。
#[must_use]
pub fn extract_removed(message: &AssistantMessage, detection: &HarmonyDetection) -> String {
    let Some(idx) = detection.content_index else {
        return String::new();
    };
    let Some(block) = message.content.get(idx) else {
        return String::new();
    };
    let start = detection.signals.first().map_or(0, |s| s.start);
    match block {
        ContentBlock::Text { text } => text[start..].to_string(),
        ContentBlock::Thinking { text, .. } => text[start..].to_string(),
        ContentBlock::ToolCall { arguments, .. } => get_tool_argument_text(arguments)
            .map(|t| t[start..].to_string())
            .unwrap_or_default(),
    }
}

/// 构造审计事件。
#[must_use]
pub fn create_audit_event(
    action: HarmonyAuditAction,
    detection: &HarmonyDetection,
    model: &Model,
    retry_n: usize,
    removed: &str,
) -> HarmonyAuditEvent {
    HarmonyAuditEvent {
        action,
        surface: detection.surface,
        signal: signal_list_label(&detection.signals),
        retry_n,
        model: model.id.clone(),
        provider: model.provider.clone(),
        tool_name: detection.tool_name.clone(),
        removed_len: removed.len(),
        removed_sha8: sha8(removed),
        removed_preview: redacted_junk_preview(removed),
    }
}

/// 记录审计事件到 tracing（全字段，完整可观测 + 消除 dead_code）。供 run_loop 双计数器
/// 各分支调用；未来可替换为 on_harmony_leak hook 把事件交给 host。
pub fn log_audit(tag: &str, ev: &HarmonyAuditEvent) {
    tracing::warn!(
        action = ?ev.action,
        surface = ?ev.surface,
        signal = %ev.signal,
        retry_n = ev.retry_n,
        model = %ev.model,
        provider = %ev.provider,
        tool = ?ev.tool_name,
        removed_len = ev.removed_len,
        removed_sha8 = %ev.removed_sha8,
        removed_preview = %ev.removed_preview,
        "{tag}"
    );
}

/// 检测选项。
#[derive(Debug, Clone, Default)]
pub struct DetectOpts {
    /// 结构合法解析的结束字节偏移（仅 tool_arg；其后 marker 置 `T`）。
    pub parsed_end: Option<usize>,
    /// content 块索引。
    pub content_index: Option<usize>,
    /// 工具名。
    pub tool_name: Option<&'static str>,
    /// 工具调用 id。
    pub tool_call_id: Option<&'static str>,
}

// ─── internals ──────────────────────────────────────────────────────────────

fn make_signal(classes: &[char], start: usize, end: usize, text: &str) -> HarmonySignal {
    if classes.first() == Some(&'H') {
        return HarmonySignal {
            classes: vec!['H'],
            start,
            end,
            text: text.to_string(),
        };
    }
    let mut sorted: Vec<char> = Vec::new();
    for &cls in SIGNAL_ORDER {
        if classes.contains(&cls) {
            sorted.push(cls);
        }
    }
    HarmonySignal {
        classes: sorted,
        start,
        end,
        text: text.to_string(),
    }
}

/// 预计算 fenced-code-block 范围（每段为 `[start, end)` 字节区间，位于 ```/~~~ 围栏内）。
fn compute_fence_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut in_fence = false;
    let mut fence_start = 0usize;
    let mut line_start = 0usize;
    while line_start <= text.len() {
        let newline = text[line_start..].find('\n').map(|p| line_start + p);
        let line_end = newline.unwrap_or(text.len());
        let line = &text[line_start..line_end];
        if FENCE_RE.is_match(line) {
            if in_fence {
                ranges.push((fence_start, line_end));
                in_fence = false;
            } else {
                fence_start = line_start;
                in_fence = true;
            }
        }
        let Some(nl) = newline else {
            break;
        };
        line_start = nl + 1;
    }
    if in_fence {
        ranges.push((fence_start, text.len()));
    }
    ranges
}

fn is_inside_fence(ranges: &[(usize, usize)], position: usize) -> bool {
    for &(start, end) in ranges {
        if position >= start && position < end {
            return true;
        }
        if start > position {
            break;
        }
    }
    false
}

/// marker 附近出现非拉丁文字 run，且周围 400 字节窗口 ASCII 占比 ≥ 0.85（异常嵌入）。
fn has_script_mismatch_near(text: &str, start: usize, end: usize) -> bool {
    let near_start = start.saturating_sub(32);
    let near_end = (end + 32).min(text.len());
    let near = &text[near_start..near_end];
    if !SCRIPT_RUN_RE.is_match(near) {
        return false;
    }
    let surrounding_start = start.saturating_sub(200);
    let surrounding_end = (end + 200).min(text.len());
    let surrounding = &text[surrounding_start..surrounding_end];
    if surrounding.is_empty() {
        return false;
    }
    let ascii = surrounding.bytes().filter(|b| *b < 128).count();
    #[allow(clippy::cast_precision_loss)]
    let ratio = ascii as f32 / surrounding.len() as f32;
    ratio >= 0.85
}

/// 工具调用参数文本。`arguments.input` 为 string 时直接用（偏移对齐原始参数）；
/// 否则 JSON 序列化整个 arguments（检测仍触发，但偏移不用于切片）。
fn get_tool_argument_text(arguments: &serde_json::Value) -> Option<String> {
    if let Some(s) = arguments.get("input").and_then(serde_json::Value::as_str) {
        return Some(s.to_string());
    }
    serde_json::to_string(arguments).ok()
}

struct Truncated {
    clean: String,
    removed: String,
}

fn truncate_at_line_and_append_sentinel(
    input: &str,
    offset: usize,
    sentinel: &str,
) -> Option<Truncated> {
    let line_start = if offset == 0 {
        0
    } else {
        // offset 前最后一个 '\n' 之后。
        input[..offset].rfind('\n').map_or(0, |p| p + 1)
    };
    if line_start == 0 {
        return None; // 会切掉全部
    }
    let head = input[..line_start].trim_end();
    if head.is_empty() {
        return None;
    }
    Some(Truncated {
        clean: format!("{head}{sentinel}"),
        removed: input[line_start..].to_string(),
    })
}

fn sha8(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let hash = hasher.finalize();
    hash.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

static PREVIEW_KEEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("[{SCRIPT_CLASS}\\s】【”“…」「、。]")).unwrap());
static PREVIEW_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:to=functions\.[A-Za-z_]\w*|analysis|commentary|assistant|user|system|developer|tool|changedFiles|RTlu|Jsii(?:_commentary)?|\x4aapgolly)").unwrap()
});

/// 隐私安全预览：保留 marker/channel/glitch token、非拉丁字符与 CJK 标点；
/// 其余（潜在源码/密钥）替换为 `·`。≤64 字符。
fn redacted_junk_preview(text: &str) -> String {
    let source: Vec<char> = text.chars().take(64).collect();
    let mut out = String::new();
    let mut i = 0;
    while i < source.len() {
        let rest: String = source[i..].iter().collect();
        if let Some(m) = PREVIEW_TOKEN_RE.find(&rest) {
            out.push_str(m.as_str());
            i += m.as_str().chars().count();
            continue;
        }
        let c = source[i];
        if PREVIEW_KEEP_RE.is_match(&c.to_string()) {
            out.push(c);
        } else {
            out.push('·');
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{Api, Model, Usage};

    fn model_responses() -> Model {
        Model::with_defaults("gpt-5", "openai", Api::OpenAiResponses)
    }

    fn text_msg(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            usage: Usage::default(),
            model: "gpt-5".into(),
            stop_reason: None,
            stop_details: None,
        }
    }

    #[test]
    fn h_signal_trips_alone() {
        // harmony 控制token 单独即触发（assistant_text）。
        let d = detect_harmony_leak(
            "here is <|call|> leaked",
            HarmonySurface::AssistantText,
            &DetectOpts::default(),
        )
        .expect("H 应触发");
        assert_eq!(d.signals[0].classes, vec!['H']);
    }

    #[test]
    fn bare_marker_does_not_trip() {
        // 裸 M（文档/测试 legitimately 携带）不触发。
        let d = detect_harmony_leak(
            "see to=functions.foo in docs",
            HarmonySurface::AssistantText,
            &DetectOpts::default(),
        );
        assert!(d.is_none(), "裸 M 不应触发");
    }

    #[test]
    fn marker_with_channel_word_trips() {
        // M + C（channel word 邻接）触发。
        let d = detect_harmony_leak(
            "assistant to=functions.edit now",
            HarmonySurface::AssistantText,
            &DetectOpts::default(),
        )
        .expect("M+C 应触发");
        assert!(d.signals[0].classes.contains(&'M'));
        assert!(d.signals[0].classes.contains(&'C'));
    }

    #[test]
    fn fence_excludes_marker() {
        // 围栏内的 marker 不触发。
        let text = "```\nto=functions.foo\n```\nclean text";
        let d = detect_harmony_leak(text, HarmonySurface::AssistantText, &DetectOpts::default());
        assert!(d.is_none(), "围栏内 marker 不应触发");
    }

    #[test]
    fn tool_arg_inert_without_parsed_end() {
        // tool_arg 无 parsed_end → T 永不置位 → 惰性，不触发。
        let d = detect_harmony_leak(
            "assistant to=functions.edit @file.txt",
            HarmonySurface::ToolArg,
            &DetectOpts::default(),
        );
        assert!(d.is_none(), "tool_arg 无 T 不应触发");
    }

    #[test]
    fn tool_arg_trips_with_trailing_signal() {
        // tool_arg：marker 在 parsed_end 之后 → T → 触发。
        let text = "valid prefix to=functions.edit leaked tail";
        let parsed_end = "valid prefix ".len();
        let d = detect_harmony_leak(
            text,
            HarmonySurface::ToolArg,
            &DetectOpts {
                parsed_end: Some(parsed_end),
                ..DetectOpts::default()
            },
        )
        .expect("tool_arg + T 应触发");
        assert!(d.signals[0].classes.contains(&'T'));
    }

    #[test]
    fn is_target_responses_only() {
        assert!(is_harmony_leak_target(&model_responses()));
        let mut m = model_responses();
        m.api = Api::OpenAiCompletions;
        assert!(!is_harmony_leak_target(&m));
    }

    #[test]
    fn signal_label_format() {
        let signals = vec![
            HarmonySignal {
                classes: vec!['H'],
                start: 0,
                end: 1,
                text: "x".into(),
            },
            HarmonySignal {
                classes: vec!['M', 'C'],
                start: 2,
                end: 3,
                text: "y".into(),
            },
        ];
        assert_eq!(signal_list_label(&signals), "H,M+C");
    }

    #[test]
    fn sha8_is_8_hex() {
        let h = sha8("hello");
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn redacted_preview_masks_source() {
        // 源码字符替换为 ·，marker 保留。
        let p = redacted_junk_preview("to=functions.edit secret_code_here");
        assert!(p.contains("to=functions.edit"));
        assert!(p.contains('·'), "源码应被脱敏");
    }

    #[test]
    fn detect_in_message_scans_text_block() {
        let msg = text_msg("leaked <|return|> token");
        let d = detect_in_message(&msg).expect("应检测到 text 块泄漏");
        assert_eq!(d.surface, HarmonySurface::AssistantText);
    }

    #[test]
    fn recover_truncates_tool_arg_at_line() {
        // hashline DSL（@ 开头）的 tool_arg，marker 在 parsed_end 后 → T → 检测 + 可恢复。
        let input = "@file.rs\nvalid line\nto=functions.edit leaked tail";
        let parsed_end = "@file.rs\nvalid line\n".len();
        let d = detect_harmony_leak(
            input,
            HarmonySurface::ToolArg,
            &DetectOpts {
                parsed_end: Some(parsed_end),
                content_index: Some(0),
                tool_name: Some("edit"),
                tool_call_id: Some("call_1"),
                ..DetectOpts::default()
            },
        )
        .expect("应检测到 tool_arg 泄漏");

        let msg = AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "call_1".into(),
                name: "edit".into(),
                arguments: serde_json::json!({ "input": input }),
            }],
            usage: Usage::default(),
            model: "gpt-5".into(),
            stop_reason: Some(StopReason::ToolUse),
            stop_details: None,
        };
        let recovered = recover_tool_call(&msg, &d).expect("应可恢复");
        assert!(recovered.removed.contains("leaked tail"));
        let ContentBlock::ToolCall { arguments, .. } = &recovered.message.content[0] else {
            panic!("应为 tool call");
        };
        let clean = arguments["input"].as_str().expect("input 应为 string");
        assert!(
            clean.ends_with("\n*** Abort\n"),
            "应以 sentinel 结尾: {clean:?}"
        );
        assert!(!clean.contains("leaked"), "泄漏文本应被移除");
    }

    #[test]
    fn recover_refuses_non_hashline_input() {
        // 非 @ 开头的 input 不可恢复。
        let input = "valid prefix to=functions.edit leaked";
        let d = detect_harmony_leak(
            input,
            HarmonySurface::ToolArg,
            &DetectOpts {
                parsed_end: Some("valid prefix ".len()),
                content_index: Some(0),
                ..DetectOpts::default()
            },
        )
        .expect("应检测到");
        let msg = AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "c".into(),
                name: "edit".into(),
                arguments: serde_json::json!({ "input": input }),
            }],
            usage: Usage::default(),
            model: "gpt-5".into(),
            stop_reason: Some(StopReason::ToolUse),
            stop_details: None,
        };
        assert!(recover_tool_call(&msg, &d).is_none(), "非 @ 开头不可恢复");
    }
}
