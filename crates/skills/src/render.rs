//! system prompt `<skills>` 段渲染。

use agent_core::Skill;

/// 渲染 system prompt 的 skills 列表段。
///
/// 列出每个 skill 的 name + description，并提示模型用 `read_file skill://<name>` 按需加载。
/// 空列表返回 `None`（不修改 prompt，保持稳定前缀指纹）。
#[must_use]
pub fn render_skills_section(skills: &[&Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from(
        "\n\n<skills>\n以下 skill 可用，需要时用 read_file skill://<name> 加载完整说明\
         （分体式 skill 的附属文件用 skill://<name>/<相对路径> 读取）：\n",
    );
    for skill in skills {
        let name = one_line(&skill.name);
        let desc = one_line(&skill.description);
        if desc.is_empty() {
            out.push_str(&format!("- {name}\n"));
        } else {
            out.push_str(&format!("- {name}: {desc}\n"));
        }
    }
    out.push_str("</skills>\n");
    Some(out)
}

/// 折叠为单行并去首尾空白（防多行注入破坏 `<skills>` 段结构）。
fn one_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{SkillLevel, SkillSource};
    use std::path::PathBuf;

    fn sk(name: &str, desc: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            file_path: PathBuf::from("/x/SKILL.md"),
            base_dir: PathBuf::from("/x"),
            source: SkillSource {
                provider: "native".to_string(),
                level: SkillLevel::User,
            },
            hide: false,
            modes: None,
        }
    }

    #[test]
    fn empty_returns_none() {
        assert!(render_skills_section(&[]).is_none());
    }

    #[test]
    fn renders_entries() {
        let a = sk("pdf", "PDF parsing");
        let b = sk("browser", "Browser automation");
        let out = render_skills_section(&[&a, &b]).unwrap();
        assert!(out.contains("<skills>"));
        assert!(out.contains("- pdf: PDF parsing"));
        assert!(out.contains("- browser: Browser automation"));
        assert!(out.contains("skill://<name>"));
        assert!(out.contains("</skills>"));
    }

    #[test]
    fn collapses_multiline_description() {
        let a = sk("x", "line1\nline2");
        let out = render_skills_section(&[&a]).unwrap();
        assert!(out.contains("- x: line1 line2"));
    }

    #[test]
    fn empty_description_omits_colon() {
        let a = sk("x", "");
        let out = render_skills_section(&[&a]).unwrap();
        assert!(out.contains("- x\n"));
    }
}
