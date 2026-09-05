//! MCP stdio 传输集成测试：python3 子进程充当行分隔 JSON-RPC server。
//!
//! 无 python3 的环境跳过（工具面回归保护，非硬依赖）。

use std::collections::HashMap;

use agent_config::McpServerConfig;
use agent_mcp::McpClient;
use serde_json::json;

const PY_SERVER: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method")
    if "id" not in req:
        continue
    if method == "initialize":
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"protocolVersion": "2024-11-05"}}), flush=True)
    elif method == "tools/list":
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [
            {"name": "py_echo", "description": "回声", "inputSchema": {"type": "object"}}
        ]}}), flush=True)
    elif method == "tools/call":
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": {
            "content": [{"type": "text", "text": "stdio-ok"}]}}), flush=True)
"#;

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[tokio::test]
async fn stdio_roundtrip_over_subprocess() {
    if !python3_available() {
        eprintln!("跳过：无 python3");
        return;
    }
    let cfg = McpServerConfig::Stdio(agent_config::McpStdioConfig {
        command: "python3".to_string(),
        args: vec!["-u".to_string(), "-c".to_string(), PY_SERVER.to_string()],
        env: HashMap::new(),
        timeout_ms: None,
    });
    let client = McpClient::connect(&cfg).await.expect("connect");
    client.initialize().await.expect("initialize");
    let tools = client.list_tools().await.expect("list_tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "py_echo");
    let out = client
        .call_tool("py_echo", json!({}))
        .await
        .expect("call_tool");
    assert_eq!(out, "stdio-ok");
    client.close().await;
}
