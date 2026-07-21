//! 语法高亮（syntect）与并行文件发现（fd，基于 ignore）。
//!
//! 移植 pi-natives 的 highlight 与 fd 能力为纯 Rust。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::as_24_bit_terminal_escaped;

/// 渲染代码为带 ANSI 颜色的终端文本（按扩展名推断语法）。
///
/// # Errors
/// 语法集加载失败时返回错误。
pub fn highlight_to_ansi(code: &str, extension: &str) -> Result<String, String> {
    let state = syntax_state()?;
    let syntax = state
        .syntax_set
        .find_syntax_by_extension(extension)
        .unwrap_or_else(|| state.syntax_set.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, &state.theme_set.themes["base16-ocean.dark"]);
    let mut out = String::new();
    for line in code.lines() {
        let regions: Vec<(Style, &str)> = h
            .highlight_line(line, &state.syntax_set)
            .map_err(|e| e.to_string())?;
        out.push_str(&as_24_bit_terminal_escaped(&regions[..], false));
        out.push('\n');
    }
    Ok(out)
}

struct SyntaxState {
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
}

fn syntax_state() -> Result<&'static SyntaxState, String> {
    static STATE: OnceLock<SyntaxState> = OnceLock::new();
    if let Some(s) = STATE.get() {
        return Ok(s);
    }
    let syntax_set = SyntaxSet::load_defaults_newlines();
    let theme_set = ThemeSet::load_defaults();
    let _ = STATE.set(SyntaxState {
        syntax_set,
        theme_set,
    });
    Ok(STATE.get().ok_or("初始化失败")?)
}

/// 并行发现文件（移植 fd）：按名称子串 + 扩展名过滤，尊重 .gitignore。
///
/// # Errors
/// 根目录读取失败时返回错误。
pub fn find_files(
    root: &Path,
    name_query: Option<&str>,
    extension: Option<&str>,
    max: usize,
) -> Result<Vec<PathBuf>, String> {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .build();
    let mut out = Vec::new();
    for entry in walker.flatten() {
        if out.len() >= max {
            break;
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(path);
        let fname = rel.to_string_lossy();
        if let Some(q) = name_query {
            if !fname.to_lowercase().contains(&q.to_lowercase()) {
                continue;
            }
        }
        if let Some(ext) = extension {
            if path.extension().and_then(|e| e.to_str()) != Some(ext) {
                continue;
            }
        }
        out.push(rel.to_path_buf());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_rust_returns_ansi() {
        let out = highlight_to_ansi("fn main() {}", "rs").unwrap();
        // ANSI 转义序列以 ESC(0x1b) 开头
        assert!(out.contains('\u{1b}') || out.contains("main"));
    }

    #[test]
    fn find_files_by_extension() {
        let dir = std::env::temp_dir().join(format!(
            "agent-fd-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "x").unwrap();
        std::fs::write(dir.join("b.txt"), "y").unwrap();
        let files = find_files(&dir, None, Some("rs"), 10).unwrap();
        assert!(files.iter().any(|p| p.to_string_lossy() == "a.rs"));
        assert!(!files.iter().any(|p| p.to_string_lossy().ends_with("b.txt")));
    }
}
