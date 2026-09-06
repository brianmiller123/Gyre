//! MCP 工具缓存装配：连接成功时快照工具清单到磁盘，连接失败时回填 stale 缓存。
//!
//! [`crate::tool::McpRegistry`] 的连接失败路径（`McpRegistry::load` 内单 server
//! 失败即跳过）不暴露失败清单，故本模块只提供纯装配函数，由装配层
//! （crates/cli）在注册表就绪后接线——数据源全部来自注册表公开 API：
//!
//! - 快照：`McpRegistry::tools()`（含 `tools/list_changed` 重拉后的最新视图，
//!   装配层可在变更监听器里重复调用）→ [`store_tools`]
//! - 回填：`McpRegistry::server_names()` 判定未连上的 server → [`hydrate_from_cache`]
//!
//! 注意：回填的缓存工具只有元信息（无可用连接），不可执行；仅用于装配层向
//! 模型/用户展示「该 server 上次提供过这些工具（stale）」。

use std::collections::BTreeMap;
use std::path::Path;

use agent_tools::Tool;

use crate::cache::{self, CachedTool, CachedTools};
use crate::tool::McpRegistry;

/// 把注册表当前工具视图按 server 分组快照写入磁盘缓存（best-effort）。
///
/// 入参用 [`McpRegistry::tools()`] 的返回值（跨 server 去重后的合并视图）；
/// 每 server 写一个缓存文件，单个写入失败只 `warn!` 不影响其余 server。
/// 连接成功后与每次清单刷新（`tools/list_changed` 重拉完成）后调用即可保持缓存新鲜。
pub fn store_tools(cache_dir: &Path, tools: &[crate::tool::McpTool]) {
    if tools.is_empty() {
        return;
    }
    // server 名 → 缓存工具（BTreeMap 保证多 server 间写入顺序稳定）。
    let mut grouped: BTreeMap<&str, Vec<CachedTool>> = BTreeMap::new();
    for tool in tools {
        grouped.entry(tool.server()).or_default().push(CachedTool {
            name: Tool::name(tool).to_string(),
            description: Tool::description(tool).to_string(),
            input_schema: Tool::schema(tool),
        });
    }
    let cached_at_ms = cache::now_ms();
    for (server, entries) in grouped {
        let cached = CachedTools {
            server: server.to_string(),
            tools: entries,
            cached_at_ms,
        };
        if let Err(e) = cache::store(cache_dir, &cached) {
            tracing::warn!(server, error = %e, "MCP 工具缓存写入失败");
        }
    }
}

/// server 未连接时从磁盘缓存回填其上一次工具清单（stale）；已连接或无有效缓存 → [`None`]。
///
/// 回填即告警（`warn!`）：带工具数与快照时间，供日志定位 stale 来源。
///
/// 调用形态（装配层，`McpRegistry::load` 之后对每个配置过的 server 询问）：
///
/// ```text
/// let registry = McpRegistry::load(&cfg).await;
/// for name in cfg.servers.keys() {
///     if let Some(cached) = hydrate_from_cache(&cache_dir, &registry, name) {
///         // server 未连上但缓存可用：cached.tools 为 stale 元信息（不可执行），
///         // cached.cached_at_ms 用于展示「数据取自 <时间> 的快照」。
///     }
/// }
/// ```
///
/// 已连接的 server 一律返回 [`None`]（有实时清单，无需回填），因此重复调用安全。
#[must_use]
pub fn hydrate_from_cache(
    cache_dir: &Path,
    registry: &McpRegistry,
    server: &str,
) -> Option<CachedTools> {
    if registry.server_names().contains(&server) {
        // 已连接：实时清单可用，无需 stale 缓存。
        return None;
    }
    let cached = cache::load(cache_dir, server)?;
    tracing::warn!(
        server,
        tools = cached.tools.len(),
        cached_at_ms = cached.cached_at_ms,
        "MCP server 未连接，回填 stale 工具缓存"
    );
    Some(cached)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CachedTool;
    use serde_json::json;

    /// 唯一临时目录（对齐 crate 内测试先例：纳秒名 + 用后清理）。
    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("gyre-mcp-registry-{tag}-{n}"))
    }

    fn seed(dir: &Path, server: &str) {
        let c = CachedTools {
            server: server.to_string(),
            tools: vec![CachedTool {
                name: format!("mcp__{server}_echo"),
                description: "回声".to_string(),
                input_schema: json!({"type": "object"}),
            }],
            cached_at_ms: 1_700_000_000_000,
        };
        cache::store(dir, &c).unwrap();
    }

    #[test]
    fn hydrate_backfills_unconnected_server_from_cache() {
        let dir = tmp_dir("backfill");
        seed(&dir, "ghost");
        // 空注册表：无任何已连 server → 「ghost 未连接」→ 回填缓存。
        let registry = McpRegistry::default();
        let cached = hydrate_from_cache(&dir, &registry, "ghost").expect("应回填 stale 缓存");
        assert_eq!(cached.server, "ghost");
        assert_eq!(cached.tools.len(), 1);
        assert_eq!(cached.cached_at_ms, 1_700_000_000_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hydrate_without_cache_returns_none() {
        let dir = tmp_dir("nocache");
        let registry = McpRegistry::default();
        assert!(hydrate_from_cache(&dir, &registry, "never-seen").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_tools_empty_is_noop() {
        let dir = tmp_dir("noop");
        let registry = McpRegistry::default();
        store_tools(&dir, &registry.tools());
        assert!(!dir.join("mcp-cache").exists(), "无工具时不应建缓存目录");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
