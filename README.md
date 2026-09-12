# Gyre

**English** | [简体中文](README_ZH.md) | [Русский](README_RU.md)

> **A high-performance Rust agent framework** — fusing the higher-order cognition and process control of Zoo-Code with the low-level code-manipulation precision of oh-my-pi, realizing a modern AI coding agent entirely within the Rust ecosystem.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-blue)](rust-toolchain.toml)
[![Edition](https://img.shields.io/badge/edition-2024-purple)](Cargo.toml)

---

## 📋 Core Features

Gyre is a **production-grade AI coding agent framework** that provides a complete *perceive → reason → act* closed loop:

### 🤖 Agent Engine
- **Five-state state machine** (NoTask → Running → Streaming → WaitingForInput → Idle): ported from Zoo-Code's mature state model for precise control of the agent lifecycle.
- **Message-driven architecture**: `say` (informational) / `ask` (interactive-blocking) message models, with an approval gateway and user receipts.
- **Multiple modes**: `code` (coding) / `architect` (architecture) / `ask` (Q&A) / `debug` (debugging) / `plan` (planning). The mode determines the system prompt and the tool subset.
- **Steering mechanism**: inject messages mid-run to dynamically intervene in the agent's reasoning direction.
- **Soft tool requirements**: warn first, then enforce, protecting the provider prefix cache.

### 🛠️ Toolchain
- **Core tools**: `read_file` / `write_file` / `list_files` / `run_command` / `grep` / `glob` / `web_search` (editing goes through the optional AST / Hashline groups)
- **Session tools**: `todo` / `ask` / `checkpoint` / `rewind` (session rollback) / `security_scan` (local secret-leak & dangerous-pattern scan)
- **AST code manipulation** (optional): `replace_block` (tree-sitter syntactic block replacement) / `ast_search` / `ast_rewrite` (ast-grep structural search & rewrite)
- **LSP integration** (optional): diagnostics / go-to-definition / find references / rename symbol
- **Hashline DSL** (optional): line-anchored batch editing, applying multiple files and ranges in one pass
- **PTY pseudo-terminal** (optional): `run_pty_command`, supports interactive terminal apps (top / vim / REPL)
- **Image processing** (optional): `read_image` / `image_gen` (DALL·E or a compatible API)
- **GitHub integration** (optional): query and operate on PRs / Issues / Actions, plus GraphQL queries
- **MCP protocol**: implements the Model Context Protocol over stdio and Streamable HTTP (initialize / tools/list / tools/call / resources/*); mount tools from any stdio or HTTP MCP server

### 🧠 Context Management
- **AppendOnlyLog**: messages are append-only; the only legal mutation path is `replaceTail` during compaction
- **StablePrefix fingerprinting**: system prompt + tool spec are snapshotted once and frozen with a SHA-256 byte fingerprint to maximize the provider-side prefix cache hit rate
- **Three-tier compaction**: Shake (jitter-based dedup) → Summarize (LLM handoff summary) → Prune (window fallback), combined with Tool-Protection to safeguard tool results
- **Token counting**: based on `tiktoken-rs`, accurately tracking per-turn usage

### 🔌 LLM Multi-Provider
- **ProviderRegistry**: a single entry point routes by `model.api`, with built-in `ProviderCallContext` authentication and concurrency rate-limiting
- **OpenAI Chat Completions**: covers OpenAI / compatible gateways / local vLLM / Ollama
- **DeepSeek**: DeepSeek Chat / Reasoner
- **GLM (Zhipu / Z.ai)**: full official API support — `thinking` toggle, `reasoning_content` thinking echo, `preserveReasoning` multi-turn continuation
- **Open adapter**: implement the `LlmProvider` trait + a one-line registration to add a new provider
- **Thinking mode** (Thinking/Reasoning): supports the thinking-token budget for `deepseek-reasoner` / GLM thinking models (glm-4.7+/glm-5+)

### 🌐 Dual Front-Ends
- **CLI / REPL**: `rustyline` line editing + Tab completion + streaming Markdown rendering + syntax highlighting
- **Web UI** (React + TypeScript + Vite): real-time bidirectional WebSocket communication, a bandwidth-optimized wire protocol (delta-first)
- **Session management**: persisted JSONL history, with support for resume / fork / export
- **Multilingual (i18n)**: CLI and Web follow the system language by default; built-in Chinese / English / Русский / 日本語; switchable in settings or config; adding a language takes just one resource file

### 🔌 ACP Protocol (Editor Integration)
- **Agent Client Protocol v1**: follows the [agentclientprotocol.com](https://agentclientprotocol.com) standard, providing a JSON-RPC 2.0 + SSE/stdio interface for compatible clients such as the Zed Editor
- **Dual transport modes**: `stdio` (the editor invokes it as a subprocess, zero port usage) / `http` (HTTP+SSE, multiplexed on the same port as the Web front-end)
- **Three-way equivalence**: ACP shares the same session management and approval mechanism as the CLI and Web front-ends; the same session can even be subscribed to by multiple clients simultaneously
- **Standard event stream**: `session/update` pushes ACP-standard events such as `agent_message_chunk` / `agent_thought_chunk` / `tool_call` / `usage_update`
- **Capability declaration**: the `initialize` handshake truthfully declares `promptCapabilities` (image support) and `mcpCapabilities` (stdio + Streamable HTTP support), passing strict client schema validation

### 🔄 Multi-Agent Orchestration
- **Sub-agent delegation** (TaskTool): a parent agent can delegate subtasks to sub-agents, with concurrency guardrails and independent token budgets
- **Swarm multi-agent**: YAML-defined DAG workflows, intra-wave parallel execution, pipeline progress callbacks
- **Supervisor monitoring**: real-time tracking of all sub-agent states (running/success/failure), with a CLI dashboard and a Web dashboard

### 🤝 Collaboration Relay
- **End-to-end encryption**: AES-256-GCM sealed bytes; the relay server is blind (routes only by room_id)
- **WebSocket bridging**: multiple users can join the same room to share an agent session in real time

### 📐 Architecture Design
- **Ports & Adapters (Hexagonal Architecture)**: all core dependencies are injected as `Arc<dyn Trait>`; the loop is completely transparent to transport / provider / tool
- **Non-invasive extension**: providers self-register via the `inventory` compile-time plugin; tools are assembled explicitly at the CLI/server composition root (`DefaultToolRegistry`)
- **Unidirectional dependencies**: `core` has zero business dependencies; `agent` depends only on Traits, not concrete implementations; the dependency graph is unidirectional and acyclic
- **Event-driven**: the loop yields `Stream<Item = AgentEvent>`; both front-ends subscribe uniformly

---

## 🚀 Quick Start

### Prerequisites

- Rust toolchain 1.85+ (see [`rust-toolchain.toml`](rust-toolchain.toml))
- An LLM API key (OpenAI / Anthropic / DeepSeek / GLM / local vLLM)

### Install & Run

```bash
# Clone the repository
git clone https://github.com/brianmiller123/Gyre.git
cd Gyre

# Copy the config and fill in your API key
cp config.example.toml .agent/config.toml

# Build (Release mode)
cargo build --release

# Run a one-off task
./target/release/agent "List the files in the current directory and show a tree"

# Or launch the interactive REPL
./target/release/agent

# Start the Web service
./target/release/agent --serve
```

### Configuration

Configuration uses TOML and is loaded in layers (later overrides earlier):

1. **User level**: `~/.config/agent/config.toml` (Windows `%APPDATA%\agent`)
2. **Project level**: `<cwd>/.agent/config.toml`

Multiple model profiles are supported; switch at runtime via `--model <alias>` or the `/model <alias>` command:

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

See [`config.example.toml`](config.example.toml) for the full set of options.

### 🌐 Multilingual (i18n)

CLI and Web **follow the system language by default** (the CLI reads `LANG` / `LC_ALL` / `LC_MESSAGES`; the Web reads `navigator.languages`), with 4 built-in languages:

| Code | Language | CLI resource | Web resource |
| ---- | -------- | ------------ | ------------ |
| `en` | English  | [`crates/i18n/locales/en.json`](crates/i18n/locales/en.json) | [`web/c5-ui/src/lib/locales.ts`](web/c5-ui/src/lib/locales.ts) |
| `zh` | 中文     | `crates/i18n/locales/zh.json` | same |
| `ru` | Русский  | `crates/i18n/locales/ru.json` | same |
| `ja` | 日本語   | `crates/i18n/locales/ja.json` | same |

**Config override** (optional; leave blank to auto-detect the system language):

```toml
# Top level of config.toml
language = "zh"   # en / zh / ru / ja
```

- **CLI**: [`main.rs`](crates/cli/src/main.rs) initializes with the system language at startup (`agent_i18n::init(None)`); once config is loaded, an explicit `language` overrides it.
- **Web**: switch in the [settings panel](web/c5-ui/src/components/agent/SettingsPanel.tsx) under "Appearance → Language"; the preference is persisted in `localStorage`, and "Auto" follows the system and updates instantly when the system language changes.

#### Adding a language (symmetric in two places)

1. **CLI**: copy `en.json` to `<code>.json` in [`crates/i18n/locales/`](crates/i18n/locales/) and translate it; then register one line each in `LOCALE_FILES`, `SUPPORTED`, and `match_supported` inside [`lib.rs`](crates/i18n/src/lib.rs).
2. **Web**: append a `Dict` in [`locales.ts`](web/c5-ui/src/lib/locales.ts), add it to `locales`/`SUPPORTED_LOCALES`, and fill in the native self-display name via `lang.<code>` in every language.

The fallback chain is uniform: current language → English → the key itself (missing keys are visible during development without errors).

---

## 📦 Module Structure

```
Gyre/
├── Cargo.toml                  # workspace config
├── crates/
│   ├── core/                   # contracts: types + traits + errors (zero business deps)
│   ├── config/                 # layered TOML config + model profiles + approval rule engine
│   ├── llm/                    # LLM provider adapters (OpenAI/Anthropic/DeepSeek/GLM/Gemini)
│   ├── context/                # context management (AppendOnlyLog + StablePrefix + compaction)
│   ├── prompt/                 # system prompt template engine
│   ├── tools/                  # toolset (filesystem/AST/LSP/search/shell/GitHub/image/todo/ask/...)
│   ├── shell/                  # in-process shell exec (brush parser + 50+ coreutils builtins, zero fork)
│   ├── agent/                  # execution-loop state machine (the agent core)
│   ├── server/                 # HTTP/WebSocket service (axum)
│   ├── acp/                    # Agent Client Protocol adapter (editor integration)
│   ├── cli/                    # CLI binary entry (REPL + command dispatch + RPC)
│   ├── ast/                    # AST manipulation (tree-sitter + ast-grep)
│   ├── search/                 # code search (grep/glob/fd/highlight/tokens)
│   ├── ttsr/                   # time-traveling stream rules (prompt-free streaming guardrails)
│   ├── mcp/                    # MCP protocol client (stdio + Streamable HTTP)
│   ├── lsp/                    # LSP language-server client
│   ├── dap/                    # DAP debug-adapter client (`debug` tool)
│   ├── browser/                # headless browser automation (minimal CDP client)
│   ├── memory/                 # cross-session long-term memory
│   ├── skills/                 # skill system (file-backed skills)
│   ├── swarm/                  # multi-agent orchestration (DAG pipelines)
│   ├── collab/                 # collaboration relay (end-to-end encryption)
│   ├── hashline/               # Hashline DSL batch editing
│   ├── pty/                    # PTY pseudo-terminal tool
│   ├── iso/                    # filesystem isolation (sandbox diff)
│   ├── supervisor/             # sub-agent monitoring bus
│   ├── proxy/                  # SOCKS5 egress proxy controller (process-wide singleton)
│   ├── discovery/              # foreign agent-tool config discovery (AGENTS.md / CLAUDE.md / Cursor / Cline)
│   ├── advisor/                # independent read-only review agent (nit / concern / blocker)
│   ├── eval/                   # persistent Python/JS eval kernels + loopback bridge
│   ├── snapcompact/            # snapshot compaction: dropped history → PNG frames for vision-model replay
│   ├── i18n/                   # multilingual message catalog (compile-time embedded + system-language detection)
│   └── telemetry/              # OpenTelemetry observability
├── web/                        # Web UI front-end (React + Vite)
│   └── c5-ui/                  # front-end source
├── vendor/                     # in-tree forks (pi-shell / pi-builtins / pi-walker / brush-core)
├── prompts/                    # system prompt templates (.md)
├── docs/                       # design docs & benchmark analyses
├── tests/                      # golden regression fixtures
└── config.example.toml         # config example
```

---

## 🔧 Usage Examples

### CLI Interactive Mode

```bash
# Start the REPL
agent

# Run a task
> Analyze src/main.rs and optimize performance

# Switch model
/model ds

# Switch mode
/mode architect

# Enable an optional tool
/tools ast on

# Manually compact the context
/compact

# Show usage
/status

# Swarm multi-agent orchestration
/swarm workflow.yaml
```

### 📚 Reference Docs

- **[Tool reference](docs/tools.md)** — every tool, its capability tier, how to enable it, and its parameters. Generated from the live tool registry via `cargo xtask docs --tools` (CI checks for drift).
- **[MCP guide](docs/mcp.md)** — how to mount servers (TOML / `mcp.json` / CLI), transports (stdio / Streamable HTTP / legacy SSE), capability gating (tools / prompts / resources), deferred connections, and troubleshooting.
- **Third-party notices** — [`THIRD-PARTY-NOTICES.txt`](THIRD-PARTY-NOTICES.txt), generated by `cargo xtask notices` (offline; CI gates on drift).
- **Extending Gyre** — the extension surface is intentionally MCP-only for now: MCP servers (tools/prompts/resources), custom slash commands (`.agent/commands/*.md`), skills, named subagents, hooks and workspace conventions. See [`docs/adr/0001-extension-host.md`](docs/adr/0001-extension-host.md) for the decision and revisit triggers.
- **Collaboration protocol** — Gyre keeps its own sealed `proto=1` wire instead of pretending to match oh-my-pi's `proto=3`; mismatched peers get an explicit error rather than a silent downgrade. See [`docs/adr/0002-collab-protocol.md`](docs/adr/0002-collab-protocol.md).
- **Built-in model catalog** — `agent models catalog [filter]` lists an offline catalog of ~26 common model families (context window, max output, thinking support). It only fills values you leave unset (`max_input_tokens` / `max_output_tokens` / thinking capability); explicit config always wins, and unknown models behave exactly as before. Runtime discovery (`agent models list`) complements it with what your account actually serves. See `crates/config/src/model_catalog.rs`.
- **Release preflight** — `cargo xtask release-check` verifies version consistency, `CHANGELOG` `[Unreleased]`, the three-language READMEs, and that the generated notices/docs match the current dependency and tool surfaces.

Recent additions worth knowing about:

- **Goal mode** (`goal` tool + `--goal`-style autonomous continuation): create a persistent objective; the engine keeps the goal context in-loop and auto-continues at stop boundaries until you call `goal({op:"complete"})` (or hit the continuation cap). `/goal` inspects and controls it.
- **Todo loop**: first-turn *eager prelude* plus a *completion reminder* that resumes the turn while todo items remain unfinished (`[todo] eager/reminders/reminders_max`).
- **Config loading**: `.env` is discovered (`$GYRE_ENV_FILE` → `<cwd>/.env` → `<cwd>/.agent/.env` → config dir → `$HOME/.env`; real env always wins; `GYRE_DOTENV=off` disables) and `${VAR}` / `${VAR:-default}` expansion works everywhere in the config.
- **Prompt chain**: `SYSTEM.md` (project overrides user), `@import` inside context files, context-file dedup, and `--append-system-prompt <TEXT|FILE>`.
- **Configurable keybindings**: `[keybindings]` overrides the action table (`app.clear`, `app.message.dequeue`, `app.retry`, …); `/hotkeys` prints the effective bindings and unknown/invalid entries fall back with a warning.
- **Interactive commands**: `/clear`, `/new`, `/retry`, `/context`, `/usage`, `/jobs`, `/queue` (runtime message queue; `Ctrl-Y` pulls the head into the editor), `/settings`, `/hotkeys`, writable `/todo`, and `<n>`-indexed `/resume`.

### Web Service

```bash
# Listen on the [server].bind address from the config (default 127.0.0.1:8080)
agent --serve
# → http://127.0.0.1:8080

# Specify the bind address (overrides config)
agent --serve 0.0.0.0:8080   # listen on all interfaces
agent --serve :3000          # port only: complete host from config (→ 127.0.0.1:3000)
```

### 🔌 ACP Protocol & Editor Integration

Gyre implements the [Agent Client Protocol (ACP) v1](https://agentclientprotocol.com) server, exposing agent capabilities to compatible clients such as the Zed Editor. ACP shares the same session management and approval mechanism as the CLI and Web front-ends — the same session can even be subscribed to by multiple clients at once.

#### Transport Modes

| Mode | How to start | Use case |
| ---- | ------------ | -------- |
| **stdio** | `agent --acp` | The editor launches it as a subprocess (recommended; zero port usage; the process terminates automatically when the editor exits) |
| **http** | `agent --serve --acp` | HTTP+SSE, multiplexed on the same port as the Web front-end; suitable for custom front-ends or remote access |

#### stdio Mode

The editor starts agent as a subprocess and exchanges newline-delimited JSON-RPC messages over stdin/stdout. No extra port is needed.

```bash
# Run directly (manual testing)
agent --acp
# Feed JSON-RPC on stdin; responses and session/update notifications come back on stdout
```

#### HTTP+SSE Mode

Shares the same HTTP service as the Web front-end, mounting two endpoints:

| Endpoint | Method | Description |
| ---- | ---- | ---- |
| `/acp/rpc` | POST | JSON-RPC 2.0 request entry (`initialize` / `session/new` / `session/prompt` …) |
| `/acp/sse/{session_id}` | GET | SSE event stream; subscribes to `session/update` notifications for the given session |

```bash
# Enable the ACP HTTP endpoints (also enable persistently via [acp] enabled = true in config.toml)
agent --serve --acp

# On startup it prints:
#   • ACP  JSON-RPC  POST /acp/rpc
#            SSE events  GET  /acp/sse/{session_id}
```

> **Auth**: HTTP mode reuses `[server] auth_token` (the same token as the Web front-end). It supports the `Authorization: Bearer <token>` header or a `?token=<token>` query parameter. If no `auth_token` is configured, no validation is performed.

#### Configuration

Control it via the `[acp]` section of `config.toml` (corresponds to `crates/acp`):

```toml
[acp]
enabled   = false             # off by default; true auto-mounts /acp/* routes on --serve
transport = "http"            # "http" (HTTP+SSE) / "stdio" / "both"
```

CLI flag overrides the config at runtime: `--acp` runs ACP in stdio mode when used alone (for editor integration; highest priority; does not start HTTP), and enables the HTTP+SSE endpoint when combined with `--serve`.

#### Zed Editor Configuration

Register Gyre as an ACP agent in Zed's `settings.json` (**stdio mode**, recommended):

```json
{
  "agent": {
    "enabled": true,
    "profiles": {
      "Gyre": {
        "name": "Agent Project",
        "transport": {
          "kind": "stdio",
          "command": "/path/to/agent",
          "args": ["--acp"],
          "env": {}
        },
        "default_model": {
          "provider": "custom",
          "name": "Gyre"
        }
      }
    },
    "default_profile": "Gyre"
  }
}
```

> **Path**: replace `/path/to/agent` with the actual path of your build artifact (e.g. `./target/release/agent`, or just `agent` once installed on `$PATH`).
>
> **Model config**: the ACP server reuses the `[default_model]` and `[[models]]` profiles from `config.toml`, loading the same provider routing. You do not need to re-enter API keys in the editor — select a model alias via the `model` parameter (or `_meta.model`) on `session/new`.

If using **HTTP mode** (agent runs persistently with `--serve --acp`), the Zed config becomes:

```json
{
  "agent": {
    "enabled": true,
    "profiles": {
      "Gyre": {
        "name": "Agent Project",
        "transport": {
          "kind": "http",
          "url": "http://127.0.0.1:8080/acp/rpc"
        }
      }
    },
    "default_profile": "Gyre"
  }
}
```

#### Supported RPC Methods

| Method | Type | Description |
| ---- | ---- | ---- |
| `initialize` | Request | Handshake + capability negotiation (returns `protocolVersion` / `agentCapabilities` / `agentInfo`) |
| `session/new` | Request | Create a new session; returns `{ sessionId }`; specify the initial model and mode via the `model` / `mode` params (or `_meta`) |
| `session/prompt` | Request | Submit a user message; blocks until the turn completes and returns `{ stopReason }`; continuously pushes `session/update` notifications in the meantime |
| `session/cancel` | Notification | Cancel the currently executing turn |
| `session/load` | Request | Resume a historical session (pass an existing `sessionId`); returns a new `sessionId` |
| `session/close` | Request | Close and release session resources |
| `session/set_mode` | Request | Switch the agent mode (`code` / `architect` / `ask` / `debug`) |

#### session/update Event Types

Standard ACP events pushed during `session/prompt` via SSE (HTTP mode) or stdout (stdio mode):

| `sessionUpdate` | Description |
| --------------- | ---- |
| `agent_message_chunk` | Incremental agent reply text |
| `agent_thought_chunk` | Incremental agent thinking / reasoning |
| `tool_call` | Tool-call creation (includes `toolCallId` / `title` / `kind` / `status` / `rawOutput`) |
| `tool_call_update` | Tool-call status / output update |
| `usage_update` | Context-window usage update (`used` / `size`) |

#### Capability Declaration (initialize response)

```json
{
  "protocolVersion": 1,
  "agentCapabilities": {
    "loadSession": true,
    "promptCapabilities": { "image": true, "embeddedContext": false },
    "mcpCapabilities": { "http": true, "stdio": true }
  },
  "agentInfo": { "name": "Gyre", "version": "0.1.0" },
  "authMethods": []
}
```

> `promptCapabilities` and `mcpCapabilities` are required fields in the ACP v1 spec; strict clients (such as Zed) validate the schema and reject the handshake if they are missing.

#### Quick Verification

```bash
# 1. Manually test stdio mode (enter JSON-RPC line by line)
agent --acp
# Input:
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"test","version":"1.0"}}}
# → returns the agentCapabilities response

# 2. Manually test HTTP mode
agent --serve --acp &
# Create a session
curl -X POST http://127.0.0.1:8080/acp/rpc \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"model":"ds","mode":"code"}}'
# → {"jsonrpc":"2.0","id":1,"result":{"sessionId":"..."}}
```


## how to build
### build web
```bash
cd web/c5-ui
npm install          # first run requires installing dependencies
npm run build        # tsc -b type checking + vite build → writes straight to ../ (the web/ root)
npm run dev          # dev server on :5173, proxies /api and /ws to agent --serve
```
### build
```bash
cargo b -r
```
---

## 📄 License

This project is open-sourced under the [MIT License](LICENSE).

---

## 🙏 Acknowledgements

### Reference Projects

The design and implementation of this project are deeply inspired by and draw on the following open-source projects:

#### [Zoo-Code](https://github.com/nicepkg/zoo-code) (conceptual reference)

Zoo-Code provided the core design blueprint for **higher-order agent cognition and process control**. This project draws the following key concepts from it:

- **Five-state state machine** (NoTask → Running → Streaming → WaitingForInput → Idle): precise modeling of the agent lifecycle
- **say/ask message model**: a clean separation between informational output and interactive blocking requests
- **Approval routing (AskDispatcher)**: a coordination mechanism between streaming output and user input
- **Mode system** (code / architect / ask / debug): system-prompt and tool-subset configuration for different scenarios
- **Provider abstraction and format transform**: format conversion and caching strategies for multi-vendor wire protocols

#### [oh-my-pi](https://github.com/nicepkg/oh-my-pi) (conceptual reference + design migration)

oh-my-pi contributed mature practices in **low-level code-manipulation precision and execution-loop engineering**. This project ported its TypeScript/Rust hybrid architecture **entirely to pure Rust**:

- **Execution loop** (agent-loop.ts): the full closed loop of streaming reasoning → tool-call parsing → batch execution → result backfill
- **AppendOnlyLog + StablePrefix**: context memory and maximizing provider prefix-cache hits
- **Compaction subsystem** (summarization / pruning / shake / tool-protection): context-window management
- **LLM dispatch** (ProviderRegistry / streamSimple): single-entry multi-provider routing
- **AST manipulation** (pi-ast): structural code operations based on tree-sitter + ast-grep
- **Native toolchain** (pi-natives): low-level capabilities such as grep / glob / highlight / tokens
- **Layered config** (settings-schema.ts): deep-merging user-level and project-level config
- **Approval rule engine** (approval.ts): per-tool allow/deny/ask rules + command-level glob matching
- **Bandwidth-optimized wire protocol**: delta-first WebSocket communication protocol

---

### Distinctive Advantages

Compared with the reference projects above, this project achieves **significant architectural improvements and performance gains** in the following areas:

| Dimension | Zoo-Code / oh-my-pi | Gyre | Improvement |
| --- | --- | --- | --- |
| **Language unity** | TypeScript runtime + Rust N-API native plugins (hybrid) | **Pure Rust single workspace** (edition 2024) | Eliminates N-API FFI overhead and cross-language serialization loss; compile-time type safety; zero runtime interpretation cost |
| **Architecture pattern** | Quasi-monolithic orchestration + partial trait abstraction | **Full Ports & Adapters (Hexagonal Architecture)** | All dependencies injected as `Arc<dyn Trait>`; the loop is fully transparent to transport/provider/tool; unit tests can stub globally |
| **Extension mechanism** | Manual central registry | **Provider self-registration** (`inventory` macro) + explicit tool assembly | Adding a provider needs no central registration code (`impl Trait + submit!`); tools are wired explicitly at the composition root |
| **Concurrency model** | Node.js event loop + N-API thread pool | **async/await + Tokio, fully async** | Native `Send + Sync`, no GIL-lock contention; `spawn_blocking` isolates CPU-heavy tasks |
| **Tool pluggability** | Fixed toolset + environment-variable switches | **Config-driven dynamic tool groups** (`[tools.enabled]`) | Toggle at runtime via `/tools <key> on\|off`; disabled tools cost zero tokens |
| **Context compaction** | Single-tier compaction strategy | **Three-tier progressive compaction** (Shake → Summarize → Prune) | Light jitter-based dedup first, then LLM summarization to fold history, then window-pruning fallback — balancing quality and token cost |
| **Multi-agent orchestration** | Only basic subtask delegation | **Swarm DAG pipelines** (YAML-defined + intra-wave parallelism) | Supports complex workflow definitions; multiple agents execute in parallel along the dependency graph, with progress callbacks and status monitoring |
| **Collaboration** | None | **End-to-end-encrypted collaboration relay** (AES-256-GCM) | Multiple users share a session in real time; the server is blind to the sealed bytes — secure and private |
| **Cross-platform** | Primarily macOS/Linux | **Windows + Linux + macOS first-class citizens** | `rustls-tls` avoids OpenSSL; `portable-pty` dual backends (ConPTY / posix); `dirs` for cross-platform config directories |
| **Tool depth** | Basic file/AST operations | **Complete tool matrix** (core + AST + LSP + Hashline + PTY + image + GitHub + MCP) | Optional tool groups loaded on demand, covering IDE-level code manipulation and DevOps scenarios |
| **Monitoring & observability** | Basic logging | **Supervisor bus + OpenTelemetry export** | Real-time sub-agent monitoring dashboard; OTLP span export; full-chain traceability |
| **Web front-end** | Simple static pages | **Modern React SPA** (TypeScript + Vite + Tailwind) | Real-time WS subscription; bandwidth-optimized delta protocol; sub-agent monitoring dashboard; file browser; settings panel |

### Special Thanks

- **Zoo-Code team**: provided an outstanding reference implementation for agent cognitive architecture and state-machine design.
- **oh-my-pi team**: set the benchmark for code-manipulation precision, context management, and execution-loop engineering.
- **Rust community**: provided excellent ecosystem libraries such as `tokio`, `tree-sitter`, `ast-grep`, `axum`, and `reqwest`, making a pure-Rust agent framework possible.
- **All open-source contributors**: thank you to every developer who has contributed to the AI coding-agent ecosystem.

---

> **Design philosophy**: re-implement the mature agent practices of the TypeScript ecosystem with Rust's system-level performance and safety, fusing the best of both to build a pluggable, high-performance, cross-platform next-generation AI coding-agent framework.
