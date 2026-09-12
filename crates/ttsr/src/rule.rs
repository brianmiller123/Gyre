//! TTSR 规则模型：frontmatter 解析 → 编译（regex / ast-grep / glob）。
//!
//! 移植 oh-my-pi `capability/rule.ts` + `capability/rule-buckets.ts` 的 TTSR 相关子集。
//! 规则文件为 Markdown：frontmatter 声明触发条件与门控，正文为命中后注入的内容。

use std::collections::BTreeMap;
use std::path::Path;

use globset::{Glob, GlobMatcher};

use crate::frontmatter::parse_frontmatter;

/// 规则作用域（决定在哪些流表面上匹配）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleScope {
    /// 模型文本流（流式中匹配）。
    Text,
    /// 思考（reasoning）流（流式中匹配）。
    Thinking,
    /// 任意工具调用（MessageEnd 快照匹配）。
    AnyTool,
    /// 指定工具（可选路径 glob：`tool:write_file(*.rs)`）。
    Tool {
        /// 工具名（如 `write_file` / `apply_hashline`）。
        name: String,
        /// 路径 glob 模式（空 = 任意路径）；匹配完整路径或 basename。
        globs: Vec<String>,
    },
}

impl RuleScope {
    /// 解析单个作用域 token。
    fn parse(token: &str) -> Result<Self, String> {
        let t = token.trim();
        match t {
            "text" => Ok(Self::Text),
            "thinking" => Ok(Self::Thinking),
            "tool" => Ok(Self::AnyTool),
            _ => {
                let Some(rest) = t.strip_prefix("tool:") else {
                    // glob 简写（如 `*.rs`）：转换为写/编辑工具作用域（omp `isLikelyFileGlob`）。
                    if is_likely_file_glob(t) {
                        return Ok(Self::Tool {
                            name: "write_file".into(),
                            globs: vec![t.to_string()],
                        });
                    }
                    return Err(format!("未知 scope: {t}"));
                };
                // tool:NAME 或 tool:NAME(GLOB)
                if let Some(open) = rest.find('(') {
                    if !rest.ends_with(')') {
                        return Err(format!("scope 括号未闭合: {t}"));
                    }
                    let name = rest[..open].trim().to_string();
                    let glob = rest[open + 1..rest.len() - 1].trim();
                    if name.is_empty() || glob.is_empty() {
                        return Err(format!("scope 格式非法: {t}"));
                    }
                    // 校验 glob 可编译（提前报错，避免运行期静默失效）。
                    compile_glob(glob)?;
                    Ok(Self::Tool {
                        name,
                        globs: vec![glob.to_string()],
                    })
                } else {
                    let name = rest.trim().to_string();
                    if name.is_empty() {
                        return Err(format!("scope 格式非法: {t}"));
                    }
                    Ok(Self::Tool {
                        name,
                        globs: Vec::new(),
                    })
                }
            }
        }
    }
}

/// 判断 token 是否形如文件 glob（含 `*` / `?` 且非 `tool:` 前缀）。
fn is_likely_file_glob(t: &str) -> bool {
    !t.starts_with("tool:") && (t.contains('*') || t.contains('?'))
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher, String> {
    Glob::new(pattern)
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("glob 编译失败 `{pattern}`: {e}"))
}

/// 中断模式（omp `interruptMode`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InterruptMode {
    /// 命中即中断流/丢弃消息并注入重试（默认）。
    #[default]
    Always,
    /// 非打断：工具作用域折进工具结果提醒（文本/思考作用域 v1 按 Always 处理）。
    Never,
}

/// 重复策略（omp `repeatPolicy`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repeat {
    /// 本会话只触发一次（注入状态持久化，压缩/恢复后不重复注入）。
    #[default]
    Once,
    /// 每次命中都触发（不抑制）。
    Always,
}

/// 编译后的 TTSR 规则。
#[derive(Debug, Clone)]
pub struct Rule {
    /// 规则名（frontmatter `name`，缺省取文件名主干）。
    pub name: String,
    /// 描述（可选；供 rulebook 展示）。
    pub description: Option<String>,
    /// 命中后注入的正文（Markdown）。
    pub body: String,
    /// 作用域列表。
    pub scopes: Vec<RuleScope>,
    /// 全局路径门（仅工具作用域有效；文本/思考流无路径上下文，含 globs 时不匹配）。
    pub globs: Vec<String>,
    /// 中断模式。
    pub interrupt_mode: InterruptMode,
    /// 重复策略。
    pub repeat: Repeat,
    /// 编译后的正则条件（OR 语义：任一命中即触发）。
    pub conditions: Vec<regex::Regex>,
    /// ast-grep 结构化条件（工具作用域，对 matcherDigest 做结构匹配）。
    pub ast_condition: Option<String>,
}

impl Rule {
    /// 是否可匹配（至少一个条件）。
    #[must_use]
    pub fn is_matchable(&self) -> bool {
        !self.conditions.is_empty() || self.ast_condition.is_some()
    }

    /// 作用域是否覆盖给定工具（工具名 + 路径）。
    #[must_use]
    pub fn covers_tool(&self, tool: &str, path: Option<&str>) -> bool {
        // 全局路径门：工具作用域受 globs 限制（omp `matchesGlobalPaths`）。
        if !self.globs.is_empty() {
            let Some(p) = path else {
                return false;
            };
            if !glob_matches(&self.globs, p) {
                return false;
            }
        }
        self.scopes.iter().any(|s| match s {
            RuleScope::AnyTool => true,
            RuleScope::Tool { name, globs } => {
                if name != tool {
                    return false;
                }
                if globs.is_empty() {
                    return true;
                }
                let Some(p) = path else {
                    return false;
                };
                glob_matches(globs, p)
            }
            RuleScope::Text | RuleScope::Thinking => false,
        })
    }

    /// 是否覆盖文本流（含全局 globs 门：文本流无路径 → 含 globs 的规则不匹配）。
    #[must_use]
    pub fn covers_stream(&self, stream: &str) -> bool {
        if !self.globs.is_empty() {
            return false;
        }
        self.scopes.iter().any(|s| match s {
            RuleScope::Text => stream == "text",
            RuleScope::Thinking => stream == "thinking",
            RuleScope::AnyTool | RuleScope::Tool { .. } => false,
        })
    }

    /// 正则条件 OR 匹配（对给定文本）。
    #[must_use]
    pub fn matches_condition(&self, text: &str) -> bool {
        self.conditions.iter().any(|re| re.is_match(text))
    }
}

/// 从规则文件内容解析规则。
///
/// `name` 为文件名主干（frontmatter `name` 缺省时使用）。
///
/// # Errors
/// frontmatter 字段非法（scope / glob / regex 编译失败）时返回错误。
pub fn parse_rule(name: &str, content: &str) -> Result<Rule, String> {
    let (fields, body) = parse_frontmatter(content);
    let body = body.trim().to_string();
    if body.is_empty() {
        return Err(format!("规则 {name} 正文为空"));
    }

    let name = first(&fields, "name").unwrap_or(name).to_string();
    let description = first(&fields, "description").map(str::to_string);

    // scope：显式 scope 字段；缺省 text。
    // 兼容 omp 的两种写法：数组（`scope: [text, tool:write_file]`）与**逗号分隔的标量**
    // （`scope: "tool:edit(*.rs), tool:write(*.rs)"`）。标量按**括号深度 0 的逗号**切分，
    // 因此 glob 里的 `{ts,tsx}` 不会被误切（H44：内置规则集依赖该写法）。
    let mut scopes: Vec<RuleScope> = Vec::new();
    if let Some(tokens) = fields.get("scope") {
        for t in tokens {
            for token in split_scope_tokens(t) {
                scopes.push(RuleScope::parse(&token)?);
            }
        }
    } else {
        scopes.push(RuleScope::Text);
    }

    // 全局路径门。
    let globs = fields
        .get("globs")
        .map(|items| {
            items.iter().try_fold(Vec::new(), |mut acc, g| {
                compile_glob(g)?;
                acc.push(g.clone());
                Ok::<_, String>(acc)
            })
        })
        .transpose()?
        .unwrap_or_default();

    let interrupt_mode =
        match first(&fields, "interruptMode").or_else(|| first(&fields, "interrupt_mode")) {
            Some("never" | "false") => InterruptMode::Never,
            _ => InterruptMode::Always,
        };

    // 重复策略（默认 once）。
    let repeat = match first(&fields, "repeat").or_else(|| first(&fields, "repeatPolicy")) {
        Some("always" | "true") => Repeat::Always,
        _ => Repeat::Once,
    };

    // 条件：condition（正则，OR）与 astCondition（ast-grep，工具作用域）。
    let mut conditions = Vec::new();
    if let Some(items) = fields.get("condition") {
        for c in items {
            let re =
                regex::Regex::new(c).map_err(|e| format!("规则 {name} 正则编译失败 `{c}`: {e}"))?;
            conditions.push(re);
        }
    }
    let ast_condition = first(&fields, "astCondition").map(str::to_string);
    if ast_condition.is_some()
        && scopes
            .iter()
            .all(|s| matches!(s, RuleScope::Text | RuleScope::Thinking))
    {
        return Err(format!(
            "规则 {name}: astCondition 仅支持工具作用域（scope: tool / tool:NAME）"
        ));
    }

    let rule = Rule {
        name,
        description,
        body,
        scopes,
        globs,
        interrupt_mode,
        repeat,
        conditions,
        ast_condition,
    };
    if !rule.is_matchable() {
        return Err(format!(
            "规则 {}: 无 condition / astCondition，无法作为 TTSR 规则",
            rule.name
        ));
    }
    Ok(rule)
}

/// 切分 scope 标量：按**括号深度 0** 的逗号拆分，忽略括号内逗号
/// （`tool:edit(*.{ts,tsx}), tool:write(*.ts)` → 两个 token）；空白与空段丢弃。
fn split_scope_tokens(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut cur = String::new();
    for ch in raw.chars() {
        match ch {
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth <= 0 => {
                let t = cur.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    out
}

fn first<'a>(fields: &'a BTreeMap<String, Vec<String>>, key: &str) -> Option<&'a str> {
    fields.get(key).and_then(|v| v.first()).map(String::as_str)
}

/// 编译并匹配路径 glob 列表（完整路径或 basename 命中即通过）。
fn glob_matches(patterns: &[String], path: &str) -> bool {
    let base = std::path::Path::new(path).file_name();
    patterns.iter().any(|p| {
        Glob::new(p).is_ok_and(|g| {
            let m = g.compile_matcher();
            m.is_match(path) || base.is_some_and(|b| m.is_match(b))
        })
    })
}

/// 从目录发现规则文件（`*.md`，非递归）。解析失败的文件被跳过并记录警告。
pub fn discover_rules(dir: &Path) -> Vec<Rule> {
    let mut rules = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return rules;
    };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_string();
        let Ok(content) = std::fs::read_to_string(&path) else {
            tracing::warn!("ttsr: 读取规则文件失败: {}", path.display());
            continue;
        };
        match parse_rule(&stem, &content) {
            Ok(rule) => rules.push(rule),
            Err(e) => tracing::warn!("ttsr: 跳过规则 {}: {e}", path.display()),
        }
    }
    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(content: &str) -> Rule {
        parse_rule("test-rule", content).expect("规则应解析成功")
    }

    #[test]
    fn parses_basic_rule_with_defaults() {
        let r = rule("---\ncondition: ['(?i)Box::leak']\n---\n禁止 Box::leak，改用 Arc<str>。");
        assert_eq!(r.name, "test-rule");
        assert_eq!(r.scopes, vec![RuleScope::Text]);
        assert_eq!(r.interrupt_mode, InterruptMode::Always);
        assert_eq!(r.repeat, Repeat::Once);
        assert!(r.matches_condition("use Box::leak here"));
        assert!(!r.matches_condition("Box::from_raw"));
    }

    #[test]
    fn frontmatter_name_overrides_filename() {
        let r = rule("---\nname: custom-name\ncondition: [x]\n---\nbody");
        assert_eq!(r.name, "custom-name");
    }

    #[test]
    fn parses_scopes_and_globs() {
        let r = rule(
            "---\nscope:\n  - text\n  - tool:write_file\n  - tool:apply_hashline(*.rs)\ncondition: [leak]\n---\nbody",
        );
        assert!(r.covers_stream("text"));
        assert!(!r.covers_stream("thinking"));
        assert!(r.covers_tool("write_file", Some("src/lib.rs")));
        assert!(r.covers_tool("apply_hashline", Some("src/lib.rs")));
        assert!(!r.covers_tool("apply_hashline", Some("src/lib.py")));
        assert!(!r.covers_tool("grep", Some("src/lib.rs")));
    }

    /// H44：omp 内置规则用**逗号分隔的标量**写 scope，且 glob 内含 `{a,b}`——
    /// 切分必须只认括号外的逗号。
    #[test]
    fn parses_comma_separated_scope_scalar() {
        let r =
            rule("---\nscope: \"tool:edit(*.rs), tool:write(*.rs)\"\ncondition: [leak]\n---\nbody");
        assert_eq!(r.scopes.len(), 2, "{:?}", r.scopes);
        assert!(r.covers_tool("edit", Some("src/a.rs")));
        assert!(r.covers_tool("write", Some("a.rs")));
        assert!(!r.covers_tool("edit", Some("a.go")));

        // 括号内逗号（glob 列表展开）不得被当作分隔符。
        let r2 = rule(
            "---\nscope: \"tool:edit(*.{ts,tsx}), tool:write(**/*.{ts,tsx})\"\ncondition: [x]\n---\nbody",
        );
        assert_eq!(r2.scopes.len(), 2, "{:?}", r2.scopes);
        assert!(r2.covers_tool("edit", Some("a.tsx")));
        assert!(r2.covers_tool("edit", Some("a.ts")));
        assert!(r2.covers_tool("write", Some("deep/a.ts")));
    }

    #[test]
    fn split_scope_tokens_drops_blanks() {
        assert_eq!(
            split_scope_tokens(" text , , tool:write_file "),
            vec!["text".to_string(), "tool:write_file".to_string()]
        );
        assert!(split_scope_tokens("   ").is_empty());
    }

    #[test]
    fn global_globs_gate_tool_scopes_and_block_text() {
        let r = rule(
            "---\nscope:\n  - text\n  - tool:write_file\nglobs:\n  - '*.rs'\ncondition: [leak]\n---\nbody",
        );
        // 文本流无路径上下文：含全局 globs 的规则不匹配（omp `matchesGlobalPaths` 语义）。
        assert!(!r.covers_stream("text"));
        assert!(r.covers_tool("write_file", Some("src/lib.rs")));
        assert!(!r.covers_tool("write_file", Some("src/lib.py")));
    }

    #[test]
    fn glob_shorthand_becomes_tool_scope() {
        let r = rule("---\nscope: ['*.rs']\ncondition: ['.*']\n---\nbody");
        assert_eq!(r.scopes.len(), 1);
        let RuleScope::Tool { name, .. } = &r.scopes[0] else {
            panic!("简写应转为 Tool 作用域");
        };
        assert_eq!(name, "write_file");
        assert!(r.covers_tool("write_file", Some("src/lib.rs")));
        assert!(!r.covers_tool("write_file", Some("src/lib.py")));
        assert!(!r.covers_stream("text"));
    }

    #[test]
    fn never_interrupt_mode_parsed() {
        let r = rule("---\ninterruptMode: never\ncondition: [x]\n---\nbody");
        assert_eq!(r.interrupt_mode, InterruptMode::Never);
    }

    #[test]
    fn repeat_always_parsed() {
        let r = rule("---\nrepeat: always\ncondition: [x]\n---\nbody");
        assert_eq!(r.repeat, Repeat::Always);
    }

    #[test]
    fn ast_condition_requires_tool_scope() {
        let err = parse_rule(
            "t",
            "---\ncondition: [x]\nastCondition: ['if ($X) clearTimeout($X)']\n---\nbody",
        )
        .unwrap_err();
        assert!(err.contains("工具作用域"));
        let ok = parse_rule(
            "t",
            "---\nscope: tool:write_file\nastCondition: ['if ($X) clearTimeout($X)']\n---\nbody",
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn empty_condition_rule_rejected() {
        let err = parse_rule("t", "---\nname: x\n---\nbody").unwrap_err();
        assert!(err.contains("无 condition"));
    }

    #[test]
    fn bad_regex_rejected() {
        assert!(parse_rule("t", "---\ncondition: ['(']\n---\nbody").is_err());
        assert!(
            parse_rule(
                "t",
                "---\nscope: [tool:write_file(]\ncondition: [x]\n---\nbody"
            )
            .is_err()
        );
    }

    #[test]
    fn discover_loads_md_files_only() {
        let dir = std::env::temp_dir().join(format!("ttsr-discover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.md"), "---\ncondition: [x]\n---\nbody a").unwrap();
        std::fs::write(dir.join("b.txt"), "not a rule").unwrap();
        let rules = discover_rules(&dir);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "a");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
