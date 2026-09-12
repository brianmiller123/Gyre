//! # agent-discovery
//!
//! 外来 Agent 工具配置发现与转换（移植 oh-my-pi `discovery` 的 v1 子集，P2-N）。
//!
//! 开箱读取用户既有的其他工具配置，统一为 Gyre 的 context section：
//! - `AGENTS.md`（通用规范，walkup + `~/.agent/AGENTS.md`）→ instruction
//! - `CLAUDE.md` / `.claude/CLAUDE.md`（Claude Code，walkup）→ instruction
//! - `~/.codex/AGENTS.md`（Codex 用户级；项目级 AGENTS.md 由上一项覆盖）→ instruction
//! - `GEMINI.md` / `.gemini/GEMINI.md`（Gemini CLI，walkup）+ `~/.gemini/GEMINI.md` → instruction
//! - `~/.config/opencode/AGENTS.md`（OpenCode 用户级）→ instruction
//! - `.cursor/rules/*.mdc`（Cursor，walkup）→ rule（frontmatter: description/globs/alwaysApply）
//! - `.windsurf/rules/*.md` + `~/.codeium/windsurf/memories/global_rules.md` + 遗留
//!   `.windsurfrules`（Windsurf，walkup）→ rule
//! - `.clinerules/*.md` / `.clinerules.md`（Cline）→ instruction
//!
//! 语义：**只读转换**，不写入、不移动任何外来文件。[`discover`] 的输出按
//! [`Source::priority`] **升序**（通用 → 具体，越具体越靠后注入）稳定排序，并做
//! 「来源 + 路径」级去重；walkup 得到的多层同名文件（不同路径）全部保留、由调用方
//! 组装顺序决定遮蔽。
//! 本 crate 零依赖（纯 std），可被 cli/server/skills 任意引用。

use std::path::{Path, PathBuf};

/// 配置来源。
///
/// 声明顺序即 [`Source::priority`] 的升序（通用 → 具体），HH24 起用于
/// [`discover`] 的稳定排序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// `.clinerules/*.md`（Cline）。
    Cline,
    /// `~/.codex/AGENTS.md`（Codex 用户级）。
    Codex,
    /// `CLAUDE.md` / `.claude/CLAUDE.md`。
    ClaudeMd,
    /// `~/.config/opencode/AGENTS.md`（OpenCode 用户级）。
    OpenCode,
    /// `GEMINI.md` / `.gemini/GEMINI.md`（Gemini CLI）。
    Gemini,
    /// `AGENTS.md`（通用/Codex 项目级）。
    AgentsMd,
    /// `.windsurf/rules/*.md` 与 `~/.codeium/windsurf/memories/global_rules.md`（Windsurf）。
    Windsurf,
    /// `.cursor/rules/*.mdc`（Cursor；最具体，最后注入）。
    CursorMdc,
}

impl Source {
    /// 注入优先级（**越大越具体、越靠后注入**；最后注入者对模型影响最强）。
    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::Cline => 1,
            Self::Codex => 2,
            Self::ClaudeMd => 3,
            Self::OpenCode => 4,
            Self::Gemini => 5,
            Self::AgentsMd => 6,
            Self::Windsurf => 7,
            Self::CursorMdc => 8,
        }
    }

    /// 展示名（注入段头）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cline => "cline",
            Self::Codex => "codex",
            Self::ClaudeMd => "claude",
            Self::OpenCode => "opencode",
            Self::Gemini => "gemini",
            Self::AgentsMd => "agents.md",
            Self::Windsurf => "windsurf",
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

/// 全部发现结果（已按来源优先级升序稳定排序、`来源+路径` 级去重）。
#[must_use]
pub fn discover(cwd: &Path) -> Vec<DiscoveredSection> {
    discover_with_home(cwd, dirs_home().as_deref())
}

/// 与 [`discover`] 相同，但显式传入用户主目录（`None` = 不扫描用户级配置）。
///
/// 测试用它避免改进程级环境变量（`HOME` 是全局状态，并行测试下会互相干扰）。
#[must_use]
pub fn discover_with_home(cwd: &Path, home: Option<&Path>) -> Vec<DiscoveredSection> {
    let mut out = Vec::new();
    out.extend(discover_clinerules(cwd, home));
    out.extend(discover_codex(cwd, home));
    out.extend(discover_claude_md(cwd, home));
    out.extend(discover_opencode(cwd, home));
    out.extend(discover_gemini(cwd, home));
    out.extend(discover_agents_md(cwd, home));
    out.extend(discover_windsurf(cwd, home));
    out.extend(discover_cursor_mdc(cwd, home));
    // 来源优先级升序（通用 → 具体）；同优先级按路径排序保证确定性。
    out.sort_by(|a, b| {
        a.source
            .priority()
            .cmp(&b.source.priority())
            .then_with(|| a.path.cmp(&b.path))
    });
    // 去重：`source:路径` 键（同一文件被多来源扫到只保留首个）；walkup 不同层级
    // 的 AGENTS.md/CLAUDE.md 内容各异，全部保留（与 Gyre 原生 context files 语义一致）。
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(format!("{:?}:{}", s.source, s.path.display())));
    out
}

/// 读取单个 instruction 文件（不存在/不可读/空白 → 空结果，不报错）。
fn read_instruction(path: &Path, source: Source, name: &str) -> Vec<DiscoveredSection> {
    if !path.is_file() {
        return Vec::new();
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    if content.trim().is_empty() {
        return Vec::new();
    }
    vec![DiscoveredSection {
        source,
        path: path.to_path_buf(),
        name: name.to_string(),
        globs: vec!["**".into()],
        content,
    }]
}

/// 沿 cwd → 根 walkup，对每个目录尝试一组相对路径（首个命中即收，每目录至多一条）。
fn walkup_first(
    cwd: &Path,
    home: Option<&Path>,
    rels: &[&str],
    source: Source,
    name: &str,
) -> Vec<DiscoveredSection> {
    let mut out = Vec::new();
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        for rel in rels {
            let found = read_instruction(&d.join(rel), source, name);
            if !found.is_empty() {
                out.extend(found);
                break;
            }
        }
        if let Some(h) = home {
            if d == h {
                break;
            }
        }
        dir = d.parent();
    }
    out
}

/// Codex 用户级 `~/.codex/AGENTS.md`（项目级 `AGENTS.md` 由 [`discover_agents_md`] 覆盖）。
fn discover_codex(_cwd: &Path, home: Option<&Path>) -> Vec<DiscoveredSection> {
    let Some(home) = home else {
        return Vec::new();
    };
    read_instruction(
        &home.join(".codex").join("AGENTS.md"),
        Source::Codex,
        "codex/AGENTS.md",
    )
}

/// Gemini CLI：`~/.gemini/GEMINI.md` + walkup `GEMINI.md` / `.gemini/GEMINI.md`。
fn discover_gemini(cwd: &Path, home: Option<&Path>) -> Vec<DiscoveredSection> {
    let mut out = Vec::new();
    if let Some(home) = home {
        out.extend(read_instruction(
            &home.join(".gemini").join("GEMINI.md"),
            Source::Gemini,
            "gemini/GEMINI.md",
        ));
    }
    out.extend(walkup_first(
        cwd,
        home,
        &["GEMINI.md", ".gemini/GEMINI.md"],
        Source::Gemini,
        "GEMINI.md",
    ));
    out
}

/// OpenCode 用户级 `~/.config/opencode/AGENTS.md`（项目级用根 `AGENTS.md`，已由通用源覆盖）。
fn discover_opencode(_cwd: &Path, home: Option<&Path>) -> Vec<DiscoveredSection> {
    let Some(home) = home else {
        return Vec::new();
    };
    read_instruction(
        &home.join(".config").join("opencode").join("AGENTS.md"),
        Source::OpenCode,
        "opencode/AGENTS.md",
    )
}

/// Windsurf：用户级 `memories/global_rules.md`、walkup `.windsurf/rules/*.md`、
/// 以及遗留单文件 `.windsurfrules`（omp provider 描述声明支持，其 loader 未实现——此处补齐）。
fn discover_windsurf(cwd: &Path, home: Option<&Path>) -> Vec<DiscoveredSection> {
    let mut out = Vec::new();
    if let Some(home) = home {
        out.extend(read_instruction(
            &home
                .join(".codeium")
                .join("windsurf")
                .join("memories")
                .join("global_rules.md"),
            Source::Windsurf,
            "global_rules",
        ));
    }
    let home_owned = home.map(Path::to_path_buf);
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let rules_dir = d.join(".windsurf").join("rules");
        if let Ok(entries) = std::fs::read_dir(&rules_dir) {
            let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
            paths.sort();
            for p in paths {
                if p.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Ok(raw) = std::fs::read_to_string(&p) else {
                    continue;
                };
                if let Some(section) = parse_rule_markdown(&p, &raw, Source::Windsurf) {
                    out.push(section);
                }
            }
        }
        out.extend(read_instruction(
            &d.join(".windsurfrules"),
            Source::Windsurf,
            "windsurfrules",
        ));
        if let Some(h) = &home_owned {
            if d == h {
                break;
            }
        }
        dir = d.parent();
    }
    out
}

/// AGENTS.md：cwd walkup + 用户级 `~/.agent/AGENTS.md`（与 Gyre 原生发现一致）。
fn discover_agents_md(cwd: &Path, home: Option<&Path>) -> Vec<DiscoveredSection> {
    let mut out = Vec::new();
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
        if let Some(h) = home {
            if d == h {
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
                    path: p,
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
fn discover_claude_md(cwd: &Path, home: Option<&Path>) -> Vec<DiscoveredSection> {
    let mut out = Vec::new();
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
        if let Some(h) = home {
            if d == h {
                break;
            }
        }
        dir = d.parent();
    }
    out
}

/// Cursor `.cursor/rules/*.mdc`（walkup）：frontmatter（description/globs/alwaysApply）+ body。
fn discover_cursor_mdc(cwd: &Path, home: Option<&Path>) -> Vec<DiscoveredSection> {
    let mut out = Vec::new();
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
                let Some(section) = parse_rule_markdown(&p, &raw, Source::CursorMdc) else {
                    continue;
                };
                out.push(section);
            }
        }
        if let Some(h) = home {
            if d == h {
                break;
            }
        }
        dir = d.parent();
    }
    out
}

/// 解析单条 rule markdown：`---` frontmatter（description/globs/alwaysApply，
/// Windsurf 的 `trigger` 亦被接受）+ markdown body。
///
/// Cursor `.mdc` 与 Windsurf `.windsurf/rules/*.md` 共用该解析器（两者 frontmatter 形状一致）。
fn parse_rule_markdown(path: &Path, raw: &str, source: Source) -> Option<DiscoveredSection> {
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
        source,
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
fn discover_clinerules(cwd: &Path, home: Option<&Path>) -> Vec<DiscoveredSection> {
    let mut out = Vec::new();
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
        if let Some(h) = home {
            if d == h {
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
        let cline: Vec<&DiscoveredSection> =
            found.iter().filter(|s| s.source == Source::Cline).collect();
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

    /// H44：Gemini CLI 用户级 + walkup `.gemini/GEMINI.md`。
    #[test]
    fn discovers_gemini_user_and_project() {
        let dir = tmpdir();
        let home = tmpdir();
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        std::fs::write(home.join(".gemini/GEMINI.md"), "用户级 Gemini 规则\n").unwrap();
        std::fs::create_dir_all(dir.join(".gemini")).unwrap();
        std::fs::write(dir.join(".gemini/GEMINI.md"), "项目 Gemini 规则\n").unwrap();

        let found = discover_with_home(&dir, Some(&home));
        let gem: Vec<&DiscoveredSection> = found
            .iter()
            .filter(|s| s.source == Source::Gemini)
            .collect();
        assert_eq!(gem.len(), 2, "{gem:?}");
        assert!(
            gem.iter().any(|s| s.content.contains("用户级 Gemini 规则")),
            "{gem:?}"
        );
        assert!(
            gem.iter().any(|s| s.content.contains("项目 Gemini 规则")),
            "{gem:?}"
        );
        // 无 home → 只剩项目级（用户级不可见）。
        let no_home = discover_with_home(&dir, None);
        let gem2: Vec<&DiscoveredSection> = no_home
            .iter()
            .filter(|s| s.source == Source::Gemini)
            .collect();
        assert_eq!(gem2.len(), 1, "{gem2:?}");
    }

    /// H44：Codex（`~/.codex/AGENTS.md`）与 OpenCode（`~/.config/opencode/AGENTS.md`）用户级。
    #[test]
    fn discovers_codex_and_opencode_user_levels() {
        let dir = tmpdir();
        let home = tmpdir();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/AGENTS.md"), "codex 用户规则\n").unwrap();
        std::fs::create_dir_all(home.join(".config/opencode")).unwrap();
        std::fs::write(
            home.join(".config/opencode/AGENTS.md"),
            "opencode 用户规则\n",
        )
        .unwrap();

        let found = discover_with_home(&dir, Some(&home));
        assert!(
            found
                .iter()
                .any(|s| s.source == Source::Codex && s.content.contains("codex 用户规则")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|s| s.source == Source::OpenCode && s.content.contains("opencode 用户规则")),
            "{found:?}"
        );
        // 用户级文件只在显式 home 下被发现。
        let no_home = discover_with_home(&dir, None);
        assert!(!no_home.iter().any(|s| s.source == Source::Codex));
        assert!(!no_home.iter().any(|s| s.source == Source::OpenCode));
    }

    /// H44：Windsurf 项目规则目录（含 frontmatter）、用户级 global_rules.md 与遗留 `.windsurfrules`。
    #[test]
    fn discovers_windsurf_rules_and_legacy() {
        let dir = tmpdir();
        let home = tmpdir();
        std::fs::create_dir_all(dir.join(".windsurf/rules")).unwrap();
        std::fs::write(
            dir.join(".windsurf/rules/style.md"),
            "---\ndescription: \"Windsurf 风格\"\nglobs: [\"**/*.rs\"]\n---\n禁止 unwrap。\n",
        )
        .unwrap();
        std::fs::write(dir.join(".windsurfrules"), "遗留单文件规则\n").unwrap();
        std::fs::create_dir_all(home.join(".codeium/windsurf/memories")).unwrap();
        std::fs::write(
            home.join(".codeium/windsurf/memories/global_rules.md"),
            "全局 Windsurf 规则\n",
        )
        .unwrap();

        let found = discover_with_home(&dir, Some(&home));
        let ws: Vec<&DiscoveredSection> = found
            .iter()
            .filter(|s| s.source == Source::Windsurf)
            .collect();
        assert_eq!(ws.len(), 3, "{ws:?}");
        assert!(
            ws.iter()
                .any(|s| s.name == "style" && s.globs == vec!["**/*.rs".to_string()]),
            "{ws:?}"
        );
        assert!(ws.iter().any(|s| s.name == "windsurfrules"), "{ws:?}");
        assert!(ws.iter().any(|s| s.name == "global_rules"), "{ws:?}");
    }

    /// H44：输出按来源优先级升序稳定排序（通用在前、具体在后）。
    #[test]
    fn discover_sorted_by_priority() {
        let dir = tmpdir();
        let home = tmpdir();
        std::fs::write(dir.join("AGENTS.md"), "a\n").unwrap();
        std::fs::create_dir_all(dir.join(".clinerules")).unwrap();
        std::fs::write(dir.join(".clinerules/x.md"), "c\n").unwrap();
        std::fs::create_dir_all(dir.join(".cursor/rules")).unwrap();
        std::fs::write(dir.join(".cursor/rules/y.mdc"), "---\n---\nbody\n").unwrap();
        std::fs::write(dir.join("GEMINI.md"), "g\n").unwrap();

        let found = discover_with_home(&dir, Some(&home));
        let prios: Vec<u8> = found.iter().map(|s| s.source.priority()).collect();
        assert!(prios.windows(2).all(|w| w[0] <= w[1]), "{prios:?}");
        assert_eq!(found.first().map(|s| s.source), Some(Source::Cline));
        assert_eq!(found.last().map(|s| s.source), Some(Source::CursorMdc));
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
        assert!(
            text.contains("外来配置[cursor]") && text.contains("适用于 **/*.rs"),
            "{text}"
        );
        assert!(text.contains("body"));
    }
}
