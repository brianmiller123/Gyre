//! 多来源 `mcp.json` 加载、禁用名单与写回（Claude / Cursor 兼容形状）。
//!
//! ## 来源与优先级（低 → 高）
//!
//! 1. user `<config_dir>/mcp.json`（[`agent_core::platform::config_dir`]，跨项目共享）；
//! 2. project `<cwd>/.agent/mcp.json`；
//! 3. project `<cwd>/mcp.json`；
//! 4. project `<cwd>/.mcp.json`（后者覆盖前者）。
//!
//! [`Config::load`](crate::Config::load) 在 TOML 分层合并之后调用
//! [`merge_mcp_json_sources`]，故 TOML `[mcp.servers.<name>]` 优先级最高：同名 server
//! 保留已有（TOML）条目并 warn，不覆盖。
//!
//! ## 禁用名单
//!
//! `disabledServers` 是跨源**并集**的拒绝名单，压过一切来源；server 条目内
//! `"enabled": false` 同样计入。合并末尾统一从 `McpConfig::servers` 剔除，
//! 因此被禁用的 server 仍保留在源文件里（`/mcp list` 可见、可 `enable` 恢复）。
//!
//! ## 形状（Claude / Cursor 兼容）
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "fs": { "command": "npx", "args": ["-y", "…"], "env": {"TOKEN": "t"}, "timeout_ms": 5000 },
//!     "remote": { "url": "https://example.com/mcp", "type": "sse", "headers": {"Authorization": "…"} }
//!   },
//!   "disabledServers": ["fs"]
//! }
//! ```
//!
//! 畸形 JSON / 单个无效 server 条目只记入 [`McpJsonLoad::warnings`] 并跳过（绝不 panic、
//! 绝不阻断启动）；未知键一律忽略（前向兼容 Cursor / Claude 扩展键）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::config::{McpConfig, McpServerConfig};

/// 单个 `mcp.json` 来源（级别 + 路径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpJsonSource {
    /// 来源级别（user / project）。
    pub level: McpJsonLevel,
    /// 文件路径。
    pub path: PathBuf,
}

/// `mcp.json` 来源级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpJsonLevel {
    /// 用户级：`<config_dir>/mcp.json`（跨项目共享）。
    User,
    /// 项目级：工作区内 `.agent/mcp.json` / `mcp.json` / `.mcp.json`。
    Project,
}

impl McpJsonLevel {
    /// 展示名（`user` / `project`）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// 按优先级（低 → 高）列出全部候选来源（不检查文件是否存在）。
#[must_use]
pub fn mcp_json_sources(cwd: &Path) -> Vec<McpJsonSource> {
    let mut out = Vec::new();
    if let Some(dir) = agent_core::platform::config_dir() {
        out.push(McpJsonSource {
            level: McpJsonLevel::User,
            path: dir.join("mcp.json"),
        });
    }
    for path in [
        cwd.join(agent_core::platform::project_config_dir_name())
            .join("mcp.json"),
        cwd.join("mcp.json"),
        cwd.join(".mcp.json"),
    ] {
        out.push(McpJsonSource {
            level: McpJsonLevel::Project,
            path,
        });
    }
    out
}

/// 单个来源的加载结果（不存在或读失败时 `servers` / `disabled` 为空）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpJsonLoad {
    /// 来源级别。
    pub level: McpJsonLevel,
    /// 文件路径。
    pub path: PathBuf,
    /// 该来源声明的 server（含被禁用者，供 `/mcp list` 展示与 `enable` 恢复）。
    pub servers: BTreeMap<String, McpServerConfig>,
    /// 该来源的拒绝名单（`disabledServers` ∪ `"enabled": false`，按文件内顺序去重）。
    pub disabled: Vec<String>,
    /// 人类可读告警（畸形 JSON / 无效条目；调用方负责 warn + 展示）。
    pub warnings: Vec<String>,
}

/// 加载全部候选来源（不存在者返回空结果；畸形只 warn 跳过，绝不 panic）。
#[must_use]
pub fn load_mcp_json_sources(cwd: &Path) -> Vec<McpJsonLoad> {
    mcp_json_sources(cwd)
        .into_iter()
        .map(|source| load_one(&source))
        .collect()
}

/// 加载单个来源。
fn load_one(source: &McpJsonSource) -> McpJsonLoad {
    let mut load = McpJsonLoad {
        level: source.level,
        path: source.path.clone(),
        servers: BTreeMap::new(),
        disabled: Vec::new(),
        warnings: Vec::new(),
    };
    if !source.path.exists() {
        return load;
    }
    let text = match std::fs::read_to_string(&source.path) {
        Ok(text) => text,
        Err(e) => {
            load.warnings.push(format!(
                "{}: 读取失败，已跳过（{e}）",
                source.path.display()
            ));
            return load;
        }
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        load.warnings.push(format!(
            "{}: JSON 解析失败，已跳过该来源",
            source.path.display()
        ));
        return load;
    };
    let Some(root) = value.as_object() else {
        load.warnings.push(format!(
            "{}: 顶层必须是 JSON 对象，已跳过该来源",
            source.path.display()
        ));
        return load;
    };

    if let Some(raw_servers) = root.get("mcpServers") {
        match raw_servers.as_object() {
            Some(map) => {
                for (name, raw) in map {
                    // `"enabled": false` 等价于把该 server 写进本来源的拒绝名单。
                    let enabled = raw.get("enabled").and_then(Value::as_bool).unwrap_or(true);
                    match serde_json::from_value::<McpServerConfig>(raw.clone()) {
                        Ok(server) => {
                            if !enabled && !load.disabled.contains(name) {
                                load.disabled.push(name.clone());
                            }
                            load.servers.insert(name.clone(), server);
                        }
                        Err(e) => load.warnings.push(format!(
                            "{}: server \"{name}\" 配置无效（需 command 或 url），已跳过（{e}）",
                            source.path.display()
                        )),
                    }
                }
            }
            None => load.warnings.push(format!(
                "{}: mcpServers 必须是对象，已跳过",
                source.path.display()
            )),
        }
    }

    match root.get("disabledServers") {
        Some(Value::Array(items)) => {
            for item in items {
                match item.as_str() {
                    Some(name) => {
                        if !load.disabled.iter().any(|d| d == name) {
                            load.disabled.push(name.to_owned());
                        }
                    }
                    None => load.warnings.push(format!(
                        "{}: disabledServers 含非字符串项，已忽略",
                        source.path.display()
                    )),
                }
            }
        }
        Some(_) => load.warnings.push(format!(
            "{}: disabledServers 必须是数组，已忽略",
            source.path.display()
        )),
        None => {}
    }
    load
}

/// 把各来源 server 并入 `cfg`（来源低 → 高覆盖；TOML `[mcp.servers]` 最高优先），
/// 再按并集拒绝名单剔除。
///
/// 语义：
/// - `cfg` 在调用前已含 TOML 条目（[`Config::load`](crate::Config::load) 的 TOML 合并
///   阶段）：这些名字**胜出**，同名 JSON 定义一律忽略并记入说明；
/// - 其余按 `loads` 顺序低 → 高插入，后者覆盖前者（user → `.agent/mcp.json` →
///   `mcp.json` → `.mcp.json`）；
/// - `disabledServers` 并集压过一切来源，最后统一剔除并记入说明。
///
/// 返回人类可读说明（被忽略的同名定义 / 被拒绝名单剔除者），供 `/mcp status` 与日志展示。
pub fn merge_mcp_json_sources(cfg: &mut McpConfig, loads: &[McpJsonLoad]) -> Vec<String> {
    let mut notes = Vec::new();
    // TOML 已有条目：最高优先，JSON 定义不覆盖。
    let pinned: BTreeSet<String> = cfg.servers.keys().cloned().collect();
    for load in loads {
        for name in load.servers.keys() {
            if pinned.contains(name) {
                notes.push(format!(
                    "{}: MCP server \"{name}\" 已被更高优先级配置（TOML [mcp.servers]）定义，忽略本来源定义",
                    load.path.display()
                ));
            }
        }
    }

    for load in loads {
        for (name, server) in &load.servers {
            if pinned.contains(name) {
                continue;
            }
            cfg.servers.insert(name.clone(), server.clone());
        }
    }

    let denied: BTreeSet<&str> = loads
        .iter()
        .flat_map(|load| load.disabled.iter().map(String::as_str))
        .collect();
    let mut removed: Vec<&str> = denied
        .iter()
        .copied()
        .filter(|name| cfg.servers.remove(*name).is_some())
        .collect();
    removed.sort_unstable();
    if !removed.is_empty() {
        notes.push(format!(
            "disabledServers 拒绝名单命中并剔除: {}",
            removed.join(", ")
        ));
    }
    notes
}

/// 读取 JSON 对象（缺失/空文件 → 空对象；畸形 → `InvalidData`，避免写回时静默毁掉用户文件）。
fn read_object(path: &Path) -> std::io::Result<Map<String, Value>> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Map::new()),
        Ok(text) => {
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{}: JSON 解析失败，拒绝覆盖写入（{e}）", path.display()),
                )
            })?;
            match value {
                Value::Object(map) => Ok(map),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{}: 顶层必须是 JSON 对象，拒绝覆盖写入", path.display()),
                )),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(e),
    }
}

/// 原子写回 JSON 对象：目录按需创建，2 空格缩进 + 尾换行（可 diff），Unix 0600。
fn write_object(path: &Path, object: Map<String, Value>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut body =
        serde_json::to_string_pretty(&Value::Object(object)).map_err(std::io::Error::other)?;
    body.push('\n');
    crate::auth::atomic_write_0600(path, body.as_bytes())
}

/// 在某来源文件的 `disabledServers` 中移除 `name`（不存在的键清空后整体删除，保持文件干净）。
fn drop_from_disabled(object: &mut Map<String, Value>, name: &str) -> bool {
    let Some(Value::Array(items)) = object.get_mut("disabledServers") else {
        return false;
    };
    let before = items.len();
    items.retain(|item| item.as_str() != Some(name));
    let changed = items.len() != before;
    if items.is_empty() {
        object.remove("disabledServers");
    }
    changed
}

/// 写入 / 更新单个 server（保留文件内其他键与 server；同时从本文件拒绝名单中移除该名字）。
///
/// `server` 经 `serde_json::to_value` 序列化（形状见模块文档）。
/// 空文件视为新文件；畸形 JSON 返回 [`std::io::ErrorKind::InvalidData`]（绝不覆盖损坏文件）。
///
/// # Errors
/// 读改写 / 序列化 / 落盘失败时返回 [`std::io::Error`]。
pub fn write_mcp_server(path: &Path, name: &str, server: &McpServerConfig) -> std::io::Result<()> {
    let mut object = read_object(path)?;
    let entry = object
        .entry("mcpServers".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(map) = entry.as_object_mut() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: mcpServers 必须是对象，拒绝写入", path.display()),
        ));
    };
    map.insert(
        name.to_owned(),
        serde_json::to_value(server).map_err(std::io::Error::other)?,
    );
    drop_from_disabled(&mut object, name);
    write_object(path, object)
}

/// 删除单个 server（同时清理本文件拒绝名单中的同名条目）。
///
/// 返回该 server 是否存在于文件（文件缺失 → `Ok(false)`）。
///
/// # Errors
/// 读改写 / 序列化 / 落盘失败时返回 [`std::io::Error`]。
pub fn remove_mcp_server(path: &Path, name: &str) -> std::io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut object = read_object(path)?;
    let removed = object
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .is_some_and(|map| map.remove(name).is_some());
    let cleaned = drop_from_disabled(&mut object, name);
    if removed || cleaned {
        write_object(path, object)?;
    }
    Ok(removed)
}

/// 设置某 server 的禁用态（写 `disabledServers`；重复设置同一状态幂等）。
///
/// `disabled = true` 追加去重（数组按键排序，文件字节稳定）；`false` 移除，
/// 列表清空时删除 `disabledServers` 键。
///
/// # Errors
/// 读改写 / 序列化 / 落盘失败时返回 [`std::io::Error`]。
pub fn set_mcp_server_disabled(path: &Path, name: &str, disabled: bool) -> std::io::Result<()> {
    let mut object = read_object(path)?;
    if disabled {
        let entry = object
            .entry("disabledServers".to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(items) = entry.as_array_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: disabledServers 必须是数组，拒绝写入", path.display()),
            ));
        };
        if !items.iter().any(|item| item.as_str() == Some(name)) {
            items.push(Value::String(name.to_owned()));
        }
        items.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    } else {
        drop_from_disabled(&mut object, name);
    }
    write_object(path, object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{McpHttpConfig, McpHttpTransport, McpStdioConfig};
    use std::collections::HashMap;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gyre-mcp-json-{}-{tag}-{:#x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn stdio(
        command: &str,
        args: &[&str],
        env: &[(&str, &str)],
        timeout_ms: Option<u64>,
    ) -> McpServerConfig {
        McpServerConfig::Stdio(McpStdioConfig {
            command: command.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            timeout_ms,
        })
    }

    fn http(url: &str, transport: McpHttpTransport) -> McpServerConfig {
        McpServerConfig::Http(McpHttpConfig {
            url: url.to_owned(),
            headers: HashMap::new(),
            timeout_ms: None,
            oauth: None,
            transport,
        })
    }

    fn load_at(cwd: &Path, path: &Path) -> McpJsonLoad {
        load_mcp_json_sources(cwd)
            .into_iter()
            .find(|l| l.path == path)
            .expect("来源应存在于候选列表")
    }

    #[test]
    fn write_then_load_roundtrips_stdio_and_http() {
        let dir = tmp_dir("roundtrip");
        let path = dir.join(".agent").join("mcp.json");
        let fs = stdio(
            "npx",
            &["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
            &[("TOKEN", "t")],
            Some(5000),
        );
        let sse = http("https://example.com/mcp", McpHttpTransport::Sse);
        let streamable = http("http://127.0.0.1:3000/mcp", McpHttpTransport::Streamable);
        write_mcp_server(&path, "fs", &fs).unwrap();
        write_mcp_server(&path, "remote-sse", &sse).unwrap();
        write_mcp_server(&path, "local", &streamable).unwrap();

        // 落盘形状（Claude / Cursor 兼容；空集合与 None 省略）。
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            raw["mcpServers"]["fs"],
            serde_json::json!({
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
                "env": {"TOKEN": "t"},
                "timeout_ms": 5000
            })
        );
        assert_eq!(
            raw["mcpServers"]["remote-sse"],
            serde_json::json!({"url": "https://example.com/mcp", "type": "sse"})
        );

        // 往返：写出的 JSON 加载回来与原文相等。
        let load = load_at(&dir, &path);
        assert!(load.warnings.is_empty(), "无告警: {:?}", load.warnings);
        assert_eq!(load.level, McpJsonLevel::Project);
        let mut expected = BTreeMap::new();
        expected.insert("fs".to_owned(), fs);
        expected.insert("remote-sse".to_owned(), sse);
        expected.insert("local".to_owned(), streamable);
        assert_eq!(load.servers, expected);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mcp.json 必须 0600");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_preserves_unrelated_keys_and_servers() {
        let dir = tmp_dir("preserve");
        let path = dir.join("mcp.json");
        std::fs::write(
            &path,
            r#"{"$schema":"x","mcpServers":{"a":{"command":"a"}},"other":1}"#,
        )
        .unwrap();
        let b = stdio("b", &["--flag"], &[], None);
        write_mcp_server(&path, "b", &b).unwrap();
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["$schema"], "x", "未知顶层键保留");
        assert_eq!(raw["other"], 1, "未知顶层键保留");
        assert_eq!(raw["mcpServers"]["a"]["command"], "a", "其他 server 保留");
        assert_eq!(raw["mcpServers"]["b"]["command"], "b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_removes_name_from_disabled_list() {
        let dir = tmp_dir("reenable");
        let path = dir.join("mcp.json");
        std::fs::write(&path, r#"{"disabledServers":["a","b"]}"#).unwrap();
        write_mcp_server(&path, "a", &stdio("a", &[], &[], None)).unwrap();
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["disabledServers"], serde_json::json!(["b"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_disabled_is_idempotent_and_drops_empty_key() {
        let dir = tmp_dir("toggle");
        let path = dir.join("mcp.json");
        set_mcp_server_disabled(&path, "a", true).unwrap();
        set_mcp_server_disabled(&path, "a", true).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first.matches("\"a\"").count(), 1, "重复禁用不产生重复项");
        let raw: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(raw["disabledServers"], serde_json::json!(["a"]));

        set_mcp_server_disabled(&path, "a", false).unwrap();
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            raw.get("disabledServers").is_none(),
            "列表清空后删除 disabledServers 键: {raw}"
        );
        // 再关一次仍幂等。
        set_mcp_server_disabled(&path, "a", false).unwrap();
        let again: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(again.get("disabledServers").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_reports_existence_and_cleans_denylist() {
        let dir = tmp_dir("remove");
        let path = dir.join("mcp.json");
        write_mcp_server(&path, "a", &stdio("a", &[], &[], None)).unwrap();
        set_mcp_server_disabled(&path, "a", true).unwrap();
        assert!(remove_mcp_server(&path, "a").unwrap());
        assert!(!remove_mcp_server(&path, "a").unwrap(), "二次删除报不存在");
        assert!(
            !remove_mcp_server(&dir.join("missing.json"), "a").unwrap(),
            "文件缺失报不存在"
        );
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw["mcpServers"].as_object().unwrap().is_empty());
        assert!(raw.get("disabledServers").is_none(), "顺手清理拒绝名单");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_keeps_existing_toml_and_applies_disabled_union() {
        // TOML 侧（`Config::load` 中先合并）：toml-only 与 dup。
        let mut cfg = McpConfig::default();
        cfg.servers
            .insert("toml-only".to_owned(), stdio("toml-cmd", &[], &[], None));
        cfg.servers
            .insert("dup".to_owned(), stdio("toml-dup", &[], &[], None));

        let loads = vec![
            McpJsonLoad {
                level: McpJsonLevel::User,
                path: PathBuf::from("/user/mcp.json"),
                servers: BTreeMap::from([
                    ("dup".to_owned(), stdio("json-dup", &[], &[], None)),
                    ("json-only".to_owned(), stdio("json-cmd", &[], &[], None)),
                    ("shared".to_owned(), stdio("user-shared", &[], &[], None)),
                    (
                        "denied".to_owned(),
                        http("https://x/mcp", McpHttpTransport::Sse),
                    ),
                ]),
                disabled: vec!["denied".to_owned()],
                warnings: vec![],
            },
            McpJsonLoad {
                level: McpJsonLevel::Project,
                path: PathBuf::from("/cwd/mcp.json"),
                servers: BTreeMap::from([
                    ("proj-only".to_owned(), stdio("proj-cmd", &[], &[], None)),
                    ("shared".to_owned(), stdio("proj-shared", &[], &[], None)),
                ]),
                disabled: vec![],
                warnings: vec![],
            },
        ];

        let notes = merge_mcp_json_sources(&mut cfg, &loads);
        assert!(
            notes.iter().any(|n| n.contains("dup")),
            "TOML 胜出须有说明: {notes:?}"
        );
        assert!(
            notes.iter().any(|n| n.contains("denied")),
            "拒绝名单剔除须有说明: {notes:?}"
        );
        // TOML 条目胜出（未被 JSON 覆盖）。
        assert_eq!(cfg.servers["dup"], stdio("toml-dup", &[], &[], None));
        assert!(cfg.servers.contains_key("toml-only"));
        assert!(cfg.servers.contains_key("json-only"));
        assert!(cfg.servers.contains_key("proj-only"));
        assert!(!cfg.servers.contains_key("denied"), "拒绝名单条目被剔除");
        // 同级 JSON：高优先级来源（项目）覆盖低优先级（用户）。
        assert_eq!(cfg.servers["shared"], stdio("proj-shared", &[], &[], None));
    }

    /// `Config::load` 端到端：TOML `[mcp.servers]` 胜出、项目级多来源按优先级合并、
    /// `disabledServers` 与 `enabled:false` 统一剔除（断言可观测的合并后 `McpConfig`）。
    #[test]
    fn config_load_merges_sources_and_applies_denylist() {
        let dir = tmp_dir("cfgload");
        std::fs::create_dir_all(dir.join(".agent")).unwrap();
        std::fs::write(
            dir.join(".agent").join("config.toml"),
            "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"\n\
             [mcp.servers.toml-dup]\ncommand = \"toml-cmd\"\n\
             [mcp.servers.toml-only]\ncommand = \"only-cmd\"\n",
        )
        .unwrap();
        // 低优先级：`.agent/mcp.json`（含 `enabled: false`）。
        std::fs::write(
            dir.join(".agent").join("mcp.json"),
            r#"{"mcpServers":{"json-only":{"command":"low"},"toml-dup":{"command":"low-dup"},
                "off":{"command":"off-cmd","enabled":false}}}"#,
        )
        .unwrap();
        // 高优先级：`mcp.json`（覆盖同名、定义拒绝名单）。
        std::fs::write(
            dir.join("mcp.json"),
            r#"{"mcpServers":{"json-only":{"command":"high"},"toml-dup":{"command":"json-dup"},
                "denied":{"url":"https://x/mcp","type":"sse"}},
                "disabledServers":["denied","off"]}"#,
        )
        .unwrap();

        let cfg = crate::Config::load(&dir).expect("加载测试配置");
        // TOML 胜出。
        assert_eq!(
            cfg.mcp.servers["toml-dup"],
            stdio("toml-cmd", &[], &[], None)
        );
        assert!(cfg.mcp.servers.contains_key("toml-only"));
        // 高优先级 JSON 覆盖低优先级 JSON。
        assert_eq!(cfg.mcp.servers["json-only"], stdio("high", &[], &[], None));
        // 并集拒绝名单（含低优先级来源的 `enabled: false`）统一剔除。
        assert!(!cfg.mcp.servers.contains_key("denied"));
        assert!(!cfg.mcp.servers.contains_key("off"));
    }

    #[test]
    fn enabled_false_and_malformed_json_only_warn() {
        let dir = tmp_dir("malformed");
        std::fs::create_dir_all(dir.join(".agent")).unwrap();
        std::fs::write(dir.join(".agent").join("mcp.json"), "not json at all {").unwrap();
        // 畸形来源不 panic、只 warn；更高优先级来源照常加载。
        std::fs::write(
            dir.join(".mcp.json"),
            r#"{"mcpServers":{"off":{"command":"x","enabled":false},"bad":{"nope":1}}}"#,
        )
        .unwrap();

        let sources = load_mcp_json_sources(&dir);
        let broken = sources
            .iter()
            .find(|l| l.path == dir.join(".agent").join("mcp.json"))
            .unwrap();
        assert!(broken.servers.is_empty());
        assert_eq!(broken.warnings.len(), 1, "{:?}", broken.warnings);

        let good = sources
            .iter()
            .find(|l| l.path == dir.join(".mcp.json"))
            .unwrap();
        assert!(good.servers.contains_key("off"), "enabled:false 仍保留条目");
        assert_eq!(good.disabled, vec!["off".to_owned()]);
        assert_eq!(
            good.warnings.len(),
            1,
            "无效 server 条目只 warn: {:?}",
            good.warnings
        );

        // 合并：畸形来源不影响合并，enabled:false 被剔除。
        let mut cfg = McpConfig::default();
        let notes = merge_mcp_json_sources(&mut cfg, &sources);
        assert!(!cfg.servers.contains_key("off"));
        assert!(notes.iter().any(|n| n.contains("off")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
