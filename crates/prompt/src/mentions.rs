//! # Inline `@file` mention expansion
//!
//! Web 与 CLI 共享同一套语法与渲染格式，保证两端「在发送前把 `@file`-提及展开为
//! 上下文块」行为一致（Roo-Code `parseMentions` 的精简子集）：
//!
//! - `@file <path>`（路径不含空白）→ 注入该文件内容。
//!
//! 渲染为附加在用户文本后的「上下文块」，CLI 直接调用本模块；Web（TS）按相同格式镜像。
//!
//! 格式示例：
//! ```text
//! <file path="src/auth.rs">
//! ```rust
//! …contents…
//! ```
//! </file>
//! ```

#![allow(clippy::module_name_repetitions)]

/// 一个解析出的提及。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mention {
    /// 文件提及（相对工作区根的路径，原样保留）。
    File(String),
}

/// 从文本解析所有 `@file <path>` 提及（去重，保持首次出现顺序）。
///
/// 语法：按空白分词；`@file` 后跟随一个不含空白的路径 token，并剥离其首尾的 `"`/`'`。
/// 大小写不敏感（`@FILE` 同样识别）。未识别的 `@xxx` 忽略。
#[must_use]
pub fn parse_mentions(text: &str) -> Vec<Mention> {
    let mut out: Vec<Mention> = Vec::new();
    for line in text.lines() {
        let mut tokens = line.split_whitespace();
        while let Some(tok) = tokens.next() {
            if tok.eq_ignore_ascii_case("@file") {
                if let Some(p) = tokens.next() {
                    let cleaned = p.trim_matches(|c: char| c == '"' || c == '\'');
                    let m = Mention::File(cleaned.to_string());
                    if !out.contains(&m) {
                        out.push(m);
                    }
                }
            }
        }
    }
    out
}

/// 按扩展名给出 Markdown 围栏语言（用于代码块高亮）。
#[must_use]
pub fn fence_lang(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "toml" => "toml",
        "ts" | "tsx" => "ts",
        "js" | "jsx" | "mjs" | "cjs" => "js",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" => "cpp",
        "rb" => "ruby",
        "sh" | "bash" | "zsh" => "bash",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "md" => "md",
        "html" => "html",
        "css" => "css",
        _ => "",
    }
}

/// 渲染单个文件上下文块。
#[must_use]
pub fn format_file_block(path: &str, content: &str) -> String {
    format!(
        "<file path=\"{path}\">\n```{lang}\n{content}\n```\n</file>",
        lang = fence_lang(path),
    )
}

/// 把若干已渲染的上下文块附加到用户文本后（无块时原样返回）。
#[must_use]
pub fn render_attached(user_text: &str, blocks: &[String]) -> String {
    if blocks.is_empty() {
        return user_text.to_string();
    }
    format!(
        "{user_text}\n\n--- attached context ---\n\n{joined}",
        joined = blocks.join("\n\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file_mentions() {
        let m = parse_mentions("fix @file src/auth.rs and also @FILE x.py");
        assert_eq!(
            m,
            vec![
                Mention::File("src/auth.rs".into()),
                Mention::File("x.py".into()),
            ]
        );
    }

    #[test]
    fn dedups_repeats_and_ignores_unknown() {
        let m = parse_mentions("@unknown @file a.rs @file a.rs");
        assert_eq!(m, vec![Mention::File("a.rs".into())]);
    }

    #[test]
    fn strips_quotes_around_paths() {
        let m = parse_mentions("@file \"src/a b.rs\"");
        // 路径含空白会被 split_whitespace 切断；此处验证引号剥离（不含空白的引号路径）。
        assert_eq!(m, vec![Mention::File("src/a".into())]);
        let m2 = parse_mentions("@file 'b.rs'");
        assert_eq!(m2, vec![Mention::File("b.rs".into())]);
    }

    #[test]
    fn fence_lang_maps_common_extensions() {
        assert_eq!(fence_lang("a.rs"), "rust");
        assert_eq!(fence_lang("a.TSX"), "ts");
        assert_eq!(fence_lang("Makefile"), "");
    }

    #[test]
    fn file_block_wraps_with_fenced_lang() {
        let b = format_file_block("src/a.rs", "fn main() {}");
        assert!(b.contains("<file path=\"src/a.rs\">"));
        assert!(b.contains("```rust"));
        assert!(b.contains("fn main() {}"));
        assert!(b.contains("</file>"));
    }

    #[test]
    fn render_attached_is_identity_without_blocks() {
        assert_eq!(render_attached("hi", &[]), "hi");
        let r = render_attached("hi", &[format_file_block("a.rs", "x")]);
        assert!(r.contains("attached context") && r.starts_with("hi"));
    }
}
