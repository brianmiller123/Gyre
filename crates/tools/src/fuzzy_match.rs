//! SEARCH 块模糊匹配：L0 精确 → L1 行级归一化 → L2 相似度阈值。
//!
//! 移植自 oh-my-pi [`normalize.ts`](../../../third/oh-my-pi/packages/coding-agent/src/edit/normalize.ts)
//! 的 `normalizeForFuzzy`。用于 [`crate::ApplyDiffTool`] / [`crate::StrReplaceTool`] 在硬匹配
//! 失败时容错（行尾空白、fancy 引号 / 破折号、空白折叠差异）。
//!
//! ## 三级策略
//!
//! - **L0 精确**：子串唯一匹配（默认，行为同原硬匹配）。
//! - **L1 归一化**：逐行 `normalize_for_fuzzy` 后唯一匹配（行内容相同但空白 / 标点微差）。
//! - **L2 相似度**：逐行归一化后平均相似度 ≥ 阈值的唯一最佳区间。
//!
//! fuzzy 默认关闭（仅 L0）；由 `PI_EDIT_FUZZY=on` + `PI_EDIT_FUZZY_THRESHOLD` 环境变量开启
//! （对齐 oh-my-pi；阶段五迁移到 `[tools].edit` 配置）。

/// 模糊匹配选项。
#[derive(Debug, Clone, Copy)]
pub struct FuzzyOpts {
    /// 是否启用 L1/L2 模糊匹配（false 时仅 L0 精确）。
    pub enabled: bool,
    /// L2 相似度阈值（0.0..=1.0）。
    pub threshold: f64,
}

impl FuzzyOpts {
    /// 关闭 fuzzy（仅 L0 精确匹配）。
    #[must_use]
    pub const fn off() -> Self {
        Self {
            enabled: false,
            threshold: 0.0,
        }
    }

    /// 从环境变量读取（`PI_EDIT_FUZZY` ∈ on/1/true 启用；`PI_EDIT_FUZZY_THRESHOLD` 默认 0.9）。
    #[must_use]
    pub fn from_env() -> Self {
        let enabled = matches!(
            std::env::var("PI_EDIT_FUZZY").ok().as_deref(),
            Some("on") | Some("1") | Some("true")
        );
        let threshold = std::env::var("PI_EDIT_FUZZY_THRESHOLD")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|t| (0.0..=1.0).contains(t))
            .unwrap_or(0.9);
        Self {
            enabled,
            threshold,
        }
    }
}

static FUZZY_OVERRIDE: std::sync::OnceLock<FuzzyOpts> = std::sync::OnceLock::new();

/// 设置全局 fuzzy 配置覆盖（装配层从 `[tools].edit` 读取后调用；优先于 env）。
pub fn set_fuzzy_opts(opts: FuzzyOpts) {
    let _ = FUZZY_OVERRIDE.set(opts);
}

/// 解析 fuzzy 选项：配置覆盖 > 环境变量。
pub(crate) fn resolve_opts() -> FuzzyOpts {
    FUZZY_OVERRIDE
        .get()
        .copied()
        .unwrap_or_else(FuzzyOpts::from_env)
}

/// 匹配方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMethod {
    /// L0 精确子串。
    Exact,
    /// L1 归一化完全匹配。
    Normalized,
    /// L2 相似度阈值匹配。
    Fuzzy,
}

/// 匹配结果：原文字节范围 + 相似度 + 方法。
#[derive(Debug, Clone, Copy)]
pub struct MatchOutcome {
    /// 匹配起始字节偏移。
    pub byte_start: usize,
    /// 匹配结束字节偏移（不含）。
    pub byte_end: usize,
    /// 相似度（1.0 = 精确 / 归一化完全匹配）。
    pub similarity: f64,
    /// 匹配方法。
    pub method: MatchMethod,
}

/// 匹配错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchError {
    /// 未找到匹配。
    NotFound,
    /// 匹配多处（含数量）。
    Ambiguous(usize),
}

/// 在 `haystack` 中找 `needle` 的唯一匹配。
///
/// 始终尝试 L0 精确；`opts.enabled` 时追加 L1 / L2。
pub fn find_unique_match(
    haystack: &str,
    needle: &str,
    opts: &FuzzyOpts,
) -> Result<MatchOutcome, MatchError> {
    if needle.is_empty() {
        return Err(MatchError::NotFound);
    }
    // L0 精确。
    match find_exact_unique(haystack, needle) {
        Ok((s, e)) => {
            return Ok(MatchOutcome {
                byte_start: s,
                byte_end: e,
                similarity: 1.0,
                method: MatchMethod::Exact,
            });
        }
        // 多处匹配直接报错（不尝试 fuzzy，避免误改）。
        Err(MatchError::Ambiguous(n)) => return Err(MatchError::Ambiguous(n)),
        Err(MatchError::NotFound) => {}
    }
    if !opts.enabled {
        return Err(MatchError::NotFound);
    }
    // L1 归一化。
    if let Some((s, e)) = find_normalized_lines_unique(haystack, needle) {
        return Ok(MatchOutcome {
            byte_start: s,
            byte_end: e,
            similarity: 1.0,
            method: MatchMethod::Normalized,
        });
    }
    // L2 相似度。
    if let Some(outcome) = find_fuzzy_lines_unique(haystack, needle, opts.threshold) {
        return Ok(outcome);
    }
    Err(MatchError::NotFound)
}

/// L0：精确子串匹配（区分未匹配 / 唯一 / 多处）。
fn find_exact_unique(haystack: &str, needle: &str) -> Result<(usize, usize), MatchError> {
    let mut count = 0usize;
    let mut first = None;
    let mut start = 0usize;
    while let Some(rel) = haystack[start..].find(needle) {
        count += 1;
        if count == 1 {
            first = Some(start + rel);
        }
        start = start + rel + needle.len().max(1);
        if start > haystack.len() {
            break;
        }
    }
    match count {
        0 => Err(MatchError::NotFound),
        1 => {
            let p = first.unwrap();
            Ok((p, p + needle.len()))
        }
        n => Err(MatchError::Ambiguous(n)),
    }
}

/// 每行起始字节偏移（含第 0 行 = 0）。
fn line_byte_starts(haystack: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, ch) in haystack.char_indices() {
        if ch == '\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// L1：逐行归一化后唯一匹配。
fn find_normalized_lines_unique(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    let h_lines: Vec<&str> = haystack.split('\n').collect();
    let n_lines: Vec<&str> = needle.split('\n').collect();
    if n_lines.is_empty() || n_lines.len() > h_lines.len() {
        return None;
    }
    let n_norm: Vec<String> = n_lines.iter().map(|l| normalize_for_fuzzy(l)).collect();
    let starts = line_byte_starts(haystack);
    let mut hit: Option<usize> = None;
    for s in 0..=h_lines.len() - n_lines.len() {
        let ok = (0..n_lines.len())
            .all(|i| normalize_for_fuzzy(h_lines[s + i]) == n_norm[i]);
        if ok {
            if hit.is_some() {
                return None; // 不唯一
            }
            hit = Some(s);
        }
    }
    hit.map(|s| line_range_bytes(&starts, s, s + n_lines.len() - 1, haystack.len()))
}

/// L2：逐行归一化平均相似度 ≥ 阈值的唯一最佳区间。
fn find_fuzzy_lines_unique(haystack: &str, needle: &str, threshold: f64) -> Option<MatchOutcome> {
    let h_lines: Vec<&str> = haystack.split('\n').collect();
    let n_lines: Vec<&str> = needle.split('\n').collect();
    if n_lines.is_empty() || n_lines.len() > h_lines.len() {
        return None;
    }
    let n_norm: Vec<String> = n_lines.iter().map(|l| normalize_for_fuzzy(l)).collect();
    let starts = line_byte_starts(haystack);
    let mut best: Option<(usize, f64)> = None;
    let mut best_count = 0usize;
    for s in 0..=h_lines.len() - n_lines.len() {
        let avg: f64 = (0..n_lines.len())
            .map(|i| similarity(&normalize_for_fuzzy(h_lines[s + i]), &n_norm[i]))
            .sum::<f64>()
            / n_lines.len() as f64;
        if avg < threshold {
            continue;
        }
        match best {
            None => {
                best = Some((s, avg));
                best_count = 1;
            }
            Some((_, b)) if avg > b + f64::EPSILON => {
                best = Some((s, avg));
                best_count = 1;
            }
            Some((_, b)) if (avg - b).abs() <= f64::EPSILON => {
                best_count += 1;
            }
            _ => {}
        }
    }
    if best_count != 1 {
        return None;
    }
    let (s, avg) = best?;
    let (byte_start, byte_end) = line_range_bytes(&starts, s, s + n_lines.len() - 1, haystack.len());
    Some(MatchOutcome {
        byte_start,
        byte_end,
        similarity: avg,
        method: MatchMethod::Fuzzy,
    })
}

/// 由行号区间计算字节范围（不含区间末尾行之后的 `\n`）。
fn line_range_bytes(starts: &[usize], start_line: usize, end_line: usize, total_len: usize) -> (usize, usize) {
    let byte_start = starts[start_line];
    let byte_end = if end_line + 1 < starts.len() {
        starts[end_line + 1] - 1
    } else {
        total_len
    };
    (byte_start, byte_end)
}

/// 归一化一行用于模糊比较：trim + fancy 引号 / 破折号归一化 + 折叠连续空白。
fn normalize_for_fuzzy(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut s = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        match ch {
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{00AB}' | '\u{00BB}' => s.push('"'),
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{0060}' | '\u{00B4}' => s.push('\''),
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => s.push('-'),
            ' ' | '\t' => {
                if !s.ends_with(' ') {
                    s.push(' ');
                }
            }
            _ => s.push(ch),
        }
    }
    s
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0usize; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

/// 归一化相似度（1 - 编辑距离 / 最大长度）。
fn similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let d = levenshtein(a, b) as f64;
    let max = a.chars().count().max(b.chars().count()) as f64;
    if max == 0.0 {
        1.0
    } else {
        1.0 - d / max
    }
}

// ── 缩进自适应（移植 oh-my-pi normalize.ts adjustIndentation）──────────────

/// 缩进 profile：分析一段文本的缩进特征（字符/单位/是否混合）。
#[derive(Debug, Clone)]
struct IndentProfile {
    lines: Vec<String>,
    char: Option<char>,
    space_only: bool,
    tab_only: bool,
    mixed: bool,
    unit: usize,
    non_empty_count: usize,
}

fn count_leading_whitespace(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

fn is_non_empty_line(line: &str) -> bool {
    !line.trim().is_empty()
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

fn build_indent_profile(text: &str) -> IndentProfile {
    let lines: Vec<String> = text.split('\n').map(String::from).collect();
    let mut char: Option<char> = None;
    let mut space_only = true;
    let mut tab_only = true;
    let mut mixed = false;
    let mut non_empty_count = 0;
    let mut counts: Vec<usize> = Vec::new();
    for line in &lines {
        if !is_non_empty_line(line) {
            continue;
        }
        non_empty_count += 1;
        let indent_len = count_leading_whitespace(line);
        let indent: String = line.chars().take(indent_len).collect();
        counts.push(indent_len);
        if indent.contains(' ') {
            tab_only = false;
        }
        if indent.contains('\t') {
            space_only = false;
        }
        if indent.contains(' ') && indent.contains('\t') {
            mixed = true;
        }
        if !indent.is_empty() {
            let first = indent.chars().next().unwrap();
            match char {
                None => char = Some(first),
                Some(c) if c != first => mixed = true,
                _ => {}
            }
        }
    }
    let mut unit = 0;
    if space_only && non_empty_count > 0 {
        let mut current = 0;
        for &cnt in &counts {
            if cnt == 0 {
                continue;
            }
            current = if current == 0 { cnt } else { gcd(current, cnt) };
        }
        unit = current;
    }
    if tab_only && non_empty_count > 0 {
        unit = 1;
    }
    IndentProfile {
        lines,
        char,
        space_only,
        tab_only,
        mixed,
        unit,
        non_empty_count,
    }
}

fn is_indentation_only_rewrite(old_text: &str, new_text: &str) -> bool {
    let o: Vec<&str> = old_text.split('\n').collect();
    let n: Vec<&str> = new_text.split('\n').collect();
    o.len() == n.len() && o.iter().zip(n.iter()).all(|(a, b)| a.trim() == b.trim())
}

fn uniform_indent_delta(old_p: &IndentProfile, actual_p: &IndentProfile) -> Option<isize> {
    let n = old_p.lines.len().min(actual_p.lines.len());
    let mut deltas: Vec<isize> = Vec::new();
    for i in 0..n {
        let (o, a) = (&old_p.lines[i], &actual_p.lines[i]);
        if !is_non_empty_line(o) || !is_non_empty_line(a) {
            continue;
        }
        deltas.push(count_leading_whitespace(a) as isize - count_leading_whitespace(o) as isize);
    }
    let d = deltas.first().copied()?;
    if deltas.iter().all(|&x| x == d) { Some(d) } else { None }
}

fn apply_indent_delta(text: &str, delta: isize, indent_char: char) -> String {
    let pad: String = std::iter::repeat_n(indent_char, delta.max(0) as usize).collect();
    text.split('\n')
        .map(|line| {
            if !is_non_empty_line(line) {
                return line.to_string();
            }
            if delta > 0 {
                format!("{pad}{line}")
            } else {
                let remove = (-(delta) as usize).min(count_leading_whitespace(line));
                line[remove..].to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn convert_leading_tabs_to_spaces(text: &str, spaces_per_tab: usize) -> String {
    if spaces_per_tab == 0 {
        return text.to_string();
    }
    text.split('\n')
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.is_empty() {
                return line.to_string();
            }
            let indent_len = count_leading_whitespace(line);
            let indent: String = line.chars().take(indent_len).collect();
            if !indent.contains('\t') || indent.contains(' ') {
                return line.to_string();
            }
            format!(
                "{}{trimmed}",
                " ".repeat(indent.chars().count() * spaces_per_tab)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 调整 `new_text` 缩进以匹配 `actual_text`（fuzzy 命中后 `old_text` 与 `actual_text` 缩进不同）。
///
/// 移植 oh-my-pi [`normalize.ts adjustIndentation`](../../../third/oh-my-pi/packages/coding-agent/src/edit/normalize.ts:303)。
/// 仅在「均匀缩进偏移」或「tab↔space 转换」时调整；混合缩进或不一致则原样返回。
#[must_use]
pub fn adjust_indentation(old_text: &str, actual_text: &str, new_text: &str) -> String {
    if old_text == actual_text || is_indentation_only_rewrite(old_text, new_text) {
        return new_text.to_string();
    }
    let old_p = build_indent_profile(old_text);
    let actual_p = build_indent_profile(actual_text);
    let new_p = build_indent_profile(new_text);
    if old_p.non_empty_count == 0 || actual_p.non_empty_count == 0 || new_p.non_empty_count == 0 {
        return new_text.to_string();
    }
    if old_p.mixed || actual_p.mixed || new_p.mixed {
        return new_text.to_string();
    }
    // tab（old/new）→ space（actual）：按 actual 单位转 tab 为 space。
    if old_p.tab_only && actual_p.space_only && new_p.tab_only && actual_p.unit > 0 {
        return convert_leading_tabs_to_spaces(new_text, actual_p.unit);
    }
    let delta = match uniform_indent_delta(&old_p, &actual_p) {
        Some(d) if d != 0 => d,
        _ => return new_text.to_string(),
    };
    let indent_char = actual_p.char.or(old_p.char).unwrap_or(' ');
    apply_indent_delta(new_text, delta, indent_char)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l0_exact_unique() {
        let m = find_unique_match("foo bar baz", "bar", &FuzzyOpts::off()).unwrap();
        assert_eq!(m.method, MatchMethod::Exact);
        assert_eq!(&"foo bar baz"[m.byte_start..m.byte_end], "bar");
    }

    #[test]
    fn l0_ambiguous_errors() {
        // "x" 在 "x x x" 匹配 3 处 → Ambiguous（即使 fuzzy off 也报多处）。
        assert!(matches!(
            find_unique_match("x x x", "x", &FuzzyOpts::off()),
            Err(MatchError::Ambiguous(3))
        ));
    }

    #[test]
    fn l0_not_found_when_fuzzy_off() {
        assert!(matches!(
            find_unique_match("foo", "bar", &FuzzyOpts::off()),
            Err(MatchError::NotFound)
        ));
    }

    #[test]
    fn l1_normalized_matches_trailing_whitespace() {
        // needle 行尾有多余空格，归一化后应匹配。
        let haystack = "fn main() {\n    println!(\"hi\");  \n}\n";
        let needle = "fn main() {\n    println!(\"hi\");\n}";
        let opts = FuzzyOpts {
            enabled: true,
            threshold: 0.9,
        };
        let m = find_unique_match(haystack, needle, &opts).unwrap();
        assert_eq!(m.method, MatchMethod::Normalized);
    }

    #[test]
    fn l1_normalized_matches_fancy_quotes() {
        let haystack = "msg = “hello”\n";
        let needle = "msg = \"hello\"";
        let opts = FuzzyOpts {
            enabled: true,
            threshold: 0.9,
        };
        let m = find_unique_match(haystack, needle, &opts).unwrap();
        assert_eq!(m.method, MatchMethod::Normalized);
        assert_eq!(&haystack[m.byte_start..m.byte_end], "msg = “hello”");
    }

    #[test]
    fn l2_fuzzy_matches_near_identical() {
        // 单字符差异，相似度高。
        let haystack = "let value = 42;\nlet other = 7;\n";
        let needle = "let value = 43;"; // 42 vs 43
        let opts = FuzzyOpts {
            enabled: true,
            threshold: 0.8,
        };
        let m = find_unique_match(haystack, needle, &opts).unwrap();
        assert_eq!(m.method, MatchMethod::Fuzzy);
        assert!(m.similarity >= 0.8);
        assert_eq!(&haystack[m.byte_start..m.byte_end], "let value = 42;");
    }

    #[test]
    fn replace_range_uses_outcome() {
        let haystack = "aaa\nbbb\nccc\n";
        let m = find_unique_match(haystack, "bbb", &FuzzyOpts::off()).unwrap();
        let mut s = haystack.to_string();
        s.replace_range(m.byte_start..m.byte_end, "BBB");
        assert_eq!(s, "aaa\nBBB\nccc\n");
    }

    #[test]
    fn normalize_collapses_whitespace_and_quotes() {
        assert_eq!(normalize_for_fuzzy("a  b\t c"), "a b c");
        assert_eq!(normalize_for_fuzzy("“x”"), "\"x\"");
        assert_eq!(normalize_for_fuzzy("a–b"), "a-b");
    }

    #[test]
    fn adjust_indentation_shifts_uniform_delta() {
        // old 0 缩进，actual 整体 +4 空格 → new 应同步 +4。
        let old = "fn a() {\n  x\n}";
        let actual = "    fn a() {\n      x\n    }";
        let new = "fn b() {\n  y\n}";
        assert_eq!(
            adjust_indentation(old, actual, new),
            "    fn b() {\n      y\n    }"
        );
    }

    #[test]
    fn adjust_indentation_noop_when_equal() {
        // old == actual → 原样返回 new。
        assert_eq!(adjust_indentation("a\nb", "a\nb", "c\nd"), "c\nd");
    }
}
