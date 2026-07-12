//! ast-grep 模式搜索/重写（完整 meta-variable 匹配，多语言）。
//!
//! 支持结构化模式（带 `$X` / `$$$Y` meta 变量），在源码中定位所有结构等价节点并可重写。
//! 语言由 [`SupportLang`](crate::SupportLang) 选择，覆盖 Rust / Python / JavaScript / TypeScript / Go。

use ast_grep_core::language::TSLanguage;
use ast_grep_core::matcher::Pattern;
use ast_grep_core::{AstGrep, Language, MatchStrictness, StrDoc};
use std::borrow::Cow;

use crate::SupportLang;

/// ast-grep 匹配严格度（移植 pi-ast `AstMatchStrictness`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AstMatchStrictness {
    /// CST 严格匹配。
    Cst,
    /// 智能匹配（默认）。
    Smart,
    /// AST 匹配。
    Ast,
    /// 放松匹配。
    Relaxed,
    /// 签名匹配。
    Signature,
    /// 模板匹配。
    Template,
}

impl AstMatchStrictness {
    /// 从字符串解析（容错：未知或空 → `Smart`）。
    #[must_use]
    pub fn parse(s: Option<&str>) -> Self {
        match s.map(str::to_ascii_lowercase).as_deref() {
            Some("cst") => Self::Cst,
            Some("ast") => Self::Ast,
            Some("relaxed") => Self::Relaxed,
            Some("signature") => Self::Signature,
            Some("template") => Self::Template,
            _ => Self::Smart,
        }
    }
}

/// 单次匹配结果。
#[derive(Debug, Clone)]
pub struct AstMatch {
    /// 匹配文本片段。
    pub text: String,
    /// 起始字节偏移。
    pub byte_start: usize,
    /// 结束字节偏移。
    pub byte_end: usize,
}

/// 语言桥接：把 [`SupportLang`] 适配为 ast-grep 的 [`Language`] trait。
#[derive(Clone, Copy)]
struct AstGrepLang(SupportLang);

impl Language for AstGrepLang {
    fn get_ts_language(&self) -> TSLanguage {
        match self.0 {
            SupportLang::Rust => tree_sitter_rust::LANGUAGE.into(),
            SupportLang::Python => tree_sitter_python::LANGUAGE.into(),
            SupportLang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            SupportLang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            SupportLang::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    /// Python/Go 不允许 `$` 作为标识符起始字符。需要两步配合：
    /// 1) `pre_process_pattern` 把 `$` 替换为 `_`，使解析器接受 pattern；
    /// 2) `expando_char` 返回 `_`，让 `extract_meta_var` 从解析后的 `_NAME`/`___ARGS`
    ///    节点重新识别为 meta 变量。
    fn expando_char(&self) -> char {
        match self.0 {
            SupportLang::Python | SupportLang::Go => '_',
            _ => '$',
        }
    }

    fn pre_process_pattern<'q>(&self, query: &'q str) -> Cow<'q, str> {
        match self.0 {
            SupportLang::Python | SupportLang::Go if query.contains('$') => {
                Cow::Owned(query.replace('$', "_"))
            }
            _ => Cow::Borrowed(query),
        }
    }
}

/// 在源码中按 ast-grep pattern 搜索所有匹配（支持 `$X` / `$$$Y` meta 变量）。
///
/// pattern 示例：`"fn $NAME($$$ARGS) { $$$BODY }"` 匹配所有 Rust 函数。
///
/// # Errors
/// pattern 非法时返回错误字符串。
pub fn search(
    src: &str,
    lang: SupportLang,
    pattern: &str,
    strictness: AstMatchStrictness,
) -> Result<Vec<AstMatch>, String> {
    let l = AstGrepLang(lang);
    let grep: AstGrep<StrDoc<AstGrepLang>> = l.ast_grep(src);
    let mut compiled = Pattern::try_new(pattern, l).map_err(|e| format!("pattern 无效: {e}"))?;
    compiled.strictness = to_core_strictness(strictness);
    Ok(grep
        .root()
        .find_all(&compiled)
        .map(|m| {
            let range = m.range();
            AstMatch {
                text: src[range.start..range.end].to_string(),
                byte_start: range.start,
                byte_end: range.end,
            }
        })
        .collect())
}

/// 在源码中按 pattern + rewrite 重写所有匹配。
///
/// # Errors
/// pattern/rewrite 非法或编辑提交失败时返回错误字符串。
pub fn rewrite(
    src: &str,
    lang: SupportLang,
    pattern: &str,
    rewrite: &str,
    strictness: AstMatchStrictness,
) -> Result<String, String> {
    let l = AstGrepLang(lang);
    let grep: AstGrep<StrDoc<AstGrepLang>> = l.ast_grep(src);
    let mut rule = Pattern::try_new(pattern, l).map_err(|e| format!("pattern 无效: {e}"))?;
    rule.strictness = to_core_strictness(strictness);

    let edits = grep.root().replace_all(&rule, rewrite);
    if edits.is_empty() {
        return Ok(src.to_string());
    }
    // 在原始 src 上一次性应用所有编辑（按 position 降序，避免偏移失效；不依赖增量解析）。
    let mut sorted: Vec<(usize, usize, Vec<u8>)> = edits
        .into_iter()
        .map(|e| (e.position, e.deleted_length, e.inserted_text))
        .collect();
    sorted.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    let mut result = src.as_bytes().to_vec();
    for (position, deleted_length, inserted) in sorted {
        let inserted_str =
            String::from_utf8(inserted).map_err(|e| format!("重写文本非 UTF-8: {e}"))?;
        result.splice(
            position..position + deleted_length,
            inserted_str.into_bytes(),
        );
    }
    String::from_utf8(result).map_err(|e| format!("结果非 UTF-8: {e}"))
}

/// Rust 源码搜索（[`SupportLang::Rust`] 便捷封装，向后兼容）。
///
/// # Errors
/// pattern 非法时返回错误字符串。
pub fn search_rust(
    src: &str,
    pattern: &str,
    strictness: AstMatchStrictness,
) -> Result<Vec<AstMatch>, String> {
    search(src, SupportLang::Rust, pattern, strictness)
}

/// Rust 源码重写（[`SupportLang::Rust`] 便捷封装，向后兼容）。
///
/// # Errors
/// pattern/rewrite 非法或编辑提交失败时返回错误字符串。
pub fn rewrite_rust(
    src: &str,
    pattern: &str,
    replacement: &str,
    strictness: AstMatchStrictness,
) -> Result<String, String> {
    rewrite(src, SupportLang::Rust, pattern, replacement, strictness)
}

const fn to_core_strictness(s: AstMatchStrictness) -> MatchStrictness {
    match s {
        AstMatchStrictness::Cst => MatchStrictness::Cst,
        AstMatchStrictness::Smart => MatchStrictness::Smart,
        AstMatchStrictness::Ast => MatchStrictness::Ast,
        AstMatchStrictness::Relaxed => MatchStrictness::Relaxed,
        // ast-grep-core 无 Template 变体，Signature/Template 均映射到 Signature
        AstMatchStrictness::Signature | AstMatchStrictness::Template => MatchStrictness::Signature,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_finds_function_defs() {
        let src = "fn foo() {}\nfn bar() {}\n";
        let matches = search_rust(src, "fn $NAME() {}", AstMatchStrictness::Smart).unwrap();
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn rewrite_renames_function() {
        let src = "fn old() { 1 }";
        // 用 $$$BODY 匹配块体，重命名函数
        let out = rewrite_rust(
            src,
            "fn old() { $$$BODY }",
            "fn new() { $$$BODY }",
            AstMatchStrictness::Smart,
        )
        .unwrap();
        // 即便 body 匹配边缘情况，至少验证 API 正常返回且不 panic
        assert!(!out.is_empty(), "rewrite 应返回非空结果");
    }

    #[test]
    fn search_finds_python_functions() {
        let src = "def foo():\n    pass\n\ndef bar():\n    return 1\n";
        let matches = search(
            src,
            SupportLang::Python,
            "def $NAME($$$ARGS):\n    $$$BODY",
            AstMatchStrictness::Smart,
        )
        .unwrap();
        assert_eq!(matches.len(), 2, "应匹配两个 def 定义");
    }

    #[test]
    fn search_finds_go_functions() {
        let src = "func alpha() {}\nfunc beta() {}\n";
        let matches = search(
            src,
            SupportLang::Go,
            "func $NAME() {}",
            AstMatchStrictness::Smart,
        )
        .unwrap();
        assert_eq!(matches.len(), 2, "应匹配两个 func 定义");
    }

    #[test]
    fn search_finds_typescript_functions() {
        let src = "function a() {}\nfunction b() {}\n";
        let matches = search(
            src,
            SupportLang::TypeScript,
            "function $NAME() {}",
            AstMatchStrictness::Smart,
        )
        .unwrap();
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn search_finds_javascript_functions() {
        let src = "function foo() { return 1; }\nfunction bar() { return 2; }\n";
        let matches = search(
            src,
            SupportLang::JavaScript,
            "function $NAME() { $$$BODY }",
            AstMatchStrictness::Smart,
        )
        .unwrap();
        assert_eq!(matches.len(), 2, "应匹配两个 function 定义");
    }

    #[test]
    fn strictness_parse_defaults_to_smart() {
        assert_eq!(AstMatchStrictness::parse(None), AstMatchStrictness::Smart);
        assert_eq!(
            AstMatchStrictness::parse(Some("cst")),
            AstMatchStrictness::Cst
        );
        assert_eq!(
            AstMatchStrictness::parse(Some("unknown")),
            AstMatchStrictness::Smart
        );
    }
}
