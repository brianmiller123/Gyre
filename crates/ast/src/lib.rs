//! # agent-ast
//!
//! AST 代码操控（移植 oh-my-pi `pi-ast/block.rs`）：
//! [`block_range_at`] —— 给定代码与 1-indexed 行号，用 tree-sitter 解析出该行起始的
//! 「命名块」行范围（爬到在该行起始的最外层命名祖先，排除整文件根）。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

mod pattern;

pub use pattern::{AstMatch, AstMatchStrictness, rewrite, rewrite_rust, search, search_rust};

use tree_sitter::{Language, Node, Parser, Point};

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
}
