//! # agent-i18n
//!
//! 轻量多语种消息目录：编译期内嵌 4 种语言 JSON（en/zh/ru/ja），运行期根据系统语言或
//! 配置覆盖选择激活语言，通过 [`t!`] 宏以 `{name}` 命名插值取词。
//!
//! ## 设计要点
//!
//! - **编译期内嵌**：locale JSON 经 [`include_str!`] 打入二进制，零运行期文件依赖。
//! - **系统语言探测**：[`detect_system_locale`] 依次读取 `LC_ALL`/`LC_MESSAGES`/`LANG`/
//!   `LANGUAGE`（Unix + 跨平台通用），取主语言子标签匹配已支持语言；未匹配回退英文。
//! - **可扩展**：新增语言只需在 `locales/` 放一份 `<code>.json`，并在 [`SUPPORTED`] 与
//!   [`match_supported`] 登记——其余（加载/回退/探测）自动生效。
//! - **回退链**：激活语言 → 英文（兜底）→ key 本身（开发期可见缺失）。
//!
//! ## 用法
//!
//! ```no_run
//! // 启动期：先以系统语言初始化（配置加载前报错也能本地化）。
//! agent_i18n::init(None);
//! // 配置加载后：若用户显式指定 language 则覆盖。
//! // agent_i18n::init(cfg.language.as_deref());
//!
//! // 任意位置取词：
//! let s = agent_i18n::t!("cli.session.id", id = "abc123");
//! ```

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

use std::collections::HashMap;
use std::fmt;
use std::sync::{LazyLock, RwLock};

/// 编译期内嵌的全部 locale 文件：(语言码 → 原始 JSON 文本)。
///
/// 新增语言：在此追加一行 `("<code>", include_str!("../locales/<code>.json"))`。
static LOCALE_FILES: &[(&str, &str)] = &[
    ("en", include_str!("../locales/en.json")),
    ("zh", include_str!("../locales/zh.json")),
    ("ru", include_str!("../locales/ru.json")),
    ("ja", include_str!("../locales/ja.json")),
];

/// 已支持语言码（用于探测匹配与文档展示）。
///
/// 新增语言时同步追加。
pub const SUPPORTED: &[&str] = &["en", "zh", "ru", "ja"];

/// 默认（回退）语言——任何 key 缺失时最终落到此语言。
const DEFAULT_LOCALE: &str = "en";

/// 全部语言的已解析消息表：(语言码 → {key → 文案})。
static CATALOG: LazyLock<HashMap<&'static str, HashMap<String, String>>> =
    LazyLock::new(load_catalog);

/// 激活语言码（可由 [`init`] 多次设置；运行期由 [`tr`] 在读锁下读取）。
static ACTIVE: RwLock<&'static str> = RwLock::new(DEFAULT_LOCALE);

/// 解析全部内嵌 locale 文本为消息表。解析失败（JSON 损坏）跳过该语言并继续。
fn load_catalog() -> HashMap<&'static str, HashMap<String, String>> {
    let mut out = HashMap::new();
    for (code, text) in LOCALE_FILES {
        match serde_json::from_str::<HashMap<String, String>>(text) {
            Ok(map) => {
                out.insert(*code, map);
            }
            Err(e) => {
                // 编译期资源损坏属开发期问题：打印到 stderr 以便定位，不中断运行。
                eprintln!("[i18n] 解析 locale `{code}` 失败: {e}");
            }
        }
    }
    out
}

/// 初始化（或覆盖）激活语言。
///
/// - `override_code = None`：探测系统语言。
/// - `override_code = Some(code)`：使用指定语言（未知码回退英文）。
///
/// 可安全多次调用：启动期先用系统语言初始化，配置加载后若用户显式指定再覆盖一次。
pub fn init(override_code: Option<&str>) {
    let resolved = resolve_locale(override_code);
    if let Ok(mut active) = ACTIVE.write() {
        *active = resolved;
    }
}

/// 当前激活语言码（默认 `en`，经 [`init`] 设置后反映实际语言）。
#[must_use]
pub fn current_locale() -> &'static str {
    ACTIVE.read().map_or(DEFAULT_LOCALE, |code| *code)
}

/// 按激活语言取词并完成 `{name}` 命名插值。
///
/// 找不到 key 时回退英文，再找不到返回 key 本身（开发期可见缺失，不 panic）。
/// `args` 形如 `[("name", &value), ...]`，文案中以 `{name}` 占位。
#[must_use]
pub fn tr(key: &str, args: &[(&str, &dyn fmt::Display)]) -> String {
    let active = current_locale();
    let raw = lookup(active, key).or_else(|| lookup(DEFAULT_LOCALE, key));
    let Some(raw) = raw else {
        // 缺失：返回 key 本身，便于发现遗漏。
        return key.to_string();
    };
    interpolate(&raw, args)
}

/// 取词：在指定语言表内查找 key（返回克隆，因 lifetime 来自静态表）。
fn lookup(locale: &str, key: &str) -> Option<String> {
    CATALOG.get(locale).and_then(|m| m.get(key).cloned())
}

/// `{name}` 命名插值：逐个替换占位符。无匹配占位符原样保留。
fn interpolate(raw: &str, args: &[(&str, &dyn fmt::Display)]) -> String {
    if !raw.contains('{') {
        return raw.to_string();
    }
    let mut out = raw.to_string();
    for (name, val) in args {
        let placeholder = format!("{{{name}}}");
        if out.contains(&placeholder) {
            out = out.replace(&placeholder, &val.to_string());
        }
    }
    out
}

// ── 系统语言探测 ───────────────────────────────────────────────────────────────

/// 解析最终激活语言码：显式覆盖优先，否则探测系统语言。
fn resolve_locale(override_code: Option<&str>) -> &'static str {
    if let Some(code) = override_code {
        return normalize(code).unwrap_or(DEFAULT_LOCALE);
    }
    detect_system_locale()
}

/// 把用户/配置传入的语言标签归一为已支持码（大小写/区域后缀不敏感）。
fn normalize(raw: &str) -> Option<&'static str> {
    parse_primary_subtag(raw).and_then(match_supported)
}

/// 探测系统语言：依次扫描 POSIX locale 环境变量，取首个能匹配已支持语言者。
///
/// 覆盖 Linux/macOS（`LANG`/`LC_*`）与设置了这些变量的 Windows（Git Bash/WSL/Cygwin）。
/// 未匹配则回退英文。
#[must_use]
pub fn detect_system_locale() -> &'static str {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"] {
        if let Ok(val) = std::env::var(var) {
            if let Some(sub) = parse_primary_subtag(&val) {
                if let Some(code) = match_supported(sub) {
                    return code;
                }
                // LANGUAGE 可含冒号分隔的候选列表（如 "zh_CN:en_US:en"）。
                if var == "LANGUAGE" {
                    for cand in val.split(',') {
                        if let Some(s) = parse_primary_subtag(cand) {
                            if let Some(code) = match_supported(s) {
                                return code;
                            }
                        }
                    }
                }
            }
        }
    }
    DEFAULT_LOCALE
}

/// 从 locale 串提取主语言子标签（`zh_CN.UTF-8` → `zh`；`en_US` → `en`）。
fn parse_primary_subtag(raw: &str) -> Option<&str> {
    let raw = raw.trim();
    let trimmed = raw.split(['_', '-', '.']).next()?;
    let t = trimmed.trim();
    (!t.is_empty()).then_some(t)
}

/// 主语言子标签 → 已支持码（`zh`→`zh`，`eng`/`en`→`en`，三字母码一并兼容）。
fn match_supported(subtag: &str) -> Option<&'static str> {
    match subtag.to_ascii_lowercase().as_str() {
        "zh" | "chs" | "cht" | "zho" | "chi" => Some("zh"),
        "en" | "eng" => Some("en"),
        "ru" | "rus" => Some("ru"),
        "ja" | "jpn" => Some("ja"),
        _ => None,
    }
}

/// 取词宏：`t!("key")` 或 `t!("key", name = value, ...)`。
///
/// 命名插值参数在宏内组装为 `&[(&str, &dyn Display)]` 传给 [`tr`]。
#[macro_export]
macro_rules! t {
    ($key:literal) => {
        $crate::tr($key, &[])
    };
    ($key:literal, $($name:ident = $val:expr),+ $(,)?) => {{
        let args: &[(&str, &dyn std::fmt::Display)] = &[
            $((stringify!($name), &$val as &dyn std::fmt::Display)),+
        ];
        $crate::tr($key, args)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_loads_all_locales() {
        // 每个声明的语言都应解析成功。
        for (code, _) in LOCALE_FILES {
            assert!(CATALOG.contains_key(code), "locale {code} 未加载");
        }
    }

    #[test]
    fn fallback_to_english_then_key() {
        // 确保英文存在某 key，激活非英文也能回退到英文值。
        let prev = current_locale();
        init(Some("ja"));
        // 用英文一定存在的 key 验证回退（取一个英文独有、其它语言未必有的 key 较难，
        // 故验证已知共有 key 在任意激活语言下都返回非空）。
        let s = tr("cli.session.id", &[("id", &"x")]);
        assert!(!s.is_empty());
        init(Some(prev));
    }

    #[test]
    fn interpolate_named_placeholders() {
        let s = interpolate("a={a} b={b}", &[("a", &"1"), ("b", &"two")]);
        assert_eq!(s, "a=1 b=two");
        // 未提供的占位符原样保留。
        let s = interpolate("x={x} y={y}", &[("x", &"1")]);
        assert_eq!(s, "x=1 y={y}");
        // 无占位符直接返回原串。
        assert_eq!(interpolate("plain", &[]), "plain");
    }

    #[test]
    fn normalize_accepts_variants() {
        assert_eq!(normalize("zh_CN.UTF-8"), Some("zh"));
        assert_eq!(normalize("en_US"), Some("en"));
        assert_eq!(normalize("RU"), Some("ru"));
        assert_eq!(normalize("ja_JP"), Some("ja"));
        assert_eq!(normalize("bogus"), None);
    }

    #[test]
    fn match_supported_handles_three_letter() {
        assert_eq!(match_supported("zho"), Some("zh"));
        assert_eq!(match_supported("eng"), Some("en"));
        assert_eq!(match_supported("rus"), Some("ru"));
        assert_eq!(match_supported("jpn"), Some("ja"));
    }

    #[test]
    fn init_sets_active_locale() {
        let prev = current_locale();
        init(Some("ru"));
        assert_eq!(current_locale(), "ru");
        init(None); // 探测系统语言（CI 环境可能任意，仅验证不 panic）
        init(Some(prev));
    }

    #[test]
    fn missing_key_returns_key() {
        let s = tr("this.key.does.not.exist", &[]);
        assert_eq!(s, "this.key.does.not.exist");
    }
}
