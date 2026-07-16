//! # agent-ast
//!
//! AST 代码操控（移植 oh-my-pi `pi-ast/block.rs`）：
//! [`block_range_at`] —— 给定代码与 1-indexed 行号，用 tree-sitter 解析出该行起始的
//! 「命名块」行范围（爬到在该行起始的最外层命名祖先，排除整文件根）。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

mod pattern;
pub mod summary;

pub use pattern::{AstMatch, AstMatchStrictness, rewrite, rewrite_rust, search, search_rust};
pub use summary::{
    summarize_code, SegmentKind, SummaryOptions, SummaryResult, SummarySegment,
};

use std::collections::BTreeSet;

use tree_sitter::{Language, Node, Parser, Point, TreeCursor};

/// 命名块行范围（1-indexed，闭区间）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRange {
    /// 起始行（含）。
    pub start_line: u32,
    /// 结束行（含）。
    pub end_line: u32,
}

/// 支持的语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportLang {
    /// Rust。
    Rust,
    /// Python。
    Python,
    /// JavaScript。
    JavaScript,
    /// TypeScript。
    TypeScript,
    /// Go。
    Go,
}

impl SupportLang {
    /// 按路径扩展名推断。
    #[must_use]
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => Some(Self::Rust),
            Some("py" | "pyi") => Some(Self::Python),
            Some("js" | "mjs" | "cjs" | "jsx") => Some(Self::JavaScript),
            Some("ts" | "mts" | "cts" | "tsx") => Some(Self::TypeScript),
            Some("go") => Some(Self::Go),
            _ => None,
        }
    }

    /// 稳定短标识符（用于工具 schema 与日志）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Go => "go",
        }
    }

    /// 从短标识符解析（`as_str` 的对偶）。
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "rust" | "rs" => Some(Self::Rust),
            "python" | "py" => Some(Self::Python),
            "javascript" | "js" => Some(Self::JavaScript),
            "typescript" | "ts" => Some(Self::TypeScript),
            "go" | "golang" => Some(Self::Go),
            _ => None,
        }
    }

    /// tree-sitter 语言句柄（供 `block_range_at`）。
    #[must_use]
    pub fn ts_language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }
}

/// 解析某行起始的命名块行范围（移植 pi-ast `block_range_at`）。
///
/// 返回 `None`（软「无块」）当：行越界/空行、语言不支持、无节点在该行起始、
/// 或解析子树含语法错误。
///
/// # Errors
/// 仅在 tree-sitter 解析器初始化失败时返回错误字符串。
pub fn block_range_at(
    code: &str,
    line: u32,
    lang: SupportLang,
) -> Result<Option<BlockRange>, String> {
    if line == 0 || code.is_empty() {
        return Ok(None);
    }
    let row = (line - 1) as usize;
    let line_str = code.lines().nth(row);
    let Some(line_str) = line_str else {
        return Ok(None);
    };
    let Some(col) = first_content_col(line_str) else {
        return Ok(None); // 空行/纯空白
    };

    let mut parser = Parser::new();
    let language = lang.ts_language();
    parser
        .set_language(&language)
        .map_err(|e| format!("set_language: {e}"))?;
    let Some(tree) = parser.parse(code, None) else {
        return Ok(None);
    };
    let root = tree.root_node();

    // 在「首个内容字符」上做 1 列宽的点范围查询（避开零宽分隔节点）。
    let point = Point::new(row, col);
    let point_end = Point::new(row, col.saturating_add(1));
    let Some(leaf) = root.named_descendant_for_point_range(point, point_end) else {
        return Ok(None);
    };
    // 叶子起始行早于 row → 落在续行/闭合括号上，无块起始于此行。
    if leaf.start_position().row != row {
        return Ok(None);
    }

    // 爬到仍在该行起始的最外层命名祖先（排除整文件根）。
    let mut node = leaf;
    while let Some(parent) = node.parent() {
        if parent.id() == root.id() {
            break;
        }
        if parent.start_position().row != row {
            break;
        }
        node = parent;
    }

    // 拒绝退化错误恢复区间：子树含语法错误则放弃。
    if node.has_error() {
        return Ok(None);
    }

    Ok(Some(BlockRange {
        start_line: node.start_position().row as u32 + 1,
        end_line: content_end_line(node),
    }))
}

/// 节点内容结束行（1-indexed）。
fn content_end_line(node: Node) -> u32 {
    node.end_position().row as u32 + 1
}

/// 某行首个非空白字符的字节列号。
fn first_content_col(line: &str) -> Option<usize> {
    for (col, byte) in line.bytes().enumerate() {
        if byte != b' ' && byte != b'\t' {
            return Some(col);
        }
    }
    None
}

// ── enclosing_block_boundaries（移植 pi-ast block.rs）──────────────────────

/// 可见行区间（1-indexed，闭区间）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    /// 起始行（含）。
    pub start_line: u32,
    /// 结束行（含）。
    pub end_line: u32,
}

/// 对每个跨越可见窗口边界的多行命名节点，返回位于窗口*之外*的边界行。
///
/// 移植 pi-ast `enclosing_block_boundaries`：把「显示匹配括号」推广到任意 tree-sitter 块——
/// 节点在可见行开口但闭合于窗口外 → 返回其闭合行；节点在可见行闭合但开口于窗口前 → 返回其起始行。
/// 因触发条件是端点落入窗口内，结果受窗口大小（而非嵌套深度）约束；同时覆盖缩进语言（Python）。
///
/// 返回 `None` 当源码解析失败 / 含语法错误（调用方应回退词法括号扫描）；
/// 否则返回排序去重的边界行（可能为空）。
///
/// # Errors
/// 仅在 tree-sitter 解析器初始化失败时返回错误字符串。
pub fn enclosing_block_boundaries(
    code: &str,
    lang: SupportLang,
    ranges: &[LineRange],
) -> Result<Option<Vec<u32>>, String> {
    let merged = normalize_ranges(ranges);
    if code.is_empty() || merged.is_empty() {
        return Ok(Some(Vec::new()));
    }

    let mut parser = Parser::new();
    let language = lang.ts_language();
    parser
        .set_language(&language)
        .map_err(|e| format!("set_language: {e}"))?;
    let Some(tree) = parser.parse(code, None) else {
        return Ok(None);
    };
    let root = tree.root_node();
    // 文件级语法错误会使错误恢复区间不可靠：交给词法扫描器，不在此产出边界。
    if root.has_error() {
        return Ok(None);
    }

    let mut boundaries = BTreeSet::new();
    let mut cursor = root.walk();
    collect_boundaries(&mut cursor, &merged, &mut boundaries);
    Ok(Some(boundaries.into_iter().collect()))
}

/// 排序、丢弃非法、合并相邻/重叠区间，使可见性测试可二分查找。
fn normalize_ranges(ranges: &[LineRange]) -> Vec<LineRange> {
    let mut v: Vec<LineRange> = ranges
        .iter()
        .copied()
        .filter(|r| r.start_line > 0 && r.end_line >= r.start_line)
        .collect();
    v.sort_by(|a, b| a.start_line.cmp(&b.start_line).then(a.end_line.cmp(&b.end_line)));
    let mut merged: Vec<LineRange> = Vec::with_capacity(v.len());
    for range in v {
        if let Some(last) = merged.last_mut() {
            if range.start_line <= last.end_line.saturating_add(1) {
                last.end_line = last.end_line.max(range.end_line);
                continue;
            }
        }
        merged.push(range);
    }
    merged
}

/// `line` 是否落在任一合并后的可见区间内（二分）。
fn is_visible(merged: &[LineRange], line: u32) -> bool {
    merged
        .binary_search_by(|range| {
            if line < range.start_line {
                std::cmp::Ordering::Greater
            } else if line > range.end_line {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// 深度优先遍历，从每个跨越可见区间边的多行命名节点收集边界行。
/// 复用单个 [`TreeCursor`] 使遍历零分配。
fn collect_boundaries(cursor: &mut TreeCursor<'_>, merged: &[LineRange], out: &mut BTreeSet<u32>) {
    let node = cursor.node();
    // 跳过整文件根：其唯一「边界」是 EOF，永不是有用的匹配行（与 block_range_at 排除根一致）。
    if node.is_named() && node.parent().is_some() {
        let start = node.start_position().row as u32 + 1;
        let end = content_end_line(node);
        if end > start {
            let start_visible = is_visible(merged, start);
            let end_visible = is_visible(merged, end);
            // 开口在可见、闭合在窗口外 → 暴露闭合行（反之亦然）。
            // 完全在窗口内或外的节点不贡献任何边界。
            if start_visible && !end_visible {
                out.insert(end);
            } else if end_visible && !start_visible {
                out.insert(start);
            }
        }
    }
    if cursor.goto_first_child() {
        loop {
            collect_boundaries(cursor, merged, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_function_block() {
        let code = "mod m {\n    fn hello() {\n        let x = 1;\n    }\n}\n";
        // 第 2 行 `fn hello() {` 起始的块
        let range = block_range_at(code, 2, SupportLang::Rust).unwrap().unwrap();
        assert_eq!(range.start_line, 2);
        assert_eq!(range.end_line, 4);
    }

    #[test]
    fn none_on_blank_line() {
        let code = "fn a() {}\n\nfn b() {}\n";
        assert!(
            block_range_at(code, 2, SupportLang::Rust)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn from_path_infers_rust() {
        assert_eq!(
            SupportLang::from_path(std::path::Path::new("a.rs")),
            Some(SupportLang::Rust)
        );
        assert_eq!(SupportLang::from_path(std::path::Path::new("a.txt")), None);
    }

    #[test]
    fn enclosing_surfaces_closer_outside_window() {
        // fn x 在 1-4 行；可见窗口仅到第 3 行 → 闭合行 4 在窗口外，应被暴露
        let code = "fn x() {\n    if y {\n    }\n}\n";
        let ranges = [LineRange {
            start_line: 1,
            end_line: 3,
        }];
        let b = enclosing_block_boundaries(code, SupportLang::Rust, &ranges)
            .unwrap()
            .unwrap();
        assert!(b.contains(&4), "应暴露闭合行 4，得到 {b:?}");
    }

    #[test]
    fn enclosing_surfaces_opener_before_window() {
        // 闭合行 4 可见、开口行 1 在窗口前 → 暴露起始行 1
        let code = "fn x() {\n    if y {\n    }\n}\n";
        let ranges = [LineRange {
            start_line: 4,
            end_line: 4,
        }];
        let b = enclosing_block_boundaries(code, SupportLang::Rust, &ranges)
            .unwrap()
            .unwrap();
        assert!(b.contains(&1), "应暴露起始行 1，得到 {b:?}");
    }

    #[test]
    fn enclosing_empty_when_block_fully_visible() {
        let code = "fn x() {\n}\n";
        let ranges = [LineRange {
            start_line: 1,
            end_line: 2,
        }];
        let b = enclosing_block_boundaries(code, SupportLang::Rust, &ranges)
            .unwrap()
            .unwrap();
        assert!(b.is_empty(), "块完全可见时无边界，得到 {b:?}");
    }

    #[test]
    fn enclosing_empty_when_no_ranges_or_empty_code() {
        let none: [LineRange; 0] = [];
        assert!(enclosing_block_boundaries("fn x() {\n}\n", SupportLang::Rust, &none)
            .unwrap()
            .unwrap()
            .is_empty());
        assert!(enclosing_block_boundaries(
            "",
            SupportLang::Rust,
            &[LineRange {
                start_line: 1,
                end_line: 3,
            }]
        )
        .unwrap()
        .unwrap()
        .is_empty());
    }

    #[test]
    fn enclosing_none_on_syntax_error() {
        // 明显非法的 Rust → root.has_error() → None（调用方应回退词法扫描）
        let code = "@@@ not valid rust @@@\n";
        let ranges = [LineRange {
            start_line: 1,
            end_line: 1,
        }];
        assert_eq!(
            enclosing_block_boundaries(code, SupportLang::Rust, &ranges).unwrap(),
            None
        );
    }
}
