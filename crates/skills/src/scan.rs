//! 通用 skill 目录扫描：`<dir>/<name>/SKILL.md`（非递归），供各 provider 复用。
//!
//! 解析复用 [`crate::frontmatter::parse_skill_file`] 的宽松规则（缺 name 回退目录名、
//! 缺 description 置空、未识别键忽略）；严格校验（agentskills.io 命名规范等）为后续项。
//! 单个文件损坏只告警跳过，不拖垮同目录其它 skill。

use std::path::Path;

use agent_core::{Skill, SkillError, SkillLevel, SkillSource};

use crate::frontmatter::parse_skill_file;

/// 扫描一个 skills 根目录下的 `<name>/SKILL.md`（非递归）。
///
/// 目录不存在（NotFound）返回空集；隐藏项（`.` 开头）跳过；符号链接目录跟随
/// （`is_dir` 语义，与 oh-my-pi `scanSkillsFromDir` 一致）。
pub(crate) fn scan_dir(
    dir: &Path,
    level: SkillLevel,
    provider_id: &str,
) -> Result<Vec<Skill>, SkillError> {
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
        match load_skill_file(&skill_md, level, provider_id) {
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
pub(crate) fn load_skill_file(
    skill_md: &Path,
    level: SkillLevel,
    provider_id: &str,
) -> Result<Skill, SkillError> {
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
        .map_or_else(|| dir_name.to_string(), str::to_string);
    let base_dir = skill_md.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok(Skill {
        name,
        description: fm.description.unwrap_or_default(),
        file_path: skill_md.to_path_buf(),
        base_dir,
        source: SkillSource {
            provider: provider_id.to_string(),
            level,
        },
        hide: fm.hide,
        modes: fm.modes,
    })
}
