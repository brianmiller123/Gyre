//! MCP stdio 传输集成测试：python3 子进程充当行分隔 JSON-RPC server。
//!
//! 无 python3 的环境跳过（工具面回归保护，非硬依赖）。

use std::collections::HashMap;

use agent_config::McpServerConfig;
use agent_mcp::McpClient;
use serde_json::json;

/// 主动向客户端发 `ping` / `roots/list` 的 server（H16）：`tools/list` 的响应**延后**到
/// 两个请求都收到客户端响应之后，并以工具描述回带结果——客户端不应答即超时失败。
const PY_SERVER_WITH_SERVER_REQUESTS: &str = r#"
import sys, json
state = {"ping": None, "roots": None}
pending_tools = None
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method is None:
        # 客户端对 server→client 请求的响应（有 id、无 method）。
        rid = msg.get("id")
        if rid == "srv-ping":
            state["ping"] = msg
        elif rid == "srv-roots":
            state["roots"] = msg
        if pending_tools is not None and state["ping"] is not None and state["roots"] is not None:
            report = {
                "ping_result": state["ping"].get("result"),
                "roots": state["roots"].get("result", {}).get("roots"),
            }
            print(json.dumps({"jsonrpc": "2.0", "id": pending_tools, "result": {"tools": [
                {"name": "report", "description": json.dumps(report), "inputSchema": {"type": "object"}}
            ]}}), flush=True)
            pending_tools = None
        continue
    if method == "initialize":
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": {"protocolVersion": "2024-11-05", "capabilities": {}}}), flush=True)
    elif method == "tools/list":
        print(json.dumps({"jsonrpc": "2.0", "id": "srv-ping", "method": "ping"}), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "id": "srv-roots", "method": "roots/list"}), flush=True)
        pending_tools = msg["id"]
"#;

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

/// H16 端到端：server 主动发 `ping` / `roots/list`，客户端必须回响应帧（此前静默丢弃）。
#[tokio::test]
async fn stdio_answers_server_initiated_requests() {
    if !python3_available() {
        eprintln!("跳过：无 python3");
        return;
    }
    let cfg = McpServerConfig::Stdio(agent_config::McpStdioConfig {
        command: "python3".to_string(),
        args: vec![
            "-u".to_string(),
            "-c".to_string(),
            PY_SERVER_WITH_SERVER_REQUESTS.to_string(),
        ],
        env: HashMap::new(),
        timeout_ms: Some(10_000),
    });
    let client = McpClient::connect(&cfg).await.expect("connect");
    client.set_roots(vec![agent_mcp::McpRoot::file(
        std::path::Path::new("/workspace"),
        "workspace",
    )]);
    client.initialize().await.expect("initialize");
    // server 的 tools/list 响应被延后到 ping / roots/list 都收到应答之后：
    // 若客户端不应答（修复前行为），此处会等到请求超时并失败。
    let tools = client.list_tools().await.expect("list_tools");
    let report: serde_json::Value =
        serde_json::from_str(&tools[0].description).expect("server 回带的报告应可解析");
    assert_eq!(report["ping_result"], json!({}), "ping 必须被应答");
    assert_eq!(report["roots"][0]["uri"], "file:///workspace");
    assert_eq!(report["roots"][0]["name"], "workspace");
    client.close().await;
}
