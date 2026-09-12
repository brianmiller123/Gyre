//! H5 流式守卫（移植 oh-my-pi `session/stream-guards.ts` 语义，快照 v18.1.2）。
//!
//! 三个独立组件：
//! 1. [`ToolCallLoopGuard`] —— 跨轮同参工具循环检测（omp `ToolCallLoopGuard`）：
//!    连续第 N 轮（阈值默认 5）发出**恰好一个**参数等价的未豁免工具调用即触发
//!    一次重定向注入；非「恰好一个」的轮次清零连击。
//! 2. [`GeminiHeaderRunDetector`] —— Gemini 推理摘要标题连跑检测（omp 同名）：
//!    thinking 流内非空标题行累计达 24 触发一次（闩锁）；文本/工具调用开始即重置。
//! 3. [`EditStreamPrecheck`] —— apply_hashline 流式预检（omp `StreamingEditGuard`
//!    的 Gyre 适配）：流中扫描完整 `[path#hash]` 段头做生成文件文件名拦截；
//!    调用结束即试解析，必败（语法错误）即中断本轮。
//!
//! 与 omp 的偏差（有意为之）：
//! - omp 编辑预检还含 removed-lines 内容校验与 patch preview；Gyre 的 hashline 是
//!   行号锚定格式且自带 stale-hash 快照回放恢复（`agent-hashline::recovery`），
//!   内容级预检会与恢复机制打架，故只保留「生成文件 + 语法必败」两个零误报检查；
//!   生成文件的**内容 marker** 检测放在工具执行前兜底（见 engine 阶段一），
//!   不进流式热路径。
//! - omp 触发后 abort 整轮等用户重新发起；Gyre 沿用 TTSR 管线语义——丢弃部分
//!   输出、注入错误消息、模型在同一 run 内自我修正（受 max_turns 硬上限保护）。

use std::collections::{HashMap, HashSet};

use agent_core::message::{AssistantMessage, ContentBlock, ToolResultMessage};
use agent_core::tool::ToolResult;

// ============================================================================
// ToolCallLoopGuard（跨轮工具循环）
// ============================================================================

/// 跨轮循环检测结果（对齐 omp `RepeatedToolCallDetection`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolLoopDetection {
    /// 重复的工具名。
    pub tool_name: String,
    /// 连续次数（触发时恒等于阈值）。
    pub count: usize,
    /// 上一轮结果文本摘要（≤200 字符）。
    pub result_summary: String,
    /// 参数规范化 JSON 摘要（≤400 字符）。
    pub arguments_summary: String,
}

/// 结果摘要长度上限（对齐 omp `RESULT_SUMMARY_LIMIT`）。
const RESULT_SUMMARY_LIMIT: usize = 200;
/// 参数摘要长度上限（对齐 omp `ARGUMENT_SUMMARY_LIMIT`）。
const ARGUMENT_SUMMARY_LIMIT: usize = 400;

/// 跨轮同参工具循环守卫（对齐 omp `ToolCallLoopGuard`）。
#[derive(Debug, Clone)]
pub struct ToolCallLoopGuard {
    threshold: usize,
    exempt: HashSet<String>,
    last_hash: Option<String>,
    count: usize,
}

impl ToolCallLoopGuard {
    /// 构造：阈值向下限 1 截断（对齐 omp `Math.max(1, Math.trunc(threshold))`）。
    #[must_use]
    pub fn new(threshold: usize, exempt_tools: &[String]) -> Self {
        Self {
            threshold: threshold.max(1),
            exempt: exempt_tools
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect(),
            last_hash: None,
            count: 0,
        }
    }

    /// 记录一轮并检测重复。轮内**恰好一个**未豁免工具调用才参与连击，
    /// 其余情况（0 个、多个、豁免工具）清零状态（对齐 omp :82-85）。
    /// 触发用严格等于（`count == threshold`）：每次连击 run 恰好触发一次，
    /// 继续重复不再触发（对齐 omp :98）。
    pub fn record_turn(
        &mut self,
        assistant: &AssistantMessage,
        tool_results: &[ToolResultMessage],
    ) -> Option<ToolLoopDetection> {
        let mut call_id: Option<&String> = None;
        let mut call_name: Option<&str> = None;
        let mut call_args: Option<&serde_json::Value> = None;
        let mut call_count = 0usize;
        for b in &assistant.content {
            if let ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } = b
            {
                call_count += 1;
                call_id = Some(id);
                call_name = Some(name);
                call_args = Some(arguments);
            }
        }
        // 非「恰好一个」或工具豁免 → 清零。
        if call_count != 1 || call_name.is_some_and(|n| self.exempt.contains(n)) {
            self.last_hash = None;
            self.count = 0;
            return None;
        }
        let name = call_name.expect("call_count == 1 保证 Some");
        let canonical = canonicalize_args(call_args.expect("call_count == 1 保证 Some"));
        let hash = format!("{name}:{canonical}");
        if self.last_hash.as_deref() == Some(hash.as_str()) {
            self.count += 1;
        } else {
            self.last_hash = Some(hash);
            self.count = 1;
        }
        if self.count != self.threshold {
            return None;
        }
        Some(ToolLoopDetection {
            tool_name: name.to_string(),
            count: self.count,
            result_summary: summarize_tool_result(tool_results, call_id.expect("同上")),
            arguments_summary: summarize_text(&canonical, ARGUMENT_SUMMARY_LIMIT),
        })
    }
}

/// 参数规范化：递归排序对象键并剔除 harness 注入的 intent 字段（对齐 omp
/// `canonicalizeToolCallValue`；键序不敏感 + intent 剔除后 JSON 等价即同一调用）。
#[must_use]
pub fn canonicalize_args(value: &serde_json::Value) -> String {
    fn canon(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(canon).collect()),
            serde_json::Value::Object(m) => {
                let mut keys: Vec<&String> = m
                    .keys()
                    .filter(|k| k.as_str() != "i" && k.as_str() != "__intent")
                    .collect();
                keys.sort();
                serde_json::Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), canon(&m[k])))
                        .collect(),
                )
            }
            other => other.clone(),
        }
    }
    canon(value).to_string()
}
// ============================================================================
// 注入文案（对齐 oh-my-pi 模板逐字：tool-call-loop-redirect.md /
// gemini-tool-call-reminder.md）
// ============================================================================

/// 渲染跨轮工具循环重定向注入文本（对齐 omp `renderToolCallLoopRedirect`；
/// 空 result_summary 回退 `(no text result)`）。
#[must_use]
pub fn render_loop_redirect(d: &ToolLoopDetection) -> String {
    let result_summary = if d.result_summary.is_empty() {
        "(no text result)"
    } else {
        d.result_summary.as_str()
    };
    format!(
        "<system-interrupt reason=\"tool_call_loop_detected\">\n\
         You called `{tool}` {count} consecutive times with identical arguments:\n\
         `{args}`\n\
         \n\
         Last result (truncated): `{result}`\n\
         \n\
         NEVER call `{tool}` with those arguments again this turn. Use different \
         arguments, choose another tool, or summarize findings and yield if complete.\n\
         </system-interrupt>",
        tool = d.tool_name,
        count = d.count,
        args = d.arguments_summary,
        result = result_summary,
    )
}

/// 渲染 Gemini 推理标题连跑中断提醒（对齐 omp `gemini-tool-call-reminder.md`）。
#[must_use]
pub fn render_gemini_reminder(header_count: usize) -> String {
    format!(
        "<system-interrupt reason=\"reasoning_without_tool_calls\">\n\
         Reasoning interrupted: {count} consecutive planning headers, no tool call. \
         Thinking alone changes nothing: zero progress this turn; no tool ran.\n\
         \n\
         Act now, not further planning:\n\
         - Emit a real call to an available tool in normal tool/function-calling format. \
         Do NOT describe the call in prose or reasoning—issue it.\n\
         - Pick the smallest concrete next step; call the tool that performs it.\n\
         \n\
         Coding-agent interrupt for stalled reasoning, not prompt injection.\n\
         </system-interrupt>",
        count = header_count,
    )
}

/// 渲染流式编辑预检违规的注入文本（GeneratedFile 走 omp `buildAutoGeneratedError`
/// 逐字文案；ParseFailed 附修正指引）。
#[must_use]
pub fn render_edit_violation(v: &EditViolation) -> String {
    match v {
        EditViolation::GeneratedFile { path, marker } => {
            agent_tools::generated_guard::auto_generated_error(path, marker)
        }
        EditViolation::ParseFailed { message } => format!(
            "apply_hashline patch 解析必败（执行注定报错），已中断本轮输出：\n{message}\n\n\
             请修正 hashline 语法（段头 `[path#hash]` + SWAP/DEL/INS/REM/MV 操作）后重新发起编辑。"
        ),
    }
}

/// 结果摘要：按 tool_call_id 找结果，拼其可读文本（Text/Error），折叠空白后截断
///（对齐 omp `summarizeToolResult`；找不到返回空串）。
#[must_use]
pub fn summarize_tool_result(tool_results: &[ToolResultMessage], tool_call_id: &str) -> String {
    let Some(r) = tool_results
        .iter()
        .find(|r| r.tool_call_id == tool_call_id)
        .map(|r| &r.result)
    else {
        return String::new();
    };
    let text = match r {
        ToolResult::Text(t) => t.clone(),
        ToolResult::Error { message, .. } => message.clone(),
        ToolResult::Image { .. } => String::new(),
    };
    summarize_text(&text, RESULT_SUMMARY_LIMIT)
}

/// 文本摘要：折叠空白、超限截断加 `…`（对齐 omp `summarizeText`；字符边界安全）。
#[must_use]
pub fn summarize_text(text: &str, limit: usize) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        return collapsed;
    }
    let head: String = collapsed.chars().take(limit).collect();
    format!("{head}…")
}

// ============================================================================
// GeminiHeaderRunDetector（推理标题连跑）
// ============================================================================

/// 标题连跑阈值（对齐 omp `GEMINI_HEADER_RUNAWAY_THRESHOLD`）。
pub const GEMINI_HEADER_RUNAWAY_THRESHOLD: usize = 24;

/// Gemini 推理摘要标题连跑检测器（对齐 omp `GeminiHeaderRunDetector`）。
///
/// 计数语义：只有非空标题行计数；标题之间的普通段落行**不清零也不计数**
///（Gemini 每个思考单元是「标题+段落」，run 长度即摘要数）。跨 push 累积，
/// 尾部不完整行留在缓冲。`fired` 闩锁保证每次 run 至多触发一次。
#[derive(Debug, Clone, Default)]
pub struct GeminiHeaderRunDetector {
    pending: String,
    count: usize,
    fired: bool,
}

impl GeminiHeaderRunDetector {
    /// 喂入一个思考增量；本次 push 使计数首次达到阈值时返回 `true`（闩锁后恒 false）。
    pub fn push(&mut self, delta: &str) -> bool {
        if self.fired || delta.is_empty() {
            return false;
        }
        let mut buf = std::mem::take(&mut self.pending);
        buf.push_str(delta);
        while let Some(pos) = buf.find('\n') {
            let line: String = buf.drain(..=pos).collect();
            let line = line.trim();
            if !line.is_empty() && is_reasoning_summary_header(line) {
                self.count += 1;
                if self.count >= GEMINI_HEADER_RUNAWAY_THRESHOLD {
                    self.fired = true;
                    self.pending = buf;
                    return true;
                }
            }
        }
        self.pending = buf;
        false
    }

    /// 当前 run 的标题行数。
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }

    /// 清空状态（离开推理通道——文本/工具调用开始——时调用；唯一重新武装途径）。
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// 标题判定（对齐 omp `isReasoningSummaryHeader`，调用方已 trim）：
/// `^#{1,6}[ \t]+\S` 或 `^\*{2,3}.*\*{2,3}$`（整行 bold / bold-italic）。
#[must_use]
pub fn is_reasoning_summary_header(line: &str) -> bool {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        let rest = &line[hashes..];
        let pad = rest.chars().take_while(|&c| c == ' ' || c == '\t').count();
        if pad >= 1
            && rest[pad..]
                .chars()
                .next()
                .is_some_and(|c| !c.is_whitespace())
        {
            return true;
        }
    }
    // bold：首 2-3 个 `*`、尾 2-3 个 `*`、中间任意（`.*` 可吸收星号）。
    let chars: Vec<char> = line.chars().collect();
    for start in 2..=3usize {
        for end in 2..=3usize {
            if chars.len() >= start + end
                && chars[..start].iter().all(|&c| c == '*')
                && chars[chars.len() - end..].iter().all(|&c| c == '*')
            {
                return true;
            }
        }
    }
    false
}

/// 是否 gemini 系模型（omp 走 `model.identity.class === "gemini"`；Gyre Model 尚无
/// class 字段，按 provider/id 启发式：`google` provider 或 id 含 `gemini`）。
#[must_use]
pub fn is_gemini_model(model: &agent_core::model::Model) -> bool {
    model.provider.eq_ignore_ascii_case("google") || model.id.to_lowercase().contains("gemini")
}

// ============================================================================
// EditStreamPrecheck（apply_hashline 流式预检）
// ============================================================================

/// 编辑流式预检违规。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditViolation {
    /// 段头路径命中生成文件名模式（流中早期拦截）。
    GeneratedFile {
        /// 工作区相对路径。
        path: String,
        /// 命中的文件名 marker。
        marker: String,
    },
    /// 调用结束、补丁语法必败（工具执行注定报错）。
    ParseFailed {
        /// 解析错误详情。
        message: String,
    },
}

#[derive(Debug, Default)]
struct EditStreamState {
    /// 原始参数 JSON 累积缓冲。
    json: String,
    /// `patch` 字段值的增量解码结果（真实换行，供段头行锚定扫描）。
    decoded_patch: String,
    /// 下次解码起点（`json` 字节偏移）；0 表示值起点尚未出现。
    decode_from: usize,
    /// patch 字符串值已闭合（其后不再有可扫描内容）。
    patch_closed: bool,
    /// 已做过生成文件判定的路径（去重，避免逐增量重复命中）。
    checked_paths: HashSet<String>,
}

impl EditStreamState {
    /// 增量解码 `patch` 字符串值：从 [`Self::decode_from`] 起逐字符消费 JSON 转义，
    /// 直至未转义闭合引号（`patch_closed`）或缓冲耗尽（等待下一增量）。
    fn advance_decode(&mut self) {
        if self.patch_closed {
            return;
        }
        if self.decode_from == 0 {
            static PATCH_VALUE_START: std::sync::LazyLock<regex::Regex> =
                std::sync::LazyLock::new(|| {
                    // 对象内 "patch" 键（前置 `{`/`,`）的字符串值起始引号。
                    regex::Regex::new(r#"[,{]\s*"patch"\s*:\s*""#).expect("patch 键正则合法")
                });
            let Some(m) = PATCH_VALUE_START.find(&self.json) else {
                return;
            };
            self.decode_from = m.end();
        }
        let bytes = self.json.as_bytes();
        let mut i = self.decode_from;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    self.patch_closed = true;
                    i += 1;
                    break;
                }
                b'\\' => {
                    if i + 1 >= bytes.len() {
                        break; // 不完整转义，等下一增量
                    }
                    let esc = bytes[i + 1];
                    match esc {
                        b'n' => self.decoded_patch.push('\n'),
                        b't' => self.decoded_patch.push('\t'),
                        b'r' => self.decoded_patch.push('\r'),
                        b'b' => self.decoded_patch.push('\u{8}'),
                        b'f' => self.decoded_patch.push('\u{c}'),
                        b'"' | b'\\' | b'/' => self.decoded_patch.push(esc as char),
                        b'u' => {
                            if i + 6 > bytes.len() {
                                break; // \uXXXX 不完整，等下一增量
                            }
                            let Ok(hex) = std::str::from_utf8(&bytes[i + 2..i + 6]) else {
                                i += 6;
                                continue;
                            };
                            // 代理对（emoji 等）降级为替换符：段头扫描不受影响。
                            let cp = u32::from_str_radix(hex, 16).unwrap_or(0xFFFD);
                            self.decoded_patch
                                .push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        }
                        _ => self.decoded_patch.push(esc as char),
                    }
                    i += if esc == b'u' { 6 } else { 2 };
                }
                _ => {
                    let rest = &self.json[i..];
                    let Some(width) = char_len_at(rest) else {
                        break; // 多字节字符被截断，等下一增量
                    };
                    self.decoded_patch.push_str(&rest[..width]);
                    i += width;
                }
            }
        }
        self.decode_from = i;
    }
}

/// 首字节处的完整 UTF-8 字符长度；被截断时返回 `None`。
fn char_len_at(s: &str) -> Option<usize> {
    let b = *s.as_bytes().first()?;
    let need = if b < 0x80 {
        1
    } else if b >= 0xF0 {
        4
    } else if b >= 0xE0 {
        3
    } else if b >= 0xC0 {
        2
    } else {
        return None;
    };
    (s.len() >= need).then_some(need)
}

/// apply_hashline 流式预检器：按 tool-call id 累积参数增量。
#[derive(Debug, Default)]
pub struct EditStreamPrecheck {
    streams: HashMap<String, EditStreamState>,
}

impl EditStreamPrecheck {
    /// 工具调用开始（仅记录；apply_hashline 之外的调用不累积）。
    pub fn on_start(&mut self, id: &str, name: &str) {
        if name == "apply_hashline" {
            self.streams
                .insert(id.to_string(), EditStreamState::default());
        }
    }

    /// 参数增量：累积、增量解码 patch 值、扫描新出现的完整段头。
    /// 返回违规即应中断流。
    pub fn on_delta(&mut self, id: &str, fragment: &str) -> Option<EditViolation> {
        let state = self.streams.get_mut(id)?;
        state.json.push_str(fragment);
        state.advance_decode();
        for (path, _hash) in section_headers(&state.decoded_patch) {
            if state.checked_paths.insert(path.clone()) {
                if let Some(marker) = agent_tools::generated_guard::is_auto_generated_path(&path) {
                    return Some(EditViolation::GeneratedFile { path, marker });
                }
            }
        }
        None
    }

    /// 调用结束：参数 JSON 完整，试解析补丁做语法必败判定。
    pub fn on_end(&mut self, id: &str) -> Option<EditViolation> {
        let state = self.streams.remove(id)?;
        if state.json.trim().is_empty() {
            return None;
        }
        let args: serde_json::Value = match serde_json::from_str(&state.json) {
            Ok(v) => v,
            Err(e) => {
                return Some(EditViolation::ParseFailed {
                    message: format!("参数 JSON 解析失败: {e}"),
                });
            }
        };
        let Some(patch) = args.get("patch").and_then(serde_json::Value::as_str) else {
            // 无 patch 字段：工具自身会走 fallback path 或报参数错误；不预判。
            return None;
        };
        if let Err(e) = agent_hashline::parse_hashline(patch) {
            return Some(EditViolation::ParseFailed { message: e });
        }
        None
    }
}

/// 从（可能不完整的）补丁文本中提取完整段头 `[path#hash]`（整行匹配，保守：
/// 行首 `[` 到行尾 `]`，path 不含 `]`/换行/`#`；hash 段可选）。
fn section_headers(text: &str) -> Vec<(String, String)> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?m)^\[([^]\n#]+)(#[^\]\n]*)?\][ \t]*$").expect("段头正则合法")
    });
    RE.captures_iter(text)
        .map(|c| {
            (
                c.get(1)
                    .map(|m| m.as_str().trim().to_string())
                    .unwrap_or_default(),
                c.get(2)
                    .map(|m| m.as_str().trim_start_matches('#').to_string())
                    .unwrap_or_default(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_with_calls(calls: Vec<(&str, &str, serde_json::Value)>) -> AssistantMessage {
        AssistantMessage {
            content: calls
                .into_iter()
                .map(|(id, name, arguments)| ContentBlock::ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments,
                    signature: None,
                })
                .collect(),
            usage: agent_core::Usage::default(),
            model: "m".into(),
            stop_reason: None,
            stop_details: None,
        }
    }

    fn result(id: &str, text: &str) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: id.to_string(),
            result: ToolResult::Text(text.to_string()),
        }
    }

    #[test]
    fn loop_guard_triggers_exactly_once_at_threshold() {
        let mut g = ToolCallLoopGuard::new(3, &[]);
        let args = serde_json::json!({"path": "a.rs", "diff": "x"});
        for _ in 0..2 {
            let a = assistant_with_calls(vec![("t1", "read_file", args.clone())]);
            assert!(g.record_turn(&a, &[]).is_none());
        }
        let a = assistant_with_calls(vec![("t1", "read_file", args.clone())]);
        let d = g.record_turn(&a, &[]).expect("第 3 次触发");
        assert_eq!(d.count, 3);
        assert_eq!(d.tool_name, "read_file");
        // 第 4 次不再触发（严格等于语义）
        assert!(g.record_turn(&a, &[]).is_none());
    }

    #[test]
    fn loop_guard_resets_on_different_or_multi_call() {
        let mut g = ToolCallLoopGuard::new(2, &[]);
        let a1 = assistant_with_calls(vec![("t1", "read_file", serde_json::json!({"p": 1}))]);
        assert!(g.record_turn(&a1, &[]).is_none());
        // 多工具调用轮清零
        let a2 = assistant_with_calls(vec![
            ("t1", "read_file", serde_json::json!({"p": 1})),
            ("t2", "hub", serde_json::json!({})),
        ]);
        assert!(g.record_turn(&a2, &[]).is_none());
        // 换参数重新计数
        let a3 = assistant_with_calls(vec![("t1", "read_file", serde_json::json!({"p": 1}))]);
        assert!(g.record_turn(&a3, &[]).is_none());
        let a4 = assistant_with_calls(vec![("t1", "read_file", serde_json::json!({"p": 1}))]);
        assert!(g.record_turn(&a4, &[]).is_some(), "连击到 2 触发");
    }

    #[test]
    fn loop_guard_canonicalizes_key_order_and_skips_intent() {
        let mut g = ToolCallLoopGuard::new(2, &[]);
        let a1 = assistant_with_calls(vec![(
            "t1",
            "edit",
            serde_json::json!({"diff": "d", "i": "skip-me", "path": "x.rs"}),
        )]);
        let a2 = assistant_with_calls(vec![(
            "t1",
            "edit",
            serde_json::json!({"path": "x.rs", "diff": "d"}),
        )]);
        assert!(g.record_turn(&a1, &[]).is_none());
        assert!(
            g.record_turn(&a2, &[]).is_some(),
            "键序不同 + intent 剔除后视作同一调用"
        );
    }

    #[test]
    fn loop_guard_exempt_tools_never_trigger() {
        let mut g = ToolCallLoopGuard::new(2, &["hub".to_string()]);
        let a = assistant_with_calls(vec![("t1", "hub", serde_json::json!({"op": "wait"}))]);
        assert!(g.record_turn(&a, &[]).is_none());
        assert!(g.record_turn(&a, &[]).is_none());
    }

    #[test]
    fn loop_guard_summarizes_result_text() {
        let mut g = ToolCallLoopGuard::new(2, &[]);
        let a = assistant_with_calls(vec![("t1", "hub", serde_json::json!({"op": "ps"}))]);
        let results = vec![result("t1", "line one\nline\ttwo")];
        assert!(g.record_turn(&a, &results).is_none());
        let d = g.record_turn(&a, &results).expect("连击到 2 触发");
        assert_eq!(d.result_summary, "line one line two");
    }

    #[test]
    fn gemini_detector_threshold_latch_and_pending() {
        let mut d = GeminiHeaderRunDetector::default();
        let headers = "## A\n".repeat(23);
        assert!(!d.push(&headers));
        // 尾部跨 push：第 24 个标题行分两片到达
        assert!(!d.push("## B"));
        assert!(d.push("\n"), "第 24 行凑齐触发");
        assert!(!d.push("## C\n"), "闩锁后恒 false");
        assert_eq!(d.count(), 24);
        d.reset();
        assert_eq!(d.count(), 0);
        assert!(!d.push("## D\n"), "reset 后重新计数");
    }

    #[test]
    fn gemini_detector_plain_lines_do_not_count_or_reset() {
        let mut d = GeminiHeaderRunDetector::default();
        let body = "## H\nplain paragraph text\n".repeat(30);
        assert!(d.push(&body), "标题行数达阈值（段落行不影响）");
    }

    #[test]
    fn gemini_detector_resets_on_text_or_toolcall() {
        let mut d = GeminiHeaderRunDetector::default();
        assert!(!d.push(&"## H\n".repeat(10)));
        d.reset(); // engine 在 TextDelta/ToolCallStart 时调用
        // 若状态泄漏，10+23=33 早超阈值；正确行为：重置后 23 不触发。
        assert!(!d.push(&"## H\n".repeat(23)));
        assert!(d.push("## final\n"), "重置后第 24 行触发");
        assert_eq!(d.count(), 24);
    }

    #[test]
    fn header_predicate_matches_atx_and_bold() {
        assert!(is_reasoning_summary_header("## Title"));
        assert!(is_reasoning_summary_header("# t"));
        assert!(is_reasoning_summary_header("###### deep"));
        assert!(is_reasoning_summary_header("###\tTab"));
        assert!(is_reasoning_summary_header("**Bold**"));
        assert!(is_reasoning_summary_header("***Bold Italic***"));
        assert!(is_reasoning_summary_header("****"));
        assert!(!is_reasoning_summary_header("####### seven"));
        assert!(!is_reasoning_summary_header("#no space"));
        assert!(!is_reasoning_summary_header("inline **bold** inline"));
        assert!(!is_reasoning_summary_header(""));
    }

    #[test]
    fn precheck_flags_generated_path_midstream() {
        let mut p = EditStreamPrecheck::default();
        p.on_start("c1", "apply_hashline");
        assert!(p.on_delta("c1", "{\"patch\": \"").is_none());
        let v = p.on_delta("c1", "[proto/api.pb.js#ab12]\\nDEL 1-2\\n");
        assert!(matches!(
            v,
            Some(EditViolation::GeneratedFile { ref path, ref marker })
                if path == "proto/api.pb.js" && marker == "api.pb.js"
        ));
    }

    #[test]
    fn precheck_parse_failure_on_end() {
        let mut p = EditStreamPrecheck::default();
        p.on_start("c1", "apply_hashline");
        p.on_delta("c1", "{\"patch\": \"[a.rs#ab12]\\nBOGUS OP x\\n\"}");
        let v = p.on_end("c1");
        assert!(
            matches!(v, Some(EditViolation::ParseFailed { .. })),
            "语法必败触发: {v:?}"
        );
    }

    #[test]
    fn precheck_valid_patch_passes() {
        let mut p = EditStreamPrecheck::default();
        p.on_start("c1", "apply_hashline");
        // 真实 hashline 语法（对齐 parser 测试样例）。
        p.on_delta(
            "c1",
            "{\"patch\": \"[a.rs#AB12]\\nSWAP 1.=1:\\n+ALPHA\\nDEL 3\\nINS.POST 2:\\n+middle\\n\"}",
        );
        assert!(p.on_end("c1").is_none());
    }

    #[test]
    fn precheck_decodes_escapes_across_delta_boundaries() {
        let mut p = EditStreamPrecheck::default();
        p.on_start("c1", "apply_hashline");
        // 转义换行分片跨越增量边界：`\\n` 拆成 `\` + `n` 两片。
        assert!(
            p.on_delta("c1", "{\"patch\": \"[safe.rs]\\nSWAP 1.=1:")
                .is_none()
        );
        assert!(p.on_delta("c1", "\\").is_none());
        // 转义 `\n` 拆成 `\` + `n` 跨增量到达；末片闭合 JSON 引号。
        assert!(p.on_delta("c1", "n+SWAP body line\\n\"}").is_none());
        assert!(p.on_end("c1").is_none());
    }

    #[test]
    fn precheck_ignores_non_edit_tools() {
        let mut p = EditStreamPrecheck::default();
        p.on_start("c1", "read_file");
        assert!(p.on_delta("c1", "{\"path\": \"dist/x.min.js\"}").is_none());
        assert!(p.on_end("c1").is_none());
    }

    #[test]
    fn section_headers_scan_is_line_anchored() {
        let text = "前缀 [a.rs#ab12] 不在行首不算\n[b.rs]\n[c.rs#zz]";
        let headers = section_headers(text);
        assert_eq!(headers.len(), 2, "{headers:?}");
        assert_eq!(headers[0].0, "b.rs");
        assert_eq!(headers[1].0, "c.rs");
    }
}
