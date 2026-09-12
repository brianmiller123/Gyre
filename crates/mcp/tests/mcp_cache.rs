//! MCP 启动预算 + 缓存快照集成测试（公开 API 面）。
//!
//! 覆盖 H15「Deferred 延迟连接」：预算内未完成握手的 server 不阻塞启动，先用**适用**
//! 缓存快照（版本 + 配置指纹 + TTL 三中）注册 Deferred 工具（元信息可见、执行时等连接
//! 就绪），后台连接完成后无缝换成 live 工具并触发变更监听器。
//!
//! 桩：python3 子进程充当 JSON-RPC server（无 python3 的环境跳过，对齐 `mcp_stdio.rs`）。

use std::collections::HashMap;
use std::time::Duration;

use agent_config::{McpConfig, McpServerConfig, McpStdioConfig};
use agent_mcp::cache::{self, CACHE_VERSION, CachedTool, CachedTools};
use agent_mcp::{McpLoadOptions, McpRegistry};
use agent_tools::Tool as _;
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
    elif req.get("method") == "prompts/list":
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"prompts": [
            {"name": "review", "description": "代码评审", "arguments": [{"name": "path", "required": True}]}
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
    let mut env = HashMap::new();
    if let Some(script) = script {
        env.insert("PY_SCRIPT".to_string(), script.to_string());
    }
    McpServerConfig::Stdio(McpStdioConfig {
        command: command.to_string(),
        args: vec!["-c".to_string(), script.unwrap_or("pass").to_string()],
        env,
        timeout_ms: Some(10_000),
    })
}

/// 轮询等待谓词成立（后台连接完成的异步边界；上限 10s）。
async fn wait_until(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// 预算内完成的 server：工具落盘快照（注册表自行写缓存，无需装配层插手）。
#[tokio::test]
async fn connected_server_snapshot_is_written_by_registry() {
    if !python3_available() {
        eprintln!("跳过：无 python3");
        return;
    }
    let dir = tmp_dir("snapshot");
    let cfg = McpConfig {
        servers: HashMap::from([("py".to_string(), stdio_cfg("python3", Some(PY_SERVER)))]),
    };
    let opts = McpLoadOptions {
        startup_budget: Duration::from_secs(10),
        cache_dir: Some(dir.clone()),
        roots: Vec::new(),
    };
    let registry = McpRegistry::load(&cfg, &opts).await;

    assert_eq!(registry.server_names(), vec!["py".to_string()]);
    let tools = registry.tools();
    assert_eq!(tools.len(), 1);
    assert!(!tools[0].is_deferred(), "预算内连上 → live 工具");

    let snapshot = cache::load(&dir, "py").expect("连接成功后应写入快照");
    assert_eq!(snapshot.version, CACHE_VERSION);
    assert_eq!(
        snapshot.config_hash,
        cache::config_fingerprint(&cfg.servers["py"]),
        "快照指纹应对应本 server 配置"
    );
    // 快照保存 server 端原始工具名（别名每次派生，不落盘）。
    assert_eq!(snapshot.tools[0].name, "py_echo");
    assert_eq!(snapshot.tools[0].description, "回声");

    // prompts/list 元信息进入注册表（斜杠命令面）。
    let prompts = registry.prompts();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].0, "py");
    assert_eq!(prompts[0].1.name, "review");
    assert!(prompts[0].1.arguments[0].required);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 预算耗尽（1ms 内不可能完成 python3 握手）→ 快照回填为 Deferred 工具，
/// 后台连接完成后自动换成 live 并触发监听器。
#[tokio::test]
async fn budget_exceeded_backfills_deferred_tools_then_swaps_to_live() {
    if !python3_available() {
        eprintln!("跳过：无 python3");
        return;
    }
    let dir = tmp_dir("deferred");
    let cfg = McpConfig {
        servers: HashMap::from([("py".to_string(), stdio_cfg("python3", Some(PY_SERVER)))]),
    };
    // 预置一份「适用」快照：上次会话留下的工具清单（server 端工具名 + 描述）。
    let server_cfg = cfg.servers["py"].clone();
    cache::store(
        &dir,
        &CachedTools {
            server: "py".to_string(),
            tools: vec![CachedTool {
                name: "py_echo".to_string(),
                description: "上次会话的回声".to_string(),
                input_schema: json!({"type": "object"}),
            }],
            cached_at_ms: cache::now_ms(),
            config_hash: cache::config_fingerprint(&server_cfg),
            version: CACHE_VERSION,
        },
    )
    .unwrap();

    let opts = McpLoadOptions {
        startup_budget: Duration::from_millis(1),
        cache_dir: Some(dir.clone()),
        roots: Vec::new(),
    };
    let started = std::time::Instant::now();
    let registry = McpRegistry::load(&cfg, &opts).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "慢 server 不得把启动拖到握手完成"
    );

    // 启动即刻：工具元信息已对模型可见，但处于 Deferred 形态。
    let tools = registry.tools();
    assert_eq!(tools.len(), 1, "缓存快照应回填为 Deferred 工具");
    assert!(tools[0].is_deferred(), "预算外 server 的工具应为 Deferred");
    assert_eq!(tools[0].name(), "mcp__py_echo");
    assert_eq!(tools[0].description(), "上次会话的回声");
    assert_eq!(registry.deferred_servers(), vec!["py".to_string()]);
    assert_eq!(
        registry.server_status()[0].1.state,
        agent_mcp::McpConnState::Connecting,
        "预算外 server 状态应为连接中"
    );

    // 后台连接完成后：换 live 工具、状态转 connected、Deferred 名单清空。
    let swapped = wait_until(|| registry.tools().first().is_some_and(|t| !t.is_deferred())).await;
    assert!(swapped, "后台连接完成后应换成 live 工具");
    assert_eq!(
        registry.server_status()[0].1.state,
        agent_mcp::McpConnState::Connected
    );
    assert!(registry.deferred_servers().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 快速失败的 server：不阻塞启动、不注册工具、状态标记 failed 并留错误原因。
#[tokio::test]
async fn fast_failure_records_failed_status_without_tools() {
    let dir = tmp_dir("failed");
    let cfg = McpConfig {
        servers: HashMap::from([(
            "ghost".to_string(),
            stdio_cfg("gyre-definitely-not-a-binary", None),
        )]),
    };
    let opts = McpLoadOptions {
        startup_budget: Duration::from_secs(5),
        cache_dir: Some(dir.clone()),
        roots: Vec::new(),
    };
    let registry = McpRegistry::load(&cfg, &opts).await;

    assert!(registry.is_empty(), "连接失败的 server 不注册工具");
    assert!(registry.server_names().is_empty());
    let status = &registry.server_status()[0];
    assert_eq!(status.0, "ghost");
    assert_eq!(status.1.state, agent_mcp::McpConnState::Failed);
    assert!(
        status.1.last_error.is_some(),
        "失败原因应写入状态供 /mcp status 展示"
    );
    assert!(
        cache::load(&dir, "ghost").is_none(),
        "失败 server 不应有快照"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 缓存不适用（配置指纹不符）时不得回填：宁可空手也不把过期清单摆给模型。
#[tokio::test]
async fn stale_snapshot_with_other_config_is_not_used() {
    if !python3_available() {
        eprintln!("跳过：无 python3");
        return;
    }
    let dir = tmp_dir("stale");
    let cfg = McpConfig {
        servers: HashMap::from([("py".to_string(), stdio_cfg("python3", Some(PY_SERVER)))]),
    };
    // 快照对应的是另一份配置（换个命令）→ 指纹不符。
    cache::store(
        &dir,
        &CachedTools {
            server: "py".to_string(),
            tools: vec![CachedTool {
                name: "py_echo".to_string(),
                description: "旧配置的工具".to_string(),
                input_schema: json!({"type": "object"}),
            }],
            cached_at_ms: cache::now_ms(),
            config_hash: cache::config_fingerprint(&stdio_cfg("uvx", None)),
            version: CACHE_VERSION,
        },
    )
    .unwrap();

    let opts = McpLoadOptions {
        startup_budget: Duration::from_millis(1),
        cache_dir: Some(dir.clone()),
        roots: Vec::new(),
    };
    let registry = McpRegistry::load(&cfg, &opts).await;
    assert!(
        registry.tools().is_empty(),
        "配置已变的快照不得作为工具元信息回填"
    );

    // 后台连接仍会完成：工具随后以 live 形态出现。
    let appeared = wait_until(|| !registry.tools().is_empty()).await;
    assert!(appeared, "后台连接完成后应注册 live 工具");
    assert!(!registry.tools()[0].is_deferred());

    let _ = std::fs::remove_dir_all(&dir);
}
