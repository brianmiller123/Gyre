//! MCP 工具清单磁盘缓存：连接成功时快照，启动预算内未完成握手的 server 用快照
//! 立即注册 Deferred 工具（元信息可见 + 连接就绪后可执行）。
//!
//! 布局：`<cache_dir>/mcp-cache/<server>.json`。写入走「临时文件 + 原子 rename」，
//! 读取对缺失/畸形内容容错（→ [`None`]），保证消费方永不因缓存损坏而失败。
//! 缓存纯 best-effort：任何读写失败都不影响 MCP 主流程。
//!
//! 适用性（[`load_applicable`]，对齐 omp `tool-cache.ts`）：版本 + 配置指纹
//! （[`config_fingerprint`]）+ TTL（[`CACHE_TTL_MS`]）三者全中才算可用——配置改了
//! 或快照过期的清单不再回填（否则会把早已不存在的工具摆给模型）。
//!
//! 装配见 [`crate::tool`]（注册表读写缓存，调用方无需自行落盘）。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use agent_config::{McpHttpTransport, McpServerConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool::fnv1a_base36;

/// 缓存子目录名（位于 `<cache_dir>` 下）。
const CACHE_SUBDIR: &str = "mcp-cache";

/// 缓存格式版本（字段或指纹语义变更即 +1；版本不符的旧缓存视为不可用）。
pub const CACHE_VERSION: u32 = 1;

/// 缓存有效期（30 天，对齐 omp `CACHE_TTL_MS`）。
pub const CACHE_TTL_MS: u128 = 30 * 24 * 60 * 60 * 1000;

/// 单个缓存工具的元信息（对标 `tools/list` 返回的 name / description / inputSchema）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedTool {
    /// 工具名（注册表别名形态，如 `mcp__<server>_<tool>`）。
    pub name: String,
    /// 描述（供 LLM）。
    pub description: String,
    /// 输入参数 JSON Schema。
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// 一个 server 的工具清单缓存快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedTools {
    /// 来源 server 名。
    pub server: String,
    /// 缓存的工具列表。
    pub tools: Vec<CachedTool>,
    /// 快照时间（Unix 毫秒；同时用于 TTL 判定与展示）。
    pub cached_at_ms: u128,
    /// 写入时的配置指纹（[`config_fingerprint`]）；与当前配置不符即失效。
    /// 缺省空串：旧格式缓存（无此字段）一律视为不可用。
    #[serde(default)]
    pub config_hash: String,
    /// 写入时的缓存格式版本（[`CACHE_VERSION`]）；不符即失效。
    #[serde(default)]
    pub version: u32,
}

/// 配置指纹：按固定序拼接影响工具清单的字段后取 FNV-1a 64 → base36（跨进程稳定）。
///
/// 只覆盖语义字段（传输类型 / 端点 / 命令与参数 / 环境 / 额外头 / OAuth 授权面）：
/// `timeout_ms` 等不影响清单内容的可调项不参与——调超时不应让缓存失效。
/// 手动拼接（而非序列化整个配置）是刻意的：指纹必须只随语义变化，不随 JSON
/// 书写风格或字段新增而漂移。
#[must_use]
pub fn config_fingerprint(cfg: &McpServerConfig) -> String {
    /// 有序 map 的 `k=v` 拼接（键排序，保证同内容同指纹）。
    fn push_sorted_map(out: &mut String, map: &std::collections::HashMap<String, String>) {
        let mut entries: Vec<(&String, &String)> = map.iter().collect();
        entries.sort();
        for (k, v) in entries {
            out.push('\x1f');
            out.push_str(k);
            out.push('=');
            out.push_str(v);
        }
    }
    let mut s = String::new();
    match cfg {
        McpServerConfig::Stdio(c) => {
            s.push_str("stdio");
            s.push('\x1f');
            s.push_str(&c.command);
            for a in &c.args {
                s.push('\x1f');
                s.push_str(a);
            }
            push_sorted_map(&mut s, &c.env);
        }
        McpServerConfig::Http(c) => {
            s.push_str(match c.transport {
                McpHttpTransport::Streamable => "http",
                McpHttpTransport::Sse => "sse",
            });
            s.push('\x1f');
            s.push_str(&c.url);
            push_sorted_map(&mut s, &c.headers);
            if let Some(o) = &c.oauth {
                s.push('\x1f');
                s.push_str(o.client_id.as_deref().unwrap_or(""));
                s.push('\x1f');
                s.push_str(o.client_secret.as_deref().unwrap_or(""));
                s.push('\x1f');
                s.push_str(o.scope.as_deref().unwrap_or(""));
                s.push('\x1f');
                s.push_str(o.redirect_uri.as_deref().unwrap_or(""));
                s.push('\x1f');
                s.push_str(o.resource.as_deref().unwrap_or(""));
            }
        }
    }
    fnv1a_base36(&s)
}

/// server 名可安全用作缓存文件名：非空、非 `.`/`..`、不含路径分隔符与 NUL
/// （配置键来自用户文件，仍拒绝穿越到缓存目录外）。
fn valid_server_name(server: &str) -> bool {
    !server.is_empty() && server != "." && server != ".." && !server.contains(['/', '\\', '\0'])
}

/// server 的缓存文件路径 `<cache_dir>/mcp-cache/<server>.json`。
fn cache_path(cache_dir: &Path, server: &str) -> PathBuf {
    cache_dir.join(CACHE_SUBDIR).join(format!("{server}.json"))
}

/// 当前 Unix 时间（毫秒）。时钟早于 epoch 时返回 0（仅影响 TTL 判定与展示，不影响功能）。
#[must_use]
pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 读取 server 的工具清单缓存。
///
/// 文件缺失、server 名非法或内容畸形（非法 JSON / 字段缺失 / `server` 不符）→
/// [`None`]（损坏时记 `debug!` 日志）。缓存损坏被视为「无缓存」，绝不向上报错。
#[must_use]
pub fn load(cache_dir: &Path, server: &str) -> Option<CachedTools> {
    if !valid_server_name(server) {
        return None;
    }
    let path = cache_path(cache_dir, server);
    let raw = fs_read(&path)?;
    match serde_json::from_str::<CachedTools>(&raw) {
        // 文件名与内容必须一致：改名/错放视为损坏。
        Ok(cached) if cached.server == server => Some(cached),
        Ok(_) => {
            tracing::debug!(server, path = %path.display(), "MCP 工具缓存内容与 server 不符，忽略");
            None
        }
        Err(e) => {
            tracing::debug!(server, path = %path.display(), error = %e, "MCP 工具缓存损坏，忽略");
            None
        }
    }
}

/// 读取 server 的缓存快照并校验适用性：版本 + 配置指纹 + TTL 任一不符即 [`None`]
/// （记 `debug!` 说明原因）。返回的快照可直接用于注册 Deferred 工具。
#[must_use]
pub fn load_applicable(
    cache_dir: &Path,
    server: &str,
    cfg: &McpServerConfig,
) -> Option<CachedTools> {
    let cached = load(cache_dir, server)?;
    if cached.version != CACHE_VERSION {
        tracing::debug!(
            server,
            version = cached.version,
            "MCP 工具缓存版本不符，忽略"
        );
        return None;
    }
    let expected = config_fingerprint(cfg);
    if cached.config_hash != expected {
        tracing::debug!(server, "MCP server 配置已变更，工具缓存失效");
        return None;
    }
    let age = now_ms().saturating_sub(cached.cached_at_ms);
    if age > CACHE_TTL_MS {
        tracing::debug!(server, age_ms = age, "MCP 工具缓存超过 TTL，忽略");
        return None;
    }
    Some(cached)
}

/// `read_to_string` 的容错包装：任何 I/O 错误（含缺失）都视为无缓存。
fn fs_read(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Some(raw),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "MCP 工具缓存读取失败，忽略");
            None
        }
    }
}

/// 原子写入 server 的工具清单缓存：先写临时文件（Unix 0600）再 rename 覆盖。
///
/// 目录不存在则创建。写临时文件失败或 rename 失败时清理临时文件并返回错误，
/// 目标文件保持上一次的完整内容（不会出现半截缓存）。
pub fn store(cache_dir: &Path, c: &CachedTools) -> std::io::Result<()> {
    if !valid_server_name(&c.server) {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("非法 MCP server 名：{:?}（不可用作缓存文件名）", c.server),
        ));
    }
    let dir = cache_dir.join(CACHE_SUBDIR);
    std::fs::create_dir_all(&dir)?;
    let body = serde_json::to_string_pretty(c)
        .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
    let path = cache_path(cache_dir, &c.server);
    // 临时名含 pid + 纳秒：并发写同一 server 不互踩。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = dir.join(format!(
        ".{}.tmp.{}-{}",
        c.server,
        std::process::id(),
        nanos
    ));
    let result = (|| -> std::io::Result<()> {
        write_private(&tmp, &body)?;
        std::fs::rename(&tmp, &path)
    })();
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            // 清理临时文件；目标文件保持上一次的完整内容。
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 以独占创建 + 私有权限写文件：Unix 下直接以 0600 建文件（无宽权限窗口）。
fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(body.as_bytes())
    }
    #[cfg(not(unix))]
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(body.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 唯一临时目录（对齐 persistence.rs 测试先例：纳秒名 + 用后清理）。
    fn tmp_dir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("gyre-mcp-cache-{tag}-{n}"))
    }

    fn stdio_cfg(command: &str) -> McpServerConfig {
        McpServerConfig::Stdio(agent_config::McpStdioConfig {
            command: command.to_string(),
            args: vec![],
            env: Default::default(),
            timeout_ms: None,
        })
    }

    fn sample(server: &str) -> CachedTools {
        CachedTools {
            server: server.to_string(),
            tools: vec![
                CachedTool {
                    name: "echo".to_string(),
                    description: "回声".to_string(),
                    input_schema: json!({"type": "object", "properties": {}}),
                },
                CachedTool {
                    name: "fetch".to_string(),
                    description: "抓取".to_string(),
                    input_schema: json!({"type": "object", "properties": {"url": {"type": "string"}}}),
                },
            ],
            cached_at_ms: now_ms(),
            config_hash: config_fingerprint(&stdio_cfg("npx")),
            version: CACHE_VERSION,
        }
    }

    #[test]
    fn store_then_load_roundtrip() {
        let dir = tmp_dir("roundtrip");
        let c = sample("github");
        store(&dir, &c).unwrap();
        let loaded = load(&dir, "github").unwrap();
        assert_eq!(loaded, c, "store→load 应无损往返");
        // 落盘为 `<dir>/mcp-cache/<server>.json`。
        assert!(dir.join("mcp-cache").join("github.json").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = tmp_dir("missing");
        assert!(load(&dir, "nope").is_none(), "缺失缓存应为 None");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_malformed_returns_none() {
        let dir = tmp_dir("malformed");
        let cache_dir = dir.join("mcp-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        // 半截 JSON（模拟写坏）。
        std::fs::write(cache_dir.join("broken.json"), "{\"server\": \"brok").unwrap();
        assert!(load(&dir, "broken").is_none(), "畸形 JSON 应为 None");
        // 合法 JSON 但字段缺失。
        std::fs::write(cache_dir.join("shape.json"), "{}").unwrap();
        assert!(load(&dir, "shape").is_none(), "缺字段应为 None");
        // 合法但 server 不符（错放文件）。
        std::fs::write(
            cache_dir.join("swap.json"),
            serde_json::to_string(&sample("other")).unwrap(),
        )
        .unwrap();
        assert!(load(&dir, "swap").is_none(), "server 不符应为 None");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_overwrites_atomically_without_tmp_leftover() {
        let dir = tmp_dir("overwrite");
        store(&dir, &sample("srv")).unwrap();
        let mut c = sample("srv");
        c.tools.clear();
        store(&dir, &c).unwrap();
        assert!(
            load(&dir, "srv").unwrap().tools.is_empty(),
            "应覆盖为最新快照"
        );
        // 目录内只剩目标文件，无临时残留。
        let entries: Vec<_> = std::fs::read_dir(dir.join("mcp-cache")).unwrap().collect();
        assert_eq!(entries.len(), 1, "不应残留临时文件：{entries:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_server_name_rejected() {
        let dir = tmp_dir("invalid");
        // store 拒绝穿越名。
        for bad in ["../escape", "", ".", "..", "a/b"] {
            assert!(
                store(&dir, &sample(bad)).is_err(),
                "server 名 {bad:?} 应被拒绝"
            );
        }
        // load 对非法名直接 None（不触盘）。
        assert!(load(&dir, "../escape").is_none());
        assert!(!dir.join("mcp-cache").exists(), "拒绝时不应建目录");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn stored_file_has_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("perm");
        store(&dir, &sample("srv")).unwrap();
        let path = dir.join("mcp-cache").join("srv.json");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "缓存文件应为 0600，实际 {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 适用性三闸门：配置指纹变、版本变、超 TTL 都不得把快照当成可用元信息。
    #[test]
    fn applicable_requires_matching_hash_version_and_ttl() {
        let dir = tmp_dir("applicable");
        let cfg = stdio_cfg("npx");
        store(&dir, &sample("srv")).unwrap();
        assert!(
            load_applicable(&dir, "srv", &cfg).is_some(),
            "配置未变故且未超期 → 可用"
        );

        // 1) 配置变了（换命令）→ 指纹不符 → 不可用。
        assert!(
            load_applicable(&dir, "srv", &stdio_cfg("uvx")).is_none(),
            "配置变更后旧快照必须失效"
        );

        // 2) 版本不符 → 不可用。
        let mut c = sample("srv");
        c.version = CACHE_VERSION + 1;
        store(&dir, &c).unwrap();
        assert!(
            load_applicable(&dir, "srv", &cfg).is_none(),
            "版本不符必须失效"
        );

        // 3) 超过 TTL → 不可用。
        let mut c = sample("srv");
        c.cached_at_ms = now_ms().saturating_sub(CACHE_TTL_MS + 1);
        store(&dir, &c).unwrap();
        assert!(
            load_applicable(&dir, "srv", &cfg).is_none(),
            "超期快照必须失效"
        );

        // 4) 旧格式（无版本/指纹字段）落盘 → 一律不可用。
        let cache_dir = dir.join("mcp-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            cache_dir.join("legacy.json"),
            serde_json::to_string(&json!({
                "server": "legacy",
                "tools": [],
                "cached_at_ms": now_ms()
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(
            load_applicable(&dir, "legacy", &cfg).is_none(),
            "旧格式缓存必须失效（缺指纹与版本）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 指纹只随语义字段变化：调超时不失效，换端点/参数/环境/授权面即失效。
    #[test]
    fn fingerprint_tracks_semantics_only() {
        let base = stdio_cfg("npx");
        assert_eq!(
            config_fingerprint(&base),
            config_fingerprint(&base),
            "同配置指纹稳定"
        );

        let mut timeout_only = stdio_cfg("npx");
        if let McpServerConfig::Stdio(c) = &mut timeout_only {
            c.timeout_ms = Some(5_000);
        }
        assert_eq!(
            config_fingerprint(&base),
            config_fingerprint(&timeout_only),
            "超时不影响工具清单，不应让缓存失效"
        );

        let mut other_args = stdio_cfg("npx");
        if let McpServerConfig::Stdio(c) = &mut other_args {
            c.args = vec!["-y".to_string(), "server-filesystem".to_string()];
        }
        assert_ne!(
            config_fingerprint(&base),
            config_fingerprint(&other_args),
            "参数变化必须换指纹"
        );

        let mut env_a = stdio_cfg("npx");
        let mut env_b = stdio_cfg("npx");
        if let McpServerConfig::Stdio(c) = &mut env_a {
            c.env.insert("A".into(), "1".into());
            c.env.insert("B".into(), "2".into());
        }
        if let McpServerConfig::Stdio(c) = &mut env_b {
            c.env.insert("B".into(), "2".into());
            c.env.insert("A".into(), "1".into());
        }
        assert_eq!(
            config_fingerprint(&env_a),
            config_fingerprint(&env_b),
            "环境变量插入顺序不影响指纹"
        );

        let http = McpServerConfig::Http(agent_config::McpHttpConfig {
            url: "http://127.0.0.1:3000/mcp".to_string(),
            headers: Default::default(),
            timeout_ms: None,
            oauth: None,
            transport: McpHttpTransport::Streamable,
        });
        let mut sse = http.clone();
        if let McpServerConfig::Http(c) = &mut sse {
            c.transport = McpHttpTransport::Sse;
        }
        assert_ne!(
            config_fingerprint(&http),
            config_fingerprint(&sse),
            "传输模式变化必须换指纹"
        );
    }
}
