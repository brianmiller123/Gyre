//! 配置键 schema（H23 尾项）：未知键诊断 + 逐键内省。
//!
//! schema 由 [`SCHEMA_FIXTURE`]（一份覆盖**全部**配置字段的 TOML 样例）解析成
//! `toml::Value` 得到——不给配置结构体加 `Serialize`，避免为了「诊断」改动加载路径的
//! 类型面（`ModelProfile` 等结构体只有 `Deserialize`）。
//!
//! 语义：
//! - **未知键只告警，不改变加载语义**：加载路径本身仍按 serde 的默认行为忽略未知键；
//!   本模块只负责把它们找出来（`Config::load_with_warnings`）。
//! - **不产生误报**：值形态是自由 map / 多形态联合 / `flatten` 的子树列在 [`OPEN_PATHS`]，
//!   其下任意键都合法；schema 里没有的键才报告。
//! - **漂移由单测钉住**：`config.rs` 中每个 `pub` 字段名、每个 `serde(rename = …)` 都必须在
//!   fixture 中出现（`#[serde(flatten)]` 字段与 [`UNCOVERED_TYPES`] 排除在外），
//!   否则测试失败——新增配置字段时不会静默漏掉诊断覆盖。
//!
//! 已知边界（**只会漏报，不会误报**）：
//! - 自由 map / 多形态联合 / `flatten` 子树（[`OPEN_PATHS`]）不检查其子键；
//! - `serde_json::Value` 字段（如 `extra_body`）任意形态，不做检查；
//! - 外部类型的表（如 `default_model.quirks`，`ProviderQuirks` 自身带
//!   `deny_unknown_fields`）以标量登记：用户写子表时不展开，交由反序列化报错；
//! - 数组表元素用 fixture 的元素模板检查；fixture 中为空数组的键无从得知元素形态。
//!
//! 另：`toml::Value` 形态的 schema 也是 `agent config keys`（逐键内省）与后续
//! `docs/settings.md` 生成的数据源。

use std::collections::BTreeMap;
use std::sync::OnceLock;

/// 覆盖全部配置字段的 TOML 样例（值只是占位，不做类型校验）。
///
/// 由 `config.rs` 的结构体字段生成并人工校对；末尾的 `schema_fixture_covers_all_fields`
/// 单测保证它不漂移。
pub const SCHEMA_FIXTURE: &str = r#"
language = "x"
user_agent = "x"

[default_model]
id = "x"
alias = "x"
api = "x"
base_url = "x"
api_key = "x"
api_keys = []
fallbacks = []
temperature = "x"
max_output_tokens = "x"
max_input_tokens = "x"
extra_body = "x"
tokenizer = "x"
quirks = "x"

auth = "x"

[default_model.compat]
supportsUsageInStreaming = "x"
supportsToolChoice = "x"
supportsReasoningEffort = "x"
supportsReasoningParams = "x"
maxTokensField = "x"

[default_model.headers.example]
placeholder = true

[[models]]
id = "x"
alias = "x"
api = "x"
base_url = "x"
api_key = "x"
api_keys = []
fallbacks = []
temperature = "x"
max_output_tokens = "x"
max_input_tokens = "x"
extra_body = "x"
tokenizer = "x"
quirks = "x"

auth = "x"

[models.compat]
supportsUsageInStreaming = "x"
supportsToolChoice = "x"
supportsReasoningEffort = "x"
supportsReasoningParams = "x"
maxTokensField = "x"

[models.headers.example]
placeholder = true

[models_roles]

[agent]
mode = "x"
append_system_prompt = "x"
compaction_threshold_tokens = "x"
compaction_reserve_tokens = "x"
approval_mode = "x"
max_mistakes = "x"
max_turns = "x"
context_window_guard = "x"
enable_thinking = "x"
reasoning_budget = "x"
auto_thinking = "x"
auto_thinking_model = "x"
async_enabled = "x"
async_max_jobs = "x"

[agent.tools]

[agent.tools.approval.example]
placeholder = true

[agent.tools.edit]
fuzzy = "x"
fuzzy_threshold = "x"
format_on_write = "x"
diagnostics_on_write = "x"
diagnostics_deduplicate = "x"
defer_diagnostics = "x"

[agent.commands]

[[agent.commands.allow]]
pattern = "x"

[[agent.commands.deny]]
pattern = "x"

[[agent.commands.ask]]
pattern = "x"

[agent.commands.interceptor]
enabled = "x"

[agent.commands.minimizer]
enabled = "x"
max_lines = "x"

[agent.stream_guards]
block_auto_generated = "x"
edit_parse_guard = "x"
tool_loop_guard = "x"
tool_loop_threshold = "x"
tool_loop_exempt_tools = []
gemini_header_guard = "x"

[server]
bind = "x"
auth_token = "x"

[skills]
enabled = "x"
custom_directories = []
ignored = []
included = []

[skills.providers]
claude = "x"
codex = "x"
opencode = "x"
github = "x"

[mcp]

[mcp.servers.example]
placeholder = true

[memory]
enabled = "x"
backend = "x"
auto_consolidate = "x"
auto_retain_every_n_turns = "x"
background_consolidate = "x"

[github]
enabled = "x"
allow_write = "x"

[tools]

[tools.enabled.example]
placeholder = true

[subagent]
enabled = "x"
max_concurrent = "x"
inherit_parent = "x"
max_output_tokens = "x"
max_registry = "x"

[acp]
enabled = "x"
transport = "x"

[ttsr]
enabled = "x"
disabled_rules = []
builtin_rules = "x"

[goals]
token_budget = "x"
time_budget_secs = "x"
hard_stop = "x"

[todo]
eager = "x"
reminders = "x"
reminders_max = "x"
mid_run_nudge = "x"

[keybindings]

[eval]
enabled = "x"
python = "x"
idle_timeout_secs = "x"

[compaction]
backend = "x"
remote_endpoint = "x"
vision_models = []
max_frames = "x"

[socks5]
enabled = "x"
host = "x"
port = "x"
username = "x"
password = "x"
connect_timeout_secs = "x"

[[hooks]]
event = "x"
tool = "x"
command = "x"
timeout_secs = "x"
"#;

/// 自由形态子树：其下任意键都合法（不做未知键检查）。
///
/// - `models_roles` / `keybindings`：`#[serde(flatten)]` 的 `HashMap`（角色名 / 键位名即键）；
/// - `mcp.servers`：命名 server → untagged stdio/http 联合（两种形态键集不同）；
/// - `tools.enabled` / `agent.tools.approval`：工具名 → 值/审批档的自由 map；
/// - `*.headers`：`BTreeMap<String, String>`（HTTP 头名任意）。
pub const OPEN_PATHS: &[&str] = &[
    "agent.tools.approval",
    "default_model.headers",
    "keybindings",
    "mcp.servers",
    "models.headers",
    "models_roles",
    "tools.enabled",
];

/// serde 键别名（别名 → 规范名）：诊断时按规范键处理，避免误报。
pub const KEY_ALIASES: &[(&str, &str)] = &[("transport", "type")];

/// 未纳入 fixture 的类型（诊断覆盖之外的显式清单）：
///
/// - `CustomCommand` / `McpStdioConfig` / `McpHttpConfig` / `McpOAuthConfig`：
///   不属于 `Config` 的 TOML 键面（前者由 `discover_commands` 从文件发现；
///   后三者位于开放子树 `mcp.servers` 之下）。
pub const UNCOVERED_TYPES: &[&str] = &[
    "CustomCommand",
    "McpHttpConfig",
    "McpOAuthConfig",
    "McpStdioConfig",
];

/// 一条配置告警（未知键）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    /// 点分键路径（如 `agent.max_turn`）。
    pub path: String,
    /// 同层最接近的已知键（编辑距离 ≤ 2 时给出）。
    pub suggestion: Option<String>,
}

impl ConfigWarning {
    /// 人类可读消息（CLI 与日志共用）。
    #[must_use]
    pub fn message(&self) -> String {
        match &self.suggestion {
            Some(s) => format!("未知配置键 `{}`（是否想写 `{s}`？）", self.path),
            None => format!("未知配置键 `{}`", self.path),
        }
    }
}

impl std::fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// 已知键的形态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    /// 点分键路径。
    pub path: String,
    /// 形态：`scalar` / `value` / `array` / `array-of-tables` / `open-map`。
    pub kind: &'static str,
}

/// schema 树（首次访问时解析 fixture；fixture 是编译期常量，解析失败即实现缺陷）。
#[must_use]
pub fn schema() -> &'static toml::Value {
    static SCHEMA: OnceLock<toml::Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        toml::from_str(SCHEMA_FIXTURE).expect("SCHEMA_FIXTURE 必须是合法 TOML（编译期常量）")
    })
}

/// 合并后配置里的未知键（深度优先、按出现顺序）。
///
/// `merged` 是分层合并后的原始 TOML；`[models.roles]` 这类会被加载管线提升的结构
/// 不会误报（schema 中 `models` 是数组表，与用户写的表不同形，直接跳过）。
#[must_use]
pub fn unknown_keys(merged: &toml::Value) -> Vec<ConfigWarning> {
    let mut out = Vec::new();
    walk_unknown(schema(), merged, "", &mut out);
    out
}

/// 全部已知键（点分路径 + 形态）；供 `agent config keys` 与文档生成。
#[must_use]
pub fn known_keys() -> Vec<KeyInfo> {
    let mut out = Vec::new();
    walk_known(schema(), "", &mut out);
    out
}

fn walk_known(value: &toml::Value, path: &str, out: &mut Vec<KeyInfo>) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                let child_path = join(path, key);
                if is_open(&child_path) {
                    out.push(KeyInfo {
                        path: child_path,
                        kind: "open-map",
                    });
                    continue;
                }
                // 中间表本身也是「已知键」（`config keys` 要能逐层展示），叶子另行登记。
                if matches!(child, toml::Value::Table(_)) {
                    out.push(KeyInfo {
                        path: child_path.clone(),
                        kind: "table",
                    });
                }
                walk_known(child, &child_path, out);
            }
        }
        toml::Value::Array(items) => {
            out.push(KeyInfo {
                path: path.to_string(),
                kind: if items.is_empty() {
                    "array"
                } else {
                    "array-of-tables"
                },
            });
            if let Some(first) = items.first() {
                walk_known(first, path, out);
            }
        }
        toml::Value::String(_) => out.push(KeyInfo {
            path: path.to_string(),
            kind: "scalar",
        }),
        _ => out.push(KeyInfo {
            path: path.to_string(),
            kind: "value",
        }),
    }
}

fn walk_unknown(
    schema: &toml::Value,
    user: &toml::Value,
    path: &str,
    out: &mut Vec<ConfigWarning>,
) {
    if is_open(path) {
        return;
    }
    match (schema, user) {
        (toml::Value::Table(sc), toml::Value::Table(uc)) => {
            for (key, value) in uc {
                let child_path = join(path, key);
                let known = sc.get(key).or_else(|| sc.get(canonical_key(key)));
                match known {
                    // 已知键：继续下行（表/数组表）；标量/自由值到此为止。
                    Some(child_schema) => walk_unknown(child_schema, value, &child_path, out),
                    None => out.push(ConfigWarning {
                        path: child_path,
                        suggestion: suggest(key, sc.keys()),
                    }),
                }
            }
        }
        // 数组表：用 schema 的首个元素做模板逐元素检查（空数组 schema = 未知元素形态 → 跳过）。
        (toml::Value::Array(sa), toml::Value::Array(ua)) => {
            if let Some(first) = sa.first() {
                for value in ua {
                    walk_unknown(first, value, path, out);
                }
            }
        }
        // 其余组合（类型不匹配等）交给反序列化报错，这里不重复诊断。
        _ => {}
    }
}

/// `key` 的规范名（消化 `serde(alias = …)`）。
fn canonical_key(key: &str) -> &str {
    KEY_ALIASES
        .iter()
        .find(|(alias, _)| *alias == key)
        .map_or(key, |(_, canonical)| *canonical)
}

/// 路径是否落在开放子树内（含其后代）。
fn is_open(path: &str) -> bool {
    OPEN_PATHS
        .iter()
        .any(|open| path == *open || path.starts_with(&format!("{open}.")))
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// 在同层已知键里找最接近的拼写（编辑距离 ≤ 2，且不为自身）。
fn suggest<'a, I>(key: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = &'a String>,
{
    let mut best: Option<(usize, &str)> = None;
    for candidate in candidates {
        let distance = levenshtein(key, candidate);
        if distance == 0 || distance > 2 {
            continue;
        }
        if best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, candidate)| candidate.to_string())
}

/// 经典 Levenshtein 距离（两行滚动；键名很短，无需优化）。
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// 逐键内省：`key` 的形态（`None` = 未知键）。
#[must_use]
pub fn key_info(key: &str) -> Option<KeyInfo> {
    known_keys().into_iter().find(|k| k.path == key)
}

/// schema 中每个表的直接子键（供 `config keys` 逐层展示）。
#[must_use]
pub fn children_of(key: &str) -> Vec<KeyInfo> {
    let prefix = if key.is_empty() {
        String::new()
    } else {
        format!("{key}.")
    };
    known_keys()
        .into_iter()
        .filter(|k| {
            k.path.starts_with(&prefix) && !k.path[prefix.len()..].contains('.') && k.path != key
        })
        .collect()
}

/// 供测试与诊断使用的 schema 表索引（路径 → 值）。
#[must_use]
pub fn key_table() -> BTreeMap<String, &'static str> {
    known_keys().into_iter().map(|k| (k.path, k.kind)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `snake_case` → `camelCase`（漂移检查用）。
    fn to_camel_case(name: &str) -> String {
        let mut out = String::new();
        for (i, part) in name.split('_').enumerate() {
            if i == 0 {
                out.push_str(part);
            } else {
                let mut chars = part.chars();
                if let Some(first) = chars.next() {
                    out.extend(first.to_uppercase());
                    out.push_str(chars.as_str());
                }
            }
        }
        out
    }

    #[test]
    fn fixture_parses_and_has_no_unknown_keys_of_its_own() {
        // fixture 自身必须「零未知键」——否则说明 fixture 的路径与 schema 树不一致。
        let merged: toml::Value = toml::from_str(SCHEMA_FIXTURE).expect("fixture 合法 TOML");
        let warnings = unknown_keys(&merged);
        assert!(warnings.is_empty(), "fixture 不应有未知键: {warnings:?}");
    }

    /// 漂移护栏：`config.rs` 里每个 `pub` 字段名与 `serde(rename)` 都必须出现在 fixture 中。
    #[test]
    fn schema_fixture_covers_all_fields() {
        let src = include_str!("config.rs");
        let fixture: toml::Value = toml::from_str(SCHEMA_FIXTURE).unwrap();

        // 收集 fixture 中出现的**所有**键名（任意层级，含子表）。
        fn collect(value: &toml::Value, out: &mut std::collections::HashSet<String>) {
            if let toml::Value::Table(t) = value {
                for (k, v) in t {
                    out.insert(k.clone());
                    collect(v, out);
                }
            } else if let toml::Value::Array(a) = value {
                for v in a {
                    collect(v, out);
                }
            }
        }
        let mut keys = std::collections::HashSet::new();
        collect(&fixture, &mut keys);

        // 解析 `pub struct X { pub field: T, }`（含嵌套泛型）。
        let mut missing = Vec::new();
        let mut current = String::new();
        let mut skip_struct = false;
        let mut flatten_next = false;
        // `#[serde(rename_all = "camelCase")]` 的容器：字段名在 TOML 里是 camelCase。
        // 该属性出现在 `pub struct` 行**之前**，故先暂存再在结构体行生效。
        let mut camel_case = false;
        let mut pending_camel = false;
        for line in src.lines() {
            let trimmed = line.trim();
            if trimmed.contains("serde(flatten)") {
                flatten_next = true;
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("pub struct ") {
                current = rest
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("")
                    .to_string();
                skip_struct = UNCOVERED_TYPES.contains(&current.as_str());
                camel_case = pending_camel;
                pending_camel = false;
                continue;
            }
            if trimmed.starts_with("#[serde(") && trimmed.contains("rename_all = \"camelCase\"") {
                pending_camel = true;
                continue;
            }
            if trimmed == "}" {
                current.clear();
                continue;
            }
            if skip_struct || current.is_empty() {
                continue;
            }
            // 字段：`pub name: Type`（类型可能含逗号，但字段名总在行首附近）。
            if let Some(rest) = trimmed.strip_prefix("pub ")
                && let Some((name, _)) = rest.split_once(':')
            {
                let name = name.trim();
                // 容器 `rename_all = "camelCase"` 时字段名以 camelCase 出现在 TOML 里。
                let effective = if camel_case {
                    to_camel_case(name)
                } else {
                    name.to_string()
                };
                // `#[serde(flatten)]` 字段不是 TOML 键（映射被展平到父表），跳过。
                if !flatten_next && !keys.contains(&effective) {
                    missing.push(format!("{current}::{name}"));
                }
                flatten_next = false;
            }
            // 重命名：`#[serde(rename = "x")]`
            if let Some(pos) = trimmed.find("rename = \"")
                && let Some(end) = trimmed[pos + 10..].find('"')
            {
                let renamed = &trimmed[pos + 10..pos + 10 + end];
                if !keys.contains(renamed) {
                    missing.push(format!("{current}::rename({renamed})"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "以下字段未纳入 SCHEMA_FIXTURE（新增配置字段时请同步 fixture）: {missing:?}"
        );
    }

    /// 别名必须显式登记（否则 serde 接受的旧键会被误报为未知键）。
    #[test]
    fn schema_key_aliases_are_declared() {
        let src = include_str!("config.rs");
        let mut declared: Vec<&str> = KEY_ALIASES.iter().map(|(alias, _)| *alias).collect();
        declared.sort_unstable();
        let mut found: Vec<&str> = Vec::new();
        for line in src.lines() {
            let trimmed = line.trim();
            // 只看 serde 属性行：测试夹具里的 `alias = "k2"` 等 TOML 片段不是键别名。
            if !trimmed.starts_with("#[serde(") {
                continue;
            }
            let mut rest = trimmed;
            while let Some(pos) = rest.find("alias = \"") {
                rest = &rest[pos + 9..];
                if let Some(end) = rest.find('"') {
                    found.push(&rest[..end]);
                    rest = &rest[end..];
                } else {
                    break;
                }
            }
        }
        found.sort_unstable();
        found.dedup();
        // enum 变体上的 alias（hook 事件旧名）不是键别名，允许出现在 KEY_ALIASES 之外；
        // 这里只要求 KEY_ALIASES 里登记的别名确实存在，且结构体字段别名不遗漏。
        assert!(
            found.iter().all(|alias| declared.contains(alias)
                || *alias == "before_tool"
                || *alias == "after_tool"),
            "config.rs 出现未登记的键别名: {found:?} / 已登记 {declared:?}"
        );
        assert!(declared.contains(&"transport"));
    }

    #[test]
    fn detects_typo_with_suggestion() {
        let merged: toml::Value =
            toml::from_str("[agent]\nmax_turn = 5\n[agnet]\nmode = \"x\"\n").unwrap();
        let warnings = unknown_keys(&merged);
        let paths: Vec<&str> = warnings.iter().map(|w| w.path.as_str()).collect();
        assert!(paths.contains(&"agent.max_turn"), "{warnings:?}");
        assert!(paths.contains(&"agnet"), "{warnings:?}");
        let agent = warnings
            .iter()
            .find(|w| w.path == "agent.max_turn")
            .unwrap();
        assert_eq!(agent.suggestion.as_deref(), Some("max_turns"), "{agent:?}");
        let agnet = warnings.iter().find(|w| w.path == "agnet").unwrap();
        assert_eq!(agnet.suggestion.as_deref(), Some("agent"), "{agnet:?}");
        assert!(agent.message().contains("max_turns"));
    }

    /// 开放子树、数组表元素、别名都不误报。
    #[test]
    fn open_paths_arrays_and_aliases_do_not_warn() {
        let merged: toml::Value = toml::from_str(
            r#"
[default_model]
id = "m"
[default_model.quirks]
omit_temperature = true
[mcp.servers.fs]
command = "npx"
[mcp.servers.fs.env]
A = "1"
[mcp.servers.remote]
url = "https://x"
type = "sse"
transport = "sse"
[tools.enabled]
browser = true
[agent.tools.approval]
write_file = "allow"
[keybindings]
"app.clear" = "f5"
[models_roles]
default = "m"
[[agent.commands.allow]]
pattern = "cargo *"
"#,
        )
        .unwrap();
        let warnings = unknown_keys(&merged);
        assert!(warnings.is_empty(), "不应误报: {warnings:?}");
    }

    #[test]
    fn known_keys_and_children_shape() {
        let keys = key_table();
        assert_eq!(keys.get("agent.max_turns"), Some(&"scalar"));
        assert_eq!(keys.get("models"), Some(&"array-of-tables"));
        assert_eq!(keys.get("agent.commands"), Some(&"table"));
        assert_eq!(keys.get("mcp.servers"), Some(&"open-map"));
        assert_eq!(keys.get("models.id"), Some(&"scalar"));
        assert_eq!(keys.get("agent.commands.allow"), Some(&"array-of-tables"));

        assert!(key_info("agent.max_turns").is_some());
        assert!(key_info("agent.nope").is_none());
        let children = children_of("agent.commands");
        let names: Vec<&str> = children.iter().map(|k| k.path.as_str()).collect();
        assert!(names.contains(&"agent.commands.allow"), "{names:?}");
        assert!(names.contains(&"agent.commands.minimizer"), "{names:?}");
        assert!(
            !names.contains(&"agent.commands.minimizer.max_lines"),
            "{names:?}"
        );
    }
}
