//! 替换边界修复：分隔符平衡 + 缺闭合符保留。
//!
//! 移植自上游 [`repairReplacementBoundaries`](../../../third/oh-my-pi/packages/hashline/src/apply.ts:804)
//! 及其全部 helper（`computeDelimiterBalance` / `findBoundaryEcho` /
//! `findOneSidedBoundaryEcho` / `findDuplicateSuffix` / `findDuplicatePrefix` /
//! `findDroppedSuffixClosers` 等）。
//!
//! ## 为何是「专项重写」而非「搬运扁平 AppliedEdit 管线」
//! Gyre 的 [`Hunk::Replace`](crate::types::Hunk::Replace) `{ start, end, body }`
//! 天然对应上游一个 `ReplacementGroup`（`payload` = `body`，`startLine`/`endLine`
//! = `start`/`end`）。上游需用 `findReplacementGroup` 从扁平的每行一个 insert/delete
//! 列表里**重组**出 group，而 Gyre 在解析期即拿到 group——因此无需引入上游的
//! `AppliedEdit[]` + `parsePatch/applyEdits` 管线，直接在 Hunk 层做两 pass 修复。
//!
//! ## 两 pass
//! 1. **局部修复**（每 group 独立）：两边边界回声去重、单边回声去重、重复头/尾去重。
//! 2. **整 patch 缺闭合符保留**：当某 group 删除的尾部结构闭合符在整 patch 分隔符
//!    残差下「应保留」时，缩短其删除区间（不删那些行），防止结构塌陷。
//!
//! 所有判定均保守：仅在分隔符平衡能精确解释失配时才动手，否则原样保留（安全方向）。

use std::collections::{BTreeMap, HashSet};

use crate::types::Anchor;

// ═══════════════════════════════════════════════════════════════════════════
// 分隔符平衡（忽略字符串 / 注释内的括号）
// ═══════════════════════════════════════════════════════════════════════════

/// `()` / `[]` / `{}` 三元组净计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DelimiterBalance {
    paren: i32,
    bracket: i32,
    brace: i32,
}

impl DelimiterBalance {
    const fn zero() -> Self {
        Self {
            paren: 0,
            bracket: 0,
            brace: 0,
        }
    }

    /// `self - b`。
    const fn delta(self, b: Self) -> Self {
        Self {
            paren: self.paren - b.paren,
            bracket: self.bracket - b.bracket,
            brace: self.brace - b.brace,
        }
    }

    /// 取反。
    const fn negate(self) -> Self {
        Self {
            paren: -self.paren,
            bracket: -self.bracket,
            brace: -self.brace,
        }
    }

    /// `self + b`。
    const fn sum(self, b: Self) -> Self {
        Self {
            paren: self.paren + b.paren,
            bracket: self.bracket + b.bracket,
            brace: self.brace + b.brace,
        }
    }

    const fn is_zero(self) -> bool {
        self.paren == 0 && self.bracket == 0 && self.brace == 0
    }

    /// `self` 是否在三分量上各自「覆盖」`target`（同号且绝对值不小于）。
    fn covers(self, target: Self) -> bool {
        covers_component(self.paren, target.paren)
            && covers_component(self.bracket, target.bracket)
            && covers_component(self.brace, target.brace)
    }
}

/// 单分量覆盖判定：`target==0` 恒满足；否则需同号且 `|candidate| >= |target|`。
const fn covers_component(candidate: i32, target: i32) -> bool {
    if target == 0 {
        return true;
    }
    (candidate > 0) == (target > 0) && candidate.abs() >= target.abs()
}

/// 跨多行的 `()` / `[]` / `{}` 净增减，跳过行注释 `//`、块注释 `/* */`、
/// 字符串 / 模板字面量内的括号。块注释与反引号模板状态跨行；`"` / `'` 行末重置
/// （它们不能跨行）。语言无关近似：无法分类的构造（如正则字面量）按字面计数，
/// 这只会**抑制**修复（安全方向），不会强行触发。
fn compute_delimiter_balance<L: AsRef<str>>(lines: &[L]) -> DelimiterBalance {
    let mut bal = DelimiterBalance::zero();
    let mut in_block_comment = false;
    for line in lines {
        let b = line.as_ref().as_bytes();
        let mut i = 0usize;
        let mut quote: Option<u8> = None;
        while i < b.len() {
            let ch = b[i];
            if in_block_comment {
                if ch == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                    in_block_comment = false;
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            if let Some(q) = quote {
                if ch == b'\\' {
                    i += 2; // 跳过转义符与被转义字符（等价上游 i++ + for-i++）
                } else if ch == q {
                    quote = None;
                    i += 1;
                } else {
                    i += 1;
                }
                continue;
            }
            match ch {
                b'"' | b'\'' | b'`' => {
                    quote = Some(ch);
                    i += 1;
                    continue;
                }
                _ => {}
            }
            if ch == b'/' && i + 1 < b.len() {
                match b[i + 1] {
                    b'/' => break, // 行注释：跳过本行余下
                    b'*' => {
                        in_block_comment = true;
                        i += 2;
                        continue;
                    }
                    _ => {}
                }
            }
            match ch {
                b'(' => bal.paren += 1,
                b')' => bal.paren -= 1,
                b'[' => bal.bracket += 1,
                b']' => bal.bracket -= 1,
                b'{' => bal.brace += 1,
                b'}' => bal.brace -= 1,
                _ => {}
            }
            i += 1;
        }
        // `"` / `'` 不跨行；只反引号模板与块注释跨行。
        if matches!(quote, Some(b'"') | Some(b'\'')) {
            quote = None;
        }
    }
    bal
}

// ═══════════════════════════════════════════════════════════════════════════
// 结构闭合符 / JSX 判定（上游 STRUCTURAL_CLOSER_RE / JSX_CLOSER_RE 等）
// ═══════════════════════════════════════════════════════════════════════════

/// 纯标点闭合符行：`}` `)` `];` `})` `},`（上游 `^\s*[)\]}]+[;,]?\s*$`）。
fn is_punct_closer(text: &str) -> bool {
    let t = text.trim();
    let core = t.strip_suffix(|c| c == ';' || c == ',').unwrap_or(t);
    let core = core.trim();
    !core.is_empty() && core.bytes().all(|c| matches!(c, b')' | b']' | b'}'))
}

/// JSX/XML 标签名字符：首字母，后续 `\w.:-`。
fn is_valid_tag_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    bytes.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b':' | b'-'))
}

/// JSX 闭合符行：`</>` / `</Name>` / `/>`（上游 `JSX_CLOSER_RE`）。
fn is_jsx_closer_line(text: &str) -> bool {
    let t = text.trim();
    let core = t.strip_suffix(|c| c == ';' || c == ',').unwrap_or(t);
    let core = core.trim();
    if core.is_empty() {
        return false;
    }
    if core == "</>" || core == "/>" {
        return true;
    }
    if let Some(rest) = core.strip_prefix("</") {
        if let Some(name) = rest.strip_suffix('>') {
            return is_valid_tag_name(name);
        }
    }
    false
}

/// 标点或 JSX 闭合符行（上游 `isStructuralCloserLine`）。
fn is_structural_closer_line(text: &str) -> bool {
    is_punct_closer(text) || is_jsx_closer_line(text)
}

/// JSX 闭合符的标签名：`Some(None)` = 片段 `</>`，`Some(Some(name))` = `</name>`，
/// `None` = 非命名闭合符（含 `/>`）。
fn jsx_closer_name(text: &str) -> Option<Option<&str>> {
    let t = text.trim();
    let core = t.strip_suffix(|c| c == ';' || c == ',').unwrap_or(t);
    let core = core.trim();
    if core == "</>" {
        return Some(None);
    }
    if let Some(rest) = core.strip_prefix("</") {
        if let Some(name) = rest.strip_suffix('>') {
            if is_valid_tag_name(name) {
                return Some(Some(name));
            }
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JsxPayloadTag {
    name: String,
    closing: bool,
    self_closing: bool,
}

fn is_jsx_tag_start(text: &[u8], i: usize) -> bool {
    match text.get(i + 1) {
        None => false,
        Some(&n) => n == b'>' || n == b'/' || n.is_ascii_alphabetic(),
    }
}

/// 从 `start` 的 `<` 起找匹配的 `>`（跳过属性引号与 `{}` 表达式）；找不到返回 `None`。
fn find_jsx_tag_end(text: &[u8], start: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let mut braces = 0i32;
    let mut i = start + 1;
    while i < text.len() {
        let ch = text[i];
        if let Some(q) = quote {
            if ch == b'\\' {
                i += 2;
                continue;
            }
            if ch == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match ch {
            b'"' | b'\'' | b'`' => quote = Some(ch),
            b'{' => braces += 1,
            b'}' if braces > 0 => braces -= 1,
            b'>' if braces == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn parse_jsx_payload_tag(raw: &str) -> Option<JsxPayloadTag> {
    let raw = raw.trim();
    if raw == "<>" {
        return Some(JsxPayloadTag {
            name: String::new(),
            closing: false,
            self_closing: false,
        });
    }
    if raw == "</>" {
        return Some(JsxPayloadTag {
            name: String::new(),
            closing: true,
            self_closing: false,
        });
    }
    let (closing, inner) = if let Some(rest) = raw.strip_prefix("</") {
        (true, rest)
    } else {
        (false, raw.strip_prefix('<')?)
    };
    let inner = inner.strip_suffix('>')?;
    let name_end = inner
        .bytes()
        .position(|c| !(c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b':' | b'-')))
        .unwrap_or(inner.len());
    if name_end == 0 {
        return None;
    }
    let name = inner[..name_end].to_string();
    let self_closing = !closing && inner.ends_with('/');
    Some(JsxPayloadTag {
        name,
        closing,
        self_closing,
    })
}

/// 扫描文本内所有 JSX 标签（跨行用 `\n` 连接后解析）。
fn read_jsx_payload_tags(text: &str) -> Vec<JsxPayloadTag> {
    let b = text.as_bytes();
    let mut tags = Vec::new();
    let mut pos = match b.iter().position(|&c| c == b'<') {
        Some(p) => p,
        None => return tags,
    };
    loop {
        if is_jsx_tag_start(b, pos) {
            if let Some(end) = find_jsx_tag_end(b, pos) {
                if let Some(tag) = parse_jsx_payload_tag(&text[pos..=end]) {
                    tags.push(tag);
                }
                pos = match b[end + 1..].iter().position(|&c| c == b'<') {
                    Some(p) => end + 1 + p,
                    None => break,
                };
            } else {
                break;
            }
        } else {
            pos = match b[pos + 1..].iter().position(|&c| c == b'<') {
                Some(p) => pos + 1 + p,
                None => break,
            };
        }
    }
    tags
}

/// `payload_prefix` 是否含未闭合的开标签匹配 `echo_lines` 中的某个闭合符
/// （若是，则该闭合符是 payload 自身结构的闭合，不可当回声丢弃）。
fn payload_has_jsx_opener_for_echo(payload_prefix: &[String], echo_lines: &[String]) -> bool {
    let joined = payload_prefix.join("\n");
    let mut open_tags: Vec<String> = Vec::new();
    for tag in read_jsx_payload_tags(&joined) {
        if tag.closing {
            if open_tags.last().is_some_and(|n| n == &tag.name) {
                open_tags.pop();
            }
        } else if !tag.self_closing {
            open_tags.push(tag.name.clone());
        }
    }
    for line in echo_lines {
        if let Some(name_opt) = jsx_closer_name(line) {
            let target = name_opt.unwrap_or("");
            if open_tags.iter().any(|n| n == target) {
                return true;
            }
        }
    }
    false
}

// ═══════════════════════════════════════════════════════════════════════════
// 数据模型
// ═══════════════════════════════════════════════════════════════════════════

/// 一个待修复的替换组（= 一个 [`Hunk::Replace`](crate::types::Hunk::Replace)）。
#[derive(Debug, Clone)]
pub(crate) struct ReplaceGroup {
    /// 起始行（含，1-indexed）。
    pub(crate) start: Anchor,
    /// 结束行（含，1-indexed）。
    pub(crate) end: Anchor,
    /// 替换正文。
    pub(crate) payload: Vec<String>,
}

/// 修复后的替换组。
#[derive(Debug, Clone)]
pub(crate) struct RepairedReplace {
    /// 起始行（含）。
    pub(crate) start: Anchor,
    /// 正文替换的区间尾（含）；保留闭合符后可能小于原始 `end`。
    pub(crate) end: Anchor,
    /// 修复后正文（可能已截头 / 截尾）。
    pub(crate) body: Vec<String>,
    /// 诊断告警（若有）。
    pub(crate) warning: Option<String>,
    /// 保留段之后的额外删除区间（`covered_tail` 个闭合符，被区间下方投影覆盖）。
    /// `None` 表示无额外删除。
    pub(crate) trailing_delete: Option<(Anchor, Anchor)>,
}

/// Pass 1 产出：已解决（含可选告警）或延迟到 Pass 2 的缺闭合符候选。
enum Slot {
    Resolved {
        start: Anchor,
        end: Anchor,
        body: Vec<String>,
        warning: Option<String>,
    },
    Candidate {
        group: ReplaceGroup,
        delta: DelimiterBalance,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Leading,
    Trailing,
}

struct DroppedSuffixClosers {
    start_line: Anchor,
    count: usize,
    balance: DelimiterBalance,
}

/// 按 `anchor.line` 聚合的插入行（`before` / `after` 分别）。
#[derive(Default)]
struct InsertedLineMaps {
    before: BTreeMap<Anchor, Vec<String>>,
    after: BTreeMap<Anchor, Vec<String>>,
}

// ═══════════════════════════════════════════════════════════════════════════
// 边界回声（两边）
// ═══════════════════════════════════════════════════════════════════════════

fn has_non_ws(s: &str) -> bool {
    s.bytes()
        .any(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
}

/// payload 头部与区间上方幸存行逐字重复的最大行数（须含非空白行）。
fn count_dup_leading(payload: &[String], start_line: Anchor, file_lines: &[&str]) -> usize {
    let max = payload.len().min((start_line - 1) as usize);
    'outer: for count in (1..=max).rev() {
        let mut has_content = false;
        for offset in 0..count {
            let line = &payload[offset];
            let fidx = start_line as usize - 1 - count + offset;
            if line.as_str() != file_lines.get(fidx).copied().unwrap_or("") {
                continue 'outer;
            }
            if has_non_ws(line) {
                has_content = true;
            }
        }
        if has_content {
            return count;
        }
    }
    0
}

/// payload 尾部与区间下方幸存行逐字重复的最大行数（须含非空白行）。
fn count_dup_trailing(payload: &[String], end_line: Anchor, file_lines: &[&str]) -> usize {
    let max = payload
        .len()
        .min(file_lines.len().saturating_sub(end_line as usize));
    'outer: for count in (1..=max).rev() {
        let mut has_content = false;
        for offset in 0..count {
            let line = &payload[payload.len() - count + offset];
            let fidx = end_line as usize + offset;
            if line.as_str() != file_lines.get(fidx).copied().unwrap_or("") {
                continue 'outer;
            }
            if has_non_ws(line) {
                has_content = true;
            }
        }
        if has_content {
            return count;
        }
    }
    0
}

/// 两边边界回声：payload 头尾同时重述区间外幸存行，且裁剪后不吞掉整个 payload、
/// 平衡守卫通过（丢弃边要么自身平衡中性，要么恰好抵消 payload/区间 delta）。
fn find_boundary_echo(group: &ReplaceGroup, file_lines: &[&str]) -> Option<(usize, usize)> {
    let leading = count_dup_leading(&group.payload, group.start, file_lines);
    if leading == 0 {
        return None;
    }
    let trailing = count_dup_trailing(&group.payload, group.end, file_lines);
    if trailing == 0 {
        return None;
    }
    if leading + trailing >= group.payload.len() {
        return None;
    }
    let leading_bal = compute_delimiter_balance(&group.payload[..leading]);
    let trailing_bal = compute_delimiter_balance(&group.payload[group.payload.len() - trailing..]);
    let dropped = leading_bal.delta(trailing_bal.negate());
    if !dropped.is_zero() {
        let delta = compute_delimiter_balance(&group.payload).delta(compute_delimiter_balance(
            &file_lines[group.start as usize - 1..group.end as usize],
        ));
        if dropped != delta {
            return None;
        }
    }
    Some((leading, trailing))
}

// ═══════════════════════════════════════════════════════════════════════════
// 单边回声
// ═══════════════════════════════════════════════════════════════════════════

/// 平衡中性的单边边界回声：payload 仅头或仅尾重述区间外幸存行。
/// 多行区间广泛适用；单行区间仅当重复边为结构闭合符（如 JSX `</section>`）才修。
fn find_one_sided_boundary_echo(
    group: &ReplaceGroup,
    file_lines: &[&str],
) -> Option<(Side, usize)> {
    let leading = count_dup_leading(&group.payload, group.start, file_lines);
    let trailing = count_dup_trailing(&group.payload, group.end, file_lines);
    if (leading > 0) == (trailing > 0) {
        return None;
    }
    let (side, count) = if leading > 0 {
        (Side::Leading, leading)
    } else {
        (Side::Trailing, trailing)
    };
    if count >= group.payload.len() {
        return None;
    }
    let echo_lines: Vec<String> = match side {
        Side::Leading => group.payload[..count].to_vec(),
        Side::Trailing => group.payload[group.payload.len() - count..].to_vec(),
    };
    if !compute_delimiter_balance(&echo_lines).is_zero() {
        return None;
    }
    let delete_count = (group.end - group.start + 1) as usize;
    if delete_count <= 1 {
        if !matches!(side, Side::Trailing) {
            return None;
        }
        if !echo_lines.iter().all(|l| is_structural_closer_line(l)) {
            return None;
        }
        let payload_prefix = &group.payload[..group.payload.len() - count];
        if payload_has_jsx_opener_for_echo(payload_prefix, &echo_lines) {
            return None;
        }
    }
    Some((side, count))
}

// ═══════════════════════════════════════════════════════════════════════════
// 重复头 / 尾（分隔符平衡失配时）
// ═══════════════════════════════════════════════════════════════════════════

/// payload 尾 k 行 == 区间下方 k 行，且这 k 行的平衡 == `delta`（去掉即归零）。
fn find_duplicate_suffix(
    group: &ReplaceGroup,
    file_lines: &[&str],
    delta: DelimiterBalance,
) -> usize {
    if delta.is_zero() {
        return 0;
    }
    let payload = &group.payload;
    let end_line = group.end;
    let max_k = payload
        .len()
        .min(file_lines.len().saturating_sub(end_line as usize));
    for k in (1..=max_k).rev() {
        let mut matches = true;
        for t in 0..k {
            let fidx = end_line as usize + t;
            if payload[payload.len() - k + t].as_str()
                != file_lines.get(fidx).copied().unwrap_or("")
            {
                matches = false;
                break;
            }
        }
        if !matches {
            continue;
        }
        if compute_delimiter_balance(&payload[payload.len() - k..]) == delta {
            return k;
        }
    }
    0
}

/// payload 头 j 行 == 区间上方 j 行，且这 j 行的平衡 == `delta`。
fn find_duplicate_prefix(
    group: &ReplaceGroup,
    file_lines: &[&str],
    delta: DelimiterBalance,
) -> usize {
    if delta.is_zero() {
        return 0;
    }
    let payload = &group.payload;
    let start_line = group.start;
    let max_j = payload.len().min((start_line - 1) as usize);
    for j in (1..=max_j).rev() {
        let mut matches = true;
        for t in 0..j {
            let fidx = start_line as usize - 1 - j + t;
            if payload[t].as_str() != file_lines.get(fidx).copied().unwrap_or("") {
                matches = false;
                break;
            }
        }
        if !matches {
            continue;
        }
        if compute_delimiter_balance(&payload[..j]) == delta {
            return j;
        }
    }
    0
}

// ═══════════════════════════════════════════════════════════════════════════
// 缺闭合符保留（Pass 2）
// ═══════════════════════════════════════════════════════════════════════════

/// payload 尾部已重述的闭合符头部长度。
fn count_payload_restated_suffix_head(payload: &[String], suffix_lines: &[&str]) -> usize {
    let max_count = payload.len().min(suffix_lines.len());
    for count in (1..=max_count).rev() {
        let mut matches = true;
        for offset in 0..count {
            if payload[payload.len() - count + offset].as_str() != suffix_lines[offset] {
                matches = false;
                break;
            }
        }
        if matches {
            return count;
        }
    }
    0
}

/// 区间下方投影（after/before 插入 + 未删行）连续闭合符与 suffix 尾部重复的长度。
fn count_projected_below_suffix_tail(
    group: &ReplaceGroup,
    file_lines: &[&str],
    deleted_lines: &HashSet<Anchor>,
    maps: &InsertedLineMaps,
    suffix_lines: &[&str],
) -> usize {
    let mut below: Vec<String> = Vec::new();
    // after(endLine)
    if let Some(lines) = maps.after.get(&group.end) {
        for text in lines {
            if !is_punct_closer(text) {
                return 0;
            }
            below.push(text.clone());
        }
    }
    let mut line = group.end + 1;
    'outer: while line as usize <= file_lines.len() {
        if let Some(lines) = maps.before.get(&line) {
            for text in lines {
                if !is_punct_closer(text) {
                    break 'outer;
                }
                below.push(text.clone());
            }
        }
        if !deleted_lines.contains(&line) {
            let text = file_lines.get(line as usize - 1).copied().unwrap_or("");
            if !is_punct_closer(text) {
                break;
            }
            below.push(text.to_string());
        }
        if let Some(lines) = maps.after.get(&line) {
            for text in lines {
                if !is_punct_closer(text) {
                    break 'outer;
                }
                below.push(text.clone());
            }
        }
        line += 1;
    }
    let max_count = below.len().min(suffix_lines.len());
    for count in (1..=max_count).rev() {
        let mut matches = true;
        for offset in 0..count {
            if below[offset].as_str() != suffix_lines[suffix_lines.len() - count + offset] {
                matches = false;
                break;
            }
        }
        if matches {
            return count;
        }
    }
    0
}

/// 投影前缀（line 1..start 的插入 + 未删行 + before(start) + payload）的平衡。
fn compute_projected_prefix_balance(
    group: &ReplaceGroup,
    file_lines: &[&str],
    deleted_lines: &HashSet<Anchor>,
    inserted_by_line: &BTreeMap<Anchor, Vec<String>>,
    maps: &InsertedLineMaps,
) -> DelimiterBalance {
    let mut prefix: Vec<String> = Vec::new();
    let mut line = 1;
    while line < group.start {
        if let Some(ins) = inserted_by_line.get(&line) {
            prefix.extend(ins.iter().cloned());
        }
        if !deleted_lines.contains(&line) {
            prefix.push(file_lines[line as usize - 1].to_string());
        }
        line += 1;
    }
    if let Some(ins) = maps.before.get(&group.start) {
        prefix.extend(ins.iter().cloned());
    }
    prefix.extend(group.payload.iter().cloned());
    compute_delimiter_balance(&prefix)
}

fn prefix_can_cover_suffix_closers(
    group: &ReplaceGroup,
    file_lines: &[&str],
    suffix_balance: DelimiterBalance,
    covered_below_balance: DelimiterBalance,
    deleted_lines: &HashSet<Anchor>,
    inserted_by_line: &BTreeMap<Anchor, Vec<String>>,
    maps: &InsertedLineMaps,
) -> bool {
    let needed_openers = suffix_balance.negate();
    let prefix_balance =
        compute_projected_prefix_balance(group, file_lines, deleted_lines, inserted_by_line, maps);
    let uncovered = prefix_balance.sum(covered_below_balance);
    uncovered.covers(needed_openers)
}

/// group 区间正上方同样被其他 hunk 删除的连续行的净平衡（扣除同处插入）。
fn net_deleted_prefix_balance(
    group: &ReplaceGroup,
    deleted_lines: &HashSet<Anchor>,
    inserted_by_line: &BTreeMap<Anchor, Vec<String>>,
    file_lines: &[&str],
) -> DelimiterBalance {
    let mut deleted: Vec<String> = Vec::new();
    let mut inserted: Vec<String> = Vec::new();
    let mut line = group.start as i64 - 1;
    while line >= 1 && deleted_lines.contains(&(line as Anchor)) {
        deleted.push(file_lines[line as usize - 1].to_string());
        if let Some(ins) = inserted_by_line.get(&(line as Anchor)) {
            inserted.extend(ins.iter().cloned());
        }
        line -= 1;
    }
    deleted.reverse();
    inserted.reverse();
    compute_delimiter_balance(&deleted).delta(compute_delimiter_balance(&inserted))
}

/// 区间删除的尾部结构闭合符中「应保留」的中间段。
fn find_dropped_suffix_closers(
    group: &ReplaceGroup,
    file_lines: &[&str],
    delta: DelimiterBalance,
    remaining_delta: DelimiterBalance,
    deleted_prefix_balance: DelimiterBalance,
    deleted_lines: &HashSet<Anchor>,
    inserted_by_line: &BTreeMap<Anchor, Vec<String>>,
    maps: &InsertedLineMaps,
) -> Option<DroppedSuffixClosers> {
    let delete_count = (group.end - group.start + 1) as usize;
    let mut suffix_length = 0usize;
    while suffix_length < delete_count
        && is_punct_closer(
            file_lines
                .get(group.end as usize - 1 - suffix_length)
                .copied()
                .unwrap_or(""),
        )
    {
        suffix_length += 1;
    }
    if suffix_length == 0 {
        return None;
    }
    let suffix_start_line = group.end - suffix_length as Anchor + 1;
    let slice_start = group.end as usize - suffix_length;
    let slice_end = group.end as usize;
    let suffix_lines: Vec<&str> = file_lines[slice_start..slice_end].to_vec();
    let restated_head = count_payload_restated_suffix_head(&group.payload, &suffix_lines);
    let covered_tail =
        count_projected_below_suffix_tail(group, file_lines, deleted_lines, maps, &suffix_lines);
    let keep_start = restated_head;
    let keep_end = suffix_length - covered_tail;
    if keep_start >= keep_end {
        return None;
    }
    let kept_lines: Vec<&str> = suffix_lines[keep_start..keep_end].to_vec();
    let kept_balance = compute_delimiter_balance(&kept_lines);
    let needed_openers = kept_balance.negate();
    let covered_below_balance = compute_delimiter_balance(&suffix_lines[keep_end..]);
    if !delta.covers(needed_openers) {
        return None;
    }
    if deleted_prefix_balance.covers(needed_openers) {
        return None;
    }
    if !remaining_delta.covers(needed_openers) {
        return None;
    }
    if !prefix_can_cover_suffix_closers(
        group,
        file_lines,
        kept_balance,
        covered_below_balance,
        deleted_lines,
        inserted_by_line,
        maps,
    ) {
        return None;
    }
    Some(DroppedSuffixClosers {
        start_line: suffix_start_line + keep_start as Anchor,
        count: keep_end - keep_start,
        balance: kept_balance,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// 告警文案（保留上游英文 marker，便于测试断言）
// ═══════════════════════════════════════════════════════════════════════════

fn describe_boundary_echo_repair(start: Anchor, leading: usize, trailing: usize) -> String {
    format!(
        "Auto-repaired a replacement boundary echo at line {start}: dropped {leading} leading and \
         {trailing} trailing payload line(s) already present outside the range. Issue the payload \
         as the final desired content for the selected range only — never restate unchanged lines \
         bordering the range."
    )
}

fn describe_one_sided_echo_repair(start: Anchor, side: Side, count: usize) -> String {
    let where_ = if side == Side::Leading {
        "above"
    } else {
        "below"
    };
    let side_s = if side == Side::Leading {
        "leading"
    } else {
        "trailing"
    };
    format!(
        "Auto-repaired a replacement boundary echo at line {start}: dropped {count} {side_s} \
         payload line(s) identical to the surviving line(s) just {where_} the range. The range \
         was one line short of the content you retyped — issue the payload as the final content \
         for the selected range only, and widen the range to consume any keeper you restate."
    )
}

fn describe_boundary_repair(start: Anchor, action: &str) -> String {
    format!(
        "Auto-repaired a delimiter-balance mismatch in the replacement at line {start}: {action}. \
         Issue the payload as the final desired content only — never restate or omit a closing \
         bracket bordering the range."
    )
}

// ═══════════════════════════════════════════════════════════════════════════
// Pass 1
// ═══════════════════════════════════════════════════════════════════════════

fn pass1(group: &ReplaceGroup, file_lines: &[&str], delta: DelimiterBalance) -> Slot {
    if let Some((leading, trailing)) = find_boundary_echo(group, file_lines) {
        let body = group.payload[leading..group.payload.len() - trailing].to_vec();
        return Slot::Resolved {
            start: group.start,
            end: group.end,
            body,
            warning: Some(describe_boundary_echo_repair(group.start, leading, trailing)),
        };
    }
    if delta.is_zero() {
        if let Some((side, count)) = find_one_sided_boundary_echo(group, file_lines) {
            let body = match side {
                Side::Leading => group.payload[count..].to_vec(),
                Side::Trailing => group.payload[..group.payload.len() - count].to_vec(),
            };
            return Slot::Resolved {
                start: group.start,
                end: group.end,
                body,
                warning: Some(describe_one_sided_echo_repair(group.start, side, count)),
            };
        }
        return Slot::Resolved {
            start: group.start,
            end: group.end,
            body: group.payload.clone(),
            warning: None,
        };
    }
    let dup_suffix = find_duplicate_suffix(group, file_lines, delta);
    if dup_suffix > 0 {
        let body = group.payload[..group.payload.len() - dup_suffix].to_vec();
        return Slot::Resolved {
            start: group.start,
            end: group.end,
            body,
            warning: Some(describe_boundary_repair(
                group.start,
                &format!(
                    "dropped {dup_suffix} duplicated trailing payload line(s) already present \
                     below the range"
                ),
            )),
        };
    }
    let dup_prefix = find_duplicate_prefix(group, file_lines, delta);
    if dup_prefix > 0 {
        let body = group.payload[dup_prefix..].to_vec();
        return Slot::Resolved {
            start: group.start,
            end: group.end,
            body,
            warning: Some(describe_boundary_repair(
                group.start,
                &format!(
                    "dropped {dup_prefix} duplicated leading payload line(s) already present \
                     above the range"
                ),
            )),
        };
    }
    Slot::Candidate {
        group: group.clone(),
        delta,
    }
}

fn slot_patch_delta(slot: &Slot, file_lines: &[&str]) -> DelimiterBalance {
    match slot {
        Slot::Candidate { delta, .. } => *delta,
        Slot::Resolved {
            start, end, body, ..
        } => {
            let deleted: Vec<&str> = file_lines[*start as usize - 1..*end as usize].to_vec();
            compute_delimiter_balance(body).delta(compute_delimiter_balance(&deleted))
        }
    }
}

/// 聚合所有 slot（替换 payload/body）+ before/after 桶的插入行到 `inserted_by_line`
/// 与分 before/after 的 `InsertedLineMaps`。replacement 正文锚定 `before(start)`。
fn build_inserted(
    groups: &[ReplaceGroup],
    slots: &[Slot],
    before: &BTreeMap<Anchor, Vec<Vec<String>>>,
    after: &BTreeMap<Anchor, Vec<Vec<String>>>,
) -> (BTreeMap<Anchor, Vec<String>>, InsertedLineMaps) {
    let mut by_line: BTreeMap<Anchor, Vec<String>> = BTreeMap::new();
    let mut maps = InsertedLineMaps::default();
    for (slot, group) in slots.iter().zip(groups) {
        let body: &[String] = match slot {
            Slot::Resolved { body, .. } => body,
            Slot::Candidate { group, .. } => &group.payload,
        };
        by_line
            .entry(group.start)
            .or_default()
            .extend(body.iter().cloned());
        maps.before
            .entry(group.start)
            .or_default()
            .extend(body.iter().cloned());
    }
    for (line, bodies) in before {
        for body in bodies {
            by_line.entry(*line).or_default().extend(body.clone());
            maps.before.entry(*line).or_default().extend(body.clone());
        }
    }
    for (line, bodies) in after {
        for body in bodies {
            by_line.entry(*line).or_default().extend(body.clone());
            maps.after.entry(*line).or_default().extend(body.clone());
        }
    }
    (by_line, maps)
}

// ═══════════════════════════════════════════════════════════════════════════
// 主入口
// ═══════════════════════════════════════════════════════════════════════════

/// 在所有 [`Replace`](crate::types::Hunk::Replace) / [`Delete`](crate::types::Hunk::Delete) /
/// [`Insert`](crate::types::Hunk::Insert) 收集后运行：对每个替换组做两 pass 边界修复，
/// 返回与 `groups` 同序的修复结果。
///
/// `deletes` / `before` / `after` 仅参与全局投影（整 patch 分隔符残差、区间上下方
/// 投影），自身不被修改。
pub(crate) fn repair_replacement_boundaries(
    groups: &[ReplaceGroup],
    deletes: &[(Anchor, Anchor)],
    before: &BTreeMap<Anchor, Vec<Vec<String>>>,
    after: &BTreeMap<Anchor, Vec<Vec<String>>>,
    file_lines: &[&str],
) -> Vec<RepairedReplace> {
    // Pass 1
    let slots: Vec<Slot> = groups
        .iter()
        .map(|g| {
            let delta = compute_delimiter_balance(&g.payload).delta(compute_delimiter_balance(
                &file_lines[g.start as usize - 1..g.end as usize],
            ));
            pass1(g, file_lines, delta)
        })
        .collect();

    // deleted_lines：替换区间 + 删除区间
    let mut deleted_lines: HashSet<Anchor> = HashSet::new();
    for g in groups {
        let mut l = g.start;
        while l <= g.end {
            deleted_lines.insert(l);
            l += 1;
        }
    }
    for (s, e) in deletes {
        let mut l = *s;
        while l <= *e {
            deleted_lines.insert(l);
            l += 1;
        }
    }

    let (inserted_by_line, maps) = build_inserted(groups, &slots, before, after);

    // remaining_delta：整 patch 分隔符残差 = 各 slot delta + 各 delete delta + 各 insert delta
    let mut remaining = DelimiterBalance::zero();
    for slot in &slots {
        remaining = remaining.sum(slot_patch_delta(slot, file_lines));
    }
    for (s, e) in deletes {
        let deleted: Vec<&str> = file_lines[*s as usize - 1..*e as usize].to_vec();
        remaining = remaining
            .sum(compute_delimiter_balance(&deleted).negate());
    }
    for bodies in before.values() {
        for body in bodies {
            remaining = remaining.sum(compute_delimiter_balance(body));
        }
    }
    for bodies in after.values() {
        for body in bodies {
            remaining = remaining.sum(compute_delimiter_balance(body));
        }
    }

    // Pass 2
    let mut out: Vec<RepairedReplace> = Vec::with_capacity(groups.len());
    for slot in &slots {
        match slot {
            Slot::Resolved {
                start,
                end,
                body,
                warning,
            } => {
                out.push(RepairedReplace {
                    start: *start,
                    end: *end,
                    body: body.clone(),
                    warning: warning.clone(),
                    trailing_delete: None,
                });
            }
            Slot::Candidate { group, delta } => {
                let dropped = find_dropped_suffix_closers(
                    group,
                    file_lines,
                    *delta,
                    remaining,
                    net_deleted_prefix_balance(group, &deleted_lines, &inserted_by_line, file_lines),
                    &deleted_lines,
                    &inserted_by_line,
                    &maps,
                );
                if let Some(d) = dropped {
                    // 保留段 [start_line, start_line+count) 不删：body 替换
                    // [group.start, start_line-1]；其后 [start_line+count, group.end]
                    // 仍删（被区间下方投影覆盖）。
                    let new_end = d.start_line - 1;
                    let trailing_delete = if d.start_line + d.count as Anchor <= group.end {
                        Some((d.start_line + d.count as Anchor, group.end))
                    } else {
                        None
                    };
                    let mut l = d.start_line;
                    while l < d.start_line + d.count as Anchor {
                        deleted_lines.remove(&l);
                        l += 1;
                    }
                    remaining = remaining.sum(d.balance);
                    out.push(RepairedReplace {
                        start: group.start,
                        end: new_end,
                        body: group.payload.clone(),
                        warning: Some(describe_boundary_repair(
                            group.start,
                            &format!(
                                "kept {} structural closing line(s) the range deleted without \
                                 restating",
                                d.count
                            ),
                        )),
                        trailing_delete,
                    });
                } else {
                    out.push(RepairedReplace {
                        start: group.start,
                        end: group.end,
                        body: group.payload.clone(),
                        warning: None,
                        trailing_delete: None,
                    });
                }
            }
        }
    }
    out
}
