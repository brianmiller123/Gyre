//! 通用 Markdown frontmatter 解析（H18 引入，供 task agent 定义复用）。
//!
//! 语法与仓库既有 markdown 资源一致（零依赖，仅支持所需子集）：
//! ```text
//! ---
//! name: scout
//! description: Read-only exploration agent
//! tools: [read_file, grep, glob]      # 内联数组
//! model: "@smol"
//! spawns: false                        # 标量（bool/字符串同一解析路径）
//! ---
//! 正文…
//! ```
//! 另支持块数组（`key:` 后跟缩进 `- item`）与 `key: a, b, c` 逗号列表
//! （[`FrontmatterValue::List`] 的两种来源）。未识别键不报错（宽松解析）；
//! frontmatter 未闭合或缺失时返回空字段表 + 原文正文。

use std::collections::BTreeMap;

/// 单个 frontmatter 值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontmatterValue {
    /// 标量（`key: value` / `key: "value"`）。
    Scalar(String),
    /// 列表（内联 `[a, b]`、块数组或逗号分隔标量）。
    List(Vec<String>),
}

impl FrontmatterValue {
    /// 取标量文本（列表取首个元素；空返回 `None`）。
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Scalar(s) => Some(s.as_str()),
            Self::List(items) => items.first().map(String::as_str),
        }
    }

    /// 取列表（标量按逗号切分后过滤空项）。
    #[must_use]
    pub fn as_list(&self) -> Vec<String> {
        match self {
            Self::Scalar(s) => split_csv(s),
            Self::List(items) => items.clone(),
        }
    }

    /// 标量的宽松布尔解析（`true/yes/on/1`；未知返回 `None`）。
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        let raw = self.as_str()?.trim().to_ascii_lowercase();
        match raw.as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        }
    }
}

/// 解析出的 frontmatter 字段（保持文件内出现顺序无关的稳定顺序）。
pub type FrontmatterFields = BTreeMap<String, FrontmatterValue>;

/// 解析 Markdown：返回 `(字段表, 正文)`。
///
/// - 首部 `---` 与闭合 `---`/`...` 之间为 frontmatter；
/// - 未闭合 → 视为无 frontmatter（正文 = 原文，字段表为空）；
/// - BOM 已处理。
#[must_use]
pub fn parse_frontmatter(content: &str) -> (FrontmatterFields, String) {
    let trimmed = content.strip_prefix('\u{feff}').unwrap_or(content);
    let lines: Vec<&str> = trimmed.lines().collect();
    let mut idx = 0;
    while idx < lines.len() && lines[idx].trim().is_empty() {
        idx += 1;
    }
    if idx >= lines.len() || lines[idx].trim() != "---" {
        return (FrontmatterFields::new(), content.to_string());
    }
    let start = idx + 1;
    let mut end = None;
    for (k, line) in lines[start..].iter().enumerate() {
        let t = line.trim();
        if t == "---" || t == "..." {
            end = Some(start + k);
            break;
        }
    }
    let Some(end) = end else {
        return (FrontmatterFields::new(), content.to_string());
    };
    let fields = parse_fields(&lines[start..end]);
    let body = lines
        .get(end + 1..)
        .map_or_else(String::new, |s| s.join("\n"));
    (fields, body.trim_start_matches('\n').to_string())
}

/// 逐行解析字段（含块数组）。
fn parse_fields(lines: &[&str]) -> FrontmatterFields {
    let mut out = FrontmatterFields::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            i += 1;
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if value.is_empty() {
            // 块数组：收集后续 `- item`。
            let (items, next) = collect_block_items(lines, i + 1);
            i = next;
            if items.is_empty() {
                out.insert(key, FrontmatterValue::Scalar(String::new()));
            } else {
                out.insert(key, FrontmatterValue::List(items));
            }
            continue;
        }
        if value.starts_with('[') && value.contains(']') {
            out.insert(key, FrontmatterValue::List(parse_inline_array(value)));
        } else {
            out.insert(key, FrontmatterValue::Scalar(unquote(value)));
        }
        i += 1;
    }
    out
}

/// 收集 `key:` 之后的块数组项（`- item`），返回 `(items, 下一行索引)`。
fn collect_block_items(lines: &[&str], mut i: usize) -> (Vec<String>, usize) {
    let mut items = Vec::new();
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('-') {
            items.push(unquote(rest.trim()));
            i += 1;
        } else {
            break;
        }
    }
    (items, i)
}

/// 解析内联数组 `[a, b, c]`。
fn parse_inline_array(value: &str) -> Vec<String> {
    let start = value.find('[').map_or(0, |p| p + 1);
    let end = value.rfind(']').unwrap_or(value.len());
    value[start..end]
        .split(',')
        .map(|s| unquote(s.trim()))
        .filter(|s| !s.is_empty())
        .collect()
}

/// 去首尾引号。
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

/// 逗号分隔标量 → 列表（`a, b, c`）。
fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| unquote(p.trim()))
        .filter(|p| !p.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars_inline_arrays_and_blocks() {
        let src = "---\nname: scout\ndescription: \"Read-only: explore\"\ntools: [read_file, grep]\nmodes:\n  - code\n  - plan\nspawns: false\n---\n正文\n第二行";
        let (fields, body) = parse_frontmatter(src);
        assert_eq!(fields["name"].as_str(), Some("scout"));
        assert_eq!(fields["description"].as_str(), Some("Read-only: explore"));
        assert_eq!(fields["tools"].as_list(), vec!["read_file", "grep"]);
        assert_eq!(fields["modes"].as_list(), vec!["code", "plan"]);
        assert_eq!(fields["spawns"].as_bool(), Some(false));
        assert_eq!(body, "正文\n第二行");
    }

    #[test]
    fn comma_scalar_is_treated_as_list() {
        let (fields, _) = parse_frontmatter("---\ntools: read_file, grep , glob\n---\n");
        assert_eq!(fields["tools"].as_list(), vec!["read_file", "grep", "glob"]);
    }

    #[test]
    fn missing_or_unclosed_frontmatter_yields_empty_fields() {
        let (fields, body) = parse_frontmatter("no frontmatter here");
        assert!(fields.is_empty());
        assert_eq!(body, "no frontmatter here");

        let (fields, body) = parse_frontmatter("---\nname: x\nno close");
        assert!(fields.is_empty());
        assert!(body.starts_with("---"));
    }

    #[test]
    fn unknown_keys_are_ignored_and_comments_skipped() {
        let (fields, _) = parse_frontmatter("---\n# comment\nname: a\nweird: b\n---\n");
        assert_eq!(fields["name"].as_str(), Some("a"));
        assert_eq!(fields["weird"].as_str(), Some("b"));
    }
}
