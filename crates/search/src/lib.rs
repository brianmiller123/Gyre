//! # agent-search
//!
//! 代码搜索（移植 oh-my-pi `pi-natives` 的 grep/glob 能力为纯 Rust）：
//! - [`grep`] 基于 `ignore`（ripgrep 核心），并行遍历并尊重 `.gitignore`。
//! - [`glob_match`] 基于 `globset`，按 glob 模式发现文件。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

use std::path::{Path, PathBuf};

pub mod extras;
pub mod tokens;

pub use extras::{find_files, highlight_to_ansi};
pub use tokens::count_tokens;

/// 一次 grep 命中。
#[derive(Debug, Clone)]
pub struct GrepHit {
    /// 命中文件路径。
    pub path: PathBuf,
    /// 1-indexed 行号。
    pub line: usize,
    /// 该行文本。
    pub text: String,
}

/// grep 单文件读取大小上限（跳过更大文件，防 OOM）。
const GREP_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// 在 `root` 下正则搜索（尊重 `.gitignore`，跳过二进制/超大文件）。
///
/// # Errors
/// 正则非法时返回错误字符串。
pub fn grep(root: &Path, pattern: &str, max_hits: usize) -> Result<Vec<GrepHit>, String> {
    let re = regex::Regex::new(pattern).map_err(|e| e.to_string())?;
    let mut hits = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .build();

    for entry in walker.flatten() {
        if hits.len() >= max_hits {
            break;
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        // 跳过过大文件，避免整段读入撑爆内存。
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > GREP_MAX_FILE_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                hits.push(GrepHit {
                    path: path.strip_prefix(root).unwrap_or(path).to_path_buf(),
                    line: i + 1,
                    text: line.to_string(),
                });
                if hits.len() >= max_hits {
                    break;
                }
            }
        }
    }
    Ok(hits)
}

/// 按 glob 模式在 `root` 下发现文件路径（尊重 `.gitignore`）。
pub fn glob_match(root: &Path, pattern: &str, max: usize) -> Result<Vec<PathBuf>, String> {
    let mut matcher = globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| e.to_string())?
        .compile_matcher();
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root).hidden(true).git_ignore(true).build();
    for entry in walker.flatten() {
        if out.len() >= max {
            break;
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(path);
        // 同时匹配相对路径与文件名
        if matcher.is_match(rel) || path.file_name().map_or(false, |n| matcher.is_match(n)) {
            out.push(rel.to_path_buf());
        }
    }
    // 静默借用，避免未使用告警
    let _ = &mut matcher;
    Ok(out)
}

/// 列出 `root` 下的条目（尊重 `.gitignore`）。
/// - `recursive = false`：仅直接子项（文件与目录，按名排序）。
/// - `recursive = true`：递归所有文件（跳过隐藏/gitignore）。
pub fn list_files(root: &Path, recursive: bool, max: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !recursive {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                if out.len() >= max {
                    break;
                }
                let path = entry.path();
                let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                out.push(rel);
            }
            out.sort();
        }
        return out;
    }
    let walker = ignore::WalkBuilder::new(root).hidden(true).git_ignore(true).build();
    for entry in walker.flatten() {
        if out.len() >= max {
            break;
        }
        let path = entry.path();
        if path == root || !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        out.push(rel);
    }
    out
}

/// 在行文本中把匹配正则的子串用 ANSI 红色加粗高亮（grep 结果渲染用）。
///
/// # Errors
/// 正则非法时返回错误。
pub fn highlight_match(line: &str, pattern: &str) -> Result<String, String> {
    let re = regex::Regex::new(pattern).map_err(|e| e.to_string())?;
    let out = re.replace_all(line, |c: &regex::Captures<'_>| {
        format!("\x1b[1;31m{}\x1b[0m", &c[0])
    });
    Ok(out.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-search-{name}-{}", nano()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn nano() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn grep_finds_pattern() {
        let dir = tmp("grep");
        fs::write(dir.join("a.rs"), "fn hello() {}\nfn world() {}\n").unwrap();
        let hits = grep(&dir, "hello", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 1);
    }

    #[test]
    fn glob_finds_by_extension() {
        let dir = tmp("glob");
        fs::write(dir.join("a.rs"), "x").unwrap();
        fs::write(dir.join("b.txt"), "y").unwrap();
        let files = glob_match(&dir, "**/*.rs", 10).unwrap();
        assert!(files.iter().any(|p| p.to_string_lossy() == "a.rs"));
        assert!(!files.iter().any(|p| p.to_string_lossy().ends_with("b.txt")));
    }

    #[test]
    fn list_files_top_level_and_recursive() {
        let dir = tmp("list");
        fs::write(dir.join("a.rs"), "x").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/b.rs"), "y").unwrap();
        let top = list_files(&dir, false, 10);
        assert!(top.iter().any(|p| p.to_string_lossy() == "a.rs"));
        assert!(top.iter().any(|p| p.to_string_lossy() == "sub"));
        let rec = list_files(&dir, true, 10);
        assert!(rec.iter().any(|p| p.to_string_lossy() == "a.rs"));
        assert!(rec.iter().any(|p| p.to_string_lossy() == "sub/b.rs"));
    }
}
