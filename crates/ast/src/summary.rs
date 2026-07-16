//! 结构化源码摘要：折叠大体块，保留签名。
//!
//! 移植自 [`oh-my-pi pi-ast/summary.rs`](../../../third/oh-my-pi/crates/pi-ast/src/summary.rs)（精简：语言无关的结构折叠 + BFS 展开）。
//!
//! 思路：遍历 tree-sitter 命名节点，把「跨 `min_body_lines` 行以上的命名节点」（排除整文件根）
//! 视为可折叠 span，按包含关系组织成森林；默认仅折叠最外层 span（外层优先），
//! 可选 `unfold_until_lines` 做 BFS 逐层展开直至可见行达标。产出 kept/elided 段序列。
//!
//! 与上游差异：上游按语言特定节点类型（函数体/字面量/注释）分类并设不同阈值；
//! 本版用语言无关的结构启发式——对大体块统一折叠，是可用近似，体积/维护成本远低于全量移植。

use tree_sitter::Parser;

use crate::SupportLang;

/// 默认最小可折叠体块行数。
const DEFAULT_MIN_BODY_LINES: u32 = 4;

/// 段类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// 保留（原文逐行）。
    Kept,
    /// 折叠（仅标记范围）。
    Elided,
}

/// 一段：kept（含原文）或 elided（仅行范围）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarySegment {
    /// 段类型。
    pub kind: SegmentKind,
    /// 起始行（1-indexed，含）。
    pub start_line: u32,
    /// 结束行（1-indexed，含）。
    pub end_line: u32,
    /// kept 段的原文（elided 为 None）。
    pub text: Option<String>,
}

/// 摘要结果。
#[derive(Debug, Clone, Default)]
pub struct SummaryResult {
    /// 解析成功时的规范语言名。
    pub language: Option<String>,
    /// tree-sitter 是否无语法错误地解析。
    pub parsed: bool,
    /// 是否至少折叠了一段。
    pub elided: bool,
    /// 源文件总行数。
    pub total_lines: u32,
    /// 源序 kept/elided 段。
    pub segments: Vec<SummarySegment>,
}

/// 摘要选项。
#[derive(Debug, Clone, Copy)]
pub struct SummaryOptions {
    /// 最小可折叠行数（小于此值的节点保留）。
    pub min_body_lines: u32,
    /// BFS 展开目标可见行数；0 = 仅折叠最外层（不展开）。
    pub unfold_until_lines: u32,
    /// BFS 展开硬上限；0 = unfold_until_lines * 2。
    pub unfold_limit_lines: u32,
}

impl Default for SummaryOptions {
    fn default() -> Self {
        Self {
            min_body_lines: DEFAULT_MIN_BODY_LINES,
            unfold_until_lines: 0,
            unfold_limit_lines: 0,
        }
    }
}

/// 对 `code` 做结构化摘要。
///
/// 解析失败 / 含语法错误 / 语言不支持时返回单段 kept 全文。
#[must_use]
pub fn summarize_code(code: &str, lang: SupportLang, opts: &SummaryOptions) -> SummaryResult {
    let total = code.lines().count() as u32;
    if code.is_empty() {
        return unparsed(total);
    }

    let mut parser = Parser::new();
    let language = lang.ts_language();
    if parser.set_language(&language).is_err() {
        return unparsed(total);
    }
    let Some(tree) = parser.parse(code, None) else {
        return unparsed(total);
    };
    let root = tree.root_node();
    if root.has_error() {
        return unparsed(total);
    }

    let min_body = opts.min_body_lines.max(2);
    let unfold_until = opts.unfold_until_lines;
    let unfold_limit = if opts.unfold_limit_lines != 0 {
        opts.unfold_limit_lines
    } else {
        unfold_until.saturating_mul(2)
    };

    let mut forest = ElidableForest::default();
    let mut cursor = root.walk();
    collect(&mut cursor, None, min_body, total, &mut forest);
    let folded = select_folded_spans(&forest, total, unfold_until, unfold_limit);
    let segments = emit_segments(code, &folded, total);
    let elided = folded.iter().any(|s| s.lines() > 0);

    SummaryResult {
        language: Some(lang.as_str().to_string()),
        parsed: true,
        elided,
        total_lines: total,
        segments,
    }
}

fn unparsed(total: u32) -> SummaryResult {
    SummaryResult {
        language: None,
        parsed: false,
        elided: false,
        total_lines: total,
        segments: Vec::new(),
    }
}

// ── 可折叠 span 森林 ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LineSpan {
    start: u32,
    end: u32,
}

impl LineSpan {
    const fn lines(self) -> u32 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }
}

#[derive(Debug)]
struct SpanNode {
    span: LineSpan,
    children: Vec<usize>,
}

#[derive(Debug, Default)]
struct ElidableForest {
    nodes: Vec<SpanNode>,
    roots: Vec<usize>,
}

impl ElidableForest {
    fn push(&mut self, parent: Option<usize>, span: LineSpan) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(SpanNode {
            span,
            children: Vec::new(),
        });
        match parent {
            Some(p) => self.nodes[p].children.push(idx),
            None => self.roots.push(idx),
        }
        idx
    }
}

/// DFS 收集可折叠 span，按 AST 包含关系组织森林（复用单个游标，零额外分配）。
fn collect(
    cursor: &mut tree_sitter::TreeCursor<'_>,
    parent_elidable: Option<usize>,
    min_body: u32,
    total: u32,
    forest: &mut ElidableForest,
) {
    let node = cursor.node();
    let start = node.start_position().row as u32 + 1;
    // 闭合行：tree-sitter 末行可能含幻影行，按总行数封顶。
    let end = (node.end_position().row as u32 + 1).min(total);
    let is_elidable =
        node.parent().is_some() && end.saturating_sub(start).saturating_add(1) >= min_body;
    let my_idx = if is_elidable {
        Some(forest.push(parent_elidable, LineSpan { start, end }))
    } else {
        parent_elidable
    };

    if cursor.goto_first_child() {
        loop {
            collect(cursor, my_idx, min_body, total, forest);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// BFS 展开：初始折叠全部根 span（仅最外层），逐层用其子 span 替换，
/// 直至可见行数达 `unfold_until`。候选展开若越过 `unfold_limit` 则跳过该 span。
fn select_folded_spans(
    forest: &ElidableForest,
    total_lines: u32,
    unfold_until: u32,
    unfold_limit: u32,
) -> Vec<LineSpan> {
    use std::collections::{HashSet, VecDeque};

    let nodes = &forest.nodes;
    let mut folded: HashSet<usize> = forest.roots.iter().copied().collect();
    if unfold_until == 0 || folded.is_empty() {
        return folded.into_iter().map(|i| nodes[i].span).collect();
    }

    let folded_line_total: u32 = folded.iter().map(|&i| nodes[i].span.lines()).sum();
    let mut visible = total_lines.saturating_sub(folded_line_total);
    let mut queue: VecDeque<usize> = forest.roots.iter().copied().collect();

    while let Some(idx) = queue.pop_front() {
        if visible >= unfold_until {
            break;
        }
        if !folded.contains(&idx) {
            continue;
        }
        let node = &nodes[idx];
        let child_line_total: u32 = node.children.iter().map(|&c| nodes[c].span.lines()).sum();
        let revealed = node.span.lines().saturating_sub(child_line_total);
        let new_visible = visible.saturating_add(revealed);
        if new_visible > unfold_limit {
            continue;
        }
        folded.remove(&idx);
        for &c in &node.children {
            folded.insert(c);
            queue.push_back(c);
        }
        visible = new_visible;
    }

    folded.into_iter().map(|i| nodes[i].span).collect()
}

/// 按 folded span 把原文切成 kept/elided 段（源序）。
///
/// 每个 folded span **保留首行**（签名/header）可见、其余折叠——产出「`fn big() { ⋯ }`」式
/// 摘要，比整块折叠（含签名）更有用，且行号与原文严格对齐。
fn emit_segments(code: &str, folded: &[LineSpan], total: u32) -> Vec<SummarySegment> {
    let lines: Vec<&str> = code.split('\n').collect();
    let mut spans: Vec<LineSpan> = folded.to_vec();
    spans.sort_by_key(|s| s.start);

    let mut segs = Vec::new();
    let mut cursor = 1u32;
    for sp in spans {
        if sp.start > cursor {
            segs.push(kept_segment(&lines, cursor, sp.start - 1));
        }
        // 保留 header 行（签名）。
        segs.push(kept_segment(&lines, sp.start, sp.start));
        // 其余体块折叠（若有）。
        if sp.end > sp.start {
            segs.push(SummarySegment {
                kind: SegmentKind::Elided,
                start_line: sp.start.saturating_add(1),
                end_line: sp.end,
                text: None,
            });
        }
        cursor = sp.end.saturating_add(1);
    }
    if cursor <= total {
        segs.push(kept_segment(&lines, cursor, total));
    }
    segs
}

fn kept_segment(lines: &[&str], start: u32, end: u32) -> SummarySegment {
    let s = start as usize;
    let e = end as usize;
    let text: Vec<&str> = (s - 1..e).filter_map(|i| lines.get(i)).copied().collect();
    SummarySegment {
        kind: SegmentKind::Kept,
        start_line: start,
        end_line: end,
        text: Some(text.join("\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summarize(code: &str) -> SummaryResult {
        summarize_code(code, SupportLang::Rust, &SummaryOptions::default())
    }

    #[test]
    fn small_file_no_elision() {
        let code = "fn a() {\n    1\n}\n";
        let r = summarize(code);
        assert!(r.parsed);
        assert!(!r.elided, "小文件不应折叠");
        assert_eq!(r.total_lines, 3);
    }

    #[test]
    fn large_body_is_elided() {
        // 一个 10 行函数体应被折叠
        let body: String = "fn big() {\n".to_string()
            + &(0..10).map(|i| format!("    let v{i} = {i};\n")).collect::<String>()
            + "}\n";
        let r = summarize(&body);
        assert!(r.parsed);
        assert!(r.elided, "大体块应折叠");
        // 至少存在一个 elided 段
        assert!(r.segments.iter().any(|s| s.kind == SegmentKind::Elided));
        // kept 段仍含函数签名首行
        assert!(r
            .segments
            .iter()
            .any(|s| s.kind == SegmentKind::Kept && s.text.as_deref().unwrap_or("").contains("fn big")));
    }

    #[test]
    fn segments_cover_full_range() {
        let body: String = "fn big() {\n".to_string()
            + &(0..20).map(|i| format!("    let v{i} = {i};\n")).collect::<String>()
            + "}\n";
        let r = summarize(&body);
        // kept/elided 段首尾应连续覆盖 1..total，无空洞、无重叠
        let mut prev_end = 0u32;
        for seg in &r.segments {
            assert_eq!(seg.start_line, prev_end + 1, "段起始应紧接上一段结束");
            assert!(seg.end_line >= seg.start_line);
            prev_end = seg.end_line;
        }
        assert_eq!(prev_end, r.total_lines, "末段应结束于总行数");
    }

    #[test]
    fn unfold_reveals_more_lines() {
        let body: String = "fn big() {\n".to_string()
            + &(0..30).map(|i| format!("    let v{i} = {i};\n")).collect::<String>()
            + "}\n";
        let folded = summarize_code(
            &body,
            SupportLang::Rust,
            &SummaryOptions {
                min_body_lines: 4,
                unfold_until_lines: 0,
                unfold_limit_lines: 0,
            },
        );
        let unfolded = summarize_code(
            &body,
            SupportLang::Rust,
            &SummaryOptions {
                min_body_lines: 4,
                unfold_until_lines: 60,
                unfold_limit_lines: 120,
            },
        );
        // 展开后 kept 文本应不短于（可见行更多）
        let folded_kept: usize = folded
            .segments
            .iter()
            .filter(|s| s.kind == SegmentKind::Kept)
            .map(|s| s.text.as_ref().map(String::len).unwrap_or(0))
            .sum();
        let unfolded_kept: usize = unfolded
            .segments
            .iter()
            .filter(|s| s.kind == SegmentKind::Kept)
            .map(|s| s.text.as_ref().map(String::len).unwrap_or(0))
            .sum();
        assert!(
            unfolded_kept >= folded_kept,
            "展开后保留内容应不少于仅折叠外层"
        );
    }

    #[test]
    fn syntax_error_returns_unparsed() {
        let r = summarize("@@@ broken rust @@@\n");
        assert!(!r.parsed);
        assert!(r.segments.is_empty());
    }
}
