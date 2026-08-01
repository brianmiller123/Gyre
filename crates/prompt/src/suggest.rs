//! # Context-aware suggestions
//!
//! 按草稿相关性对工作区文件评分排序（纯函数，无 I/O）。文件枚举由调用方完成
//!（服务器/CLI 复用 `agent_search::find_files`，尊重 `.gitignore`）。

#![allow(clippy::module_name_repetitions)]

use serde::Serialize;

/// 一个被评过分的文件建议。
#[derive(Debug, Clone, Serialize)]
pub struct FileSuggestion {
    /// 相对工作区根的路径。
    pub path: String,
    /// 相关性总分（越大越相关）。
    pub score: f64,
    /// 命中原因（人类可读，UI 展示）。
    pub reason: String,
}


/// 语言 token → 相关扩展名映射（query 里出现 "rust" 倾向 .rs/.toml 等）。
fn lang_exts(token: &str) -> Option<&'static [&'static str]> {
    match token {
        "rust" | "rs" | "cargo" => Some(&["rs", "toml"]),
        "typescript" | "ts" => Some(&["ts", "tsx"]),
        "javascript" | "js" => Some(&["js", "jsx", "mjs"]),
        "python" | "py" => Some(&["py"]),
        "go" | "golang" => Some(&["go"]),
        "java" => Some(&["java"]),
        "c" => Some(&["c", "h"]),
        "cpp" | "c++" => Some(&["cpp", "cc", "hpp", "h"]),
        "react" => Some(&["tsx", "jsx"]),
        _ => None,
    }
}

/// 把 query 切成小写词汇 token（去停用词、去 <2 字符噪声）。
fn tokenize(query: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "a", "an", "to", "of", "and", "or", "for", "in", "on", "with", "is", "are", "be",
        "this", "that", "it", "my", "me", "please", "help", "do", "fix", "add", "make", "need",
        "want", "i",
    ];
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() >= 2)
        .map(str::to_lowercase)
        .filter(|s| !STOP.contains(&s.as_str()))
        .collect()
}

/// 提取路径的 basename（不含扩展名）与扩展名。
fn split_path(path: &str) -> (String, Option<String>) {
    // 归一化分隔符。
    let norm = path.replace('\\', "/");
    let base = norm.rsplit('/').next().unwrap_or(norm.as_str());
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), Some(e.to_lowercase())),
        _ => (base.to_string(), None),
    };
    (stem, ext)
}

/// 按 `query`（草稿文本）相关性对候选 `files` 评分，返回降序前 `limit` 条。
///
/// 评分信号（叠加）：
/// - 语言 token 命中扩展名（`rust` → `.rs`）：+2.0
/// - basename 含 token：+3.0（强）
/// - 任一路径段含 token：+1.5
/// - 全路径含 token：+0.5（兜底）
///
/// 无 token 或无命中返回空列表。纯函数，可单测。
#[must_use]
pub fn score_files(query: &str, files: &[String], limit: usize) -> Vec<FileSuggestion> {
    let tokens = tokenize(query);
    if tokens.is_empty() || files.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<FileSuggestion> = Vec::new();
    for path in files {
        let norm = path.replace('\\', "/");
        let (stem, ext) = split_path(path);
        let segments: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
        let mut score = 0.0_f64;
        let mut reasons: Vec<&str> = Vec::new();

        for tok in &tokens {
            let basename_hit = stem.to_lowercase().contains(tok);
            let seg_hit = segments
                .iter()
                .any(|s| s.to_lowercase().contains(tok));
            let path_hit = norm.to_lowercase().contains(tok);

            if let Some(exts) = lang_exts(tok) {
                if ext.as_deref().is_some_and(|e| exts.contains(&e)) {
                    score += 2.0;
                    reasons.push("language");
                }
            }
            if basename_hit {
                score += 3.0;
                reasons.push("name");
            } else if seg_hit {
                score += 1.5;
                reasons.push("path");
            } else if path_hit {
                score += 0.5;
                reasons.push("mention");
            }
        }

        if score > 0.0 {
            out.push(FileSuggestion {
                path: path.clone(),
                score,
                reason: dedup_reasons(&reasons),
            });
        }
    }
    // 稳定排序：分数降序，同分按路径字典序（避免并行/顺序抖动）。
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });
    out.truncate(limit);
    out
}

fn dedup_reasons(rs: &[&str]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for r in rs {
        if !seen.contains(r) {
            seen.push(r);
        }
    }
    seen.join(" + ")
}


#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<String> {
        vec![
            "src/auth.rs".into(),
            "src/auth_login.rs".into(),
            "tests/auth_test.rs".into(),
            "web/src/Auth.tsx".into(),
            "README.md".into(),
            "Cargo.toml".into(),
            "node_modules/x.js".into(),
        ]
    }

    #[test]
    fn ranks_by_basename_then_path() {
        let r = score_files("auth login", &files(), 5);
        // basename 含两个 token 的文件应排最高。
        assert_eq!(r.first().map(|s| s.path.as_str()), Some("src/auth_login.rs"));
        // README / Cargo.toml 无命中，不出现。
        assert!(r.iter().all(|s| s.path != "README.md" && s.path != "Cargo.toml"));
    }

    #[test]
    fn language_token_boosts_extension() {
        let r = score_files("rust auth", &files(), 5);
        // .rs 文件获得语言加成，应高于 .tsx。
        let rs_score = r
            .iter()
            .find(|s| s.path == "src/auth.rs")
            .map(|s| s.score)
            .unwrap_or(0.0);
        let tsx_score = r
            .iter()
            .find(|s| s.path == "web/src/Auth.tsx")
            .map(|s| s.score)
            .unwrap_or(0.0);
        assert!(rs_score > tsx_score);
    }

    #[test]
    fn empty_or_stopword_query_returns_nothing() {
        assert!(score_files("the a please", &files(), 5).is_empty());
        assert!(score_files("", &files(), 5).is_empty());
        assert!(score_files("auth", &[], 5).is_empty());
    }

    #[test]
    fn respects_limit() {
        assert!(score_files("auth", &files(), 2).len() <= 2);
    }

    #[test]
    fn split_path_handles_extensions() {
        let (stem, ext) = split_path("a/b/c.TS");
        assert_eq!(stem, "c");
        assert_eq!(ext.as_deref(), Some("ts"));
        let (stem, ext) = split_path("Makefile");
        assert_eq!(stem, "Makefile");
        assert!(ext.is_none());
    }
}
