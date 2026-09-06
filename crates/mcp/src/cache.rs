//! MCP 工具清单磁盘缓存：server 连接失败时回填上一次成功拉取的工具列表。
//!
//! 布局：`<cache_dir>/mcp-cache/<server>.json`。写入走「临时文件 + 原子 rename」，
//! 读取对缺失/畸形内容容错（→ [`None`]），保证消费方永不因缓存损坏而失败。
//! 缓存纯 best-effort：任何读写失败都不影响 MCP 主流程。
//!
//! 装配见 [`crate::registry`]（连接成功快照 / 失败回填）。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 缓存子目录名（位于 `<cache_dir>` 下）。
const CACHE_SUBDIR: &str = "mcp-cache";

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
    /// 快照时间（Unix 毫秒；用于装配层标注 stale）。
    pub cached_at_ms: u128,
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

/// 当前 Unix 时间（毫秒）。时钟早于 epoch 时返回 0（仅影响 stale 展示，不影响功能）。
pub(crate) fn now_ms() -> u128 {
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

    fn sample(server: &str) -> CachedTools {
        CachedTools {
            server: server.to_string(),
            tools: vec![
                CachedTool {
                    name: format!("mcp__{server}_echo"),
                    description: "回声".to_string(),
                    input_schema: json!({"type": "object", "properties": {}}),
                },
                CachedTool {
                    name: format!("mcp__{server}_fetch"),
                    description: "抓取".to_string(),
                    input_schema: json!({"type": "object", "properties": {"url": {"type": "string"}}}),
                },
            ],
            cached_at_ms: 1_700_000_000_000,
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
}
