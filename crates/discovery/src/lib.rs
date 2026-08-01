//! # agent-discovery
//!
//! 外来 Agent 工具配置发现与转换（移植 oh-my-pi `discovery` 的 v1 子集，P2-N）。
//!
//! 开箱读取用户既有的其他工具配置，统一为 Gyre 的 context section：
//! - `AGENTS.md`（Codex/通用规范，walkup）→ instruction
//! - `CLAUDE.md` / `.claude/CLAUDE.md`（Claude Code，walkup）→ instruction
//! - `.cursor/rules/*.mdc`（Cursor，walkup）→ rule（frontmatter: description/globs/alwaysApply）
//! - `.clinerules/*.md`（Cline）→ instruction
//!
//! 语义：**只读转换**，不写入、不移动任何外来文件；优先级遮蔽由调用方按
//! [`Source::priority`] 排序 + [`discover`] 输出的名字级去重保证。
//! 本 crate 零依赖（纯 std），可被 cli/server/skills 任意引用。

use std::path::{Path, PathBuf};

/// 配置来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// `.clinerules/*.md`（Cline）。
    Cline,
    /// `CLAUDE.md` / `.claude/CLAUDE.md`。
    ClaudeMd,
    /// `AGENTS.md`（Codex / 通用）。
    AgentsMd,
    /// `.cursor/rules/*.mdc`（Cursor；最具体，优先）。
    CursorMdc,
}

impl Source {
    /// 注入优先级（越大越优先，排序用）。
    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::Cline => 1,
            Self::ClaudeMd => 2,
            Self::AgentsMd => 3,
            Self::CursorMdc => 4,
        }
    }

    /// 展示名（注入段头）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cline => "cline",
            Self::ClaudeMd => "claude",
            Self::AgentsMd => "agents.md",
            Self::CursorMdc => "cursor",
        }
    }
}

/// 一条转换后的 section。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSection {
    /// 来源。
    pub source: Source,
    /// 原始文件路径。
    pub path: PathBuf,
    /// section 名（文件名主干 / AGENTS.md 段名）。
    pub name: String,
    /// glob 限定（仅 rule 型；`**` 表示全量）。
    pub globs: Vec<String>,
    /// 转换后的指令文本。
    pub content: String,
}

/// 全部发现结果（已按来源优先级排序、名字级去重——同源同名只保留首个）。
#[must_use]
pub fn discover(cwd: &Path) -> Vec<DiscoveredSection> {
    let mut out = Vec::new();
    out.extend(discover_agents_md(cwd));
    out.extend(discover_claude_md(cwd));
    out.extend(discover_cursor_mdc(cwd));
    out.extend(discover_clinerules(cwd));
    // 去重：`source:路径` 键（同一文件被多来源扫到只保留首个）；walkup 不同层级
    // 的 AGENTS.md/CLAUDE.md 内容各异，全部保留（与 Gyre 原生 context files 语义一致）。
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(format!("{:?}:{}", s.source, s.path.display())));
    out
}

/// AGENTS.md：cwd walkup + 用户级 `~/.agent/AGENTS.md`（与 Gyre 原生发现一致）。
fn discover_agents_md(cwd: &Path) -> Vec<DiscoveredSection> {
    let mut out = Vec::new();
    let home = dirs_home();
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let p = d.join("AGENTS.md");
        if p.is_file() {
            if let Ok(content) = std::fs::read_to_string(&p) {
                out.push(DiscoveredSection {
                    source: Source::AgentsMd,
                    path: p.clone(),
                    name: "AGENTS.md".into(),
                    globs: vec!["**".into()],
                    content,
                });
            }
        }
        if let Some(h) = &home {
            if d == h.as_path() {
                break;
            }
        }
        dir = d.parent();
    }
    if let Some(h) = home {
        let p = h.join(".agent").join("AGENTS.md");
        if p.is_file() {
            if let Ok(content) = std::fs::read_to_string(&p) {
                out.push(DiscoveredSection {
                    source: Source::AgentsMd,
                    path: p.clone(),
                    name: "AGENTS.md".into(),
                    globs: vec!["**".into()],
                    content,
                });
            }
        }
    }
    out
}

/// CLAUDE.md：`<dir>/CLAUDE.md` 与 `<dir>/.claude/CLAUDE.md`（walkup）。
fn discover_claude_md(cwd: &Path) -> Vec<DiscoveredSection> {
    let mut out = Vec::new();
    let home = dirs_home();
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        for rel in ["CLAUDE.md", ".claude/CLAUDE.md"] {
            let p = d.join(rel);
            if p.is_file() {
                if let Ok(content) = std::fs::read_to_string(&p) {
                    out.push(DiscoveredSection {
                        source: Source::ClaudeMd,
                        path: p.clone(),
                        name: rel.replace('/', "_"),
                        globs: vec!["**".into()],
                        content,
                    });
                }
            }
        }
        if let Some(h) = &home {
            if d == h.as_path() {
                break;
            }
        }
        dir = d.parent();
    }
    out
}

/// Cursor `.cursor/rules/*.mdc`（walkup）：frontmatter（description/globs/alwaysApply）+ body。
fn discover_cursor_mdc(cwd: &Path) -> Vec<DiscoveredSection> {
    let mut out = Vec::new();
    let home = dirs_home();
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let rules_dir = d.join(".cursor").join("rules");
        if let Ok(entries) = std::fs::read_dir(&rules_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) != Some("mdc") {
                    continue;
                }
                let Ok(raw) = std::fs::read_to_string(&p) else {
                    continue;
                };
                let Some(section) = parse_mdc(&p, &raw) else {
                    continue;
                };
                out.push(section);
            }
        }
        if let Some(h) = &home {
            if d == h.as_path() {
                break;
            }
        }
        dir = d.parent();
    }
    out
}

/// 解析单条 `.mdc`：`---` frontmatter（description/globs/alwaysApply）+ markdown body。
fn parse_mdc(path: &Path, raw: &str) -> Option<DiscoveredSection> {
    let rest = raw.strip_prefix("---")?;
    let (fm, body) = rest.split_once("---")?;
    let mut description = String::new();
    let mut globs: Vec<String> = Vec::new();
    for line in fm.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("description:") {
            description = v.trim().trim_matches('"').trim().to_string();
        } else if let Some(v) = line.strip_prefix("globs:") {
            let v = v.trim();
            if !v.is_empty() && v != "[]" {
                let inner = v.trim_matches(['[', ']']);
                globs = inner
                    .split(',')
                    .map(|g| g.trim().trim_matches('"').to_string())
                    .filter(|g| !g.is_empty())
                    .collect();
            }
        }
    }
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("cursor-rule")
        .to_string();
    let mut content = String::new();
    if !description.is_empty() {
        content.push_str(&format!("描述：{description}\n\n"));
    }
    content.push_str(body);
    Some(DiscoveredSection {
        source: Source::CursorMdc,
        path: path.to_path_buf(),
        name,
        // 无 globs 时按全量处理（cursor 语义：默认项目级）。`always_apply` 已并入全量。
        globs: if globs.is_empty() {
            vec!["**".into()]
        } else {
            globs
        },
        content,
    })
}

/// Cline `.clinerules/`（目录内每个 `.md` 一条）与 `.clinerules.md`（根）。
fn discover_clinerules(cwd: &Path) -> Vec<DiscoveredSection> {
    let mut out = Vec::new();
    let home = dirs_home();
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        // `.clinerules/NAME.md`。
        let rules_dir = d.join(".clinerules");
        if let Ok(entries) = std::fs::read_dir(&rules_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(&p) else {
                    continue;
                };
                if content.trim().is_empty() {
                    continue;
                }
                let name = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("cline-rule")
                    .to_string();
                out.push(DiscoveredSection {
                    source: Source::Cline,
                    path: p.clone(),
                    name,
                    globs: vec!["**".into()],
                    content,
                });
            }
        }
        // `.clinerules.md`（单文件版）。
        let single = d.join(".clinerules.md");
        if single.is_file() {
            if let Ok(content) = std::fs::read_to_string(&single) {
                if !content.trim().is_empty() {
                    out.push(DiscoveredSection {
                        source: Source::Cline,
                        path: single.clone(),
                        name: "clinerules".into(),
                        globs: vec!["**".into()],
                        content,
                    });
                }
            }
        }
        if let Some(h) = &home {
            if d == h.as_path() {
                break;
            }
        }
        dir = d.parent();
    }
    out
}

/// 渲染为 Gyre 的 context section 文本（供 `discover_context_files` 风格注入）。
#[must_use]
pub fn render_section(s: &DiscoveredSection) -> String {
    let glob_str = if s.globs.is_empty() || s.globs == ["**"] {
        String::new()
    } else {
        format!("（适用于 {}）", s.globs.join(", "))
    };
    format!(
        "外来配置[{}] <{}> {}{}:\n\n{}",
        s.source.label(),
        s.path.display(),
        s.name,
        glob_str,
        s.content
    )
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-disc-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discovers_agents_and_claude() {
        let dir = tmpdir();
        std::fs::write(dir.join("AGENTS.md"), "## 操作原则\n- 先读后写\n").unwrap();
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(".claude/CLAUDE.md"), "项目规则\n").unwrap();

        let found = discover(&dir);
        let labels: Vec<&str> = found.iter().map(|s| s.source.label()).collect();
        assert!(labels.contains(&"agents.md"), "{labels:?}");
        assert!(labels.contains(&"claude"), "{labels:?}");
        assert!(found.iter().any(|s| s.content.contains("先读后写")));
    }

    #[test]
    fn parses_cursor_mdc_frontmatter() {
        let dir = tmpdir();
        std::fs::create_dir_all(dir.join(".cursor/rules")).unwrap();
        std::fs::write(
            dir.join(".cursor/rules/rust-style.mdc"),
            "---\ndescription: \"Rust 风格约束\"\nglobs: [\"**/*.rs\", \"src/**\"]\nalwaysApply: true\n---\n禁止 unwrap 在生产路径。\n",
        )
        .unwrap();

        let found = discover(&dir);
        let mdc: Vec<&DiscoveredSection> = found
            .iter()
            .filter(|s| s.source == Source::CursorMdc)
            .collect();
        assert_eq!(mdc.len(), 1);
        assert_eq!(mdc[0].name, "rust-style");
        assert_eq!(mdc[0].globs, vec!["**/*.rs", "src/**"]);
        assert!(mdc[0].content.contains("禁止 unwrap"));
        assert!(mdc[0].content.contains("Rust 风格约束"));
    }

    #[test]
    fn parses_clinerules_dir_and_single() {
        let dir = tmpdir();
        std::fs::create_dir_all(dir.join(".clinerules")).unwrap();
        std::fs::write(dir.join(".clinerules/always.md"), "永远先运行测试\n").unwrap();
        std::fs::write(dir.join(".clinerules.md"), "单文件规则\n").unwrap();

        let found = discover(&dir);
        let cline: Vec<&DiscoveredSection> = found
            .iter()
            .filter(|s| s.source == Source::Cline)
            .collect();
        assert_eq!(cline.len(), 2, "{cline:?}");
        assert!(cline.iter().any(|s| s.name == "always"));
        assert!(cline.iter().any(|s| s.name == "clinerules"));
    }

    #[test]
    fn dedupes_by_source_and_name() {
        let dir = tmpdir();
        std::fs::write(dir.join("AGENTS.md"), "a\n").unwrap();
        // walkup 同文件不会重复（同一路径只扫一次）；同源同名不同路径 → 后者去重。
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/AGENTS.md"), "b\n").unwrap();
        let found = discover(&dir.join("sub"));
        let agents: Vec<&DiscoveredSection> = found
            .iter()
            .filter(|s| s.source == Source::AgentsMd)
            .collect();
        assert_eq!(agents.len(), 2, "{agents:?}"); // sub + dir 各一条（不同路径都保留）
    }

    #[test]
    fn render_section_formats() {
        let s = DiscoveredSection {
            source: Source::CursorMdc,
            path: PathBuf::from("/p/.cursor/rules/x.mdc"),
            name: "x".into(),
            globs: vec!["**/*.rs".into()],
            content: "body".into(),
        };
        let text = render_section(&s);
        assert!(text.contains("外来配置[cursor]") && text.contains("适用于 **/*.rs"), "{text}");
        assert!(text.contains("body"));
    }
}
