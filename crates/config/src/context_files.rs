//! 上下文文件收集（H40）：`AGENTS.md` 族发现 → `@import` 展开 → 双通道去重。
//!
//! 移植 oh-my-pi `system-prompt.ts` 的 `loadProjectContextFiles` +
//! `dedupeContainedContextFiles`：
//!
//! - **权威顺序**：depth 越大离 cwd 越远、越不权威（用户级 = [`u32::MAX`]）。
//! - **包含去重**：把内容切成「归一化段落块」（空行分段，围栏代码块内不分段），若某个
//!   **更权威**（depth 更小）的文件按顺序包含某个文件的全部块，则该文件被丢弃——
//!   避免同一段规则在用户级与项目级重复注入浪费上下文。
//! - **`@import`**：见 [`crate::at_imports`]（在拼接前逐文件展开）。
//! - **SYSTEM.md**：项目级覆盖用户级（见 [`system_prompt_file`]）。

use std::path::{Path, PathBuf};

use crate::at_imports::expand_at_imports;

/// 一个已展开的上下文文件。
#[derive(Debug, Clone)]
pub struct ContextFile {
    /// 来源路径（提示词标签用）。
    pub path: PathBuf,
    /// 距离 cwd 的层数（0 = cwd；用户级 = [`u32::MAX`]）。
    pub depth: u32,
    /// 展开 `@import` 后的内容。
    pub content: String,
    /// 是否用户级（渲染标签不同）。
    pub user_level: bool,
}

/// 收集上下文文件（用户级 + 项目 walkup），展开 `@import`，但**不去重**。
///
/// 顺序为「用户级 → 由远及近的项目级」（近者最后注入，覆盖语义在后）。
#[must_use]
pub fn collect_context_files(cwd: &Path) -> Vec<ContextFile> {
    let mut out = Vec::new();
    if let Some(cfg) = agent_core::config_dir() {
        let p = cfg.join("AGENTS.md");
        if let Ok(content) = std::fs::read_to_string(&p) {
            out.push(ContextFile {
                depth: u32::MAX,
                content: expand_at_imports(&content, &p),
                path: p,
                user_level: true,
            });
        }
    }
    let home = dirs::home_dir();
    let mut project: Vec<ContextFile> = Vec::new();
    let mut current = Some(cwd);
    let mut depth = 0u32;
    while let Some(dir) = current {
        let p = dir
            .join(agent_core::project_config_dir_name())
            .join("AGENTS.md");
        if let Ok(content) = std::fs::read_to_string(&p) {
            project.push(ContextFile {
                depth,
                content: expand_at_imports(&content, &p),
                path: p,
                user_level: false,
            });
        }
        if let Some(h) = &home
            && dir == h.as_path()
        {
            break;
        }
        current = dir.parent();
        depth = depth.saturating_add(1);
    }
    // cwd 最近者最后注入（覆盖语义）。
    out.extend(project.into_iter().rev());
    out
}

/// 项目级 `SYSTEM.md` 覆盖用户级（`<config_dir>/SYSTEM.md`）；内容同样展开 `@import`。
#[must_use]
pub fn system_prompt_file(cwd: &Path) -> Option<ContextFile> {
    let project = cwd.join("SYSTEM.md");
    if let Ok(content) = std::fs::read_to_string(&project) {
        return Some(ContextFile {
            depth: 0,
            content: expand_at_imports(&content, &project),
            path: project,
            user_level: false,
        });
    }
    let user = agent_core::config_dir()?.join("SYSTEM.md");
    let content = std::fs::read_to_string(&user).ok()?;
    Some(ContextFile {
        depth: u32::MAX,
        content: expand_at_imports(&content, &user),
        path: user,
        user_level: true,
    })
}

/// 去掉「被更权威文件完全包含」的上下文文件（`@import` 展开后做，避免把展开结果重复注入）。
#[must_use]
pub fn dedupe_contained(files: Vec<ContextFile>) -> Vec<ContextFile> {
    if files.len() < 2 {
        return files;
    }
    // 按 depth 降序排（最不权威在前）；稳定排序保持同 depth 的调用方顺序。
    let mut sorted = files;
    sorted.sort_by_key(|f| std::cmp::Reverse(f.depth));
    let blocks: Vec<Vec<String>> = sorted.iter().map(|f| split_blocks(&f.content)).collect();
    let mut keep = vec![true; sorted.len()];
    for (i, block) in blocks.iter().enumerate() {
        // 更权威者 = 排在其后（depth 更小，或用户级之外）。
        for candidate in blocks.iter().skip(i + 1) {
            if blocks_contain(candidate, block) {
                keep[i] = false;
                break;
            }
        }
    }
    sorted
        .into_iter()
        .zip(keep)
        .filter_map(|(f, k)| k.then_some(f))
        .collect()
}

/// 内容 → 归一化段落块（空行分段；围栏代码块内不分段；trim 后丢弃空块）。
#[must_use]
pub fn split_blocks(content: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut in_fence = false;
    for line in content.lines() {
        let trimmed_start = line.trim_start();
        if trimmed_start.starts_with("```") || trimmed_start.starts_with("~~~") {
            in_fence = !in_fence;
            current.push(line);
            continue;
        }
        if !in_fence
            && line.trim().is_empty()
            && !current.is_empty()
            && !current.last().is_some_and(|l| l.trim().is_empty())
        {
            let block = current.join("\n").trim().to_string();
            if !block.is_empty() {
                blocks.push(block);
            }
            current.clear();
            continue;
        }
        current.push(line);
    }
    let tail = current.join("\n").trim().to_string();
    if !tail.is_empty() {
        blocks.push(tail);
    }
    blocks
}

/// `needle` 是否作为**连续子序列**出现在 `source` 中（同 omp `promptBlocksContain`）。
#[must_use]
pub fn blocks_contain(source: &[String], needle: &[String]) -> bool {
    if source.is_empty() || needle.is_empty() || needle.len() > source.len() {
        return false;
    }
    (0..=source.len() - needle.len()).any(|start| {
        needle
            .iter()
            .enumerate()
            .all(|(off, b)| source[start + off] == *b)
    })
}

/// 渲染为注入用段落（保持既有 `项目约定（…）` 标签格式）。
#[must_use]
pub fn render_context_file(file: &ContextFile) -> String {
    if file.user_level {
        format!("项目约定（用户级）:\n\n{}", file.content)
    } else {
        format!("项目约定（{}）:\n\n{}", file.path.display(), file.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, depth: u32, content: &str, user_level: bool) -> ContextFile {
        ContextFile {
            path: PathBuf::from(name),
            depth,
            content: content.to_string(),
            user_level,
        }
    }

    #[test]
    fn split_blocks_keeps_fenced_code_together() {
        let blocks = split_blocks("a\n\nb\n\n```\nx\n\n y\n```\n\nc\n");
        assert_eq!(blocks.len(), 4, "{blocks:?}");
        assert_eq!(blocks[2], "```\nx\n\n y\n```");
        assert_eq!(blocks[3], "c");
        assert!(split_blocks("   \n\n").is_empty());
    }

    #[test]
    fn dedupe_drops_file_fully_contained_by_more_authoritative_one() {
        let shared = "规则 A\n\n规则 B\n";
        let files = vec![
            // 用户级（最不权威）内容被项目级完整包含 → 丢弃。
            file("user", u32::MAX, shared, true),
            file("cwd/.agent", 0, "规则 A\n\n规则 B\n\n规则 C\n", false),
        ];
        let kept = dedupe_contained(files);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].path, PathBuf::from("cwd/.agent"));

        // 不是连续子序列（顺序不同）→ 两份都保留。
        let files = vec![
            file("user", u32::MAX, "规则 A\n\n规则 B\n", true),
            file("proj", 0, "规则 B\n\n规则 A\n", false),
        ];
        assert_eq!(dedupe_contained(files).len(), 2);
    }

    #[test]
    fn dedupe_outranks_by_depth_not_position() {
        // 更深的目录（更不权威）被 cwd 文件包含 → 丢弃深者，保留 cwd。
        let files = vec![
            file("cwd", 0, "X\n\nY\n", false),
            file("parent", 1, "X\n", false),
        ];
        let kept = dedupe_contained(files);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].path, PathBuf::from("cwd"));
        // 反向：cwd 只有 `X`，父目录含 `X\n\nY` → cwd 被丢弃（父更权威地包含它？否——
        // depth 0 更权威，只有「更权威者包含」才丢，故此处两者都保留）。
        let files = vec![
            file("cwd", 0, "X\n", false),
            file("parent", 1, "X\n\nY\n", false),
        ];
        assert_eq!(dedupe_contained(files).len(), 2);
    }

    #[test]
    fn collect_and_render_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join(".agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("AGENTS.md"), "顶层规则\n\n见 @docs.md\n").unwrap();
        // @import 相对**引用文件**所在目录（`.agent/`），不是 cwd。
        std::fs::write(agent_dir.join("docs.md"), "被导入的内容\n").unwrap();
        let files = collect_context_files(dir.path());
        assert_eq!(files.len(), 1, "{files:?}");
        // @import 已展开，且渲染保留标签。
        assert!(
            files[0].content.contains("被导入的内容"),
            "{}",
            files[0].content
        );
        assert!(!files[0].content.contains("@docs.md"));
        let rendered = render_context_file(&files[0]);
        assert!(rendered.starts_with("项目约定（"), "{rendered}");
        assert!(rendered.contains("顶层规则"));
    }

    #[test]
    fn system_prompt_project_overrides_user() {
        let cwd = tempfile::tempdir().unwrap();
        // 无 SYSTEM.md → None（不改变默认 system prompt）。
        assert!(system_prompt_file(cwd.path()).is_none());
        let project = cwd.path().join("SYSTEM.md");
        std::fs::write(&project, "项目系统提示\n").unwrap();
        let f = system_prompt_file(cwd.path()).expect("项目级 SYSTEM.md 应命中");
        assert_eq!(f.path, project);
        assert!(f.content.contains("项目系统提示"));
        assert!(!f.user_level);
    }
}
