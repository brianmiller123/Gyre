//! 跨工具 skill 发现 provider：Claude Code / OpenAI Codex / OpenCode / GitHub Copilot。
//!
//! 路径与优先级语义对齐 oh-my-pi `discovery/{claude,codex,opencode,github}.ts`：
//!
//! | provider | id | priority | user 级 | project 级 |
//! |---|---|---|---|---|
//! | Claude Code | `claude` | 80 | `~/.claude/skills`（尊重 `CLAUDE_CONFIG_DIR`） | `.claude/skills`（cwd 向上 walkup，跳过 home） |
//! | OpenAI Codex | `codex` | 70 | `~/.codex/skills` | `.codex/skills`（仅 cwd） |
//! | OpenCode | `opencode` | 55 | `~/.config/opencode/skills` | `.opencode/skills`（仅 cwd） |
//! | GitHub Copilot | `github` | 30 | ——（无） | `.github/skills`（仅 cwd） |
//!
//! native provider（priority 100，见 [`crate::native`]）恒参与去重：同名 skill 高
//! priority 者胜。frontmatter 沿用宽松解析（见 [`crate::scan`]）；每 provider 每目录
//! 独立容错——一个目录坏只告警跳过，不拖垮其它目录/provider。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_core::{Skill, SkillError, SkillLevel, SkillLoadOptions, SkillProvider};
use async_trait::async_trait;

use crate::native::NativeSkillProvider;
use crate::scan::scan_dir;

/// 装配 native + 按开关启用的跨工具 provider。
///
/// 返回顺序即插入顺序；[`crate::SkillRegistry::with_providers`] 会按 priority 降序稳定排序
/// （native 100 > claude 80 > codex 70 > opencode 55 > github 30）。
#[must_use]
pub fn cross_tool_providers(
    cwd: PathBuf,
    toggles: &ProviderToggles,
) -> Vec<Arc<dyn SkillProvider>> {
    let mut providers: Vec<Arc<dyn SkillProvider>> =
        vec![Arc::new(NativeSkillProvider::new(cwd.clone()))];
    if toggles.claude {
        providers.push(claude_provider(cwd.clone()));
    }
    if toggles.codex {
        providers.push(codex_provider(cwd.clone()));
    }
    if toggles.opencode {
        providers.push(opencode_provider(cwd.clone()));
    }
    if toggles.github {
        providers.push(github_provider(cwd));
    }
    providers
}

/// 跨工具 provider 开关（缺省全开；native 恒开，不在表内）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderToggles {
    /// Claude Code（`.claude/skills`）。
    pub claude: bool,
    /// OpenAI Codex（`.codex/skills`）。
    pub codex: bool,
    /// OpenCode（`.opencode/skills` / `~/.config/opencode/skills`）。
    pub opencode: bool,
    /// GitHub Copilot（`.github/skills`）。
    pub github: bool,
}

impl Default for ProviderToggles {
    fn default() -> Self {
        Self {
            claude: true,
            codex: true,
            opencode: true,
            github: true,
        }
    }
}

/// Claude Code skill 发现（`.claude/skills`，priority 80）。
#[must_use]
pub fn claude_provider(cwd: PathBuf) -> Arc<dyn SkillProvider> {
    Arc::new(DirSetProvider::new(
        "claude",
        80,
        claude_dirs(dirs::home_dir().as_deref(), &cwd),
    ))
}

/// OpenAI Codex skill 发现（`.codex/skills`，priority 70）。
#[must_use]
pub fn codex_provider(cwd: PathBuf) -> Arc<dyn SkillProvider> {
    Arc::new(DirSetProvider::new(
        "codex",
        70,
        codex_dirs(dirs::home_dir().as_deref(), &cwd),
    ))
}

/// OpenCode skill 发现（`.opencode/skills`，priority 55）。
#[must_use]
pub fn opencode_provider(cwd: PathBuf) -> Arc<dyn SkillProvider> {
    Arc::new(DirSetProvider::new(
        "opencode",
        55,
        opencode_dirs(dirs::home_dir().as_deref(), &cwd),
    ))
}

/// GitHub Copilot skill 发现（`.github/skills`，priority 30）。
#[must_use]
pub fn github_provider(cwd: PathBuf) -> Arc<dyn SkillProvider> {
    Arc::new(DirSetProvider::new("github", 30, github_dirs(&cwd)))
}

/// 固定目录集 provider：一组 `(目录, 层级)` + 统一扫描与逐目录容错。
struct DirSetProvider {
    id: &'static str,
    priority: u32,
    dirs: Vec<(PathBuf, SkillLevel)>,
}

impl DirSetProvider {
    const fn new(id: &'static str, priority: u32, dirs: Vec<(PathBuf, SkillLevel)>) -> Self {
        Self { id, priority, dirs }
    }
}

#[async_trait]
impl SkillProvider for DirSetProvider {
    fn id(&self) -> &str {
        self.id
    }

    fn priority(&self) -> u32 {
        self.priority
    }

    /// 逐目录扫描；任一目录 IO 失败只告警跳过（该目录缺失 NotFound 时本就为空集）。
    async fn discover(&self, _opts: &SkillLoadOptions) -> Result<Vec<Skill>, SkillError> {
        let mut out = Vec::new();
        for (dir, level) in &self.dirs {
            match scan_dir(dir, *level, self.id) {
                Ok(skills) => out.extend(skills),
                Err(e) => {
                    tracing::warn!(
                        target: "agent_skills",
                        provider = self.id,
                        path = %dir.display(),
                        error = %e,
                        "跨工具 skill 目录扫描失败，已跳过"
                    );
                }
            }
        }
        Ok(out)
    }
}

/// Claude 目录集：user `<config>/skills`（`CLAUDE_CONFIG_DIR` 优先）+ project walkup。
fn claude_dirs(home: Option<&Path>, cwd: &Path) -> Vec<(PathBuf, SkillLevel)> {
    let mut dirs = Vec::new();
    if let Some(config) = claude_config_dir(home, cwd) {
        dirs.push((config.join("skills"), SkillLevel::User));
    }
    dirs.extend(
        project_walkup_dirs(cwd, ".claude", home)
            .into_iter()
            .map(|d| (d, SkillLevel::Project)),
    );
    dirs
}

/// Claude 用户级配置目录：`CLAUDE_CONFIG_DIR`（相对路径按 cwd 解析，对齐 omp
/// `resolveClaudePaths`）优先，否则 `<home>/.claude`；home 未知且未设置 env 时返回 `None`。
fn claude_config_dir(home: Option<&Path>, cwd: &Path) -> Option<PathBuf> {
    if let Ok(v) = std::env::var("CLAUDE_CONFIG_DIR") {
        let v = v.trim();
        if !v.is_empty() {
            let p = Path::new(v);
            return Some(if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            });
        }
    }
    home.map(|h| h.join(".claude"))
}

/// Codex 目录集：user `<home>/.codex/skills` + project `<cwd>/.codex/skills`。
fn codex_dirs(home: Option<&Path>, cwd: &Path) -> Vec<(PathBuf, SkillLevel)> {
    let mut dirs = Vec::new();
    if let Some(h) = home {
        dirs.push((h.join(".codex").join("skills"), SkillLevel::User));
    }
    dirs.push((cwd.join(".codex").join("skills"), SkillLevel::Project));
    dirs
}

/// OpenCode 目录集：user `<home>/.config/opencode/skills` + project `<cwd>/.opencode/skills`。
fn opencode_dirs(home: Option<&Path>, cwd: &Path) -> Vec<(PathBuf, SkillLevel)> {
    let mut dirs = Vec::new();
    if let Some(h) = home {
        dirs.push((
            h.join(".config").join("opencode").join("skills"),
            SkillLevel::User,
        ));
    }
    dirs.push((cwd.join(".opencode").join("skills"), SkillLevel::Project));
    dirs
}

/// GitHub Copilot 目录集：仅 project `<cwd>/.github/skills`（对齐 omp：github 无 user 级）。
fn github_dirs(cwd: &Path) -> Vec<(PathBuf, SkillLevel)> {
    vec![(cwd.join(".github").join("skills"), SkillLevel::Project)]
}

/// 自 cwd 向上枚举 `<ancestor>/<base>/skills`；home 目录本身排除（由 user 级扫描覆盖，
/// 对齐 omp claude.ts 的注释语义），无 home 则止于文件系统根。
fn project_walkup_dirs(cwd: &Path, base: &str, home: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        if home == Some(dir) {
            break;
        }
        out.push(dir.join(base).join("skills"));
        current = dir.parent();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::SkillResolver;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_root(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("agent-skills-x-{}-{tag}-{n}", std::process::id()))
    }

    /// 写入 `<root>/<file_rel>`（file_rel 为 SKILL.md 相对路径，如 `.claude/skills/demo/SKILL.md`），
    /// 返回该文件路径。
    fn write_skill(root: &Path, file_rel: &str, name: &str, description: &str) -> PathBuf {
        let md = root.join(file_rel);
        std::fs::create_dir_all(md.parent().unwrap()).unwrap();
        std::fs::write(
            &md,
            format!("---\nname: {name}\ndescription: {description}\n---\n正文"),
        )
        .unwrap();
        md
    }

    fn provider(
        id: &'static str,
        priority: u32,
        dirs: Vec<(PathBuf, SkillLevel)>,
    ) -> Arc<dyn SkillProvider> {
        Arc::new(DirSetProvider::new(id, priority, dirs))
    }

    // ── 纯路径解析 ──────────────────────────────────────────────────────────

    #[test]
    fn claude_dirs_resolves_user_and_walkup_skipping_home() {
        let home = Path::new("/home/fake");
        let cwd = Path::new("/home/fake/work/sub");
        let dirs = claude_dirs(Some(home), cwd);
        // user 首位
        assert_eq!(
            dirs[0],
            (PathBuf::from("/home/fake/.claude/skills"), SkillLevel::User)
        );
        // walkup：cwd → home 之前，home 自身被排除
        let project: Vec<_> = dirs[1..].iter().map(|(d, _)| d.clone()).collect();
        assert_eq!(
            project,
            vec![
                PathBuf::from("/home/fake/work/sub/.claude/skills"),
                PathBuf::from("/home/fake/work/.claude/skills"),
            ]
        );
    }

    #[test]
    fn fixed_layout_dirs_match_omp_source_paths() {
        let home = Path::new("/home/fake");
        let cwd = Path::new("/proj");
        assert_eq!(
            codex_dirs(Some(home), cwd),
            vec![
                (PathBuf::from("/home/fake/.codex/skills"), SkillLevel::User),
                (PathBuf::from("/proj/.codex/skills"), SkillLevel::Project),
            ]
        );
        assert_eq!(
            opencode_dirs(Some(home), cwd),
            vec![
                (
                    PathBuf::from("/home/fake/.config/opencode/skills"),
                    SkillLevel::User
                ),
                (PathBuf::from("/proj/.opencode/skills"), SkillLevel::Project),
            ]
        );
        assert_eq!(
            github_dirs(cwd),
            vec![(PathBuf::from("/proj/.github/skills"), SkillLevel::Project)]
        );
        // 未设 `CLAUDE_CONFIG_DIR` 的默认分支（env 分支不做进程级注入，避免并行测试串扰）
        assert_eq!(
            claude_dirs(Some(home), cwd)[0].0,
            PathBuf::from("/home/fake/.claude/skills")
        );
    }
    /// 各 provider 布局在真实文件系统上的发现（临时目录充当 home 与项目根）。
    #[tokio::test]
    async fn discovers_each_provider_layout() {
        let home = unique_root("home");
        let root = unique_root("proj");
        let cwd = root.clone();
        std::fs::create_dir_all(&cwd).unwrap();

        // project 级（walkup 自 cwd 必经 root）
        write_skill(
            &root,
            ".claude/skills/claude-p/SKILL.md",
            "claude-p",
            "项目级",
        );
        write_skill(&root, ".codex/skills/codex-p/SKILL.md", "codex-p", "项目级");
        write_skill(
            &root,
            ".opencode/skills/opencode-p/SKILL.md",
            "opencode-p",
            "项目级",
        );
        write_skill(
            &root,
            ".github/skills/github-p/SKILL.md",
            "github-p",
            "项目级",
        );
        // user 级（github 无 user 级）
        write_skill(
            &home,
            ".claude/skills/claude-u/SKILL.md",
            "claude-u",
            "用户级",
        );
        write_skill(&home, ".codex/skills/codex-u/SKILL.md", "codex-u", "用户级");
        write_skill(
            &home,
            ".config/opencode/skills/opencode-u/SKILL.md",
            "opencode-u",
            "用户级",
        );

        let p = provider("claude", 80, claude_dirs(Some(&home), &cwd));
        let mut got = p.discover(&SkillLoadOptions::default()).await.unwrap();
        got.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = got.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["claude-p", "claude-u"]);
        assert!(got.iter().all(|s| s.source.provider == "claude"));
        let project = got.iter().find(|s| s.name == "claude-p").unwrap();
        assert_eq!(project.source.level, SkillLevel::Project);
        assert_eq!(
            project.file_path,
            root.join(".claude/skills/claude-p/SKILL.md")
        );

        let g = provider("github", 30, github_dirs(&cwd));
        let got = g.discover(&SkillLoadOptions::default()).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "github-p");
        assert_eq!(got[0].source.level, SkillLevel::Project);

        let c = provider("codex", 70, codex_dirs(Some(&home), &cwd));
        let mut got = c.discover(&SkillLoadOptions::default()).await.unwrap();
        got.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = got.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["codex-p", "codex-u"]);

        let o = provider("opencode", 55, opencode_dirs(Some(&home), &cwd));
        let mut got = o.discover(&SkillLoadOptions::default()).await.unwrap();
        got.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = got.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["opencode-p", "opencode-u"]);

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 坏目录 / 坏文件不拖垮同 provider 其它目录与其它 skill。
    #[tokio::test]
    async fn tolerates_broken_dirs_and_files() {
        let root = unique_root("broken");
        let cwd = root.clone();
        std::fs::create_dir_all(&cwd).unwrap();

        // 好目录：两个 skill
        write_skill(&root, ".github/skills/good/SKILL.md", "good", "好的");
        write_skill(&root, ".github/skills/good2/SKILL.md", "good2", "也好");
        // 坏文件：非法 UTF-8
        let bad_dir = root.join(".github/skills/bad");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("SKILL.md"), [0xff, 0xfe]).unwrap();
        // 坏目录：skills 根位置是普通文件（read_dir 失败）
        std::fs::write(root.join(".claude"), "not a dir").unwrap();

        let mut dirs = github_dirs(&cwd);
        dirs.push((root.join(".claude"), SkillLevel::Project)); // 坏目录：该位置是普通文件
        dirs.push((root.join(".claude/skills"), SkillLevel::Project)); // 途经上面的普通文件
        let p = provider("github", 30, dirs);

        let got = p.discover(&SkillLoadOptions::default()).await.unwrap();
        let mut names: Vec<_> = got.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["good", "good2"],
            "坏文件/坏目录不应拖垮其它 skill"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // ── 装配：开关 + 去重优先级 ─────────────────────────────────────────────

    #[test]
    fn toggles_assemble_expected_providers() {
        let cwd = PathBuf::from("/proj");
        let ids = |t: &ProviderToggles| -> Vec<String> {
            let mut v: Vec<_> = cross_tool_providers(cwd.clone(), t);
            v.sort_by_key(|p| std::cmp::Reverse(p.priority()));
            v.iter().map(|p| p.id().to_string()).collect()
        };
        assert_eq!(
            ids(&ProviderToggles::default()),
            vec!["native", "claude", "codex", "opencode", "github"]
        );
        assert_eq!(
            ids(&ProviderToggles {
                claude: false,
                ..Default::default()
            }),
            vec!["native", "codex", "opencode", "github"]
        );
        assert_eq!(
            ids(&ProviderToggles {
                codex: false,
                opencode: false,
                github: false,
                ..Default::default()
            }),
            vec!["native", "claude"]
        );
    }

    /// 跨 provider 同名去重：native > claude > codex（priority first-wins），
    /// 来源标签随 catalog 暴露，开关关闭后对应来源不再出现。
    #[tokio::test]
    async fn cross_tool_registry_dedups_by_priority_and_honors_toggles() {
        let root = unique_root("dedup");
        let cwd = root.clone();
        std::fs::create_dir_all(&cwd).unwrap();

        write_skill(&root, "native-skills/dup/SKILL.md", "dup", "native 版");
        write_skill(&root, ".claude/skills/dup/SKILL.md", "dup", "claude 版");
        write_skill(&root, ".codex/skills/dup/SKILL.md", "dup", "codex 版");
        write_skill(
            &root,
            ".claude/skills/only-claude/SKILL.md",
            "only-claude",
            "claude 独有",
        );
        write_skill(
            &root,
            ".github/skills/only-gh/SKILL.md",
            "only-gh",
            "github 独有",
        );

        let opts = SkillLoadOptions {
            custom_directories: vec![root.join("native-skills")],
            ..Default::default()
        };

        // 全开：dup 由 native 胜出；claude/github 独有名可见
        let all = crate::SkillRegistry::cross_tool(cwd.clone(), &ProviderToggles::default());
        let cat = all.load(&opts).await.unwrap();
        let dup = cat.find("dup").expect("dup 应存在");
        assert_eq!(dup.source.provider, "native", "native 优先级最高");
        assert_eq!(dup.description, "native 版");
        assert_eq!(cat.find("only-claude").unwrap().source.provider, "claude");
        assert_eq!(cat.find("only-gh").unwrap().source.provider, "github");

        // 关掉 claude：only-claude 消失；dup 不受影响（native 仍最高）
        let no_claude = crate::SkillRegistry::cross_tool(
            cwd.clone(),
            &ProviderToggles {
                claude: false,
                ..Default::default()
            },
        );
        let cat = no_claude.load(&opts).await.unwrap();
        assert!(
            cat.find("only-claude").is_none(),
            "claude 关闭后其 skill 不应出现"
        );

        // 只留 native：全部跨工具名消失
        let native_only = crate::SkillRegistry::cross_tool(
            cwd.clone(),
            &ProviderToggles {
                claude: false,
                codex: false,
                opencode: false,
                github: false,
            },
        );
        let cat = native_only.load(&opts).await.unwrap();
        assert!(cat.find("only-claude").is_none() && cat.find("only-gh").is_none());
        assert_eq!(cat.find("dup").unwrap().source.provider, "native");

        // native 关掉同名后，claude(80) 胜过 codex(70)：把 custom_directories 指向 claude 同名集验证次序
        // （此处直接断言 provider 次序即可，见 toggles_assemble_expected_providers）

        let _ = std::fs::remove_dir_all(&root);
    }

    /// skill:// 解析在跨工具目录下照常工作（base_dir 取自胜出 skill）。
    #[tokio::test]
    async fn cross_tool_skill_url_resolves() {
        let root = unique_root("url");
        let cwd = root.clone();
        std::fs::create_dir_all(&cwd).unwrap();
        let md = write_skill(
            &root,
            ".claude/skills/urltest/SKILL.md",
            "urltest",
            "可解析",
        );

        let registry = crate::SkillRegistry::cross_tool(cwd, &ProviderToggles::default());
        let cat = registry.load(&SkillLoadOptions::default()).await.unwrap();
        let p = SkillResolver::resolve(&cat, "skill://urltest").unwrap();
        assert_eq!(p, md);

        let _ = std::fs::remove_dir_all(&root);
    }
}
