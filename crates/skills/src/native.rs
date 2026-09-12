//! 原生 skill 发现 provider：扫描 `<config_dir>/skills`（user）与 `.agent/skills`（project walkup）。

use std::path::{Path, PathBuf};

use agent_core::{Skill, SkillError, SkillLevel, SkillLoadOptions, SkillProvider, config_dir};
use async_trait::async_trait;

use crate::scan::scan_dir;

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
    pub const fn new(cwd: PathBuf) -> Self {
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
            out.extend(scan_dir(
                &cfg.join("skills"),
                SkillLevel::User,
                PROVIDER_ID,
            )?);
        }
        // project: 自 cwd 向上 walkup 的 .agent/skills
        for dir in project_skill_dirs(&self.cwd) {
            out.extend(scan_dir(&dir, SkillLevel::Project, PROVIDER_ID)?);
        }
        // 自定义目录（视作 user）
        for dir in &opts.custom_directories {
            out.extend(scan_dir(dir, SkillLevel::User, PROVIDER_ID)?);
        }
        Ok(out)
    }
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

    /// H22：`enabled: false` 显式停用 / 缺描述 → 发现阶段跳过（对齐 omp requireDescription）。
    #[tokio::test]
    async fn skips_disabled_and_descriptionless_skills() {
        let root = unique_skills_root();
        let disabled = root.join("disabled");
        std::fs::create_dir_all(&disabled).unwrap();
        std::fs::write(
            disabled.join("SKILL.md"),
            "---\nname: disabled\ndescription: nope\nenabled: false\n---\nbody",
        )
        .unwrap();
        let nodesc = root.join("nodesc");
        std::fs::create_dir_all(&nodesc).unwrap();
        std::fs::write(nodesc.join("SKILL.md"), "---\nname: nodesc\n---\nbody").unwrap();

        let provider = NativeSkillProvider::new(std::env::temp_dir());
        let opts = SkillLoadOptions {
            enabled: true,
            custom_directories: vec![root.clone()],
            ..Default::default()
        };
        let skills = provider.discover(&opts).await.unwrap();
        assert!(
            skills.iter().all(|s| s.name != "disabled"),
            "enabled: false 的 skill 不应被发现"
        );
        assert!(
            skills.iter().all(|s| s.name != "nodesc"),
            "缺描述的 skill 不应被发现"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
