//! Skill 聚合、去重、过滤与 `skill://` URL 解析。

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use agent_core::{
    Mode, Skill, SkillError, SkillLoadOptions, SkillProvider, SkillResolver,
};

use crate::native::NativeSkillProvider;

/// Skill 注册表：聚合多 provider，priority first-wins 去重 + glob 过滤。
pub struct SkillRegistry {
    providers: Vec<Arc<dyn SkillProvider>>,
}

impl SkillRegistry {
    /// 仅 native provider 的默认构造。
    #[must_use]
    pub fn native(cwd: PathBuf) -> Self {
        Self::with_providers(vec![Arc::new(NativeSkillProvider::new(cwd))])
    }

    /// 自定义 provider 集合（按 [`SkillProvider::priority`] 降序稳定排序）。
    #[must_use]
    pub fn with_providers(providers: Vec<Arc<dyn SkillProvider>>) -> Self {
        let mut providers = providers;
        providers.sort_by_key(|p| std::cmp::Reverse(p.priority()));
        Self { providers }
    }

    /// 加载并去重、过滤。
    ///
    /// # Errors
    /// 任一 provider discover 失败时返回 [`SkillError`]。
    pub async fn load(&self, opts: &SkillLoadOptions) -> Result<SkillCatalog, SkillError> {
        if !opts.enabled {
            return Ok(SkillCatalog::default());
        }
        let mut raw = Vec::new();
        for p in &self.providers {
            raw.extend(p.discover(opts).await?);
        }
        let ignored = compile_globs(&opts.ignored);
        let included = compile_globs(&opts.included);

        let mut skills: Vec<Skill> = Vec::new();
        let mut seen_names: HashSet<String> = HashSet::new();
        let mut seen_paths: HashSet<PathBuf> = HashSet::new();
        let mut warnings: Vec<String> = Vec::new();

        for s in raw {
            if matches_any(&s.name, &ignored) {
                continue;
            }
            if !included.is_empty() && !matches_any(&s.name, &included) {
                continue;
            }
            // realpath 去重（symlink 安全）
            let rp = std::fs::canonicalize(&s.file_path).unwrap_or_else(|_| s.file_path.clone());
            if !seen_paths.insert(rp) {
                continue;
            }
            // name 去重 first-wins
            if !seen_names.insert(s.name.clone()) {
                warnings.push(format!(
                    "name collision: \"{}\" 已加载，跳过重复 ({})",
                    s.name,
                    s.file_path.display()
                ));
                continue;
            }
            skills.push(s);
        }
        // 稳定排序：name 大小写不敏感
        skills.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(SkillCatalog { skills, warnings })
    }
}

/// 已加载的 skill 集合。
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    /// 去重排序后的 skill 列表。
    pub skills: Vec<Skill>,
    /// 加载过程中的告警（如 name collision）。
    pub warnings: Vec<String>,
}

impl SkillCatalog {
    /// 返回适合注入 system prompt 的 skill 引用（非 hide + 命中 mode + read 可用）。
    #[must_use]
    pub fn for_prompt(&self, mode: Mode, has_read: bool) -> Vec<&Skill> {
        if !has_read {
            return Vec::new();
        }
        self.skills
            .iter()
            .filter(|s| !s.hide && s.available_for_mode(mode))
            .collect()
    }

    /// 按精确名查找。
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

impl SkillResolver for SkillCatalog {
    fn resolve(&self, url: &str) -> Result<PathBuf, SkillError> {
        resolve_skill_url(url, &self.skills)
    }
}

/// 解析 `skill://` URL 为磁盘路径。
///
/// - `skill://<name>` → 该 skill 的 `SKILL.md`
/// - `skill://<name>/<rel>` → skill 目录内相对路径
///
/// # Errors
/// 缺名 / 未知名 / 绝对路径 / 路径遍历时返回 [`SkillError`]。
pub fn resolve_skill_url(url: &str, skills: &[Skill]) -> Result<PathBuf, SkillError> {
    let rest = url.strip_prefix("skill://").unwrap_or(url);
    let (name, path_part) = match rest.find('/') {
        Some(idx) => (&rest[..idx], Some(&rest[idx + 1..])),
        None => (rest, None),
    };
    let name = name.trim();
    if name.is_empty() {
        return Err(SkillError::MissingName);
    }
    let skill = skills.iter().find(|s| s.name == name).ok_or_else(|| {
        let available = skills
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        SkillError::Unknown {
            name: name.to_string(),
            available,
        }
    })?;
    match path_part {
        None => Ok(skill.file_path.clone()),
        Some(rel) => {
            let decoded = percent_decode(rel);
            validate_relative_path(&decoded)?;
            Ok(skill.base_dir.join(&decoded))
        }
    }
}

/// 校验相对路径：拒绝绝对路径与 `..` 遍历。
fn validate_relative_path(rel: &str) -> Result<(), SkillError> {
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(SkillError::AbsolutePath);
    }
    for comp in p.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => return Err(SkillError::Traversal),
            Component::RootDir | Component::Prefix(_) => return Err(SkillError::AbsolutePath),
        }
    }
    Ok(())
}

/// 极简 percent-decode（处理 `%XX`）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn compile_globs(patterns: &[String]) -> Vec<glob::Pattern> {
    patterns.iter().filter_map(|p| glob::Pattern::new(p).ok()).collect()
}

fn matches_any(name: &str, patterns: &[glob::Pattern]) -> bool {
    patterns.iter().any(|p| p.matches(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{SkillLevel, SkillSource};

    fn skill(name: &str, base: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: "d".to_string(),
            file_path: PathBuf::from(base).join("SKILL.md"),
            base_dir: PathBuf::from(base),
            source: SkillSource {
                provider: "native".to_string(),
                level: SkillLevel::User,
            },
            hide: false,
            modes: None,
        }
    }

    #[test]
    fn resolve_skill_root() {
        let s = skill("pdf", "/skills/pdf");
        let p = resolve_skill_url("skill://pdf", &[s]).unwrap();
        assert_eq!(p, PathBuf::from("/skills/pdf/SKILL.md"));
    }

    #[test]
    fn resolve_skill_subpath() {
        let s = skill("pdf", "/skills/pdf");
        let p = resolve_skill_url("skill://pdf/references/tables.md", &[s]).unwrap();
        assert_eq!(p, PathBuf::from("/skills/pdf/references/tables.md"));
    }

    #[test]
    fn resolve_skill_unknown() {
        let s = skill("pdf", "/skills/pdf");
        let err = resolve_skill_url("skill://nope", &[s]).unwrap_err();
        assert!(matches!(err, SkillError::Unknown { .. }));
    }

    #[test]
    fn resolve_skill_traversal_rejected() {
        let s = skill("pdf", "/skills/pdf");
        let err = resolve_skill_url("skill://pdf/../../etc/passwd", &[s]).unwrap_err();
        assert!(matches!(err, SkillError::Traversal));
    }

    #[test]
    fn resolve_skill_missing_name() {
        let err = resolve_skill_url("skill://", &[]).unwrap_err();
        assert!(matches!(err, SkillError::MissingName));
    }

    #[test]
    fn for_prompt_filters_hide_and_mode() {
        let mut a = skill("a", "/x/a");
        a.hide = true;
        let mut b = skill("b", "/x/b");
        b.modes = Some(vec![Mode::Debug]);
        let cat = SkillCatalog {
            skills: vec![a, b],
            warnings: vec![],
        };
        // Code 模式：a 被 hide 排除，b 被 mode 排除
        assert!(cat.for_prompt(Mode::Code, true).is_empty());
        // Debug 模式 + read：b 可见
        let visible = cat.for_prompt(Mode::Debug, true);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].name, "b");
        // 无 read 工具：全空
        assert!(cat.for_prompt(Mode::Debug, false).is_empty());
    }

    #[test]
    fn find_by_name() {
        let cat = SkillCatalog {
            skills: vec![skill("pdf", "/x/pdf")],
            warnings: vec![],
        };
        assert!(cat.find("pdf").is_some());
        assert!(cat.find("nope").is_none());
    }

    #[test]
    fn resolver_impl_via_catalog() {
        let cat = SkillCatalog {
            skills: vec![skill("pdf", "/x/pdf")],
            warnings: vec![],
        };
        let p = SkillResolver::resolve(&cat, "skill://pdf").unwrap();
        assert_eq!(p, PathBuf::from("/x/pdf/SKILL.md"));
    }
}
