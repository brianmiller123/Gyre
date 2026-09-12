//! 通用 skill 目录扫描：`<dir>/<name>/SKILL.md`（非递归），供各 provider 复用。
//!
//! 解析复用 [`crate::frontmatter::parse_skill_file`]：缺 name 回退目录名、未识别键忽略。
//! **严格校验**（对齐 oh-my-pi `discovery/helpers.ts`，H22）：
//! - `enabled: false` → 跳过（显式停用）；
//! - 描述缺失/空 → 跳过并告警（omp `requireDescription: true`；无描述的 skill
//!   无法被模型判断何时加载，进 prompt 只会浪费 token）。
//!
//! 单个文件损坏只告警跳过，不拖垮同目录其它 skill。
//!
//! ### 关于「必需描述」的行为差异
//! 本仓库此前的宽松行为是「缺描述则渲染为无冒号条目」。现在改为跳过：与 omp 一致，
//! 且避免 `<skills>` 段出现无法判读的裸名称。需要临时停用而不删除目录时用 `enabled: false`。

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
            Ok(Some(skill)) => out.push(skill),
            // 严格校验拒绝（enabled:false / 缺描述）——非错误，静默跳过。
            Ok(None) => {}
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
///
/// 返回 `Ok(None)` 表示文件合法但被「严格校验」拒绝（`enabled: false` 或缺描述）。
pub(crate) fn load_skill_file(
    skill_md: &Path,
    level: SkillLevel,
    provider_id: &str,
) -> Result<Option<Skill>, SkillError> {
    let content = std::fs::read_to_string(skill_md)?;
    let fm = parse_skill_file(&content).frontmatter;
    // 显式停用（对齐 omp `frontmatter.enabled === false` → 不加载）。
    if fm.enabled == Some(false) {
        return Ok(None);
    }
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
    // 描述必需（omp `requireDescription: true`）：缺失/空 → 拒绝该 skill。
    let description = fm
        .description
        .map(|d| d.trim().to_string())
        .unwrap_or_default();
    if description.is_empty() {
        return Ok(None);
    }
    let base_dir = skill_md.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok(Some(Skill {
        name,
        description,
        file_path: skill_md.to_path_buf(),
        base_dir,
        source: SkillSource {
            provider: provider_id.to_string(),
            level,
        },
        hide: fm.hide,
        modes: fm.modes,
    }))
}
