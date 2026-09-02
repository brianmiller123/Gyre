//! 命令输出最小化器：把 git / cargo / python 等冗长命令输出压缩为摘要。
//!
//! 设计要点：
//! - 每个过滤器是**确定性纯函数**（无时间/随机依赖），同一输入恒产出同一摘要；
//! - [`Minimizer::apply`] 按注册顺序取**第一个**命中（`matches` 为真且 `minimize` 返回
//!   `Some`）的过滤器结果，其余过滤器不再参与；`minimize` 返回 `None` 表示本次不适用；
//! - 无过滤器命中时，若行数超过 `max_lines` 阈值，走通用 head(100)+tail(100) 折叠；
//! - 摘要行数**严格小于**原始行数才返回 `Some`，避免对已足够精简的输出做无意义压缩；
//! - 输出中的折叠标记与仓库语言一致（中文）。

use std::collections::{BTreeMap, BTreeSet};

/// 最小化结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Minimized {
    /// 摘要文本（已含结尾换行）。
    pub summary: String,
    /// 命中的过滤器名（通用折叠时为 `"generic"`），供调用方提示压缩来源。
    pub filter: &'static str,
}

/// 单个输出过滤器。实现必须是确定性纯函数：同一 `(cmd, args, output)` 恒返回同一结果。
pub trait OutputFilter: Send + Sync {
    /// 过滤器名（用于输出提示）。
    fn name(&self) -> &'static str;
    /// 是否适用于该命令（按命令名与参数判断，应保持廉价）。
    fn matches(&self, cmd: &str, args: &[String]) -> bool;
    /// 压缩输出；返回 `None` 表示本次不适用（不压缩）。
    fn minimize(&self, output: &str) -> Option<String>;
}

/// 命令输出最小化器：按注册顺序应用过滤器，兜底做通用行折叠。
pub struct Minimizer {
    filters: Vec<Box<dyn OutputFilter>>,
    max_lines: usize,
}

impl Minimizer {
    /// 构造最小化器：`filters` 按优先级注册（第一个命中的结果胜出）；
    /// `max_lines` 为通用折叠的行数阈值（`0` = 禁用通用折叠）。
    #[must_use]
    pub fn new(filters: Vec<Box<dyn OutputFilter>>, max_lines: usize) -> Self {
        Self { filters, max_lines }
    }

    /// 压缩命令输出；返回 `None` 表示无过滤器适用且无需通用折叠（输出原样保留）。
    #[must_use]
    pub fn apply(&self, cmd: &str, args: &[String], output: &str) -> Option<Minimized> {
        for filter in &self.filters {
            if filter.matches(cmd, args) {
                if let Some(summary) = filter.minimize(output) {
                    return Some(Minimized {
                        summary: normalize_summary(&summary),
                        filter: filter.name(),
                    });
                }
            }
        }
        generic_fold(output, self.max_lines)
            .filter(|summary| summary.lines().count() < output.lines().count())
            .map(|summary| Minimized {
                summary: normalize_summary(&summary),
                filter: "generic",
            })
    }
}

/// 内置过滤器集（按优先级）：git status → git diff → git log → cargo → python。
#[must_use]
pub fn default_filters() -> Vec<Box<dyn OutputFilter>> {
    vec![
        Box::new(GitStatusFilter),
        Box::new(GitDiffFilter),
        Box::new(GitLogFilter),
        Box::new(CargoFilter),
        Box::new(PythonFilter),
    ]
}

/// 禁用态最小化器：空过滤器 + 0 阈值，[`Minimizer::apply`] 恒返回 `None`。
#[must_use]
pub fn disabled() -> Minimizer {
    Minimizer::new(Vec::new(), 0)
}

/// 把形如 `git status --short` 的命令字符串拆为（程序名, 参数表），供 [`OutputFilter::matches`] 匹配。
#[must_use]
pub fn split_command(command: &str) -> (String, Vec<String>) {
    let mut parts = command.split_whitespace();
    let cmd = parts.next().unwrap_or_default().to_string();
    let args: Vec<String> = parts.map(str::to_string).collect();
    (cmd, args)
}

/// 规范化摘要文本：去除多余尾部空白，保证恰好以一个换行结尾（契约：summary 已含换行）。
fn normalize_summary(summary: &str) -> String {
    let mut s = summary.trim_end().to_string();
    s.push('\n');
    s
}

/// 摘要行数严格小于原始行数时返回 `Some(summary)`，否则 `None`（无压缩价值）。
fn strictly_smaller(summary: &str, original: &str) -> Option<String> {
    if summary.lines().count() < original.lines().count() {
        Some(summary.to_string())
    } else {
        None
    }
}

/// 通用折叠：保留的头部行数。
const GENERIC_HEAD: usize = 100;
/// 通用折叠：保留的尾部行数。
const GENERIC_TAIL: usize = 100;

/// 通用折叠：行数超阈值且确有可折叠内容时，保留头尾各 100 行，中间以标记行折叠。
fn generic_fold(output: &str, max_lines: usize) -> Option<String> {
    if max_lines == 0 {
        return None;
    }
    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();
    // 未超阈值，或折叠窗口已能覆盖全文（无内容可折叠）时不压缩。
    if total <= max_lines || total <= GENERIC_HEAD + GENERIC_TAIL {
        return None;
    }
    let folded = total - GENERIC_HEAD - GENERIC_TAIL;
    let mut out = String::new();
    for l in &lines[..GENERIC_HEAD] {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&format!("… [已折叠 {folded} 行] …\n"));
    for l in &lines[total - GENERIC_TAIL..] {
        out.push_str(l);
        out.push('\n');
    }
    Some(out)
}

// ── 过滤器 1：git status ──────────────────────────────────────────────────────

/// `git status` 输出过滤器：折叠未跟踪目录、按节统计并截断关键文件名。
pub struct GitStatusFilter;

impl OutputFilter for GitStatusFilter {
    fn name(&self) -> &'static str {
        "git-status"
    }

    fn matches(&self, cmd: &str, args: &[String]) -> bool {
        cmd == "git"
            && args.iter().any(|a| a == "status")
            && !args.iter().any(|a| a == "--porcelain")
    }

    fn minimize(&self, output: &str) -> Option<String> {
        git_status_minimize(output)
    }
}

/// 状态节标题（长格式）。
const STATUS_SECTIONS: [&str; 4] = [
    "Changes to be committed:",
    "Changes not staged for commit:",
    "Unmerged paths:",
    "Untracked files:",
];
/// 单节最多展示的条目数，超出追加 `… 还有 N 条`。
const STATUS_MAX_ENTRIES: usize = 10;

fn git_status_minimize(output: &str) -> Option<String> {
    if output.trim().is_empty() {
        return None;
    }
    let lines: Vec<&str> = output.lines().collect();
    let has_sections = lines.iter().copied().any(is_status_section);
    let has_short = lines.iter().copied().any(is_short_status);
    let has_summary = lines.iter().copied().any(is_status_summary);
    // 非 status 长/短格式或总结输出（`--porcelain` 已在 matches 排除，此处防御）→ 不适用。
    if !has_sections && !has_short && !has_summary {
        return None;
    }

    let mut out = String::new();
    // 实际压缩动作标记：截断条目 / 折叠未跟踪目录（避免对已精简输出做无意义替换）。
    let mut truncated = false;
    let mut folded_any = false;

    // 前置信息（On branch / Your branch / HEAD detached）。
    for l in &lines {
        if is_status_preamble(l) {
            out.push_str(l);
            out.push('\n');
        }
    }

    // 长格式：逐节统计条目并截断关键文件名。
    let mut i = 0usize;
    let mut sections_emitted = 0usize;
    while i < lines.len() {
        if !is_status_section(lines[i]) {
            i += 1;
            continue;
        }
        let header = lines[i].trim_end();
        i += 1;
        let mut entries: Vec<String> = Vec::new();
        while i < lines.len() && !is_status_section(lines[i]) {
            let raw = lines[i].trim_end();
            if is_status_entry(raw) && !is_status_hint(raw) {
                entries.push(raw.trim_start().to_string());
            }
            i += 1;
        }
        if entries.is_empty() {
            continue;
        }
        sections_emitted += 1;
        out.push_str(header);
        out.push('\n');
        out.push_str(&format!("共 {} 条变更\n", entries.len()));
        // 未跟踪节（条目为裸路径）全部参与目录折叠；其余节原样展示。
        let shown = if header == "Untracked files:" {
            let folded = fold_untracked(&entries, false);
            folded_any |= folded.iter().any(|e| e.contains(" 个文件)"));
            folded
        } else {
            entries
        };
        let shown_count = shown.len();
        for e in &shown[..shown_count.min(STATUS_MAX_ENTRIES)] {
            out.push('\t');
            out.push_str(e);
            out.push('\n');
        }
        if shown_count > STATUS_MAX_ENTRIES {
            truncated = true;
            let extra = shown_count - STATUS_MAX_ENTRIES;
            out.push_str(&format!("… 还有 {extra} 条\n"));
        }
    }

    // 短格式（git status -s）：无节标题，整体作为一节处理，仅 `?? ` 未跟踪条目折叠目录。
    if sections_emitted == 0 && has_short {
        let entries: Vec<String> = lines
            .iter()
            .filter(|l| is_short_status(l))
            .map(std::string::ToString::to_string)
            .collect();
        let shown = fold_untracked(&entries, true);
        folded_any |= shown.iter().any(|e| e.contains(" 个文件)"));
        out.push_str(&format!("共 {} 条变更\n", entries.len()));
        let shown_count = shown.len();
        for e in &shown[..shown_count.min(STATUS_MAX_ENTRIES)] {
            out.push('\t');
            out.push_str(e);
            out.push('\n');
        }
        if shown_count > STATUS_MAX_ENTRIES {
            truncated = true;
            let extra = shown_count - STATUS_MAX_ENTRIES;
            out.push_str(&format!("… 还有 {extra} 条\n"));
        }
    }

    // 末尾总结行（nothing to commit / no changes added）。
    for l in &lines {
        if is_status_summary(l) {
            out.push_str(l.trim_end());
            out.push('\n');
        }
    }
    // 摘要须有实际压缩动作（截断/折叠/行数减少），否则原样返回。
    if !truncated && !folded_any && out.lines().count() >= lines.len() {
        return None;
    }
    Some(out)
}

/// 长格式节标题判定（含尾部空白容忍）。
fn is_status_section(l: &str) -> bool {
    STATUS_SECTIONS.contains(&l.trim_end())
}

/// 前置信息行：`On branch …` / `Your branch …` / `HEAD detached …`。
fn is_status_preamble(l: &str) -> bool {
    l.starts_with("On branch") || l.starts_with("Your branch") || l.starts_with("HEAD detached")
}

/// 末尾总结行：`nothing to commit …` / `no changes added to commit …`。
fn is_status_summary(l: &str) -> bool {
    l.contains("nothing to commit") || l.starts_with("no changes added")
}

/// 长/短格式条目行：制表符开头（长格式）或 `XY ` 状态码（短格式）。
fn is_status_entry(l: &str) -> bool {
    l.starts_with('\t') || is_short_status(l)
}

/// 提示行（`(use "git add …" …)` 等）：不计入条目。
fn is_status_hint(l: &str) -> bool {
    l.trim_start().starts_with("(use ")
}

/// 短格式状态码行：形如 `?? dir/`、` M src/foo.rs`（两个状态字符后跟空格）。
fn is_short_status(l: &str) -> bool {
    let b = l.as_bytes();
    b.len() >= 3 && b[2] == b' ' && is_status_byte(b[0]) && is_status_byte(b[1])
}

/// git 短格式状态字符（含首位空格：未暂存改动形如 ` M …`）。
const fn is_status_byte(c: u8) -> bool {
    matches!(
        c,
        b' ' | b'M' | b'A' | b'D' | b'R' | b'C' | b'U' | b'?' | b'!' | b'T' | b'X'
    )
}

/// 把未跟踪条目按**父目录**折叠：同一目录下 ≥2 项合并为 `dir/ (N 个文件)`，保持首次出现顺序。
///
/// `only_untracked` 为真时仅折叠 `?? ` 前缀条目（短格式），其余状态码条目原样保留。
fn fold_untracked(entries: &[String], only_untracked: bool) -> Vec<String> {
    let foldable = |e: &str| !only_untracked || e.starts_with("?? ");
    // 第一遍：统计每个父目录下的条目数（BTreeMap 仅用于确定性查询，展示顺序由第二遍决定）。
    let mut dir_count: BTreeMap<&str, usize> = BTreeMap::new();
    for e in entries {
        if foldable(e) {
            if let Some(dir) = top_dir(strip_untracked_prefix(e)) {
                *dir_count.entry(dir).or_insert(0) += 1;
            }
        }
    }
    let mut emitted: BTreeSet<&str> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for e in entries {
        if foldable(e) {
            if let Some(dir) = top_dir(strip_untracked_prefix(e)) {
                let n = dir_count.get(dir).copied().unwrap_or(0);
                if n >= 2 {
                    if emitted.insert(dir) {
                        out.push(format!("{}{dir} ({n} 个文件)", entry_prefix(e)));
                    }
                    continue;
                }
            }
        }
        out.push(e.clone());
    }
    out
}

/// 条目行自带的状态前缀（短格式未跟踪为 `?? `，长格式为空串）。
fn entry_prefix(e: &str) -> &str {
    if e.starts_with("?? ") { "?? " } else { "" }
}

/// 去掉条目行前的 `?? `（短格式未跟踪标记）与空白，得到裸路径。
fn strip_untracked_prefix(e: &str) -> &str {
    e.trim_start_matches("?? ").trim()
}

/// 取路径的**父目录**前缀（含尾斜杠）；无目录分隔符时返回 `None`。
fn top_dir(path: &str) -> Option<&str> {
    path.rfind('/').map(|i| &path[..=i])
}

// ── 过滤器 2：git diff ────────────────────────────────────────────────────────

/// `git diff` 输出过滤器：保留 stat 行与 hunk 头，丢弃 hunk 内的 +/- 内容行。
pub struct GitDiffFilter;

impl OutputFilter for GitDiffFilter {
    fn name(&self) -> &'static str {
        "git-diff"
    }

    fn matches(&self, cmd: &str, args: &[String]) -> bool {
        cmd == "git" && args.iter().any(|a| a == "diff")
    }

    fn minimize(&self, output: &str) -> Option<String> {
        git_diff_minimize(output)
    }
}

/// hunk 头前缀。
const HUNK_HEADER: &str = "@@";

fn git_diff_minimize(output: &str) -> Option<String> {
    if output.trim().is_empty() {
        return None;
    }
    let lines: Vec<&str> = output.lines().collect();
    let has_stat = lines.iter().copied().any(is_diff_stat_line);
    let has_hunk = lines.iter().any(|l| l.starts_with(HUNK_HEADER));
    if !has_stat && !has_hunk {
        return None; // 非 diff 输出（空 diff / --name-only / --numstat 等）
    }
    let mut out = String::new();
    let mut adds = 0usize;
    let mut dels = 0usize;
    for l in &lines {
        if is_diff_stat_line(l) || l.starts_with(HUNK_HEADER) {
            out.push_str(l);
            out.push('\n');
        } else if !has_stat {
            // 无 --stat：手工统计 +/- 内容行（排除 `+++` / `---` 文件头）。
            if l.starts_with('+') && !l.starts_with("+++") {
                adds += 1;
            } else if l.starts_with('-') && !l.starts_with("---") {
                dels += 1;
            }
        }
    }
    if !has_stat && (adds > 0 || dels > 0) {
        out.push_str(&format!("统计：+{adds} -{dels}\n"));
    }
    strictly_smaller(&out, output)
}

/// diff stat 行判定：` path | 3 ++-`（文件行）或 ` 3 files changed, … insertions(+), … deletions(-)`（汇总行）。
fn is_diff_stat_line(l: &str) -> bool {
    if let Some(idx) = l.find(" | ") {
        if l.as_bytes().get(idx + 3).is_some_and(u8::is_ascii_digit) {
            return true;
        }
    }
    l.contains("files changed")
        || l.contains("file changed")
        || l.contains("insertions(")
        || l.contains("deletions(")
}

// ── 过滤器 3：git log ─────────────────────────────────────────────────────────

/// `git log` 输出过滤器：保留前 15 条提交（--oneline 时前 30 行）。
pub struct GitLogFilter;

impl OutputFilter for GitLogFilter {
    fn name(&self) -> &'static str {
        "git-log"
    }

    fn matches(&self, cmd: &str, args: &[String]) -> bool {
        cmd == "git" && args.iter().any(|a| a == "log")
    }

    fn minimize(&self, output: &str) -> Option<String> {
        git_log_minimize(output)
    }
}

/// 长格式最多保留的提交条数。
const LOG_KEEP_COMMITS: usize = 15;
/// --oneline 最多保留的行数。
const LOG_KEEP_ONELINE: usize = 30;

fn git_log_minimize(output: &str) -> Option<String> {
    if output.trim().is_empty() {
        return None;
    }
    let lines: Vec<&str> = output.lines().collect();
    let long_format = lines.iter().any(|l| l.starts_with("commit "));
    let oneline = !long_format && lines.iter().copied().any(is_oneline_entry);
    if !long_format && !oneline {
        return None; // 非 log 输出
    }
    let mut out = String::new();
    if oneline {
        let total = lines.len();
        let end = total.min(LOG_KEEP_ONELINE);
        for l in &lines[..end] {
            out.push_str(l);
            out.push('\n');
        }
        if total > LOG_KEEP_ONELINE {
            let extra = total - LOG_KEEP_ONELINE;
            out.push_str(&format!("… 还有 {extra} 条提交\n"));
        }
    } else {
        let blocks = split_commit_blocks(&lines);
        let total = blocks.len();
        let end = total.min(LOG_KEEP_COMMITS);
        for b in &blocks[..end] {
            push_commit_summary(&mut out, b);
        }
        if total > LOG_KEEP_COMMITS {
            let extra = total - LOG_KEEP_COMMITS;
            out.push_str(&format!("… 还有 {extra} 条提交\n"));
        }
    }
    strictly_smaller(&out, output)
}

/// --oneline 条目行：≥7 位十六进制哈希 + 空格 + 主题。
fn is_oneline_entry(l: &str) -> bool {
    let b = l.as_bytes();
    let hex_len = b.iter().take_while(|c| c.is_ascii_hexdigit()).count();
    hex_len >= 7 && b.get(hex_len) == Some(&b' ')
}

/// 按 `commit ` 头把长格式输出切分为提交块（正文中的空行不产生新块）。
fn split_commit_blocks<'a>(lines: &[&'a str]) -> Vec<Vec<&'a str>> {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for l in lines {
        if l.is_empty() {
            continue;
        }
        let is_new_commit = l.starts_with("commit ");
        if is_new_commit && blocks.last().is_some_and(|b| !b.is_empty()) {
            blocks.push(Vec::new());
        }
        if blocks.is_empty() {
            blocks.push(Vec::new());
        }
        if let Some(b) = blocks.last_mut() {
            b.push(l);
        }
    }
    blocks
}

/// 从一条提交块提取 `commit <hash>` + Author/Date + 主题（首个缩进行）。
fn push_commit_summary(out: &mut String, block: &[&str]) {
    for l in block {
        if l.starts_with("commit ") || l.starts_with("Author:") || l.starts_with("Date:") {
            out.push_str(l);
            out.push('\n');
        } else if l.starts_with("    ") || l.starts_with('\t') {
            out.push_str(l.trim_start());
            out.push('\n');
            return;
        }
    }
}

// ── 过滤器 4：cargo ───────────────────────────────────────────────────────────

/// `cargo build/test/check/clippy` 输出过滤器：只保留错误/警告（含上下文）、进度与结果汇总行。
pub struct CargoFilter;

impl OutputFilter for CargoFilter {
    fn name(&self) -> &'static str {
        "cargo"
    }

    fn matches(&self, cmd: &str, args: &[String]) -> bool {
        cmd == "cargo"
            && args
                .first()
                .is_some_and(|a| matches!(a.as_str(), "build" | "test" | "check" | "clippy"))
    }

    fn minimize(&self, output: &str) -> Option<String> {
        cargo_minimize(output)
    }
}

/// 诊断行后附带保留的上下文行数（如 `--> src/…`）。
const CARGO_CONTEXT_LINES: usize = 2;

fn cargo_minimize(output: &str) -> Option<String> {
    if output.trim().is_empty() {
        return None;
    }
    let lines: Vec<&str> = output.lines().collect();
    // 适用性：输出中须出现 cargo 诊断/进度/结果标记之一，否则视为非 cargo 输出。
    if !lines.iter().copied().any(is_cargo_marker) {
        return None;
    }
    let mut kept = vec![false; lines.len()];

    // 1) 错误/警告/提示行及其后 2 行上下文（如 `--> src/…`）。
    for (i, l) in lines.iter().enumerate() {
        if is_cargo_diagnostic(l) {
            let end = (i + 1 + CARGO_CONTEXT_LINES).min(lines.len());
            kept[i..end].fill(true);
        }
    }
    // 2) 进度与结果汇总行（进度仅保留最后一条）。
    let mut last_progress: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if t.starts_with("Compiling ") || t.starts_with("Checking ") {
            last_progress = Some(i);
        }
        if t.starts_with("Finished ")
            || l.contains("could not compile")
            || t.starts_with("test result:")
        {
            kept[i] = true;
        }
    }
    if let Some(i) = last_progress {
        kept[i] = true;
    }

    let dropped = kept.iter().filter(|k| !**k).count();
    if dropped == 0 {
        return None; // 无可折叠内容
    }
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if kept[i] {
            out.push_str(l);
            out.push('\n');
        }
    }
    out.push_str(&format!("… 已折叠 {dropped} 行（cargo 输出）…\n"));
    // 摘要行数须严格小于原始行数（含上下文行时小输出可能反而变长 → 不压缩）。
    if out.lines().count() >= lines.len() {
        return None;
    }
    Some(out)
}

/// cargo 输出标记行：诊断、进度或结果汇总。
fn is_cargo_marker(l: &str) -> bool {
    let t = l.trim_start();
    is_cargo_diagnostic(l)
        || t.starts_with("Compiling ")
        || t.starts_with("Checking ")
        || t.starts_with("Finished ")
        || l.contains("could not compile")
        || t.starts_with("test result:")
}

/// 诊断行判定：error/warning/note/help 开头（含 `error[E…]`、`= note:` 等变体）。
fn is_cargo_diagnostic(l: &str) -> bool {
    let t = l.trim_start();
    t.starts_with("error")
        || t.starts_with("warning")
        || t.starts_with("note:")
        || t.starts_with("help:")
        || t.starts_with("= note:")
        || t.starts_with("= help:")
}

// ── 过滤器 5：python / pytest ─────────────────────────────────────────────────

/// python / python3 输出过滤器：保留最后一块 traceback、失败测试名与结果汇总行。
pub struct PythonFilter;

impl OutputFilter for PythonFilter {
    fn name(&self) -> &'static str {
        "python"
    }

    fn matches(&self, cmd: &str, _args: &[String]) -> bool {
        matches!(cmd, "python" | "python3" | "pytest")
    }

    fn minimize(&self, output: &str) -> Option<String> {
        python_minimize(output)
    }
}

fn python_minimize(output: &str) -> Option<String> {
    if output.trim().is_empty() {
        return None;
    }
    let lines: Vec<&str> = output.lines().collect();
    let has_traceback = lines
        .iter()
        .any(|l| l.contains("Traceback (most recent call last)"));
    let has_failed = lines.iter().any(|l| l.trim_start().starts_with("FAILED "));
    let has_summary = lines.iter().copied().any(is_python_summary);
    if !has_traceback && !has_failed && !has_summary {
        return None; // 非 traceback / pytest 输出
    }
    let mut kept = vec![false; lines.len()];

    // 1) 最后一块 traceback（Traceback 起 → 帧行 → 异常行）。
    if let Some(t) = lines
        .iter()
        .rposition(|l| l.contains("Traceback (most recent call last)"))
    {
        kept[t] = true;
        let mut k = t + 1;
        while k < lines.len() && (lines[k].starts_with("  File ") || lines[k].starts_with("    ")) {
            kept[k] = true;
            k += 1;
        }
        // 异常行：帧后的首个非空、非缩进行（pytest 中常为 `E   AssertionError: …`）。
        while k < lines.len() && lines[k].trim().is_empty() {
            k += 1;
        }
        if k < lines.len() && !lines[k].starts_with("  ") {
            kept[k] = true;
        }
    }

    // 2) 失败测试名（`FAILED tests/…`）。
    for (i, l) in lines.iter().enumerate() {
        if l.trim_start().starts_with("FAILED ") {
            kept[i] = true;
        }
    }

    // 3) 结果汇总行（ERROR / short summary / `= N passed, M failed … =`）。
    for (i, l) in lines.iter().enumerate() {
        if is_python_summary(l) {
            kept[i] = true;
        }
    }

    let dropped = kept.iter().filter(|k| !**k).count();
    if dropped == 0 {
        return None;
    }
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if kept[i] {
            out.push_str(l);
            out.push('\n');
        }
    }
    out.push_str(&format!("… 已折叠 {dropped} 行（python 输出）…\n"));
    // 摘要行数须严格小于原始行数（traceback 即全文时无可压缩 → 不压缩）。
    if out.lines().count() >= lines.len() {
        return None;
    }
    Some(out)
}

/// pytest 结果汇总行：`ERROR …` / `= short test summary info =` / `= N passed, M failed … =` 等。
fn is_python_summary(l: &str) -> bool {
    let t = l.trim_start();
    t.starts_with("ERROR")
        || t.starts_with("= short test summary info =")
        || (t.contains(" passed") && (t.contains(" failed") || t.contains(" error")))
        || (t.starts_with('=') && t.contains("in ") && t.ends_with('='))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 断言同一输入两次调用产出完全一致的结果（确定性：纯函数、无时间/随机）。
    fn assert_deterministic(f: &dyn Fn(&str) -> Option<String>, input: &str) {
        assert_eq!(f(input), f(input), "minimize 应确定性，输入: {input:?}");
    }

    // ── 通用最小化器 ─────────────────────────────────────────────────────────

    #[test]
    fn apply_generic_fold_head_tail() {
        let m = Minimizer::new(Vec::new(), 10);
        let output: String = (1..=250).map(|i| format!("line {i}\n")).collect();
        let r = m.apply("echo", &[], &output).expect("超阈值应触发通用折叠");
        assert_eq!(r.filter, "generic");
        assert!(r.summary.starts_with("line 1\n"), "应保留头部");
        assert!(r.summary.ends_with("line 250\n"), "应保留尾部");
        assert!(r.summary.contains("… [已折叠 50 行] …\n"), "应含折叠标记");
        assert!(r.summary.ends_with('\n'), "summary 应以换行结尾");
        assert_deterministic(&|o| m.apply("echo", &[], o).map(|x| x.summary), &output);
    }

    #[test]
    fn apply_within_threshold_returns_none() {
        let m = Minimizer::new(Vec::new(), 100);
        let output = "a\nb\nc\n";
        assert!(m.apply("echo", &[], output).is_none(), "未超阈值不压缩");
    }

    #[test]
    fn apply_first_matching_filter_wins() {
        struct First;
        struct Second;
        impl OutputFilter for First {
            fn name(&self) -> &'static str {
                "first"
            }
            fn matches(&self, _cmd: &str, _args: &[String]) -> bool {
                true
            }
            fn minimize(&self, _output: &str) -> Option<String> {
                Some("first-summary".to_string())
            }
        }
        impl OutputFilter for Second {
            fn name(&self) -> &'static str {
                "second"
            }
            fn matches(&self, _cmd: &str, _args: &[String]) -> bool {
                true
            }
            fn minimize(&self, _output: &str) -> Option<String> {
                Some("second-summary".to_string())
            }
        }
        let m = Minimizer::new(vec![Box::new(First), Box::new(Second)], 0);
        let r = m.apply("any", &[], "output").expect("应命中过滤器");
        assert_eq!(r.filter, "first");
        assert_eq!(r.summary, "first-summary\n");
    }

    #[test]
    fn disabled_always_none() {
        let m = disabled();
        assert!(
            m.apply("git", &["status".to_string()], "anything\n")
                .is_none()
        );
    }

    #[test]
    fn split_command_parses_program_and_args() {
        assert_eq!(
            split_command("git status --short"),
            (
                "git".to_string(),
                vec!["status".to_string(), "--short".to_string()]
            )
        );
        assert_eq!(split_command("cargo"), ("cargo".to_string(), Vec::new()));
    }

    // ── git status ───────────────────────────────────────────────────────────

    const STATUS_LONG: &str = "On branch main\nYour branch is ahead of 'origin/main' by 1 commit.\n  (use \"git push\" to publish your local commits)\n\nChanges not staged for commit:\n  (use \"git add <file>...\" to update what will be committed)\n  (use \"git restore <file>...\" to discard changes in working directory)\n\tmodified:   src/foo.rs\n\tmodified:   src/bar.rs\n\tmodified:   src/baz.rs\n\nUntracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n\tsrc/new/a.rs\n\tsrc/new/b.rs\n\tnotes.txt\n\nno changes added to commit (use \"git add\" and/or \"git commit -a\")\n";

    #[test]
    fn git_status_long_folds_and_counts() {
        let f = GitStatusFilter;
        let s = f.minimize(STATUS_LONG).expect("长格式 status 应命中");
        assert!(s.contains("On branch main\n"), "应保留分支信息");
        assert!(
            s.contains("Changes not staged for commit:\n共 3 条变更\n"),
            "应保留节头与计数"
        );
        assert!(s.contains("\tmodified:   src/foo.rs\n"), "应保留关键文件名");
        assert!(
            s.contains("Untracked files:\n共 3 条变更\n"),
            "未跟踪节应计数"
        );
        assert!(s.contains("\tsrc/new/ (2 个文件)\n"), "未跟踪目录应折叠");
        assert!(!s.contains("\tsrc/new/a.rs\n"), "折叠组成员不再单列");
        assert!(s.contains("\tnotes.txt\n"), "单文件应原样保留");
        assert!(s.contains("no changes added to commit"), "应保留末尾总结行");
        assert!(!s.contains("  (use \""), "应丢弃 (use ...) 提示行");
        assert_deterministic(&|o| f.minimize(o), STATUS_LONG);
    }

    #[test]
    fn git_status_overflow_note() {
        let mut input = String::from("Changes not staged for commit:\n");
        for i in 0..12 {
            input.push_str(&format!("\tmodified:   src/f{i}.rs\n"));
        }
        let s = GitStatusFilter.minimize(&input).expect("应命中");
        assert!(s.contains("… 还有 2 条\n"), "超出 10 条应追加剩余计数: {s}");
        assert!(s.contains("共 12 条变更\n"), "计数应为原始条目数");
    }

    #[test]
    fn git_status_short_format() {
        let input = " M src/foo.rs\n?? src/a.rs\n?? src/b.rs\n?? notes.txt\n";
        let s = GitStatusFilter.minimize(input).expect("短格式应命中");
        assert!(s.contains("共 4 条变更\n"));
        assert!(
            s.contains("\t?? src/ (2 个文件)\n"),
            "短格式未跟踪应折叠: {s}"
        );
        assert!(s.contains("\t M src/foo.rs\n"));
    }

    #[test]
    fn git_status_miss_and_empty() {
        let f = GitStatusFilter;
        assert!(
            f.minimize("just some text\nno status markers\n").is_none(),
            "非 status 输出不适用"
        );
        assert!(f.minimize("").is_none(), "空输出不适用");
        assert!(
            !f.matches("git", &["status".to_string(), "--porcelain".to_string()]),
            "--porcelain 应排除"
        );
        assert!(
            f.matches("git", &["status".to_string()]),
            "git status 应命中"
        );
    }

    // ── git diff ─────────────────────────────────────────────────────────────

    const DIFF_HUNKS: &str = "diff --git a/src/main.rs b/src/main.rs\n\
index 1234567..89abcde 100644\n\
--- a/src/main.rs\n\
+++ b/src/main.rs\n\
@@ -1,5 +1,6 @@\n\
 fn main() {\n\
-    println!(\"old\");\n\
+    println!(\"new\");\n\
+    println!(\"extra\");\n\
 }\n\
diff --git a/Cargo.toml b/Cargo.toml\n\
index abc..def 100644\n\
--- a/Cargo.toml\n\
+++ b/Cargo.toml\n\
@@ -10,3 +10,4 @@\n\
 edition = \"2021\"\n\
+rust-version = \"1.85\"\n";

    #[test]
    fn git_diff_keeps_hunks_and_stats() {
        let f = GitDiffFilter;
        let s = f.minimize(DIFF_HUNKS).expect("diff 输出应命中");
        assert!(s.contains("@@ -1,5 +1,6 @@\n"), "应保留 hunk 头");
        assert!(s.contains("@@ -10,3 +10,4 @@\n"), "应保留全部 hunk 头");
        assert!(s.contains("统计：+3 -1\n"), "无 --stat 应追加增减统计: {s}");
        assert!(
            !s.contains("println!(\"new\")"),
            "应丢弃 hunk 内 +/- 内容行"
        );
        assert!(
            !s.contains("diff --git a/src/main.rs"),
            "应丢弃 diff 文件头"
        );
        assert_deterministic(&|o| f.minimize(o), DIFF_HUNKS);
    }

    #[test]
    fn git_diff_stat_only() {
        let input = " src/main.rs | 3 ++-\n src/other.rs | 5 +++--\n 1 file changed, 8 insertions(+), 2 deletions(-)\nrandom noise line\nmore noise here\n";
        let s = GitDiffFilter.minimize(input).expect("stat 输出应命中");
        assert!(s.contains("src/main.rs | 3 ++"), "应保留 stat 行");
        assert!(
            s.contains("1 file changed, 8 insertions(+), 2 deletions(-)"),
            "应保留 stat 汇总"
        );
        assert!(!s.contains("noise"), "非 stat 行应折叠");
        assert!(!s.contains("统计："), "--stat 已含统计，不应再追加");
    }

    #[test]
    fn git_diff_miss_and_empty() {
        let f = GitDiffFilter;
        assert!(
            f.minimize("hello world\nnothing to do\n").is_none(),
            "非 diff 输出不适用"
        );
        assert!(f.minimize("").is_none(), "空输出不适用");
        assert!(f.matches("git", &["diff".to_string(), "--stat".to_string()]));
        assert!(!f.matches("git", &["status".to_string()]));
    }

    // ── git log ──────────────────────────────────────────────────────────────

    fn long_log(n: usize) -> String {
        let mut out = String::new();
        for i in 0..n {
            let h = format!("{:040x}", i + 1);
            out.push_str(&format!("commit {h} (HEAD -> main)\n"));
            out.push_str("Author: Test <test@example.com>\n");
            out.push_str("Date:   Mon Aug 1 10:00:00 2026 +0800\n\n");
            out.push_str(&format!("    Subject {i}\n\n"));
        }
        out
    }

    #[test]
    fn git_log_long_truncates_commits() {
        let f = GitLogFilter;
        let s = f.minimize(&long_log(20)).expect("长格式 log 应命中");
        assert!(s.contains("commit 0000000000000000000000000000000000000001 (HEAD -> main)\n"));
        assert!(s.contains("Subject 0\n"), "应保留首条提交主题");
        assert!(s.contains("… 还有 5 条提交\n"), "应追加剩余提交计数");
        assert!(!s.contains("Subject 15"), "超出的提交应被截断");
        assert_deterministic(&|o| f.minimize(o), &long_log(20));
    }

    #[test]
    fn git_log_oneline_truncates() {
        let input: String = (0..40)
            .map(|i| format!("abc123{i:x} subject {i}\n"))
            .collect();
        let s = GitLogFilter.minimize(&input).expect("oneline log 应命中");
        assert!(s.contains("… 还有 10 条提交\n"));
        assert!(!s.contains("subject 30"));
    }

    #[test]
    fn git_log_miss_and_empty() {
        let f = GitLogFilter;
        assert!(
            f.minimize("random text\nnot a log\n").is_none(),
            "非 log 输出不适用"
        );
        assert!(f.minimize("").is_none(), "空输出不适用");
        assert!(f.matches("git", &["log".to_string(), "--oneline".to_string()]));
        assert!(!f.matches("git", &["status".to_string()]));
    }

    // ── cargo ────────────────────────────────────────────────────────────────

    const CARGO_OUT: &str = "Compiling foo v0.1.0\nCompiling bar v0.2.0\nCompiling baz v0.3.0\nCompiling qux v0.4.0\nCompiling quux v0.5.0\nCompiling corge v0.6.0\nCompiling grault v0.7.0\nCompiling garply v0.8.0\nCompiling waldo v0.9.0\nCompiling fred v0.10.0\nCompiling plugh v0.11.0\nCompiling xyzzy v0.12.0\nwarning: unused variable: `x`\n  --> src/main.rs:9:7\n   |\nerror[E0308]: mismatched types\n  --> src/main.rs:5:3\n   |\nerror: could not compile `foo` due to previous error\n";

    #[test]
    fn cargo_keeps_diagnostics_and_summaries() {
        let f = CargoFilter;
        let s = f.minimize(CARGO_OUT).expect("cargo 输出应命中");
        assert!(
            s.contains("warning: unused variable: `x`\n  --> src/main.rs:9:7\n"),
            "应保留诊断与上下文"
        );
        assert!(
            s.contains("error[E0308]: mismatched types\n  --> src/main.rs:5:3\n"),
            "应保留 error[E..] 汇总"
        );
        assert!(s.contains("error: could not compile"), "应保留编译失败汇总");
        assert!(
            s.contains("Compiling xyzzy v0.12.0\n"),
            "应保留最后一条进度"
        );
        assert!(!s.contains("Compiling foo v0.1.0"), "早期进度行应折叠");
        assert!(
            s.contains("… 已折叠 11 行（cargo 输出）…\n"),
            "应含折叠标记: {s}"
        );
        assert_deterministic(&|o| f.minimize(o), CARGO_OUT);
    }

    #[test]
    fn cargo_miss_and_empty() {
        let f = CargoFilter;
        assert!(
            f.minimize("hello\nworld\n").is_none(),
            "无 cargo 标记不适用"
        );
        assert!(f.minimize("").is_none(), "空输出不适用");
        assert!(f.matches("cargo", &["build".to_string()]));
        assert!(f.matches("cargo", &["clippy".to_string(), "--all".to_string()]));
        assert!(!f.matches("cargo", &["run".to_string()]));
        assert!(!f.matches("git", &["status".to_string()]));
    }

    // ── python / pytest ──────────────────────────────────────────────────────

    const PY_TRACEBACK: &str = "============================= test session starts =============================\nplatform linux -- Python 3.12.0\ncollected 3 items\n\ntests/test_a.py F.\ntests/test_b.py F.\ntests/test_c.py F.\ntests/test_d.py F.\ntests/test_e.py F.\ntests/test_f.py F.\ntests/test_g.py F.\ntests/test_h.py F.\ntests/test_i.py F.\ntests/test_j.py F.\n\nTraceback (most recent call last):\n  File \"/usr/lib/python3.12/runpy.py\", line 196, in _run_module_as_main\n    return _run_code(code, main_globals, None)\n  File \"/tmp/foo.py\", line 3, in <module>\n    main()\n  File \"/tmp/foo.py\", line 7, in main\n    raise ValueError(\"boom\")\nValueError: boom\n";

    const PY_PYTEST: &str = "============================= test session starts =============================\nplatform linux -- Python 3.12.0\ncollected 3 items\n\ntests/test_x.py F..\n\n=================================== FAILURES ===================================\n_________________________________ test_a ______________________________________\n\n    def test_a():\n>       assert 1 == 2\nE       AssertionError: assert 1 == 2\n\ntests/test_x.py:5: AssertionError\n=========================== short test summary info ===========================\nFAILED tests/test_x.py::test_a - AssertionError: assert 1 == 2\n========================= 1 failed, 2 passed in 0.05s =========================\n";

    #[test]
    fn python_keeps_last_traceback() {
        let f = PythonFilter;
        let s = f.minimize(PY_TRACEBACK).expect("traceback 输出应命中");
        assert!(s.contains("Traceback (most recent call last):\n"));
        assert!(
            s.contains("File \"/tmp/foo.py\", line 7, in main\n"),
            "应保留帧行"
        );
        assert!(s.contains("ValueError: boom\n"), "应保留异常行");
        assert!(
            !s.contains("test session starts"),
            "应折叠 traceback 前的会话噪声"
        );
        assert!(s.contains("… 已折叠"), "应含折叠标记");
        assert_deterministic(&|o| f.minimize(o), PY_TRACEBACK);
    }

    #[test]
    fn python_pytest_summary_and_failures() {
        let f = PythonFilter;
        let s = f.minimize(PY_PYTEST).expect("pytest 输出应命中");
        assert!(
            s.contains("FAILED tests/test_x.py::test_a - AssertionError: assert 1 == 2\n"),
            "应保留失败测试名"
        );
        assert!(s.contains("1 failed, 2 passed in 0.05s"), "应保留结果汇总");
        assert!(!s.contains("def test_a()"), "应折叠测试细节");
        assert!(s.contains("… 已折叠"), "应含折叠标记");
    }

    #[test]
    fn python_miss_and_empty() {
        let f = PythonFilter;
        assert!(
            f.minimize("hello world\n").is_none(),
            "非 python 输出不适用"
        );
        assert!(f.minimize("").is_none(), "空输出不适用");
        assert!(f.matches("python3", &["-m".to_string(), "pytest".to_string()]));
        assert!(f.matches("python", &[]));
        assert!(!f.matches("node", &[]));
    }

    #[test]
    fn default_filters_contains_all_five() {
        let filters = default_filters();
        let names: Vec<&str> = filters.iter().map(|f| f.name()).collect();
        for want in ["git-status", "git-diff", "git-log", "cargo", "python"] {
            assert!(names.contains(&want), "缺内置过滤器 {want}: {names:?}");
        }
    }
}
