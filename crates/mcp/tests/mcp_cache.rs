//! MCP 工具缓存装配集成测试：连接成功快照 + 连接失败回填（公开 API 面）。
//!
//! 失败注入沿用 crate 内先例：stdio 配置指向不存在的命令（连接失败 → server 被
//! `McpRegistry::load` 跳过），成功路径用 python3 子进程充当 JSON-RPC server
//!（无 python3 的环境跳过，对齐 `mcp_stdio.rs`）。

use std::collections::HashMap;

use agent_config::{McpConfig, McpServerConfig, McpStdioConfig};
use agent_mcp::cache;
use agent_mcp::{McpRegistry, hydrate_from_cache, store_tools};
use serde_json::json;

const PY_SERVER: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    if "id" not in req:
        continue
    if req.get("method") == "initialize":
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"protocolVersion": "2024-11-05"}}), flush=True)
    elif req.get("method") == "tools/list":
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [
            {"name": "py_echo", "description": "回声", "inputSchema": {"type": "object"}}
        ]}}), flush=True)
"#;

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// 唯一临时目录（对齐 crate 内测试先例：纳秒名 + 用后清理）。
fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("gyre-mcp-cache-it-{tag}-{n}"))
}

fn stdio_cfg(command: &str, script: Option<&str>) -> McpServerConfig {
    let args = match script {
        Some(s) => vec!["-u".to_string(), "-c".to_string(), s.to_string()],
        None => vec![],
    };
    McpServerConfig::Stdio(McpStdioConfig {
        command: command.to_string(),
        args,
        env: HashMap::new(),
        timeout_ms: None,
    })
}

#[tokio::test]
async fn cache_snapshot_connected_and_backfill_failed_server() {
    if !python3_available() {
        eprintln!("跳过：无 python3");
        return;
    }
    let dir = tmp_dir("assembly");
    let mut servers = HashMap::new();
    servers.insert("py".to_string(), stdio_cfg("python3", Some(PY_SERVER)));
    // 失败注入：命令不存在 → 连接失败 → server 被跳过。
    servers.insert(
        "ghost".to_string(),
        stdio_cfg("gyre-definitely-not-a-binary", None),
    );
    let cfg = McpConfig { servers };

    let registry = McpRegistry::load(&cfg).await;
    // py 已连接，ghost 连接失败被跳过。
    assert_eq!(
        registry.server_names(),
        vec!["py"],
        "失败的 ghost 不应出现在已连列表"
    );

    // 连接成功后的快照：py 的工具落盘，ghost 无工具不落盘。
    store_tools(&dir, &registry.tools());
    let py = cache::load(&dir, "py").expect("py 快照应写入缓存");
    assert_eq!(py.tools.len(), 1);
    assert_eq!(py.tools[0].name, "mcp__py_echo"); // 工具名自带 server 前缀 → 去冗余（见 mint_tool_name）
    assert_eq!(py.tools[0].description, "回声");
    assert_eq!(py.tools[0].input_schema, json!({"type": "object"}));
    assert!(
        cache::load(&dir, "ghost").is_none(),
        "失败 server 不应有快照"
    );

    // 失败回填：ghost 未连接 → 回填预置的 stale 缓存；py 已连接 → None。
    let stale = agent_mcp::cache::CachedTools {
        server: "ghost".to_string(),
        tools: vec![agent_mcp::cache::CachedTool {
            name: "mcp__ghost_old".to_string(),
            description: "上次提供的工具".to_string(),
            input_schema: json!({"type": "object"}),
        }],
        cached_at_ms: 1_700_000_000_000,
    };
    cache::store(&dir, &stale).unwrap();
    let backfilled = hydrate_from_cache(&dir, &registry, "ghost").expect("ghost 应回填 stale 缓存");
    assert_eq!(backfilled.server, "ghost");
    assert_eq!(backfilled.tools.len(), 1);
    assert_eq!(backfilled.cached_at_ms, 1_700_000_000_000);
    assert!(
        hydrate_from_cache(&dir, &registry, "py").is_none(),
        "已连接 server 不应回填 stale 缓存"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
