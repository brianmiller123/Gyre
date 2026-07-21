//! 原生 skill 发现 provider：扫描 `<config_dir>/skills`（user）与 `.agent/skills`（project walkup）。

use std::path::{Path, PathBuf};

use agent_core::{
    Skill, SkillError, SkillLevel, SkillLoadOptions, SkillProvider, SkillSource, config_dir,
};
use async_trait::async_trait;

use crate::frontmatter::parse_skill_file;

const PROVIDER_ID: &str = "native";

/// 原生 skill 发现 provider。
///
/// 发现位置（非递归 `*/SKILL.md`）：
/// - user：`<config_dir>/skills`（Linux `~/.config/agent/skills`）
/// - project：自 cwd 向上 walkup 的 `<ancestor>/.agent/skills`（止于 home）
/// - 自定义目录（来自 [`SkillLoadOptions::custom_directories`]，视作 user）
pub struct NativeSkillProvider {
    cwd: PathBuf,
}

impl NativeSkillProvider {
    /// 构造；`cwd` 为 project walkup 起点。
    #[must_use]
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl SkillProvider for NativeSkillProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn priority(&self) -> u32 {
        100
    }

    async fn discover(&self, opts: &SkillLoadOptions) -> Result<Vec<Skill>, SkillError> {
        let mut out = Vec::new();
        // user: <config_dir>/skills
        if let Some(cfg) = config_dir() {
            out.extend(scan_dir(&cfg.join("skills"), SkillLevel::User)?);
        }
        // project: 自 cwd 向上 walkup 的 .agent/skills
        for dir in project_skill_dirs(&self.cwd) {
            out.extend(scan_dir(&dir, SkillLevel::Project)?);
        }
        // 自定义目录（视作 user）
        for dir in &opts.custom_directories {
            out.extend(scan_dir(dir, SkillLevel::User)?);
        }
        Ok(out)
    }
}

/// 扫描一个 skills 根目录下的 `<name>/SKILL.md`（非递归）。
fn scan_dir(dir: &Path, level: SkillLevel) -> Result<Vec<Skill>, SkillError> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(SkillError::Io(e)),
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if fname.starts_with('.') || !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        match load_skill_file(&skill_md, level) {
            Ok(skill) => out.push(skill),
            Err(e) => {
                tracing::warn!(
                    target: "agent_skills",
                    path = %skill_md.display(),
                    error = %e,
                    "failed to load skill file"
                );
            }
        }
    }
    Ok(out)
}

/// 加载单个 SKILL.md 为 [`Skill`]。
fn load_skill_file(skill_md: &Path, level: SkillLevel) -> Result<Skill, SkillError> {
    let content = std::fs::read_to_string(skill_md)?;
    let fm = parse_skill_file(&content).frontmatter;
    let dir_name = skill_md
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("skill");
    let name = fm
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| dir_name.to_string());
    let base_dir = skill_md.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok(Skill {
        name,
        description: fm.description.unwrap_or_default(),
        file_path: skill_md.to_path_buf(),
        base_dir,
        source: SkillSource {
            provider: PROVIDER_ID.to_string(),
            level,
        },
        hide: fm.hide,
        modes: fm.modes,
    })
}

/// 自 cwd 向上枚举 `.agent/skills` 候选，止于 home 目录。
fn project_skill_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let home = dirs::home_dir();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        out.push(dir.join(".agent").join("skills"));
        if let Some(h) = &home {
            if dir == h.as_path() {
                break;
            }
        }
        current = dir.parent();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{SkillLevel, SkillLoadOptions, SkillResolver};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_skills_root() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("agent-skills-it-{}-{n}", std::process::id()))
    }

    /// 端到端：真实文件系统发现 → registry 聚合 → catalog 解析 skill://。
    #[tokio::test]
    async fn discovers_custom_dir_and_resolves_skill_url() {
        let root = unique_skills_root();
        let demo = root.join("demo");
        std::fs::create_dir_all(&demo).unwrap();
        std::fs::write(
            demo.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\n# Demo\n正文",
        )
        .unwrap();

        let provider = NativeSkillProvider::new(std::env::temp_dir());
        let opts = SkillLoadOptions {
            enabled: true,
            custom_directories: vec![root.clone()],
            ..Default::default()
        };

        // 发现
        let skills = provider.discover(&opts).await.unwrap();
        let found = skills
            .iter()
            .find(|s| s.name == "demo")
            .expect("应发现 demo");
        assert_eq!(found.description, "Demo skill");
        assert_eq!(found.source.level, SkillLevel::User);
        assert_eq!(found.base_dir, demo);

        // 聚合 + 解析
        let registry = crate::SkillRegistry::with_providers(vec![std::sync::Arc::new(provider)]);
        let cat = registry.load(&opts).await.unwrap();
        let path = SkillResolver::resolve(&cat, "skill://demo").unwrap();
        assert_eq!(path, demo.join("SKILL.md"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 目录名缺 SKILL.md 时跳过；frontmatter 缺 name 时回退目录名。
    #[tokio::test]
    async fn skips_missing_skill_md_and_defaults_name() {
        let root = unique_skills_root();
        // 缺 SKILL.md 的目录（应跳过）
        std::fs::create_dir_all(root.join("empty")).unwrap();
        // 仅目录名（无 frontmatter name）
        let named = root.join("by-dir");
        std::fs::create_dir_all(&named).unwrap();
        std::fs::write(
            named.join("SKILL.md"),
            "---\ndescription: By dir\n---\nbody",
        )
        .unwrap();

        let provider = NativeSkillProvider::new(std::env::temp_dir());
        let opts = SkillLoadOptions {
            enabled: true,
            custom_directories: vec![root.clone()],
            ..Default::default()
        };
        let skills = provider.discover(&opts).await.unwrap();
        assert!(
            skills.iter().all(|s| s.name != "empty"),
            "缺 SKILL.md 的目录不应被发现"
        );
        let by_dir = skills
            .iter()
            .find(|s| s.name == "by-dir")
            .expect("应回退目录名");
        assert_eq!(by_dir.description, "By dir");

        let _ = std::fs::remove_dir_all(&root);
    }
}
