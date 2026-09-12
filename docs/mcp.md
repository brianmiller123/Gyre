# MCP（Model Context Protocol）接入指南

本文是**使用向**参考：如何接服务器、支持哪些传输、能力如何门控、常见故障怎么定位。
字段级 schema 与全部注释示例以 [`config.example.toml`](../config.example.toml) 的
`[mcp]` 段为准（本文不复制字段表，避免两处漂移）。

## 1. 三种接法

| 方式 | 位置 | 适用 |
|---|---|---|
| TOML 段 | `[mcp.servers.<name>]`（项目 `.agent/config.toml` 或用户级 `config.toml`） | 本机固定配置（stdio / HTTP / SSE 均可） |
| 外部 JSON | `mcp.json`：项目 `<cwd>/.agent/mcp.json` → `<cwd>/mcp.json` → `<cwd>/.mcp.json`（后者优先）、用户 `<config_dir>/agent/mcp.json` | 复用 Claude/Cursor 既有配置 |
| CLI / REPL | `agent mcp add\|remove\|enable\|disable\|list\|status`、REPL `/mcp ...` | 增删与启停（`--project` 写项目级） |

优先级：`[mcp.servers]` **胜过**同名 `mcp.json` 条目；`disabledServers` 与每 server 的
`enabled = false` 是拒绝名单，**压过一切来源**。

```toml
[mcp.servers.filesystem]
command = "npx"                                     # 含 command → stdio 传输
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.servers.remote]
url = "http://127.0.0.1:3000/mcp"                   # 含 url → HTTP 传输
# type = "sse"                                      # legacy HTTP+SSE（2024-11-05 前规范）
timeout_ms = 30000

[mcp.servers.remote.headers]
Authorization = "Bearer ${MCP_TOKEN}"               # 支持 ${VAR} / ${VAR:-default}
```

Remote MCP 可用 OAuth（`agent mcp login <name>`，凭据落 `oauth.toml`）：缺省走
RFC 9728/8414 自动发现 + RFC 7591 动态客户端注册，过期/401 自动刷新重试；不开放 DCR
的服务商可显式给 `client_id`（见 `config.example.toml` 注释）。

## 2. 能力门控（为什么有的 server 只有 tools）

连接时会解析 server 的 `initialize` 能力：

- **tools** → 注册为 `mcp__<server>__<tool>` 工具，进入模型工具面；
- **prompts** → 注册为斜杠命令 `/<server>:<prompt>`（REPL 与 RPC 的
  `available_commands` 都能看到，缺参数时提示 `missing required: ...`）；
- **resources** → 通过 `resources/list` / `resources/read` 按需读取；
- 只有 resources 而没有 tools 的 server **也能连上**（早期版本会因未解析能力而连接失败）。

Gyre 会把自己的会话工作目录登记为 MCP **roots**（`roots/list` 能拿到范围），并应答
server 发起的 `ping` / `roots/list` 请求；这两条是 server→client 方向的请求，客户端
不应主动调用 `session/request_permission` 这类反向方法。

连接失败/超时的 server **不影响启动**：默认 250ms 启动预算内未完成握手的 server 转入
后台连接，有可用缓存快照时先以 **deferred** 工具注册（元信息对模型可见，执行时等连接
就绪；失败回填含 server 名与快照来源的可读错误），连接完成后无缝替换为 live 工具。

## 3. 排障

| 现象 | 检查 |
|---|---|
| server 没出现在工具面 | `agent mcp status`（含 `connecting` / `failed` 态）、`/mcp list`；确认不在 `disabledServers` |
| 工具可见但调用报错 | 多为 deferred 工具：等后台连接完成，或看错误文本里的 server 名与快照来源 |
| stdio server 起不来 | 直接手工执行同一 `command`/`args`；确认 `env` 里有它需要的变量（`.env` 也会被加载） |
| HTTP 401 | `headers.Authorization` 的 `${VAR}` 是否展开（`${VAR:-default}` 可用；`GYRE_DOTENV=off` 时不会读 `.env`） |
| 只想用 prompts | 连接后 `/mcp` 列出 prompt 数量；斜杠命令名是 `/<server>:<prompt>` |

## 4. 相关代码

- 配置与来源合并：`crates/config/src/config.rs`（`McpConfig` / `McpServerConfig`）、
  `crates/config/src/mcp_json.rs`（外部 JSON 发现）
- 客户端与传输：`crates/mcp/src/{client,stdio,http,sse}.rs`（能力解析、roots、请求分发）
- 工具注册：`crates/mcp/src/tool.rs`（deferred 工具与缓存快照）
- 命令面：`crates/cli/src/manage.rs`（`agent mcp ...`）、`crates/cli/src/repl.rs`（`/mcp`）
