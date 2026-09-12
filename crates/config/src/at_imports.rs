//! `@`-import 展开（H40）：上下文文件（`AGENTS.md` / `CLAUDE.md` / `GEMINI.md` / …）
//! 中的 `@path/to/file` 引用按 Claude Code 语义内联。
//!
//! 移植 oh-my-pi `discovery/at-imports.ts`：
//!
//! - `@` 必须位于行首或空白之后（`git@github.com`、`user@example.com` 不算 import）。
//! - 相对路径相对**引用它的文件**所在目录解析（不是 cwd）；`~/...` 相对 HOME。
//! - 围栏代码块（``` / ~~~）与行内代码跨度（`` `…` ``）内的 `@token` 原样保留，
//!   技术示例（`npm install @types/node`）不被误展开。
//! - 递归深度上限 [`MAX_AT_IMPORT_DEPTH`]（5 跳，同 Claude Code 文档），环引用静默断开
//!   （visited 集合整棵树共享，跨文件的环也能断）。
//! - 文件读不到时保留原始 `@token`（不让一个笔误吞掉正文）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use regex::Regex;

/// 最大递归 `@`-import 跳数（对齐 Claude Code 文档上限）。
pub const MAX_AT_IMPORT_DEPTH: usize = 5;

/// 候选 `@import`：行首或单个空白 + `@` + path-like 起始字符 + 非空白余量。
fn at_import_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(^|[ \t])@([./~A-Za-z0-9_-][^\s]*)").expect("at-import 正则编译失败")
    })
}

/// 需要从路径 token 尾部剥离的标点（句读/右括号/引号）。
fn trailing_punct_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"[.,;:!?)\]}"']+$"#).expect("标点正则编译失败"))
}

/// 展开 `content` 中相对 `file_path` 目录的 `@path` 引用；无命中时原样返回。
#[must_use]
pub fn expand_at_imports(content: &str, file_path: &Path) -> String {
    let home = dirs::home_dir();
    let absolute = std::fs::canonicalize(file_path).unwrap_or_else(|_| file_path.to_path_buf());
    let base = absolute
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let mut visited: HashSet<PathBuf> = HashSet::new();
    visited.insert(absolute);
    expand(
        content,
        &base,
        0,
        MAX_AT_IMPORT_DEPTH,
        home.as_deref(),
        &mut visited,
    )
}

/// 以显式 HOME 展开（测试用，避免依赖真实 `$HOME`）。
#[must_use]
pub fn expand_at_imports_with_home(content: &str, file_path: &Path, home: Option<&Path>) -> String {
    let absolute = std::fs::canonicalize(file_path).unwrap_or_else(|_| file_path.to_path_buf());
    let base = absolute
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let mut visited: HashSet<PathBuf> = HashSet::new();
    visited.insert(absolute);
    expand(content, &base, 0, MAX_AT_IMPORT_DEPTH, home, &mut visited)
}

fn expand(
    content: &str,
    base_dir: &Path,
    depth: usize,
    max_depth: usize,
    home: Option<&Path>,
    visited: &mut HashSet<PathBuf>,
) -> String {
    if depth >= max_depth {
        return content.to_string();
    }
    let mut out = String::with_capacity(content.len());
    for segment in split_markdown_segments(content) {
        if segment.is_code {
            out.push_str(&segment.text);
            continue;
        }
        // 逐行展开文本段（保留行尾换行，行间边界不丢）。
        let mut lines = segment.text.split('\n');
        let mut first = true;
        for line in &mut lines {
            if !first {
                out.push('\n');
            }
            first = false;
            out.push_str(&expand_line(
                line, base_dir, depth, max_depth, home, visited,
            ));
        }
    }
    out
}

fn expand_line(
    line: &str,
    base_dir: &Path,
    depth: usize,
    max_depth: usize,
    home: Option<&Path>,
    visited: &mut HashSet<PathBuf>,
) -> String {
    if !line.contains('@') {
        return line.to_string();
    }
    // 收集命中：@ 的绝对位置 + 待替换区间 + 路径 token。
    let mut matches: Vec<(usize, usize, String)> = Vec::new();
    for caps in at_import_re().captures_iter(line) {
        let whole = caps.get(0).expect("整段");
        let leading = caps.get(1).expect("前导").as_str();
        let raw_token = caps.get(2).expect("token").as_str();
        let at_pos = whole.start() + leading.len();
        if is_inside_inline_code(line, at_pos) {
            continue;
        }
        let token = trailing_punct_re().replace(raw_token, "");
        if token.is_empty() {
            continue;
        }
        matches.push((at_pos, at_pos + 1 + token.len(), token.to_string()));
    }
    if matches.is_empty() {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut cursor = 0usize;
    for (start, end, token) in matches {
        out.push_str(&line[cursor..start]);
        match resolve_and_expand(&token, base_dir, depth, max_depth, home, visited) {
            Some(expanded) => out.push_str(&expanded),
            None => out.push_str(&line[start..end]),
        }
        cursor = end;
    }
    out.push_str(&line[cursor..]);
    out
}

fn resolve_and_expand(
    import_path: &str,
    base_dir: &Path,
    depth: usize,
    max_depth: usize,
    home: Option<&Path>,
    visited: &mut HashSet<PathBuf>,
) -> Option<String> {
    let resolved = resolve_import_path(import_path, base_dir, home)?;
    let canonical = std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
    if visited.contains(&canonical) {
        return None; // 环引用：静默保留原文
    }
    let content = std::fs::read_to_string(&canonical).ok()?;
    visited.insert(canonical.clone());
    let base = canonical
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    Some(expand(&content, &base, depth + 1, max_depth, home, visited))
}

/// 解析 import 路径：`~` / `~/x` → HOME；绝对路径原样；其余相对引用文件目录。
fn resolve_import_path(import_path: &str, base_dir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if import_path == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(rest) = import_path.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    let p = Path::new(import_path);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    Some(base_dir.join(p))
}

struct Segment {
    is_code: bool,
    text: String,
}

/// 按围栏代码块切分 markdown（开/闭围栏同字符且闭合不短于开启）。
fn split_markdown_segments(content: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut buffer = String::new();
    let mut in_code = false;
    let (mut fence_char, mut fence_len) = ('\0', 0usize);
    let lines: Vec<&str> = content.split('\n').collect();
    let last_idx = lines.len().saturating_sub(1);
    for (i, line) in lines.iter().enumerate() {
        let line_text = if i == last_idx {
            (*line).to_string()
        } else {
            format!("{line}\n")
        };
        match match_fence(line) {
            Some((c, len)) if !in_code => {
                // 开启围栏：先冲刷文本段。
                if !buffer.is_empty() {
                    segments.push(Segment {
                        is_code: false,
                        text: std::mem::take(&mut buffer),
                    });
                }
                in_code = true;
                fence_char = c;
                fence_len = len;
                buffer.push_str(&line_text);
            }
            Some((c, len)) if in_code && c == fence_char && len >= fence_len => {
                buffer.push_str(&line_text);
                segments.push(Segment {
                    is_code: true,
                    text: std::mem::take(&mut buffer),
                });
                in_code = false;
                fence_char = '\0';
                fence_len = 0;
            }
            _ => buffer.push_str(&line_text),
        }
    }
    if !buffer.is_empty() {
        segments.push(Segment {
            is_code: in_code,
            text: buffer,
        });
    }
    segments
}

/// 行首（可含空白）是否为 ``` 或 ~~~ 围栏；返回（字符, 数量）。
fn match_fence(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start_matches([' ', '\t']);
    let c = trimmed.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|ch| *ch == c).count();
    (len >= 3).then_some((c, len))
}

/// `position` 是否落在该行未闭合的行内代码跨度内（反引号奇偶扫描）。
fn is_inside_inline_code(line: &str, position: usize) -> bool {
    let bytes = line.as_bytes();
    let mut in_span = false;
    let mut i = 0usize;
    while i < position && i < bytes.len() {
        if bytes[i] == b'`' {
            while i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            in_span = !in_span;
        } else {
            i += 1;
        }
    }
    in_span
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn inlines_relative_import_from_referencing_file_dir() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "docs/inner.md", "内层内容\n");
        let main = write(dir.path(), "AGENTS.md", "前\n见 @docs/inner.md\n后\n");
        let out = expand_at_imports("前\n见 @docs/inner.md\n后\n", &main);
        assert!(out.contains("内层内容"), "{out}");
        assert!(!out.contains("@docs/inner.md"), "{out}");
        assert!(out.starts_with("前\n见 "), "{out}");
    }

    #[test]
    fn preserves_email_and_fenced_and_inline_code() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "real.md", "不该被引用\n");
        let main = write(
            dir.path(),
            "AGENTS.md",
            "联系 user@example.com\n```\nnpm install @types/node\n```\n行内 `@real.md` 保留\n",
        );
        let src =
            "联系 user@example.com\n```\nnpm install @types/node\n```\n行内 `@real.md` 保留\n";
        let out = expand_at_imports(src, &main);
        assert_eq!(out, src, "邮箱/围栏/行内代码内的 @token 不展开");
    }

    #[test]
    fn strips_trailing_punctuation_and_keeps_missing_token() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "note.md", "NOTE\n");
        let main = write(dir.path(), "AGENTS.md", "");
        let out = expand_at_imports("see @note.md. and @missing.md!\n", &main);
        assert!(out.contains("NOTE"), "{out}");
        assert!(
            !out.contains("@note.md."),
            "句末句号随 token 一起被替换: {out}"
        );
        assert!(
            out.contains("@missing.md"),
            "读不到的文件保留原 token: {out}"
        );
    }

    #[test]
    fn recursive_imports_and_cycles_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        // a → b → a：环引用应静默断开而不是无限展开。
        let a = write(dir.path(), "a.md", "A\n@b.md\n");
        write(dir.path(), "b.md", "B\n@a.md\n");
        let out = expand_at_imports(&std::fs::read_to_string(&a).unwrap(), &a);
        assert!(out.contains('A') && out.contains('B'), "{out}");
        assert!(!out.contains("@b.md"), "b 被展开: {out}");
        // 深度上限：链式引用在 5 跳内展开，超出部分保持原 token。
        let mut content = String::from("end\n");
        for i in (0..8).rev() {
            let body = format!("d{i}\n{content}");
            write(dir.path(), &format!("d{i}.md"), &body);
            content = body;
        }
        let out = expand_at_imports(&content, &dir.path().join("d0.md"));
        assert!(out.contains("d0") && out.contains("d5"), "{out}");
        assert!(
            out.contains("@d7.md") || out.contains("d6"),
            "超过上限的层级保留 token: {out}"
        );
    }

    #[test]
    fn tilde_resolves_against_explicit_home() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), "shared.md", "SHARED\n");
        let dir = tempfile::tempdir().unwrap();
        let main = write(dir.path(), "AGENTS.md", "见 @~/shared.md\n");
        let out = expand_at_imports_with_home("见 @~/shared.md\n", &main, Some(home.path()));
        assert!(out.contains("SHARED"), "{out}");
    }
}
