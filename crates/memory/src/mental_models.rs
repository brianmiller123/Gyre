//! 心智模型（hindsight 风格）扩展：seeds 播种 + 会话级心智模型存储 + 启动注入。
//!
//! 对标 oh-my-pi 的 `hindsight/mental-models.ts` 理念（内置种子 + 项目积累 +
//! `<mental_models>` 注入块），但保持 Gyre 的 markdown + LLM 合并路线，不引入向量/嵌入：
//!
//! - seeds：内置默认（[`SEEDS_JSON`]，仓库级通用工程准则）+ `seeds_paths` 追加项目自定义种子
//!   （每文件支持 markdown 或 JSON 数组两种格式）；
//! - 项目积累：`mental_models.md`，按时间戳追加条目（[`crate::store::LocalMemoryStore::add_mental_model`]）；
//! - 注入：[`merge_mental_models`] 把 seeds 在前、项目在后合并，超 [`MentalModelsConfig::max_inject_chars`]
//!   截断保留尾部（最新）；
//! - 提炼：[`MENTAL_MODEL_CONSOLIDATION_PROMPT`] 交给 LLM 去重/按主题分组/丢弃泛泛而谈。

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 内置种子常量：仓库级默认心智模型（通用工程准则，中英双语正文）。
pub const SEEDS_JSON: &str = r#"[
  {"name": "先读 AGENTS.md 再动手", "text": "动手前先读 AGENTS.md / README，理解项目惯例与约束再改代码。（Read AGENTS.md before acting.）"},
  {"name": "写代码前先跑测试", "text": "改代码前先跑相关测试建立基线，完成后用测试与冒烟验证，不凭感觉声称完成。（Run tests before and after changes.）"},
  {"name": "最小化改动", "text": "只改必要文件，复用既有模式，不引入第二套惯例。（Make minimal diffs, reuse existing conventions.）"},
  {"name": "证据优先", "text": "以工具输出为证据，不臆断；失败先复现再修。（Evidence over assumption; reproduce before fixing.）"},
  {"name": "保持可维护性", "text": "删除无用代码，命名表意，注释解释为什么而非重复什么。（Delete dead code, name things clearly.）"},
  {"name": "小步可验证", "text": "改动尽量小步、可独立验证，便于回滚与评审。（Small verifiable steps.）"}
]"#;

/// 心智模型配置。
#[derive(Debug, Clone)]
pub struct MentalModelsConfig {
    /// 项目自定义种子文件路径（每文件 markdown 或 JSON 数组），追加到内置种子之后。
    pub seeds_paths: Vec<PathBuf>,
    /// 注入预算上限（字符数）；默认 2000。
    pub max_inject_chars: usize,
}

impl Default for MentalModelsConfig {
    fn default() -> Self {
        Self {
            seeds_paths: Vec::new(),
            max_inject_chars: 2000,
        }
    }
}

/// 一条心智模型种子。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SeedEntry {
    /// 名称（如「先读 AGENTS.md 再动手」）。
    pub name: String,
    /// 正文（一条可执行的工程准则）。
    pub text: String,
}

/// 心智模型合并提示词：把当前 mental_models.md 交给 LLM 去重/分组/提炼。
///
/// `{}` 处嵌入待整理的 mental_models.md 全文。
pub const MENTAL_MODEL_CONSOLIDATION_PROMPT: &str = "\
# 当前心智模型（待整理）\n\n{}\n\n\
请将以上心智模型整理为精炼清单：\n\
1. 去重：合并表述相同或重复的条目；\n\
2. 按主题分组（如：工程流程、代码质量、协作沟通）；\n\
3. 只保留具体、可操作的条目，丢弃泛泛而谈的内容；\n\
4. 输出 Markdown 列表。";

/// 组装心智模型合并 prompt。
#[must_use]
pub fn mental_model_consolidation_prompt(current: &str) -> String {
    MENTAL_MODEL_CONSOLIDATION_PROMPT.replace("{}", current)
}

/// 加载种子：内置默认在前，`seeds_paths` 自定义种子按序追加。
///
/// 自定义文件缺失/不可读时静默跳过。
#[must_use]
pub fn load_seeds(config: &MentalModelsConfig) -> Vec<SeedEntry> {
    let mut seeds = parse_seeds_json(SEEDS_JSON).unwrap_or_default();
    for path in &config.seeds_paths {
        seeds.extend(load_seed_file(path));
    }
    seeds
}

/// 合并 seeds（前）与项目积累（后），超 `max_chars` 截断保留尾部（最新）。
///
/// 全部为空时返回 `None`。
#[must_use]
pub fn merge_mental_models(seeds: &[SeedEntry], project: &str, max_chars: usize) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !seeds.is_empty() {
        let mut block = String::from("## 心智模型种子（工程准则）\n");
        for seed in seeds {
            block.push_str(&format!("- **{}**：{}\n", seed.name, seed.text));
        }
        parts.push(block);
    }
    let project = project.trim();
    if !project.is_empty() {
        parts.push(project.to_string());
    }
    if parts.is_empty() {
        return None;
    }
    let merged = parts.join("\n\n");
    let max = max_chars.max(1);
    let out = if merged.chars().count() > max {
        truncate_keep_tail(&merged, max)
    } else {
        merged
    };
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 从文本解析种子：支持裸 JSON 数组 `[{name,text}]` 或 `{"seeds": [...]}` 包装；
/// 解析失败返回 `None`。
fn parse_seeds_json(text: &str) -> Option<Vec<SeedEntry>> {
    if let Ok(entries) = serde_json::from_str::<Vec<SeedEntry>>(text) {
        return Some(entries);
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let arr = value.get("seeds")?.as_array()?;
    let entries: Vec<SeedEntry> = arr
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();
    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
}

/// 加载单个自定义种子文件：先按 JSON 数组尝试；失败则按 markdown 整篇作为一条种子
/// （name 取文件名 stem）。
fn load_seed_file(path: &Path) -> Vec<SeedEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    if let Some(entries) = parse_seeds_json(&text) {
        return entries;
    }
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "自定义种子".to_string());
    vec![SeedEntry {
        name,
        text: text.to_string(),
    }]
}

/// 截断保留尾部（最新内容），尽量对齐到行首避免切开条目；`max_chars` 至少为 1。
fn truncate_keep_tail(text: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(1);
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let tail: String = text
        .chars()
        .rev()
        .take(max_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    // 对齐行首：丢掉第一行可能被切开的残段
    match tail.find('\n') {
        Some(idx) => tail[idx + 1..].to_string(),
        None => tail,
    }
}

/// Unix 秒 → `YYYY-MM-DD HH:MM:SS`（UTC，无外部依赖的日期换算）。
#[must_use]
pub fn format_ts(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
}

/// Howard Hinnant 的 civil_from_days 算法：epoch 天数 → (年, 月, 日)。
fn civil_from_days(z_days: i64) -> (i64, i64, i64) {
    let z = z_days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn nano() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn tmp() -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("agent-mem-mm-{}-{:#x}", std::process::id(), nano()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn builtin_seeds_parse_to_5_8_entries() {
        let seeds = parse_seeds_json(SEEDS_JSON).unwrap();
        assert!(
            (5..=8).contains(&seeds.len()),
            "内置种子应 5-8 条，实际 {}",
            seeds.len()
        );
        for seed in &seeds {
            assert!(!seed.name.is_empty());
            assert!(!seed.text.is_empty());
        }
        // 契约中举例的两条必须存在
        let names: Vec<&str> = seeds.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"先读 AGENTS.md 再动手"));
        assert!(names.contains(&"写代码前先跑测试"));
    }

    #[test]
    fn load_seeds_builtin_plus_custom_paths() {
        let root = tmp();
        let json_path = root.join("custom.json");
        let md_path = root.join("custom.md");
        std::fs::write(
            &json_path,
            r#"{"seeds":[{"name":"J1","text":"json 种子一"}]}"#,
        )
        .unwrap();
        std::fs::write(&md_path, "markdown 整篇种子正文").unwrap();
        let config = MentalModelsConfig {
            seeds_paths: vec![json_path.clone(), md_path.clone()],
            max_inject_chars: 2000,
        };
        let seeds = load_seeds(&config);
        let names: Vec<&str> = seeds.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"先读 AGENTS.md 再动手"), "内置在前");
        assert!(names.contains(&"J1"), "JSON 文件追加");
        assert!(names.contains(&"custom"), "markdown 文件整篇为一条");
        // 缺失文件静默跳过
        let config = MentalModelsConfig {
            seeds_paths: vec![root.join("missing.json")],
            max_inject_chars: 2000,
        };
        let seeds = load_seeds(&config);
        assert!(!seeds.is_empty(), "内置种子仍在");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn merge_seeds_first_project_after() {
        let seeds = vec![
            SeedEntry {
                name: "S1".into(),
                text: "种子".into(),
            },
            SeedEntry {
                name: "S2".into(),
                text: "种子二".into(),
            },
        ];
        let project = "- [2026-08-02 10:00:00] 项目约定";
        let merged = merge_mental_models(&seeds, project, 2000).unwrap();
        let seed_pos = merged.find("S1").unwrap();
        let proj_pos = merged.find("项目约定").unwrap();
        assert!(seed_pos < proj_pos, "seeds 在前，项目在后");
        assert!(merged.contains("## 心智模型种子"));
    }

    #[test]
    fn merge_truncates_keeping_tail() {
        let seeds = vec![SeedEntry {
            name: "S1".into(),
            text: "种子".into(),
        }];
        let project = "- [2026-08-02 10:00:00] 旧条目\n- [2026-08-03 10:00:00] 最新条目";
        let merged = merge_mental_models(&seeds, project, 30).unwrap();
        assert!(merged.chars().count() <= 30);
        assert!(merged.contains("最新条目"), "保留尾部最新");
        assert!(!merged.contains("旧条目"), "头部被截断");
    }

    #[test]
    fn merge_empty_returns_none() {
        assert!(merge_mental_models(&[], "  ", 2000).is_none());
        assert!(merge_mental_models(&[], "", 2000).is_none());
    }

    #[test]
    fn format_ts_known_values() {
        assert_eq!(format_ts(0), "1970-01-01 00:00:00");
        // 2026-08-02 00:00:00 UTC（与 Python datetime 交叉验证）
        assert_eq!(format_ts(1_785_628_800), "2026-08-02 00:00:00");
        // 2026-08-02 12:34:56
        assert_eq!(
            format_ts(1_785_628_800 + 12 * 3600 + 34 * 60 + 56),
            "2026-08-02 12:34:56"
        );
    }

    #[test]
    fn consolidation_prompt_shapes() {
        let p = mental_model_consolidation_prompt("现有条目");
        assert!(p.contains("现有条目"));
        assert!(p.contains("去重"));
        assert!(p.contains("按主题分组"));
        assert!(p.contains("丢弃泛泛而谈"));
        assert!(p.contains("Markdown"));
    }
}
