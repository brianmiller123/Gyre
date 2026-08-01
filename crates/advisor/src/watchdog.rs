//! `WATCHDOG.md` 评审准则发现：cwd walkup + 用户级 + 项目级。
//!
//! 移植 oh-my-pi `advisor/watchdog.ts`：advisor 的 system prompt 注入仓库/用户
//! 自定义评审准则（如「禁止吞掉 ENOENT」「验收标准变化必须报告」）。

use std::path::{Path, PathBuf};

/// 发现并读取全部 `WATCHDOG.md`（cwd walkup → `~/.gyre` → `<cwd>/.gyre`，去重）。
///
/// 返回 `(来源路径, 内容)` 列表，按优先级（项目级最后 = 最优先）排列。
#[must_use]
pub fn discover_watchdog(cwd: &Path) -> Vec<(PathBuf, String)> {
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    // 1) cwd 逐级向上。
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let candidate = d.join("WATCHDOG.md");
        if candidate.is_file() && seen.insert(candidate.clone()) {
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                out.push((candidate, text));
            }
        }
        dir = d.parent();
    }
    // 2) 用户级 `~/.gyre/WATCHDOG.md`。
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let candidate = home.join(".gyre").join("WATCHDOG.md");
        if candidate.is_file() && seen.insert(candidate.clone()) {
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                out.push((candidate, text));
            }
        }
    }
    // 3) 项目级 `<cwd>/.gyre/WATCHDOG.md`。
    let candidate = cwd.join(".gyre").join("WATCHDOG.md");
    if candidate.is_file() && seen.insert(candidate.clone()) {
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            out.push((candidate, text));
        }
    }
    out
}

/// 拼接全部 watchdog 内容为单段（供 advisor system prompt）。
#[must_use]
pub fn render_watchdog(cwd: &Path) -> String {
    let found = discover_watchdog(cwd);
    if found.is_empty() {
        return String::new();
    }
    let mut out = String::from("## 评审准则（WATCHDOG.md）\n");
    for (path, text) in &found {
        out.push_str(&format!("### 来自 {}\n{text}\n", path.display()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_walkup_and_project() {
        let dir = std::env::temp_dir().join(format!("agent-wd-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(dir.join(".gyre")).unwrap();
        std::fs::create_dir_all(dir.join("sub/deep/.gyre")).unwrap();
        std::fs::write(dir.join(".gyre/WATCHDOG.md"), "project rules\n").unwrap();
        std::fs::write(dir.join("WATCHDOG.md"), "root rules\n").unwrap();
        std::fs::write(dir.join("sub/deep/.gyre/WATCHDOG.md"), "deep rules\n").unwrap();

        let found = discover_watchdog(&dir.join("sub/deep"));
        // walkup 命中 root 级；cwd 项目级（sub/deep/.gyre）命中；dir/.gyre 非当前项目级不命中。
        let texts: Vec<&str> = found.iter().map(|(_, t)| t.as_str()).collect();
        assert!(texts.contains(&"root rules\n"), "{texts:?}");
        assert!(texts.contains(&"deep rules\n"), "{texts:?}");
        assert!(!texts.contains(&"project rules\n"), "{texts:?}");
    }

    #[test]
    fn render_empty_without_files() {
        let dir = std::env::temp_dir().join("agent-wd-none");
        assert!(render_watchdog(&dir).is_empty());
    }
}
