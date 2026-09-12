//! 极简 frontmatter 解析（TTSR 规则文件用，零依赖）。
//!
//! 支持语法（对齐 oh-my-pi 规则 frontmatter 子集）：
//! - `key: value`（标量）
//! - `key: "quoted value"`（去引号）
//! - `key: [a, b, c]`（内联数组）
//! - `key:` + 缩进 `- item`（块数组）
//! - 布尔标量 `true` / `false`（以字符串保留，由调用方解释）
//!
//! 解析失败的行被宽松忽略（未知键不报错）；frontmatter 缺失或未闭合时返回空字段集，
//! 整个文件按正文处理。

use std::collections::BTreeMap;

/// 解析 frontmatter 与正文。返回 `(字段多值表, 正文)`。
///
/// frontmatter 必须是文件开头的 `---` 行起始、以 `---`（或 `...`）行闭合的块；
/// 未闭合时 `---` 之后的全部内容按正文处理。
pub fn parse_frontmatter(content: &str) -> (BTreeMap<String, Vec<String>>, &str) {
    let mut fields = BTreeMap::new();
    let body = match split_frontmatter(content) {
        Some((fm, body)) => {
            parse_fields(fm, &mut fields);
            body
        }
        None => content,
    };
    (fields, body)
}

/// 分离 frontmatter 与正文。返回 `(frontmatter 文本, 正文)`。
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let mut lines = content.lines();
    let first = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    // 找闭合行（`---` 或 `...`）。
    let mut end: Option<usize> = None;
    for (i, line) in content.lines().enumerate().skip(1) {
        let t = line.trim_end();
        if t == "---" || t == "..." {
            end = Some(i);
            break;
        }
    }
    let Some(end) = end else {
        return None; // 未闭合：按无 frontmatter 处理
    };
    // 闭合围栏行的字节起点（fm 不含围栏本身）。
    let mut fence_offset = 0usize;
    for (i, line) in content.lines().enumerate() {
        if i == end {
            break;
        }
        fence_offset += line.len() + 1; // +1 换行符
    }
    fence_offset = fence_offset.min(content.len());
    let fm_start = content.find('\n').map_or(content.len(), |p| p + 1);
    if fm_start > fence_offset {
        return None;
    }
    let body = &content[fence_offset..];
    let body = body
        .strip_prefix("---")
        .or_else(|| body.strip_prefix("..."))
        .unwrap_or(body);
    let body = body.strip_prefix('\n').unwrap_or(body);
    let fm = &content[fm_start..fence_offset];
    Some((fm, body))
}

/// 逐行解析 frontmatter 字段。
fn parse_fields(fm: &str, out: &mut BTreeMap<String, Vec<String>>) {
    let lines: Vec<&str> = fm.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_end();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let Some((key, rest)) = trimmed.split_once(':') else {
            i += 1;
            continue;
        };
        let key = key.trim().to_string();
        if key.is_empty() {
            i += 1;
            continue;
        }
        let value = rest.trim();
        if value.is_empty() {
            // 块数组：后续 `- item` 缩进行。
            let items = collect_block_items(&lines, &mut i);
            if !items.is_empty() {
                out.entry(key).or_default().extend(items);
            }
            i += 1;
            continue;
        }
        if let Some(items) = parse_inline_array(value) {
            out.entry(key).or_default().extend(items);
        } else {
            out.entry(key).or_default().push(unquote(value));
        }
        i += 1;
    }
}

/// 收集 `key:` 之后的块数组项（`- item`），并推进游标（停在非项行）。
fn collect_block_items(lines: &[&str], i: &mut usize) -> Vec<String> {
    let mut items = Vec::new();
    let mut j = *i + 1;
    while j < lines.len() {
        let line = lines[j].trim();
        if let Some(rest) = line.strip_prefix('-') {
            items.push(unquote(rest.trim()));
            j += 1;
        } else if line.is_empty() {
            j += 1;
        } else {
            break;
        }
    }
    *i = j.saturating_sub(1);
    items
}

/// 解析内联数组 `[a, b, c]`；非数组返回 `None`。
fn parse_inline_array(value: &str) -> Option<Vec<String>> {
    let v = value.trim();
    if !v.starts_with('[') || !v.ends_with(']') {
        return None;
    }
    let inner = &v[1..v.len() - 1];
    if inner.trim().is_empty() {
        return Some(Vec::new());
    }
    Some(inner.split(',').map(|s| unquote(s.trim())).collect())
}

/// 去除首尾配对的引号（单/双）并按 YAML 语义解转义。
///
/// - 双引号：`\\` → `\`、`\"` → `"`、`\n`/`\t`/`\r` 转义为控制字符；
///   未知转义（如正则里的 `\b`/`\d`）**原样保留**（宽松：宁可让正则看到反斜杠，
///   也不吞字符）。omp 的规则文件普遍写成 `condition: "import\\("`，即 YAML
///   双引号里的 `\\` 表示正则的反斜杠——不解转义会让正则编译失败（H44 内置规则集实测）。
/// - 单引号：YAML 里 `''` 表示一个字面单引号，其余原样。
fn unquote(s: &str) -> String {
    let t = s.trim();
    let bytes = t.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        return unescape_double_quoted(&t[1..t.len() - 1]);
    }
    if bytes.len() >= 2 && bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'' {
        return t[1..t.len() - 1].replace("''", "'");
    }
    t.to_string()
}

/// YAML 双引号标量转义（未知转义原样保留）。
fn unescape_double_quoted(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('/') => out.push('/'),
            Some('0') => out.push('\0'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H44：YAML 双引号标量必须解转义（omp 内置规则写 `condition: "import\\("`）。
    #[test]
    fn double_quoted_scalars_unescape_yaml_escapes() {
        let content =
            "---\ncondition: \"import\\\\(\"\nother: \"\\\\bfoo\\\\b\"\nsingle: 'it''s'\n---\nbody";
        let (fields, _) = parse_frontmatter(content);
        assert_eq!(fields["condition"], vec!["import\\(".to_string()]);
        // 未知转义（正则 `\b`）原样保留。
        assert_eq!(fields["other"], vec!["\\bfoo\\b".to_string()]);
        assert_eq!(fields["single"], vec!["it's".to_string()]);
    }

    #[test]
    fn parses_scalar_and_array_fields() {
        let content = r#"---
name: box-leak
condition: ["(?i)Box::leak", "Box::from_raw"]
interruptMode: always
---
规则正文
"#;
        let (fields, body) = parse_frontmatter(content);
        assert_eq!(fields["name"], vec!["box-leak"]);
        assert_eq!(fields["condition"], vec!["(?i)Box::leak", "Box::from_raw"]);
        assert_eq!(fields["interruptMode"], vec!["always"]);
        assert!(body.contains("规则正文"));
    }

    #[test]
    fn parses_block_array() {
        let content = "---\nscope:\n  - text\n  - tool:write\n---\nbody";
        let (fields, body) = parse_frontmatter(content);
        assert_eq!(fields["scope"], vec!["text", "tool:write"]);
        assert_eq!(body, "body");
    }

    #[test]
    fn handles_quoted_values_and_comments() {
        let content = "---\ndescription: \"禁 Box::leak\"\n# comment\nrepeat: once\n---\n";
        let (fields, _) = parse_frontmatter(content);
        assert_eq!(fields["description"], vec!["禁 Box::leak"]);
        assert_eq!(fields["repeat"], vec!["once"]);
    }

    #[test]
    fn no_frontmatter_means_all_body() {
        let content = "plain markdown body\n---\nnot frontmatter";
        let (fields, body) = parse_frontmatter(content);
        assert!(fields.is_empty());
        assert_eq!(body, content);
    }

    #[test]
    fn unterminated_frontmatter_falls_back_to_body() {
        let content = "---\nname: x\nno closing fence";
        let (fields, body) = parse_frontmatter(content);
        assert!(fields.is_empty());
        assert_eq!(body, content);
    }
}
