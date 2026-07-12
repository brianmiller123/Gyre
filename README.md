# agent-project

> **高性能 Rust 智能体框架**——融合 Zoo-Code 的高阶认知与流程控制，以及 oh-my-pi 的底层代码操控精确性，在纯 Rust 生态中实现现代化 AI 编程智能体。

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-blue)](rust-toolchain.toml)
[![Edition](https://img.shields.io/badge/edition-2024-purple)](Cargo.toml)

---

## 📋 核心功能

agent-project 是一个**生产级 AI 编程智能体框架**，提供完整的「感知→推理→行动」闭环：

### 🤖 智能体引擎
- **五态状态机**（NoTask → Running → Streaming → WaitingForInput → Idle）：移植 Zoo-Code 成熟的状态模型，精确控制智能体生命周期。
- **消息驱动架构**：`say`（信息性）/ `ask`（交互阻塞）消息模型，支持审批网关与用户回执。
- **多模式**：`code`（编码）/ `architect`（架构）/ `ask`（问答）/ `debug`（调试），模式决定 system prompt 与工具子集。
- **Steering 机制**：运行时中途注入消息，动态干预智能体推理方向。
- **软工具需求**：先提醒后强制，保护 Provider 前缀缓存。

### 🛠️ 工具链
- **核心工具**：`read_file` / `write_file` / `str_replace` / `apply_diff` / `list_files` / `run_command` / `grep` / `glob`
- **AST 代码操控**（可选）：`replace_block`（tree-sitter 句法块替换）/ `ast_search` / `ast_rewrite`（ast-grep 结构化搜索重写）
- **LSP 集成**（可选）：诊断 / 跳转到定义 / 查找引用 / 重命名符号
- **Hashline DSL**（可选）：行锚定批量编辑，多文件多区间一次性应用
- **PTY 伪终端**（可选）：`run_pty_command`，支持交互式终端应用（top / vim / REPL）
- **图像处理**（可选）：`read_image` / `image_gen`（DALL·E 或兼容 API）
- **GitHub 集成**（可选）：PR / Issue / Actions 查询与操作，GraphQL 查询
- **MCP 协议**：兼容 Model Context Protocol，可挂载任意 MCP server 工具

### 🧠 上下文管理
- **AppendOnlyLog**：消息只追加，唯一变异路径是压缩时合法 `replaceTail`
- **StablePrefix 指纹化**：system prompt + tool spec 一次快照、SHA-256 字节指纹冻结，最大化 Provider 端前缀缓存命中率
- **三级压缩**：Shake（抖动去冗余）→ Summarize（LLM handoff 摘要）→ Prune（窗口兜底），配合 Tool-Protection 保护工具结果
- **Token 计数**：基于 `tiktoken-rs`，精确追踪每轮用量

### 🔌 LLM 多 Provider
- **ProviderRegistry**：单一入口按 `model.api` 路由，内置 `ProviderCallContext` 鉴权与并发限流
- **OpenAI Chat Completions**：覆盖 OpenAI / 兼容网关 / 本地 vLLM / Ollama
- **DeepSeek**：DeepSeek Chat / Reasoner
- **GLM（智谱 / Z.ai）**：官方 API 完整支持 —— `thinking` 开关、`reasoning_content` 思考回显、`preserveReasoning` 多轮续接
- **开放适配器**：实现 `LlmProvider` trait + 一行注册即可接入新 Provider
- **思考模式**（Thinking/Reasoning）：支持 `deepseek-reasoner` / GLM 思考模型（glm-4.7+/glm-5+）的思考 token 预算

### 🌐 双前端
- **CLI / REPL**：`rustyline` 行编辑 + Tab 补全 + Markdown 流式渲染 + 色彩高亮
- **Web UI**（React + TypeScript + Vite）：实时 WebSocket 双向通信，带宽优化线协议（增量优先）
- **会话管理**：持久化 JSONL 历史，支持恢复 / fork / 导出
- **多语种（i18n）**：CLI 与 Web 默认跟随系统语言，内置中文 / English / Русский / 日本語，可在设置或配置中切换，扩展新语言仅需加一个资源文件

### 🔌 ACP 协议（编辑器集成）
- **Agent Client Protocol v1**：遵循 [agentclientprotocol.com](https://agentclientprotocol.com) 标准，为 Zed Editor 等兼容客户端提供 JSON-RPC 2.0 + SSE/stdio 接口
- **双传输模式**：`stdio`（编辑器作为子进程调用，零端口占用）/ `http`（HTTP+SSE，与 Web 前端同端口复用）
- **三端等价**：ACP 与 CLI、Web 前端共享同一份会话管理与审批机制，同一会话可被多客户端同时订阅
- **标准事件流**：`session/update` 推送 `agent_message_chunk` / `agent_thought_chunk` / `tool_call` / `usage_update` 等 ACP 标准事件
- **能力声明**：`initialize` 握手时如实声明 `promptCapabilities`（image 支持）与 `mcpCapabilities`（stdio 支持），严格客户端 schema 校验通过

### 🔄 多 Agent 编排
- **子 Agent 委派**（TaskTool）：父 Agent 可委派子任务给子 Agent，支持并发护栏与独立 Token 预算
- **Swarm 多代理**：YAML 定义 DAG 工作流，波内并行执行，管道进度回调
- **Supervisor 监控**：实时追踪所有子 Agent 状态（运行/成功/失败），CLI Dashboard 与 Web 仪表盘

### 🤝 协同中继
- **端到端加密**：AES-256-GCM 密封字节，中继服务器盲视（仅按 room_id 路由）
- **WebSocket 桥接**：多用户可加入同一房间，实时共享智能体会话

### 📐 架构设计
- **Ports & Adapters（六边形架构）**：所有核心依赖以 `Arc<dyn Trait>` 注入，循环对传输/Provider/Tool 完全透明
- **零侵入扩展**：`inventory` 编译期插件自荐注册，新增 Provider/Tool 无需修改中央注册清单
- **单向依赖**：`core` 零业务依赖，`agent` 仅依赖 Trait 而非具体实现，依赖链单向无环
- **事件驱动**：循环产出 `Stream<Item = AgentEvent>`，前端/CLI 统一订阅

---

## 🚀 快速开始

### 前置要求

- Rust 工具链 1.85+（见 [`rust-toolchain.toml`](rust-toolchain.toml)）
- 一个 LLM API key（OpenAI / Anthropic / DeepSeek / GLM / 本地 vLLM）

### 安装与运行

```bash
# 克隆仓库
git clone https://github.com/agent-project/agent-project.git
cd agent-project

# 复制配置并填入 API key
cp config.example.toml .agent/config.toml

# 构建（Release 模式）
cargo build --release

# 运行单次任务
./target/release/agent "列出当前目录文件并用树状图展示"

# 或启动交互式 REPL
./target/release/agent

# 启动 Web 服务
./target/release/agent --serve
```

### 配置文件

配置使用 TOML 格式，分层加载（后者覆盖前者）：

1. **用户级**：`~/.config/agent/config.toml`（Windows `%APPDATA%\agent`）
2. **项目级**：`<cwd>/.agent/config.toml`

支持多模型 profile，运行时通过 `--model <alias>` 或 `/model <alias>` 命令切换：

```toml
[default_model]
id               = "gpt-4o-mini"
api              = "openai-completions"
base_url         = "https://api.openai.com/v1"
api_key          = "${OPENAI_API_KEY}"
temperature      = 0.2
max_output_tokens = 4096

[[models]]
id        = "deepseek-chat"
alias     = "ds"
api       = "openai-completions"
base_url  = "https://api.deepseek.com"
api_key   = "${DEEPSEEK_API_KEY}"
```

详细配置选项见 [`config.example.toml`](config.example.toml)。

### 🌐 多语种（i18n）

CLI 与 Web 默认**跟随系统语言**（CLI 读 `LANG` / `LC_ALL` / `LC_MESSAGES`；Web 读 `navigator.languages`），内置 4 种语言：

| 代码 | 语言 | CLI 资源 | Web 资源 |
| ---- | ---- | -------- | -------- |
| `en` | English  | [`crates/i18n/locales/en.json`](crates/i18n/locales/en.json) | [`web/c5-ui/src/lib/locales.ts`](web/c5-ui/src/lib/locales.ts) |
| `zh` | 中文     | `crates/i18n/locales/zh.json` | 同上 |
| `ru` | Русский  | `crates/i18n/locales/ru.json` | 同上 |
| `ja` | 日本語   | `crates/i18n/locales/ja.json` | 同上 |

**配置覆盖**（可选，留空则自动探测系统语言）：

```toml
# 置于 config.toml 顶层
language = "zh"   # en / zh / ru / ja
```

- **CLI**：在 [`main.rs`](crates/cli/src/main.rs) 启动期先以系统语言初始化（`agent_i18n::init(None)`），配置加载后若指定 `language` 再覆盖。
- **Web**：在 [设置面板](web/c5-ui/src/components/agent/SettingsPanel.tsx) 的「外观 → 语言」下拉切换；偏好持久化于 `localStorage`，选「Auto」则跟随系统并在系统语言变化时即时切换。

#### 新增一种语言（两处对称）

1. **CLI**：在 [`crates/i18n/locales/`](crates/i18n/locales/) 复制 `en.json` 为 `<code>.json` 并翻译；再在 [`lib.rs`](crates/i18n/src/lib.rs) 的 `LOCALE_FILES` 与 `SUPPORTED`、`match_supported` 各登记一行。
2. **Web**：在 [`locales.ts`](web/c5-ui/src/lib/locales.ts) 追加一个 `Dict` 并加入 `locales`/`SUPPORTED_LOCALES`，并在各语言里补 `lang.<code>` 的母语自显示名。

回退链统一为：当前语言 → 英文 → key 本身（开发期可见缺失，不报错）。

---

## 📦 模块结构

```
agent-project/
├── Cargo.toml                  # workspace 配置
├── crates/
│   ├── core/                   # 契约层：类型 + Trait + 错误（零业务依赖）
│   ├── config/                 # TOML 分层配置 + 模型 profile + 审批规则引擎
│   ├── llm/                    # LLM Provider 适配器（OpenAI/Anthropic/DeepSeek）
│   ├── context/                # 上下文管理（AppendOnlyLog + StablePrefix + 压缩）
│   ├── prompt/                 # System prompt 模板引擎
│   ├── tools/                  # 工具集（文件系统/AST/LSP/搜索/Shell/GitHub/图像）
│   ├── agent/                  # 执行循环状态机（智能体核心）
│   ├── server/                 # HTTP/WebSocket 服务（axum）
│   ├── cli/                    # CLI 二进制入口（REPL + 命令调度）
│   ├── ast/                    # AST 操控（tree-sitter + ast-grep）
│   ├── search/                 # 代码搜索（grep/glob/fd/highlight/tokens）
│   ├── mcp/                    # MCP 协议客户端
│   ├── memory/                 # 跨会话长期记忆
│   ├── skills/                 # Skill 系统（file-backed 技能）
│   ├── swarm/                  # 多 Agent 编排（DAG 管道）
│   ├── collab/                 # 协同中继（端到端加密）
│   ├── hashline/               # Hashline DSL 批量编辑
│   ├── pty/                    # PTY 伪终端工具
│   ├── lsp/                    # LSP 语言服务器客户端
│   ├── iso/                    # 文件系统隔离（沙箱 diff）
│   ├── supervisor/             # 子 Agent 监控总线
│   ├── i18n/                   # 多语种消息目录（编译期内嵌 + 系统语言探测）
│   └── telemetry/              # OpenTelemetry 可观测性
├── web/                        # Web UI 前端（React + Vite）
│   └── c5-ui/                  # 前端源码
├── prompts/                    # System prompt 模板（.md）
├── plans/                      # 架构设计文档
└── config.example.toml         # 配置示例
```

---

## 🔧 使用示例

### CLI 交互模式

```bash
# 启动 REPL
agent

# 执行任务
> 分析 src/main.rs 并优化性能

# 切换模型
/model ds

# 切换模式
/mode architect

# 启用可选工具
/tools ast on

# 手动压缩上下文
/compact

# 查看用量
/status

# Swarm 多代理编排
/swarm workflow.yaml
```

### Web 服务

```bash
agent --serve
# → http://127.0.0.1:8080
```

### 🔌 ACP 协议与编辑器集成

agent-project 实现了 [Agent Client Protocol (ACP) v1](https://agentclientprotocol.com) 服务端，可将智能体能力暴露给 Zed Editor 等兼容客户端。ACP 与 CLI、Web 前端共享同一套会话管理与审批机制——同一会话甚至可被多客户端同时订阅。

#### 传输模式

| 模式 | 启动方式 | 适用场景 |
| ---- | -------- | -------- |
| **stdio** | `agent --acp-stdio` | 编辑器作为子进程调用（推荐，零端口占用，进程随编辑器退出自动终止） |
| **http** | `agent --serve --acp` | HTTP+SSE，与 Web 前端同端口复用；适合自定义前端或远程接入 |

#### stdio 模式

编辑器以子进程方式启动 agent，通过 stdin/stdout 交换换行分隔的 JSON-RPC 消息。无需额外端口。

```bash
# 直接运行（手动测试）
agent --acp-stdio
# stdin 输入 JSON-RPC，stdout 返回响应与 session/update 通知
```

#### HTTP+SSE 模式

与 Web 前端共用同一 HTTP 服务，挂载两个端点：

| 端点 | 方法 | 说明 |
| ---- | ---- | ---- |
| `/acp/rpc` | POST | JSON-RPC 2.0 请求入口（`initialize` / `session/new` / `session/prompt` …） |
| `/acp/sse/{session_id}` | GET | SSE 事件流，订阅指定会话的 `session/update` 通知 |

```bash
# 启用 ACP HTTP 端点（也可在 config.toml [acp] enabled = true 持久启用）
agent --serve --acp

# 启动后输出：
#   • ACP  JSON-RPC  POST /acp/rpc
#            SSE 事件  GET  /acp/sse/{session_id}
```

> **鉴权**：HTTP 模式复用 `[server] auth_token`（与 Web 前端同一份 token）。支持 `Authorization: Bearer <token>` 头或 `?token=<token>` 查询参数。未配置 `auth_token` 则不校验。

#### 配置

在 `config.toml` 中通过 `[acp]` 段控制（对应 `crates/acp`）：

```toml
[acp]
enabled   = false             # 默认关；true 则 --serve 时自动挂载 /acp/* 路由
transport = "http"            # "http"（HTTP+SSE）/ "stdio" / "both"
```

CLI flag 可运行时覆盖配置：`--acp`（启用 HTTP+SSE）、`--acp-stdio`（纯 stdio，最高优先级，不启动 HTTP）。

#### Zed Editor 配置

在 Zed 的 `settings.json` 中将 agent-project 注册为 ACP agent（**stdio 模式**，推荐）：

```json
{
  "agent": {
    "enabled": true,
    "profiles": {
      "agent-project": {
        "name": "Agent Project",
        "transport": {
          "kind": "stdio",
          "command": "/path/to/agent",
          "args": ["--acp-stdio"],
          "env": {}
        },
        "default_model": {
          "provider": "custom",
          "name": "agent-project"
        }
      }
    },
    "default_profile": "agent-project"
  }
}
```

> **路径**：将 `/path/to/agent` 替换为编译产物实际路径（如 `./target/release/agent`，或安装到 `$PATH` 后直接写 `agent`）。
>
> **模型配置**：ACP 服务端复用 `config.toml` 的 `[default_model]` 与 `[[models]]` profile，加载同一套 Provider 路由。编辑器侧无需重复填入 API key——模型选择在 `session/new` 时通过 `model` 参数（或 `_meta.model`）指定别名即可。

若使用 **HTTP 模式**（agent 以 `--serve --acp` 常驻运行），Zed 配置改为：

```json
{
  "agent": {
    "enabled": true,
    "profiles": {
      "agent-project": {
        "name": "Agent Project",
        "transport": {
          "kind": "http",
          "url": "http://127.0.0.1:8080/acp/rpc"
        }
      }
    },
    "default_profile": "agent-project"
  }
}
```

#### 支持的 RPC 方法

| 方法 | 类型 | 说明 |
| ---- | ---- | ---- |
| `initialize` | 请求 | 握手 + 能力协商（返回 `protocolVersion` / `agentCapabilities` / `agentInfo`） |
| `session/new` | 请求 | 创建新会话，返回 `{ sessionId }`；支持通过 `model` / `mode` 参数（或 `_meta`）指定初始模型与模式 |
| `session/prompt` | 请求 | 投递用户消息，阻塞至 turn 完成返回 `{ stopReason }`；期间持续推送 `session/update` 通知 |
| `session/cancel` | 通知 | 取消当前正在执行的 turn |
| `session/load` | 请求 | 恢复历史会话（传入已有 `sessionId`），返回新 `sessionId` |
| `session/close` | 请求 | 关闭并释放会话资源 |
| `session/set_mode` | 请求 | 切换智能体模式（`code` / `architect` / `ask` / `debug`） |

#### session/update 事件类型

`session/prompt` 期间通过 SSE（HTTP 模式）或 stdout（stdio 模式）推送的标准 ACP 事件：

| `sessionUpdate` | 说明 |
| --------------- | ---- |
| `agent_message_chunk` | 智能体回复文本增量 |
| `agent_thought_chunk` | 智能体思考 / reasoning 增量 |
| `tool_call` | 工具调用创建（含 `toolCallId` / `title` / `kind` / `status` / `rawOutput`） |
| `tool_call_update` | 工具调用状态 / 输出更新 |
| `usage_update` | 上下文窗口用量更新（`used` / `size`） |

#### 能力声明（initialize 响应）

```json
{
  "protocolVersion": 1,
  "agentCapabilities": {
    "loadSession": true,
    "promptCapabilities": { "image": true, "embeddedContext": false },
    "mcpCapabilities": { "http": false, "stdio": true }
  },
  "agentInfo": { "name": "agent-project", "version": "0.1.0" },
  "authMethods": []
}
```

> `promptCapabilities` 与 `mcpCapabilities` 为 ACP v1 规范围必填字段，严格客户端（如 Zed）会做 schema 校验，缺失即拒绝握手。

#### 快速验证

```bash
# 1. 手动测试 stdio 模式（逐行输入 JSON-RPC）
agent --acp-stdio
# 输入：
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"test","version":"1.0"}}}
# → 返回 agentCapabilities 响应

# 2. 手动测试 HTTP 模式
agent --serve --acp &
# 创建会话
curl -X POST http://127.0.0.1:8080/acp/rpc \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"model":"ds","mode":"code"}}'
# → {"jsonrpc":"2.0","id":1,"result":{"sessionId":"..."}}
```


## how to build
### build web
```bash
cd web/c5-ui
npm install          # 首次需要安装依赖
npm run build        # tsc -b 类型检查 + vite build → 产物在 dist/
npm run deploy       # 把 dist/ 内容拷贝到 ../（即 web/ 根目录）
```
### build 
```bash
cargo b -r
```
---

## 📄 许可证

本项目基于 [MIT 许可证](LICENSE) 开源。

---

## 🙏 致谢

### 参考项目

本项目的设计与实现深受以下开源项目的启发与借鉴：

#### [Zoo-Code](https://github.com/nicepkg/zoo-code)（概念参考）

Zoo-Code 提供了**智能体高阶认知与流程控制**的核心设计蓝图，本项目从中汲取了以下关键概念：

- **五态状态机**（NoTask → Running → Streaming → WaitingForInput → Idle）：智能体生命周期的精确建模
- **say/ask 消息模型**：信息性输出与交互阻塞请求的清晰分离
- **审批路由（AskDispatcher）**：在流式输出与用户输入间的协调机制
- **模式系统**（code / architect / ask / debug）：不同场景下的 system prompt 与工具子集配置
- **Provider 抽象与 format transform**：多厂商线协议的格式转换与缓存策略

#### [oh-my-pi](https://github.com/nicepkg/oh-my-pi)（概念参考 + 设计迁移）

oh-my-pi 贡献了**底层代码操控精确性与执行循环工程化**的成熟实践，本项目将其 TypeScript/Rust 混合架构**全部移植为纯 Rust**：

- **执行循环**（agent-loop.ts）：流式推理 → 工具调用解析 → 批量执行 → 结果回填的完整闭环
- **AppendOnlyLog + StablePrefix**：上下文记忆与 Provider 前缀缓存最大化命中
- **压缩子系统**（summarization / pruning / shake / tool-protection）：上下文窗口管理
- **LLM 分发**（ProviderRegistry / streamSimple）：单一入口多 Provider 路由
- **AST 操控**（pi-ast）：基于 tree-sitter + ast-grep 的代码结构性操作
- **原生工具链**（pi-natives）：grep / glob / highlight / tokens 等底层能力
- **分层配置**（settings-schema.ts）：用户级与项目级配置深度合并
- **审批规则引擎**（approval.ts）：逐工具 allow/deny/ask 规则 + 命令级 glob 匹配
- **带宽优化线协议**：增量优先的 WebSocket 通信协议

---

### 独特优势

相较于上述参考项目，本项目在以下方面实现了**显著的架构改进与性能提升**：

| 维度 | Zoo-Code / oh-my-pi | agent-project | 改进 |
| --- | --- | --- | --- |
| **语言统一** | TypeScript 运行时 + Rust N-API 原生插件（混合架构） | **纯 Rust 单 workspace**（edition 2024） | 消除 N-API FFI 开销与跨语言序列化损耗，编译期类型安全，零运行时解释成本 |
| **架构模式** | 类单体编排 + 部分 trait 抽象 | **完整 Ports & Adapters（六边形架构）** | 所有依赖以 `Arc<dyn Trait>` 注入，循环对传输/Provider/Tool 完全透明，单测可全局替身化 |
| **扩展机制** | 手动注册中央清单 | **编译期零侵入发现**（`inventory` macro） | 新增 Provider/Tool 无需修改任何中央注册代码，`impl Trait + submit!` 即可 |
| **并发模型** | Node.js 事件循环 + N-API 线程池 | **async/await + Tokio 全异步** | 原生 `Send + Sync`，无 GIL 锁竞争，`spawn_blocking` 隔离 CPU 密集任务 |
| **工具可插拔** | 固定工具集 + 环境变量开关 | **配置驱动动态工具组**（`[tools.enabled]`） | 运行时 `/tools <key> on|off` 动态切换，未启用工具零 Token 开销 |
| **上下文压缩** | 单级压缩策略 | **三级渐进压缩**（Shake → Summarize → Prune） | 先轻度抖动去冗余，再 LLM 摘要折叠历史，最后窗口裁剪兜底，平衡质量与 Token 消耗 |
| **多 Agent 编排** | 仅基本子任务委派 | **Swarm DAG 管道**（YAML 定义 + 波内并行） | 支持复杂工作流定义，多 Agent 按依赖图并行执行，进度回调与状态监控 |
| **协同能力** | 无 | **端到端加密协同中继**（AES-256-GCM） | 多用户实时共享会话，服务器盲视密封字节，安全隐私 |
| **跨平台** | 主要面向 macOS/Linux | **Windows + Linux + macOS 一等公民** | `rustls-tls` 免 OpenSSL，`portable-pty` 双后端（ConPTY / posix），`dirs` 跨平台配置目录 |
| **工具深度** | 基础文件/AST 操作 | **完整工具矩阵**（核心 + AST + LSP + Hashline + PTY + 图像 + GitHub + MCP） | 可选工具组按需加载，覆盖 IDE 级别代码操控与 DevOps 场景 |
| **监控与可观测** | 基本日志 | **Supervisor 总线 + OpenTelemetry 导出** | 实时子 Agent 监控仪表盘，OTLP span 导出，全链路可追溯 |
| **Web 前端** | 简单静态页面 | **现代化 React SPA**（TypeScript + Vite + Tailwind） | 实时 WS 订阅、带宽优化增量协议、子 Agent 监控仪表盘、文件浏览、设置面板 |

### 特别感谢

- **Zoo-Code 团队**：为智能体认知架构与状态机设计提供了卓越的参考实现。
- **oh-my-pi 团队**：在代码操控精确性、上下文管理与执行循环工程化方面树立了标杆。
- **Rust 社区**：提供了 `tokio`、`tree-sitter`、`ast-grep`、`axum`、`reqwest` 等优秀生态库，使纯 Rust 智能体框架成为可能。
- **所有开源贡献者**：感谢每一位为 AI 编程智能体生态做出贡献的开发者。

---

> **构建理念**：将 TypeScript 生态中成熟的智能体实践经验，以 Rust 的系统级性能与安全性重新实现，融合两家之长，打造可插拔、高性能、跨平台的下一代 AI 编程智能体框架。
