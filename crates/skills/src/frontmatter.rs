//! 极简 YAML frontmatter 解析（仅支持 skill 所需字段，零依赖）。
//!
//! 支持语法：
//! - `key: value`（标量）
//! - `key: "quoted value"`
//! - `key: [a, b, c]`（内联数组）
//! - `key:` + 缩进 `- item`（块数组）
//! - bool: `true` / `false`
//!
//! 未识别的键被忽略（宽松解析）；frontmatter 未闭合或缺失时按无 frontmatter 处理。

use agent_core::Mode;

/// 解析出的 frontmatter。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Frontmatter {
    /// skill 名（缺省取目录名）。
    pub name: Option<String>,
    /// 描述（native provider 必填）。
    pub description: Option<String>,
    /// 是否对模型隐藏（frontmatter `hide` 或 `disable-model-invocation` 归一）。
    pub hide: bool,
    /// 限定模式（`None` 或空 = 所有）。
    pub modes: Option<Vec<Mode>>,
    /// 文件 glob（保留，远期按文件类型自动激活）。
    pub globs: Option<Vec<String>>,
}

/// 解析结果。
#[derive(Debug, Clone)]
pub(crate) struct ParsedSkillFile {
    /// frontmatter。
    pub frontmatter: Frontmatter,
    /// 正文（已剥离 frontmatter）；保留供未来 `/skill:<name>` 命令注入正文使用。
    #[allow(dead_code)]
    pub body: String,
}

/// 解析 SKILL.md 内容。
pub(crate) fn parse_skill_file(content: &str) -> ParsedSkillFile {
    let (fm, body) = split_frontmatter(content);
    let frontmatter = parse_fields(fm.as_deref());
    ParsedSkillFile { frontmatter, body }
}

/// 分离 frontmatter 与正文。返回 `(frontmatter 文本, 正文)`。
fn split_frontmatter(content: &str) -> (Option<String>, String) {
    let trimmed = content.strip_prefix('\u{feff}').unwrap_or(content);
    let lines: Vec<&str> = trimmed.lines().collect();
    // 跳过前导空行，定位首个非空行
    let mut idx = 0;
    while idx < lines.len() && lines[idx].trim().is_empty() {
        idx += 1;
    }
    if idx >= lines.len() || lines[idx].trim() != "---" {
        return (None, content.to_string());
    }
    let start = idx + 1;
    // 找结束分隔符（`---` 或 `...`）
    let mut end = None;
    for (k, line) in lines[start..].iter().enumerate() {
        let t = line.trim();
        if t == "---" || t == "..." {
            end = Some(start + k);
            break;
        }
    }
    let Some(end) = end else {
        return (None, content.to_string());
    };
    let fm = lines[start..end].join("\n");
    let body = lines
        .get(end + 1..)
        .map_or_else(String::new, |s| s.join("\n"));
    let body = body.trim_start_matches('\n').to_string();
    (Some(fm), body)
}

/// 逐行解析 frontmatter 字段。
fn parse_fields(fm: Option<&str>) -> Frontmatter {
    let mut out = Frontmatter::default();
    let Some(fm) = fm else { return out };
    let lines: Vec<&str> = fm.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            i += 1;
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if value.is_empty() {
            // 块数组：收集后续 `- item`
            let items = collect_block_items(&lines, &mut i);
            apply_array(&mut out, key, items);
        } else if value.starts_with('[') && value.contains(']') {
            let items = parse_inline_array(value);
            apply_array(&mut out, key, items);
            i += 1;
        } else {
            apply_scalar(&mut out, key, &unquote(value));
            i += 1;
        }
    }
    out
}

/// 收集 `key:` 之后的块数组项（`- item`），并推进游标。
fn collect_block_items(lines: &[&str], i: &mut usize) -> Vec<String> {
    let mut items = Vec::new();
    *i += 1;
    while *i < lines.len() {
        let trimmed = lines[*i].trim();
        if trimmed.is_empty() {
            *i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('-') {
            items.push(unquote(rest.trim()));
            *i += 1;
        } else {
            break;
        }
    }
    items
}

/// 解析内联数组 `[a, b, c]`。
fn parse_inline_array(value: &str) -> Vec<String> {
    let start = value.find('[').map(|p| p + 1).unwrap_or(0);
    let end = value.rfind(']').unwrap_or(value.len());
    let inner = &value[start..end];
    inner
        .split(',')
        .map(|s| unquote(s.trim()))
        .filter(|s| !s.is_empty())
        .collect()
}

/// 去除首尾引号。
fn unquote(s: &str) -> String {
    let s = s.trim();
    let n = s.len();
    if n >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..n - 1].to_string()
    } else {
        s.to_string()
    }
}

fn apply_scalar(out: &mut Frontmatter, key: &str, value: &str) {
    match key {
        "name" => out.name = Some(value.to_string()),
        "description" => out.description = Some(value.to_string()),
        "hide" => out.hide = value.eq_ignore_ascii_case("true"),
        "disable-model-invocation" => {
            if value.eq_ignore_ascii_case("true") {
                out.hide = true;
            }
        }
        _ => {}
    }
}

fn apply_array(out: &mut Frontmatter, key: &str, items: Vec<String>) {
    match key {
        "modes" => {
            let modes: Vec<Mode> = items.iter().filter_map(|s| parse_mode(s)).collect();
            if !modes.is_empty() {
                out.modes = Some(modes);
            }
        }
        "globs" => {
            if !items.is_empty() {
                out.globs = Some(items);
            }
        }
        _ => {}
    }
}

fn parse_mode(s: &str) -> Option<Mode> {
    match s.trim() {
        "code" => Some(Mode::Code),
        "architect" => Some(Mode::Architect),
        "ask" => Some(Mode::Ask),
        "debug" => Some(Mode::Debug),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_frontmatter() {
        let content = "---\nname: my-skill\ndescription: A skill\n---\nbody text";
        let parsed = parse_skill_file(content);
        assert_eq!(parsed.frontmatter.name.as_deref(), Some("my-skill"));
        assert_eq!(parsed.frontmatter.description.as_deref(), Some("A skill"));
        assert_eq!(parsed.body, "body text");
    }

    #[test]
    fn parses_quoted_description_with_colon() {
        let content = "---\ndescription: \"Has: colon\"\n---\nbody";
        let parsed = parse_skill_file(content);
        assert_eq!(
            parsed.frontmatter.description.as_deref(),
            Some("Has: colon")
        );
    }

    #[test]
    fn parses_inline_modes_array() {
        let content = "---\nmodes: [code, debug]\n---\nbody";
        let parsed = parse_skill_file(content);
        assert_eq!(
            parsed.frontmatter.modes,
            Some(vec![Mode::Code, Mode::Debug])
        );
    }

    #[test]
    fn parses_block_globs_array() {
        let content = "---\nglobs:\n  - \"**/*.rs\"\n  - \"**/*.toml\"\n---\nbody";
        let parsed = parse_skill_file(content);
        assert_eq!(
            parsed.frontmatter.globs,
            Some(vec!["**/*.rs".to_string(), "**/*.toml".to_string()])
        );
    }

    #[test]
    fn hide_and_disable_model_invocation() {
        assert!(parse_skill_file("---\nhide: true\n---\nx").frontmatter.hide);
        assert!(
            parse_skill_file("---\ndisable-model-invocation: true\n---\nx")
                .frontmatter
                .hide
        );
        assert!(
            !parse_skill_file("---\nhide: false\n---\nx")
                .frontmatter
                .hide
        );
    }

    #[test]
    fn no_frontmatter_returns_all_as_body() {
        let content = "just body, no fm";
        let parsed = parse_skill_file(content);
        assert!(parsed.frontmatter.name.is_none());
        assert_eq!(parsed.body, content);
    }

    #[test]
    fn unclosed_frontmatter_treated_as_none() {
        let content = "---\nname: x\nbody without close";
        let parsed = parse_skill_file(content);
        assert!(parsed.frontmatter.name.is_none());
    }
}
