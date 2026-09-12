# Gyre × oh-my-pi 全面差距分析（2026-09-11 · 第四轮逐文件核实）

> **对象**：`third/oh-my-pi`（TypeScript/Bun monorepo 快照，`packages/coding-agent` 为主体 + 8 个 Rust `pi-*` crate）vs 本仓库 Gyre（Rust workspace 33 crates + 4 个 vendor fork，HEAD `1bdc14d` + 70 项未提交改动）。
> **方法**：14 路并行只读 scout 分域逐文件核实（CLI / 工具 / 配置 / 扩展 / RPC·ACP / MCP / 提示词 / 运行时 / LLM / 交互 / 外围 / 服务端·Web·collab / 构建 / 文档·测试），每条结论要求双侧 `file:line`；main 直读复核 12 项承重结论（见 §2）。全部结论基于**磁盘当前内容**（含未提交改动），未使用 git HEAD 快照。**未运行任何一侧测试**（`cargo check --workspace --all-targets` 除外，见 §2）。
> **与前三轮的关系**：本报告取代 `docs/oh-my-pi-gap-analysis-2026-09-06.md` 作为当前基线；凡前报告结论已在本轮被代码推翻者，在 §2 显式更正。

---

## 一、结论摘要

### 1.1 规模与总体判断

| 维度 | oh-my-pi | Gyre | 比值 |
|---|---|---|---|
| 自有源代码 | 1,452,946 行 TS（18 packages）+ 222,926 行 Rust（8 pi-crates） | 116,998 行 Rust（33 crates） | ≈ 7 %（对 TS+Rust 总量） |
| 另计 | Python（robomp/omp-rpc）、shell、bazel/nix、137 篇文档 | vendor fork 168,905 行 Rust、web 前端 11,123 行 TS/TSX | — |
| CLI | 41 子命令 + 3 别名 + 18 个 launch flag 组 | 4 顶层动词 / 16 动作 + 15 flag | 子命令 ~10 % |
| RPC 命令 | 42 变体 | 12（真实会话命令 9） | ~29 % |
| 斜杠命令 | 77 条顶层 | 33 条 | ~43 % |
| 内置工具 | 29 + 3 隐藏 | ~30（命名/参数面有系统性差异） | 数量接近，契约不兼容 |
| Hook 事件 | 26 类（+ 45 扩展事件） | 3 类 + turn_end | ~12 % |
| 配置键 | 482 | ~90（可直接对应 ~35） | ~7 % |
| 提示词资源 | 178 篇 .md | 7 篇 .md（其余内联） | ~4 % |
| 文档 | 137 篇（含 31 per-tool、4 MCP、settings 840 行） | 16 篇（14 篇为分析报告） | ~12 % |
| 测试 | 2,365 文件 / ~24,700 用例 | 1,441 `#[test]` + 12 集成文件 | 量级差 ~10× |
| LLM wire | 14 api id / 86 provider 注册文件 / 4727 模型目录 / 104 compat 位 | 7 `Api` 枚举 / 5 适配器 / 0 目录 / 0 compat | ~6 % |

**总体判断**：Gyre 的**单机核心 Agent 闭环已高忠实移植**——会话持久化骨架、压缩三级级联、任务委派、steering、pause gate、异步作业管理本体、流式守卫、记忆读写/排序/向量、hashline 闭环、MCP 全链（含 OAuth/Deferred/缓存/多源）、i18n。差距**不在"能不能跑"，而在四个"编排/生态层"**：

1. **交互基础设施整层缺失**（无 raw 模式按键层 → 无 Esc 分级中断、无 Ctrl-C 优雅中止、无消息队列、无键位定制）；
2. **宿主协议不互操作**（RPC 命令面 12 vs 42、id 类型/响应信封不兼容、ready 宣告 v2 却不实现 negotiate；ACP 权限请求形状非标准、`stopReason` 恒 `end_turn`）；
3. **扩展生态零起点**（插件/市场/扩展/hook 事件面/自定义工具全缺）；
4. **Provider/协议纵深**（openai-responses 死链、无 compat 表、无静态模型目录、采样参数/推理计量/成本缺失）；
5. **装配与寻址层缺失**（无 Agent 注册表 → hub/irc 无子代理寻址地基；无统一 SDK → cli/server/rpc 三处装配漂移，server 已漏接 `agent_discovery` 与四段行为章节）。

以及**工程交付面**：发布编排、NOTICES、MSRV 一致性、测试网与用户文档。

### 1.2 最高影响发现（Top 12，全部本轮独立复核）

> 状态列：✅ 已在第二轮修复（§九） · 🟡 部分修复 · ⬜ 未动。

| # | 状态 | 发现 | 证据 | 影响 |
|---|---|---|---|---|
| 1 | ✅ | **未知 CLI flag 被静默当作 prompt 送 LLM** | `crates/cli/src/main.rs:46-50`（`trailing_var_arg + allow_hyphen_values`）；实测 `agent --model __nope__ --smoke-test` 建会话后进入 prompt 路径 | 计费风险：`--smoke-test`/`-v`/`--license` 等都会变成任务文本 |
| 2 | 🟡 | **REPL 无 Ctrl-C/SIGINT 处理** | 全仓仅 `crates/cli/src/rpc.rs:1914` 有 `tokio::signal::ctrl_c()`；`main.rs:1324` 仅 `Interrupted => continue` | 运行中 Ctrl-C 直接杀进程（无优雅 abort/落盘）；提示符下无法用 Ctrl-C 退出 |
| 3 | ✅ | **RPC 宣告 v2 却不实现 `negotiate_protocol`** | `crates/cli/src/rpc.rs:361` `supported_protocol_versions:[1,2]`；无 negotiate 分支（落到 unknown 回 error） | 官方 TS/Python 客户端握手直接抛错，无法连接 |
| 4 | ✅ | **RPC 请求 `id` 强制 `u64`、响应信封自定义** | `crates/cli/src/rpc.rs:58` `id: u64`；响应为 `{type:"state"/"ok"/...}` vs omp `{type:"response",command,success,data}` | 即使命令支持也互操作失败 |
| 5 | ✅ | **`openai-responses` 无适配器 → Codex OAuth 死链** | `crates/llm/src/openai.rs:30-35` `SUPPORTED` 仅 `[OpenAiCompletions, OllamaChat]`；`crates/llm/src/oauth/codex.rs:80` 存 `openai-responses` | 登录成功但无 wire 可用 |
| 6 | ✅ | **MCP 未解析 server `capabilities`，`tools/list` 失败即整体连接失败** | `crates/mcp/src/client.rs:441-449` 只取 instructions；`crates/mcp/src/tool.rs:984-990` list_tools 失败 → `client.close()` | 纯 resources/prompts MCP server 完全不可用 |
| 7 | ✅ | **四个行为契约段只在 REPL 路径注入** | `TOOL_POLICY_SECTION` 等仅 `crates/cli/src/main.rs:836-839`；`crates/cli/src/rpc.rs:1568`、`crates/server/src/lib.rs:1274` 均未注入 | Web GUI / RPC 会话拿不到 Tool Policy / Delegation / Workflow / Delivery |
| 8 | ✅ | **`agent models --help` / `agent help` 不显示帮助** | `crates/cli/src/manage.rs` 无 help 分支；实测 `agent models --help` 建会话并加载技能后退出 | 子命令可发现性为零 |
| 9 | ⬜ | **任务/子代理无定义化** | Gyre `crates/agent/src/task_tool.rs` 单文件 621 行（task/tasks/output_schema）；omp `src/task/` 26 文件（discovery/spawn-policy/read-only-policy/worktree/soft-budget） | 子代理同质化、可无限烧 token、worktree 隔离原语零调用 |
| 10 | ✅ | **ACP `session/request_permission` 形状非标准** | Gyre 发 `{sessionId,prompt,options}`（`crates/acp/src/rpc.rs:411-426`）；ACP v1 要求 `{sessionId,toolCall,options}` | 严格客户端（Zed）schema 校验失败 → 审批不可用 |
| 11 | ✅ | **skill 描述仅折叠换行，未转义 `<>`/反引号** | `crates/skills/src/render.rs:30-40` | SKILL.md 描述可闭合 `</skills>` 注入 system prompt |
| 12 | ✅ | **vendor P0 已解除（更正前报告）** | `cargo metadata` exit 0；`cargo check --workspace --all-targets` exit 0（15.8 s） | 前报告"工作树不可构建"不再成立 |

---

## 二、工作树状态与本轮更正

### 2.1 工作树状态（实测）

- 未提交改动 **70 项**（`git status --porcelain`）：M 44 / D 5 / ?? 21。**第二轮修复后为 87 项**（新增 `crates/sdk/`、`crates/llm/src/openai_responses.rs` 等，见 §9.4）。含本报告依据的 H3/H5/H6/H14/H15 落地文件（`crates/core/src/jobs.rs`、`crates/agent/src/stream_guards.rs`、`crates/core/src/session_event.rs`、`crates/tools/src/generated_guard.rs`、`crates/config/src/mcp_json.rs`、`crates/mcp/tests/mcp_sse.rs`）。
- `vendor/{brush-core,pi-builtins,pi-shell,pi-walker}` **已存在**，根 manifest 5 处引用可解析。
- `cargo metadata --no-deps` exit 0；`cargo check --workspace --all-targets` exit 0（仅 1 条 `proc-macro-error2` future-incompat 警告）。
- `web/index.html` 引用的 `assets/index-Bq-beHRn.js` / `index-BiPFPdXH.css` 与 `web/assets/` 实际文件一致 → 前端构建产物自洽。

### 2.2 对 `2026-09-06` 报告的更正（代码直读）

| 前报告结论 | 本轮事实 | 证据 |
|---|---|---|
| §〇 P0：vendor 删除、工作树不可构建 | **已解除**：vendor 在盘、metadata/check 均通过 | `cargo metadata` / `cargo check` exit 0 |
| §三 #1 `switch_branch` 静默降级 | **已修复**：无摘要器且确有独有后缀 → `BranchSummaryUnavailable` 显式报错 | `crates/context/src/lib.rs:378-380` |
| §三 #2 legacy SSE 不可达 | **已修复**：`type = "sse"` 传输判别已接线 | `crates/mcp/src/sse.rs`、`crates/config/src/mcp_json.rs` |
| §三 #4 `supports_thinking` 恒 false | **已修复**：由 `cfg.agent.enable_thinking` 派生 | `crates/cli/src/main.rs:2244` |
| §三 #6 `MessageUpdate` 死变体 | 仍为死变体（无生产者） | `crates/core/src/llm.rs` |
| §三 #10 workspace 死条目 | 仍存在且**实为 6 条**：`ab_glyph`/`lsp-types`（各有 crate 本地版本未用 `workspace=true`）、`schemars`/`agent-cli`/`pi-shell`/`pi-walker`（零消费者） | `Cargo.toml:60,85,128,169,174,176` |
| §五 H3/H5/H6/H14/H15 落地 | **确认落地**（见 2.1）；剩余偏差以本报告 §4 为准 | 各文件实读 |
| §五 H10「ACP authenticate 空 Ok」 | **已修复为显式错误**；但 session list/resume/fork、斜杠拦截、权限形状仍缺 | `crates/acp/src/rpc.rs:55-61` |
| §六 #6 「README 声称 export」 | 确认 **export 全缺**（`crates/server` 的 `render_session_transcript` 仅服务端内部渲染，非导出功能） | `crates/server/src/lib.rs:2599` |
| §六 #2 「pi-walker 零消费」 | 仍零生产消费者（仅 `vendor/pi-builtins/Cargo.toml:88` 内部引用） | grep 全仓 |

> **计数口径更正**：前报告称 omp 斜杠命令 73、CLI 子命令 41、prompts/tools 44。本轮实测为 **斜杠 77**（缩进感知提取）、**CLI 41**（`cli-commands.ts:22-227`）、**prompts/tools 53**（目录实测）。

---

## 三、结构与模块映射

### 3.1 顶层对应

| omp | Gyre | 对应度 |
|---|---|---|
| `packages/coding-agent/src/*`（主体，~885k 行） | `crates/{cli,agent,context,tools,config,...}`（~117k 行） | 部分 |
| `packages/ai`（248k 行，86 provider） | `crates/llm` + `crates/core/{model,llm}.rs` | 窄 |
| `packages/agent`（34k 行：loop/compaction/pause） | `crates/agent` + `crates/context` | 部分 |
| `packages/tui`（56k 行差分渲染） | 无（rustyline + `markdown.rs`） | 有意不迁移 |
| `packages/utils`（56k 行） | 散入各 crate | 部分 |
| `packages/hashline`（11k 行） | `crates/hashline` | 高 |
| `packages/snapcompact`（3.8k 行） | `crates/snapcompact` | 部分 |
| `packages/mnemopi`（31k 行） | `crates/memory` | 部分 |
| `packages/catalog`（60k 行模型目录） | 无 | 缺失 |
| `packages/wire`（collab 协议） | `crates/collab` | 不兼容 |
| `packages/collab-web` | `crates/server` + `web/c5-ui` + `collab_guest.html` | 形态不同（Gyre 超集） |
| `packages/{agent,omptype,stats,metaharness,natives,browser-relay,typescript-edit-benchmark}` | 部分（`crates/{ast,browser,supervisor,swarm}`）/ 无 | 混合 |
| Rust `crates/{pi-ast,pi-builtins,pi-iso,pi-natives,pi-shell,pi-vcs,pi-voice,pi-walker}` | `crates/{ast}`、`vendor/{pi-shell,pi-builtins,pi-walker,brush-core}`；pi-iso→`crates/iso`；pi-vcs/pi-voice 无 | 部分 |

### 3.2 Gyre 侧 crates（33）与职责

`acp`(2.6k) `advisor`(0.7k) `agent`(11.4k) `ast`(1.1k) `browser`(1.8k) `cli`(12.6k) `collab`(1.5k) `config`(5.0k) `context`(5.9k) `core`(6.7k) `dap`(1.8k) `discovery`(0.4k) `eval`(2.3k) `hashline`(3.8k) `i18n`(0.3k) `iso`(1.3k) `llm`(8.3k) `lsp`(2.4k) `mcp`(7.3k) `memory`(5.2k) `prompt`(0.7k) `proxy`(0.7k) `pty`(0.7k) `search`(0.9k) `server`(4.5k) `shell`(0.4k) `skills`(1.6k) `snapcompact`(0.6k) `supervisor`(2.0k) `swarm`(2.0k) `telemetry`(0.1k) `tools`(19.2k) `ttsr`(1.5k)

---

## 四、分域差距明细

> 类型 ∈ {缺失, 部分, 偏差, 超集}；优先级 ∈ {高, 中, 低}。每条含 omp 锚点 / Gyre 现状 / 影响 / 建议。

### 4.1 CLI 命令面

**规模**：omp 41 子命令（`cli-commands.ts:22-227`，含 2 隐藏 + 别名 `img`/`wt`/`q`）+ 表外 `--smoke-test`/`--license`/`__omp_worker_*`/`--profile`；`commands/` 42 文件。Gyre：clap 单结构体 15 flag（`main.rs:39-113`）+ 手写 `manage::route`（`manage.rs:140-195`，4 动词/16 动作）。

| 项 | omp 位置 | omp 用途 | Gyre 现状 | 类型 | 影响 | 建议 | 优先级 |
|---|---|---|---|---|---|---|---|
| 未知 flag 静默成 prompt | `args.ts:352-360` 硬报错 exit 2 | 参数严格性 | `main.rs:46-50` `allow_hyphen_values` 吞掉一切 | 偏差 | **计费风险**（实测 `--smoke-test` 进入 prompt 路径） | 收窄 `allow_hyphen_values`，未知 `-x` 一律报错 | 高 |
| 38/41 子命令缺失 | `cli-commands.ts:22-227` | acp/agents/bench/commit/completions/config get-set/gc/grep/git/join/plugin/ps/say/share/setup/shell/read/render/ssh/stats/usage/token/update/ttsr/worktree/… | 仅 models/auth/mcp/config 4 动词 | 缺失 | 管理/运维入口面窄 | 按 omp 表分批，先 `usage`/`token`/`stats`/`gc`/`completions` | 高 |
| 子命令 help 失效 | `utils/src/cli.ts:445,468-477` | `agent models --help` | `manage.rs` 无 help 分支（实测进 prompt） | 缺失 | 可发现性为零 | `route` 首查 `--help`/`help` | 高 |
| shell 补全缺失 | `commands/completions.ts:19-31`、`commands/complete.ts:24-29` | bash/zsh/fish + `__complete` | 仅 rustyline 行内补全 | 缺失 | 无法 tab 补全子命令 | clap_complete + `__complete` | 中 |
| 无 `--json` | 18+ 子命令支持 | 机器可读输出 | 全仓无 | 缺失 | 脚本化困难 | 统一 `--json` | 中 |
| launch flag 缺 ~30 | `flag-tables.ts` | `-p/--print`、`-c/--continue`、`--config`、`--profile/--alias`、`--add-dir`、`--no-session`、`--export`、`--tools/--no-lsp/--no-pty`、`--thinking`、`--models`、`--yolo`、`--plan/--prewalk*`、`--from-claude/--from-codex` 等 | 15 flag | 缺失 | 自动化/脚本兼容差 | 按分组补 | 中 |
| `--version` 格式 | `omp/<ver>`，短名 `-v` | 品牌与惯例 | `agent 0.1.0`，短名 `-V` | 偏差 | 版本嗅探脚本失配 | 统一输出与短名 | 低 |
| `--approval-mode` 非法值静默降级 | `flag-tables.ts:226-235` warn+保留 | 参数校验 | `main.rs:503-507` 静默 AlwaysAsk | 偏差 | 误配置不可见 | 报错或 warn | 中 |
| 保留词拦截时机/文案 | `cli-commands.ts:237-255`（9 词）+ i18n | 防误注入 | 23 词（`main.rs:1931-1955`）但拦截在装好 Agent **之后**`main.rs:1285`，无逃生口，硬编码中文 | 偏差 | 误拦截成本高 | 前移到派发前 + `agent launch` 逃生 | 中 |
| 超集 | — | — | `--serve/--lang/--otlp/--socks*/--list-sessions/--rpc/--rpc-forward-ask`、`agent mcp add/…`、`auth login/logout` | 超集 | 保留 | — | — |

### 4.2 工具面

**规模**：omp 29 builtin（`tools/builtin-names.ts:1-31`）+ 3 hidden（`yield/goal/think`）+ 别名 `search→grep`/`find→glob` + `image_gen`/`vibe_*` 动态；Gyre core 7 + ast 3 + image 2 + lsp 2 + hashline/pty/debug/ssh/browser/github + hub/todo/ask/checkpoint/rewind/security_scan/memory×5/task。

| 项 | omp 位置 | omp 用途 | Gyre 现状 | 类型 | 影响 | 建议 | 优先级 |
|---|---|---|---|---|---|---|---|
| `glob` 参数面 | `tools/glob.ts:43-53` | `path`(; 多目标)/`hidden`(true)/`gitignore`(true)/`limit`(200) | `search.rs:395-403` 仅 `pattern`，上限硬编码 100 | 部分 | 无法限目录/排除 gitignore/调条数，静默截断 | 四字段全补 | 高 |
| `read` 截断上限 | `session/streaming-output.ts:9-10`：3000 行/50 KB + artifact 溢出 | 大文件保护 | `fs.rs:1702` 仅 2 MiB 字节，无行限/溢出 | 偏差 | 一次性灌满上下文 | 加 3000 行/50 KB + `read.defaultLimit` | 高 |
| `edit` 模式族 | `edit/index.ts:592-780` 五模式 | patch/apply_patch/hashline/sloppy/replace | 仅 hashline（`crates/hashline/src/tool.rs:63-75`），字段 `patch` vs omp `input` | 部分 | 字段名不兼容，omp 提示词/技能直接失效 | 至少加 `input` 别名；评估 `replace` | 高 |
| `task` 参数面 | `task/types.ts:150-177` | `name`/`agent`/`effort`/`schemaMode`/`isolated`/`context` | `task_tool.rs:411-431`：`task`/`tasks[]`/`output_schema` | 部分 | 无命名/角色/预算/隔离；`outputSchema` 命名不一致 | 统一命名 + 补 `name`/`agent`/`effort` | 高 |
| `ask` 结构 | `tools/ask.ts:57-80` | `questions[]` + `id`/`multi`/`recommended`/`preview` | `ask.rs:45-69` 单问题，无 id/multi/recommended | 偏差 | 多问题/推荐选项不可用 | 迁移到 `questions[]` | 高 |
| `todo` op 集 | `tools/todo.ts:69-92` | `init/start/done/rm/drop/block/unblock/append/view`，按 **content** 定位 | `todo.rs:264-298`：`view/write/start/update/complete/abandon/block/unblock/pending`，按 **id** 定位 | 偏差 | op 名与定位方式全不同 | 统一 op 名与 content 定位 | 中 |
| `memory_edit` op 集 | `tools/memory-edit.ts:6-12` | `update/forget/invalidate` + `replacement_id` | `memory_tool.rs:405-419`：`search/forget/banks/clear` | 偏差 | 几乎正交，缺 update/invalidate | 合并 op 集 | 高 |
| `github` op 集 | `tools/gh.ts:73-105` | `repo_view/file_read/pr_create/pr_checkout/pr_push/search_*/run_watch` 等 11 | `github.rs:59-113`：`get_pr/list_prs/get_issue/create_pr/merge_pr/comment/…` 11 | 偏差 | 仅 `create_pr/title/body/base/head` 重合 | 以 omp op 表为蓝本重写 | 中 |
| `run_command` 缺 `pty` | `tools/bash.ts:316-333` | `command/env/timeout/cwd/pty/async` | `shell.rs:81-101` 有 async，**无 pty**（另设 `run_pty_command`） | 偏差 | 接口不兼容 | 加 `pty?:bool` 或文档固化 | 中 |
| `read` 富格式 | `tools/read-pdf.ts`、`read-selector.ts` | PDF/SVG(:img)/SQLite 查询参数/30+ 归档格式 | `fs.rs` zip+tar.gz；SQLite 仅 `db:table`/`db::SELECT`；无 PDF | 部分 | 文档/归档类不可读 | 补 tar.{bz2,xz,zst} + PDF | 中 |
| 隐藏工具 | `builtin-names.ts:35` | `yield`/`goal`/`think` | 全无 | 缺失 | 子代理无结构化 yield；无 goal 工具 | 按需补 `think`/`goal` | 中 |
| `manage_skill`/`inspect_image` | `tools/manage-skill.ts:14-28`、`inspect-image.ts:38-43` | 技能管理、视觉问答 | 全无（有 `read_image` 直接回图块） | 缺失 | 无法自动铸技能；无问答式看图 | 需要时移植 | 中 |
| `lsp_apply` 接线 | omp 用 `lsp{apply:true}` | 应用 LSP edits | `crates/tools/src/lsp_apply.rs` 存在，但 `main.rs:2130-2134` 只注册 `LspTool` | 偏差 | 工具写了但装不上 | 统一装配点注册 | 中 |
| `computer`/`tts`/`vibe_*`/`xdev` | `tools/{computer,tts,vibe,xdev}.ts` | 桌面/语音/长驻会话/设备挂载 | 全无 | 缺失 | 能力面窄 | 按产品需要 | 低 |
| 工具名别名 | `builtin-names.ts:39-52` | `search→grep`/`find→glob` | 无别名机制 | 缺失 | 旧提示词/技能调用失败 | 注册表 `get/specs` 加规范化 | 中 |
| 超集 | — | — | `intercept`/`minimizer`/`fuzzy_match`/`generated_guard`/`snapshot`/`conflict://`/`xd://resolve|reject`/`read_image`/`image_gen` | 超集 | 保留并文档标注 | — | — |

### 4.3 配置 / 设置面

**规模**：omp `SETTINGS_SCHEMA` **482 键**（`settings-schema.ts:473`），分层 global/project/overlay/runtime/profile，有 `SETTING_HOOKS` 与订阅信号（无文件 watcher）；另有 `models.yml`、`packages/catalog`。Gyre `crates/config/src/config.rs` **~90 键路径**（user+project 深合），`config.example.toml` 292 行；可直接对应 ~35 键。

| 项 | omp 位置 | omp 用途 | Gyre 现状 | 类型 | 影响 | 建议 | 优先级 |
|---|---|---|---|---|---|---|---|
| Settings 运行时单例 | `settings.ts:540` | 运行期读改 | 启动一次加载后 Arc（`main.rs:609`/`server lib.rs:996`） | 缺失 | 改配置需重启 | 进程级 Config 句柄 | 高 |
| 热更新/订阅 | `settings.ts:811-824,894-909,3004-3060` | 显式 reload + hooks + 6 信号 | 仅审批模式/SOCKS5/`/tools` 三处可运行期改 | 部分 | 多数改动需重启 | reload + 变更事件 | 高 |
| 内省 / config get-set | `cli/config-cli.ts:243-424` | `list/get/set/reset/path/init-xdg` + 凭据掩码 | 仅 `config path`/`config check` | 缺失 | 无 schema 内省 | 实现 get/set/list/reset | 高 |
| 自定义 provider 键面 | `models-config-schema-bundle.ts:298-337` | `headers/compat/auth(none|oauth|keyless)/discovery/modelOverrides/transport/remoteCompaction` | `ModelProfile` 仅 id/alias/api/base_url/api_key/api_keys/fallbacks/temperature/max_*/extra_body | 缺失 | 自定义网关/兼容整形/无鉴权端点不可配 | 扩 provider 段 + compat 子集 | 高 |
| 静态模型目录 | `catalog/src/models.json`（66 provider/4727 模型） | 元数据/cost/窗口/能力 | 无（`manage.rs:423` 自注无 catalog） | 缺失 | 元数据全靠手写，压缩/成本不准 | 引入精简内置目录 | 高 |
| 运行时模型发现 | `model-discovery.ts:397-414`、20+ `modelsUrl` | 各 provider `/models` | `crates/llm/src/model_discovery.rs:103-112` 仅 3 api | 部分 | 覆盖窄 | 扩到 responses/zai 等 | 中 |
| 模型选择语法 | `model-resolver.ts:206-235` | `provider/id`、`:thinking`、glob、`@role`、模糊 | 仅 alias/id + role | 缺失 | 脚本/UX 差 | 实现解析器 | 中 |
| 采样参数 | `settings-schema.ts:1582-1691` | temperature/top_p/top_k/min_p/penalties | 仅每模型 temperature | 缺失 | 采样控制缺失 | 扩 `CompletionRequest` | 中 |
| `.env` 加载 | `packages/utils/src/env.ts:240-259` | 4 文件、真实 env 优先 | 无 | 缺失 | 项目级密钥不生效 | 实现 dotenv | 高 |
| `${VAR}` 默认值 | `discovery/helpers.ts:464-495` | `${VAR:-default}` | `env.rs:7-26` 仅 `${VAR}`，未设→空串 | 偏差 | 配置错误被掩盖 | 支持 `:-` + 告警 | 中 |
| `--config` overlay | `settings.ts:156,541-543,1824-1865` | 可重复覆盖 | 无 | 缺失 | 无法临时覆盖 | 加 `--config` | 中 |
| retry 参数 | `settings-schema.ts:1781-1802` | enabled/maxRetries/baseDelay/maxDelay | 硬编码 `RATE_LIMIT_MAX_ATTEMPTS=3` | 缺失 | 无法调优 | 加 `[retry]` | 高 |
| fallback 链 | `retry-fallback-chains.ts:100-360` | 角色/通配链 + cooldown + revert | 仅 `[[models]].fallbacks` 线性 | 部分 | 无角色链/冷却/回切 | 扩展为链+抑制表 | 高 |
| `workspace.additionalDirectories` | `settings-schema.ts:1538` | 额外工作区根 | 无 | 缺失 | 多根不可用 | 加数组并接入工具层 | 高 |
| `tools.approvalMode` 默认 | `settings-schema.ts:4072` 默认 `yolo` | 审批档 | `always-ask`（`GX:73`） | 偏差 | 默认姿态相反（Gyre 更安全） | 明确取舍并文档化 | 高 |
| 未知配置键 | ConfigFile schema 报错 | 拼写校验 | serde 静默忽略（测试 `G:2003-2017`） | 偏差 | 拼写错误静默 | `deny_unknown_fields` 或告警 | 中 |
| 缺失族（量化） | — | UI/主题/键位 ~60、记忆后端 ~65、provider 行为 ~30、逐工具开关 ~40、压缩 ~20 | — | 缺失 | — | 按需分批 | 中/低 |
| 超集 | — | — | `[server]`/`[acp]`/`[socks5]`/`language`/`user_agent`/`[agent.commands]` 规则/`[[hooks]]`/`[goals]` | 超集 | 保留 | — | — |

### 4.4 扩展机制（hooks / 插件 / 自定义工具·命令 / skills）

**规模**：omp `src/extensibility/` 58 文件 19,206 行（extensions 5,002 / plugins 7,651 / hooks 1,409 / custom-tools 657 / custom-commands 1,105）+ 市场链。Gyre：`crates/core/src/hook.rs`(104) + `crates/cli/src/hooks_cfg.rs`(353) + `crates/skills`(7 文件) + `.md` 自定义命令。

| 项 | omp 位置 | omp 用途 | Gyre 现状 | 类型 | 影响 | 建议 | 优先级 |
|---|---|---|---|---|---|---|---|
| Hook 事件全集 | `extensibility/hooks/types.ts:388-403,481-510`（**26 事件**） | session_*/context/agent_*/turn_*/auto_*/ttsr/todo/tool_call/result | `hook.rs:11-31` 仅 3 变体（BeforeTool/AfterTool/Stop）+ TurnEndContext | 缺失 | 会话/压缩/重试/规则生命周期无法挂钩 | 扩枚举，先补 session/compact/agent/turn | 高 |
| stop continuation | `shared-events.ts:97-107,393-403` | 停止前注入上下文并续跑 | `Stop{success}` 纯通知 | 缺失 | 无法做"未完成即续跑"门禁 | `before_stop` 返回 `Continue{context}` | 中 |
| 可取消 session_* 事件 | `shared-events.ts:348-416` | 切换/分支/压缩/树可否决 | 无（`session_event.rs` 仅 6 个只读展示事件） | 缺失 | 无法拦截压缩/切换 | 先接 compact | 中 |
| tool_call 参数改写 | `shared-events.ts:310-332` | deny + 替换 input | `hook.rs:61-67` 仅 deny reason | 部分 | 无法参数纠偏/脱敏 | 返回 `Deny|ReplaceArgs` | 中 |
| 输入/上下文/系统提示改写 | `extensions/types.ts:1141-1145,1257,1285` | 每轮 system prompt/入模消息改写 | 无 | 缺失 | 无法注入/裁剪 | 加 `context`/`before_agent_start` 挂点 | 中 |
| TS 进程内扩展 API | `extensions/types.ts`（1786 行） | 45 事件 + registerTool/Provider/UI/flags | 无宿主；替代形态=编译期 `inventory` provider + MCP + 声明式规则 | 缺失（**明确不 1:1 迁移**） | 无进程内扩展 | 文档化三条替代路径边界 | 低 |
| 插件包/市场 | `plugins/types.ts`、`marketplace/*` | 包清单 + install/update/doctor | 无（仅保留词提示） | 缺失 | 无第三方分发 | 以 MCP + 配置片段作轻量分发单元 | 高 |
| 自定义工具目录 | `discovery/builtin.ts:731-833`、`custom-tools/loader.ts` | `<configDir>/tools/*.ts` | 无（仅内建+MCP） | 缺失 | 除 MCP 外无法加工具 | 编译期 `inventory` 或 MCP-only 策略 | 高 |
| 自定义命令参数替换 | `utils/command-args.ts:41-73` | `$1/$@/$ARGUMENTS` + Handlebars | `repl.rs:1403-1415` 仅尾部追加 `# 命令参数` | 偏差 | 上游 `.omp/commands/*.md`（如 triage.md）直接不可用 | 实现占位符替换 + 无占位符回退 | 中 |
| 命令覆盖优先级 | project>user、TS>.md、builtin 遮蔽 | 确定链 | 扁平表、user 级优先（`config.rs:1112-1129`） | 偏差 | 项目命令无法覆盖用户命令 | project>user + 冲突告警 | 中 |
| skill frontmatter 严格校验 | `discovery/helpers.ts:396-401`、`agent-plugin-format.ts:124-167` | 缺 description 丢弃、name 闭集校验 | `frontmatter.rs:80-112,159-183` 宽松、未知键静默 | 偏差 | 非法 skill 进 prompt；错误不可见 | `require_description` + name 正则 + 逐项 warning | 高 |
| skill 描述注入防护 | `autolearn/managed-skills.ts:62-69` | 剔控制符/`<`/`>`/反引号 | `render.rs:30-40` 仅折叠换行 | 缺失 | `</skills>` 可注入 | 渲染处统一转义 + 限长 | 高 |
| skills provider 覆盖 | 10 provider | native/plugins/claude/agents/codex/opencode/github/managed | 5（native/claude/codex/opencode/github） | 部分 | 插件内与 managed skill 不可见 | 加 agents provider | 中 |
| managed skills / autolearn | `autolearn/managed-skills.ts`、`tools/manage-skill.ts` | 自动铸技能（隔离目录） | 无 | 缺失 | 无自学习技能 | 需要时移植（须复用隔离策略） | 低 |
| containRoot 校验 | `capability/skill.ts:52-58` | realpath 包含判定 | 仅词法拒 `..`/绝对路径 | 缺失 | symlink 可逃逸 | 加 realpath 判定 | 中 |
| 热重载 | `session-tools.ts:1308-1326` | refreshSkills | 无 | 缺失 | 新增 skill 需重启 | `/reload` 重跑 Registry::load | 中 |
| 超集 | — | — | `[[hooks]]` shell 命令规则**真实执行**（omp 该面未接线）；TOML hooks + before_tool deny | 超集 | 保留并文档化协议 | — | — |

### 4.5 协议：RPC 与 ACP

**规模**：omp RPC `RpcCommand` **42 变体**（`rpc-types.ts:28-93`）；ACP agent 方法 12 + 6 `_omp/*`，client 侧 `request_permission`/`fs/*`/`terminal/*`。Gyre RPC 12 命令（`rpc.rs:661-674,1977-2104`）；ACP 8 方法（`crates/acp/src/rpc.rs:47-77`），client 侧仅 `request_permission`。

| 项 | omp 位置 | omp 用途 | Gyre 现状 | 类型 | 影响 | 建议 | 优先级 |
|---|---|---|---|---|---|---|---|
| `negotiate_protocol` 入站 | `rpc-types.ts:30`、`rpc-mode.ts:1011-1015` | v2 升级协商 | 无处理 → unknown error；ready 却宣告 `[1,2]`（`rpc.rs:361`） | 缺失 | 官方客户端握手失败 | 实现协商 + 分片门控在协商后 | 高 |
| 请求 `id` 类型 | 客户端发字符串 `req_<n>` | 关联 | `rpc.rs:58` 强制 `u64` | 偏差 | 字符串 id 直接 `invalid json request` | 改 `Value` 或双轨 | 高 |
| 响应信封 | `{type:"response",command,success,data}` | 统一应答 | `{type:"state"/"ok"/"messages"/"tree"/"models"/"done"/"error"}` | 偏差 | 宿主无法通用解析 | 对齐 omp 信封 | 高 |
| 命令面 30 缺 | `rpc-types.ts:34-93` | steer/follow_up/abort_and_prompt/new_session/set_todos/set_auto_compaction/set_auto_retry/get_session_stats/export_html/switch_session/branch/handoff/get_messages_page/login… | 12 | 缺失 | 嵌入方只能单轮对话 | 先 steer/set_todos/get_session_stats/bash | 高 |
| 出站 v1/v2 门控 | `rpc-frame.ts:288-306` | v1 一帧一对象 | `rpc.rs` 无条件分片 | 偏差 | v1 客户端收到 `rpc_chunk` 不理解 | 协商前走收缩/overflow | 高 |
| 事件映射宽度 | `rpc-mode.ts:979-981` 原样透传 | 宿主状态机 | `map_event` 仅 6 类投影 | 部分 | tool id/result/子代理/会话事件丢失 | 透传结构化事件 | 高 |
| `tool_result` 内容 | `tool_execution_end` 带 `result`/`isError` | 展示输出 | 只发 `{name,ok}` | 偏差 | 宿主看不到输出/错误 | 加 result/isError | 高 |
| 子代理帧 | `rpc-subagents.ts` + 订阅级别 | 子代理可观测 | 无 | 缺失 | 宿主看不到子代理 | 三帧 + 订阅级别 | 中 |
| 宿主工具/URI/扩展 UI 子协议 | `host-tools.ts`、`host-uris.ts`、`rpc-types.ts:376-440` | 宿主提供工具/虚拟 FS/UI | 无 | 缺失 | 嵌入方无法扩展 | 按需 | 中 |
| ACP 权限请求形状 | `protocol.ts:130-134` `{sessionId,toolCall,options}` | 标准审批 | `crates/acp/src/rpc.rs:411-426` 发 `{sessionId,prompt,options}` | 偏差 | 严格客户端拒收 | 改 `toolCall` 字段 | 高 |
| ACP `stopReason` | `acp-agent.ts:1635-1661` 完整映射 | 取消/超长/拒答区分 | 恒 `end_turn`；HTTP 甚至返回非法 `"timeout"`（`http.rs:242`） | 偏差 | 编辑器无法区分 | 补映射 | 高 |
| ACP `session/load` 回放 | `acp-agent.ts:2245-2507` | 恢复并回放历史 | `rpc.rs:250-265` 仅 create，无回放 | 部分 | 打开旧会话空白 | 补三态回放 | 高 |
| ACP `session/list`/`resume`/`fork` | `acp-agent.ts:712-750` | 会话管理 | 全无（server 层已有 resume/fork 能力） | 缺失 | 编辑器无历史/分叉 | 透出 server 能力 | 中 |
| ACP `available_commands` | `acp-agent.ts:2094-2100` 用**通知** | 斜杠菜单 | Gyre 用**非标准请求方法**（`rpc.rs:305-323`），且目录硬编码 15 条（`main.rs:229-252`） | 偏差 | omp/Zed 不调用 → 菜单空 | 改推送通知 + 由注册表生成 | 高 |
| ACP `initialize` 能力 | `acp-agent.ts:629-673` | authMethods/sessionCapabilities/mcp http+sse | `rpc.rs:152-195`：`authMethods:[]`，无 sessionCapabilities，mcp 无 sse，`agentInfo.name="agent-project"` | 部分 | 编辑器不显示列表/恢复；品牌名错 | 补能力 + 真实名 | 高 |
| ACP tool_call 字段 | `acp-event-mapper.ts:478-503` | `rawInput`/`content`/`locations` | `types.rs:127-140` 仅 id/title/kind/status/rawOutput | 部分 | 无法跳转文件/diff | 补 rawInput+locations | 中 |
| 超集 | — | — | ACP over HTTP+SSE（omp 仅 stdio）；`--rpc-forward-ask` 反向审批 | 超集 | 保留 | — | — |

### 4.6 MCP

**结论**：Gyre `crates/mcp` 是**结构化移植且部分反超**——三传输、initialize（协议 2025-11-25 + 头 + session id）、tools/list+call、prompts/list+get→斜杠、401 OAuth 刷新单飞、重连熔断半开、Deferred + 250 ms 启动预算、语义指纹缓存、多源 mcp.json、CLI 写回、instructions 注入、`mcp://` 路由、动态工具源。**主要缺口在双向能力与内容保真**。

| 项 | omp 位置 | omp 用途 | Gyre 现状 | 类型 | 影响 | 建议 | 优先级 |
|---|---|---|---|---|---|---|---|
| **capabilities 解析/能力门控（真 bug）** | `mcp/client.ts:214-216,285,332` | 按 capabilities 决定是否发请求 | `crates/mcp/src/client.rs:441-449` 只取 instructions；`tool.rs:984-990` list_tools 失败即 `client.close()` | 缺失 | **纯 resources/prompts server 整体连接失败** | 解析 capabilities + 按能力发请求 | 高 |
| server→client 请求应答 | `mcp/client.ts:55-68`（ping/roots/list） | 双向能力 | `stdio.rs:94-99`、`sse.rs:290`、`http.rs:169-186` 当响应丢弃；`client.rs:290` 声明 `capabilities:{}` | 缺失 | server 永远拿不到 roots 应答 | 实现请求分发 + roots 能力 | 高 |
| tools/call 结果保真 | `mcp/tool-bridge.ts:207-239` | image/resource 块 + isError | `client.rs:487-499` 仅拼 `content[].text`，无 isError | 部分 | 图像类 MCP 结果丢失 | 保留块结构 + isError | 高 |
| HTTP 常驻 GET SSE + Last-Event-ID | `transports/http.ts:200-371` | 服务端推送续传 | `http.rs:11-12` 自述未实现 | 缺失 | 无推送 | 实现 GET 流 | 中 |
| `/mcp` 交互子命令 | `mcp-command-controller.ts:408-459` | test/reauth/unauth/resources/prompts/notifications/reload/reconnect | `manage.rs:171-183`+`repl.rs:1001-1056` 仅 add/remove/enable/disable/list/status/login/logout | 部分 | 运行中无法热重载/自检 | 逐项转 `manage::route` | 中 |
| resources/templates + subscribe | `client.ts:328-446` | 模板 URI + 更新订阅 | 无（`tool.rs:456-458` 仅 debug 日志） | 缺失 | 资源模板不可达 | 按需 | 中 |
| 多源发现广度 | `discovery/*` 12 provider | claude/codex/cursor/windsurf/vscode/gemini/opencode/plugins… | `mcp_json.rs:73-93` 4 文件 | 部分 | 生态发现窄 | 扩到主要外部工具 | 中 |
| 工具参数出站加工 | `tool-bridge.ts:90-183` | 剥 INTENT、剔空占位、`local://`→路径 | `tool.rs:194-197` 原样透传 | 缺失 | 严格 schema server 拒收 | 加加工层 | 中 |
| 结构错误分类 | `mcp/errors.ts`（290 行） | stage/failure/retryable/trace_id | `client.rs:20-63` 简单 | 部分 | 排障粗 | 按需 | 中 |
| per-server `enabled`（TOML） | `types.ts:66-70` | 配置级禁用 | `config.rs:947-963` TOML 无 `enabled` | 缺失 | 无法在 TOML 禁单 server | 补字段 | 中 |
| 超集 | — | — | 重连五态熔断半开、语义指纹缓存（0600+原子写）、父子共享动态工具源 | 超集 | 保留 | — | — |

### 4.7 提示词与 system prompt 资源

**规模**：omp `prompts/` **178 篇 .md**（tools 53 / system 78 / agents 8 / memories 9 / goals 6 / bench 6 / advisor 5 / security 3 / skills 2 / steering 2 / session 1 / 根 5），装配 `system-prompt.ts`(1040 行，Handlebars)。Gyre 根 `prompts/` **7 篇**（system-{code,architect,ask,debug,plan} 共 41 行 + review + compaction-summary 113 行），其余内联常量。

| 项 | omp 位置 | omp 用途 | Gyre 现状 | 类型 | 影响 | 建议 | 优先级 |
|---|---|---|---|---|---|---|---|
| **四段行为章节装配（rpc/server）** | `system-prompt.md:103,147,174,219,246` | 行为契约主体 | 仅 `main.rs:836-839`；`rpc.rs:1568`、`server/lib.rs:1274` 未注入 | 缺失 | Web/RPC 无 Tool Policy/Delegation/Workflow/Delivery | 抽公共构造，三处统一 | 高 |
| 子代理角色模板 | `prompts/agents/`（7 角色）+ `system/subagent-*.md` | reviewer/scout/librarian 等 | `task_tool.rs:584` 仅通用指令 | 缺失 | 子代理同质化 | 建 `prompts/agents/*.md` + 按 `agent` 选模板 | 高 |
| 上下文文件双通道去重 | `system-prompt.ts:425,451` | depth 排序 + 段落包含去重 | `config.rs:1164` 与 `discovery/lib.rs:71` 两通道**互不去重** | 偏差 | 祖先 AGENTS.md 重复注入 | 统一收集器 + 段落去重 | 高 |
| `@import` 展开 | `discovery/at-imports.ts`（273 行） | 兼容 Claude/Goose/Cline 的 `@path` | 全仓无 | 缺失 | 外来配置引用原样进 prompt | 收集后注入前展开 | 中 |
| SYSTEM.md 覆盖链 | `system-prompt.ts:487-505` | 用户/项目覆盖主模板 | 无 | 缺失 | 无法替换 system prompt | 加发现链 | 中 |
| `--append-system-prompt` | `system-prompt.ts:387,595,1025-1030` | 宿主追加 | 全仓无 | 缺失 | 宿主无法追加指令 | AgentBuilder 加参数 | 中 |
| 工作区树/多根 | `workspace-tree.ts`、`project-prompt.md:33-48` | depth≤3 树 + 多根 | `prompt/src/lib.rs:99` 仅一行 cwd | 缺失 | 探索成本高 | 树生成器 + 多根 | 中 |
| rules alwaysApply/globs | `system-prompt.ts:962-973` | alwaysApply 全文 / 其余索引 + `rule://` | `discovery/lib.rs:299-313` 全文无条件注入，globs 降级散文 | 偏差 | token 膨胀；无按需 | 区分两类 + `rule://` | 中 |
| 通知模板文件化 | `prompts/system/*`（约 70 篇） | 通知/中断模板 | 内联 `engine.rs:1075,1123,1162`、`keywords.rs:28-74` | 部分 | 无法审阅/本地化 | 抽 `prompts/notifications/` | 中 |
| goals / memories / plan / advisor / security / review 变体 | 各目录 | 场景化模板 | 仅各 1 条内联或 1 篇 | 缺失 | 场景语义薄 | 按域迁移 | 中 |
| `<file path>` 包装 + auto-loaded 语义 | `project-prompt.md:8-29` | 结构化包裹 + "NEVER grep AGENTS.md" | `config.rs:1170,1191` 纯文本前缀 | 缺失 | 模型仍反复 grep | 加包裹 + 声明 | 中 |
| 超集 | — | — | i18n（4 locale × 338 key）omp 无；`crates/prompt` 的 enhance/suggest/mentions | 超集 | 保留 | — | — |

### 4.8 运行时核心（会话 / 压缩 / 子代理 / 异步 / 记忆）

**规模**：omp `packages/agent`(34k) + `src/{session,task,memories,memory-backend}` + `packages/mnemopi`(31k) + `packages/snapcompact`。Gyre 对应 6 crate。**判断：单机骨架高忠实，缺"编排/生命周期层"**。

| 项 | omp 位置 | omp 用途 | Gyre 现状 | 类型 | 影响 | 建议 | 优先级 |
|---|---|---|---|---|---|---|---|
| **job 生命周期接线** | `agent-session.ts:1989,2047,4275`、`executor.ts:3527`、`sdk.ts:4281` | 会话结束 cancelAll+wait+drain；teardown reap | 方法全在 `core/jobs.rs`，但**生产 0 调用**（grep 确认） | 缺失 | 退出丢投递、作业不取消/不排空 | CLI/server/session 关闭点接入 | 高 |
| 子代理定义发现 | `task/discovery.ts:43-139`（`.omp/agents/*.md`） | 分层去重 + frontmatter | 全无 | 缺失 | 单一匿名 task | `.gyre/agents/*.md` 解析 | 高 |
| spawn-policy / 只读白名单 | `spawn-policy.ts:19-72`、`read-only-policy.ts:10-30` | allow/deny + 递归深度 + 只读集 | 仅全局 MAX_INFLIGHT=16；子工具表无 task（不能嵌套） | 部分 | 只 1 层、无只读委派 | 加 spawns/maxRecursionDepth + 只读集 | 高 |
| 软预算 + grace | `executor.ts:107-131,1980-2029` | 请求预算→steer→强 yield→grace(5) | 无 | 缺失 | 子代理可无限烧 token | engine 计数 + steer + 硬停 | 高 |
| worktree 隔离接线 | `worktree.ts`(1028 行)、`isolation-runner.ts` | CoW 隔离 + owner + 合并 | `crates/iso` 原语齐备但 `with_isolation` **0 调用**；`engine.rs:1382` 恒 false 守卫 | 部分 | 隔离不可达，子代理直改父工作区 | TaskTool 装配 | 高 |
| 自动压缩触发策略 | `compaction.ts:337,362,307,358` | 阈值=窗口−reserve(max(15%,16384))；5 种 reason | `engine.rs:372` 固定 0.8；Overflow/Idle/Incomplete 定义了但 0 调用 | 偏差 | 窗口=0 永不压缩；无 overflow/idle | 加绝对/百分比/reserve + 分支 | 高 |
| token 计算 | `tokenizer.ts:12-291` | 各模型族原生编码 + 图像 1200 + 帧常量 | `crates/context/src/token.rs:24-31` 仅 tiktoken cl100k/o200k | 偏差 | 非 OpenAI 计数失真；snapcompact 帧误算 → 压缩后误再触发 | 家族编码/系数 + 帧常量 5024 | 高 |
| todo eager prelude | `todo-tracker.ts:133,174`、`prompts/eager-todo.md` | 首轮/压缩后注入建清单 | 仅静态提示 `prompt_sections.rs:89-90` | 缺失 | 首轮不落计划 | engine 首轮注入 | 高 |
| todo 完成提醒/对账 | `todo-tracker.ts:204,258-292` | 终止轮未完成→reminder+续跑 | 无 | 缺失 | 半途停下不续跑 | 停止分支读 TodoState | 高 |
| goal loop | `goals/runtime.ts:384-512`、`goal-tool.ts:58` | objective + 预算 + 续跑 + 证据审计 | 仅预算记账 `lib.rs:48-116` | 部分 | 无目标状态机/工具/续跑 | goal 工具 + continuation prompt | 高 |
| `--continue`/breadcrumb | `session-paths.ts:216,256`、`args.ts:241` | `-c` 恢复最近会话 | 仅 `--resume <id>` | 缺失 | 必须记 id | `.last` + `--continue` | 高 |
| session header 字段 | `session-entries.ts:35-54` | id/title/cwd/parent/promptCacheKey | `persistence.rs:28-36` 仅 version/created_at/agent | 部分 | resume 不恢复 cwd/溯源 | 扩 header（serde default 保兼容） | 高 |
| memory 后台 rollout/lease | `memories/index.ts:345-601`、`storage.ts:137-488` | 后台两阶段 + SQLite 租约 + 30s 续租 | `main.rs:2491-2600` 改为 Stop hook **内联 await** LLM | 缺失 | 会话末阻塞 agent；无历史回填 | 迁 jobs 队列 + 两阶段 | 高 |
| structured append 竞态 | omp SQLite/O_APPEND | 并发安全 | `structured.rs:1066-1077` read-modify-write（`store.rs:290-304` 已 O_APPEND，不一致） | 偏差 | 并发写丢数据 | 改 O_APPEND | 高 |
| fork 语义 | `session-manager.ts:1498-1536` | 新 header(parent)+artifacts | `persistence.rs:419-441` `fs::copy` + `.leaf` | 部分 | 无 parent、不复制 artifacts | fork 后重写 header + 复制 | 中 |
| 压缩记录/回放 | `session-entries.ts:34-52` | CompactionEntry + firstKeptEntryId | `persistence.rs` 仅 Header/Node，压缩全量重写 | 缺失 | 活跃历史不可回放 | 加 CompactionEntry | 中 |
| 统一 OutputSink | `streaming-output.ts:52-92` | 行缓冲/列上限/节流/artifact 溢出 | 无；`shell.rs`/`ssh.rs`/`fs.rs` 各写一份 | 缺失 | 语义漂移 | 抽公共 sink | 中 |
| auto-background | `async/auto-background.ts:9-78` | 前景 60 s 自动转后台 | 无（仅显式 `async:true`） | 缺失 | 长命令不自动后台化 | 移植阈值 + race | 中 |
| yield queue | `session/yield-queue.ts:43-288` | kind/epoch/恰好一次 | 无；结果直接 append | 缺失 | 陈旧结果可入上下文 | async-result kind + epoch | 中 |
| unexpected-stop 分类器 | `unexpected-stop-classifier.ts:44-154` | "承诺继续却停止" | 全无 | 缺失 | 第四条恢复路径缺 | 移植 + 3 次上限 | 中 |
| auto-title | `title-generator.ts:140-220` | tiny 模型生成标题 | 仅手动 `_titles.json` | 缺失 | 新会话无标题 | 首轮后生成 | 中 |
| plan mode | `plan-mode/approved-plan.ts` 等 6 文件 | 批准流/计划文件/handoff/保护/切模型 | 仅 `Mode::Plan` + 写限制 | 部分 | 无闭环 | 迁移 4 件套 | 中 |
| 超集 | — | — | supervisor 子代理总线（7 态 + REST/WS）、活跃叶子 sidecar、CJK 分词/MMR/中文时间/L1 嵌入/多 bank、goal hard_stop、守卫中断上限 | 超集 | 保留 | — | — |

### 4.9 LLM 层

**规模**：omp 14 api id / 86 provider 注册文件（69 带 login，26 OAuth 文件）/ `models.json` 66 provider 4727 模型 / 104 compat 位 + 4327 行 KDL 规则 / 11 方言 / ~15 真 OAuth 流。Gyre 7 `Api`（`core/src/model.rs:9-31`，`openai-responses` 无适配器）/ 5 适配器（`llm/src/plugin.rs:39-53`）/ 3 OAuth 流 / 0 目录 / 0 compat / 1 XML 方言。

| 项 | omp 位置 | omp 用途 | Gyre 现状 | 类型 | 影响 | 建议 | 优先级 |
|---|---|---|---|---|---|---|---|
| OpenAI Responses wire | `providers/openai-responses.ts:924` | `/v1/responses` | 枚举在，无适配器（`openai.rs:30-35` 排除） | 缺失 | gpt-5*/Responses-only 不可用 | 新增适配器 | 高 |
| Codex Responses | `openai-codex-responses.ts:2950` | ChatGPT 后端 + `x-codex-*` | OAuth 存 `openai-responses`（`oauth/codex.rs:80`）但无 wire | 缺失 | Codex 登录死链 | 随上行 + 补头 | 高 |
| compat 位表/引擎 | `catalog/src/compat/*`（104 flags） | 按模型声明差异 | 无 | 缺失 | quirk 全硬编码 | 建精简 compat 表 | 高 |
| 静态模型目录 | `catalog/src/models.ts:43` | cost/窗口/能力 | 无 | 缺失 | 元数据全靠手写 | 引入精简目录 | 高 |
| OAuth 流覆盖 | `registry/oauth/*`（~15 真流） | copilot/xai/gemini-cli/antigravity/gitlab-duo/kimi… | 仅 anthropic/codex/zai（`oauth/mod.rs:133-139`） | 缺失 | 多 provider 无法登录 | 按需补流 | 高 |
| 401 自动刷新/换号 | `auth-retry.ts:8,159,214`（上限 64） | 401/403 换号 | 无（仅 registry fallback） | 缺失 | 401 即失败 | 实现换号刷新 | 高 |
| 采样参数 | compat `supportsSamplingParams` | top_p/top_k/penalties/stop/seed | 仅 temperature | 缺失 | 采样控制缺失 | 扩 `CompletionRequest` | 中 |
| reasoning token 计量 | `openai-shared.ts:3707-3760` 等 | 独立字段 | `core/message.rs:116-127` 无；Gemini 并入 output | 缺失 | 计费/展示失真 | 加字段并回填 | 中 |
| redacted/encrypted 思考 | `ai/src/types.ts:721`、`anthropic-wire.ts:155` | 密文思考回放 | `core/message.rs:149-178` 无 | 缺失 | 部分模型多轮 400 | 加变体 | 中 |
| 成本计算 | `catalog/src/models.ts:67-110` | 缓存乘子计价 | `cost_usd` 恒 0（`message.rs:126` 从不赋值） | 缺失 | 成本显示 0 | 加计价 | 中 |
| Usage 全字段 | `catalog/src/types.ts:101-155` | reasoningTokens/credits/cttl… | 5 字段 | 缺失 | 口径窄 | 扩字段 | 中 |
| OpenAI prompt cache | `openai-completions.ts:1582-1641` | prompt_cache_key/断点/TTL 30m/24h | `openai.rs:171-217` 无；`cache_key` 仅赋值不发送 | 缺失 | 缓存未命中 | 发送 key + 断点 | 中 |
| Anthropic 断点分布 | `anthropic.ts:3497-3509`（1 system + 2 消息，tools 从不标记） | 配额分配 | `transform.rs:76-125` 标 system+tools+1 消息 | 偏差 | 配额浪费在 tools 上 | 去 tools 断点、消息侧 2 条 | 中 |
| thinking 档位/钳位 | `stream.ts:1742-1767`、`anthropic.ts:3435-3457` | 每档预算 + max_tokens 钳位 | 单一 budget，无钳位 | 部分/偏差 | budget>max_tokens 时 400 | 加档位表 + 钳位 | 中 |
| 方言库 | `dialect/factory.ts:13-25`（11） | in-band 工具协议 | 仅 `Dialect::Xml` | 部分 | 私有标记模型不稳 | 按需 | 中 |
| 错误分类/Retry-After | `error/flags.ts:20-46`（20 位）、`utils/retry-after.ts:73-91` | 结构化错误 + HTTP-date | 7 变体；仅整数秒否则 5s | 部分 | 排障粗、退避不准 | 扩 flags + 日期解析 | 中 |
| in-band 流错误 | `openai-completions.ts:638-662` | SSE 内 error→HTTP | 无 | 缺失 | 错误被当文本吞 | 加检测 | 中 |
| Ollama 原生协议 | `providers/ollama.ts:773` | `/api/chat` NDJSON | `Api::OllamaChat` 走 OpenAI 兼容 | 偏差 | 原生能力缺 | native 适配器 | 中 |
| 额度/quota 报告 | `ai/src/usage.ts:104-119` + `usage/*` 20 provider | 订阅配额展示 | 无 | 缺失 | 订阅用户无用量视图 | 按需 | 中 |
| 其他 wire（Bedrock/Vertex/Azure/Cursor/Devin/GitLab Duo） | `providers/*` | — | 无 | 缺失 | 企业/订阅接入窄 | 按需 | 中/低 |
| 超集 | — | — | `api_keys` 轮换、fallback 模型链、`stable_prefix_len`、inventory 插件注册、Anthropic signature/GLM preserveReasoning/Gemini thoughtSignature 回放 | 超集 | 保留 | — | — |

### 4.10 交互层与斜杠命令

**规模**：omp `slash-commands/` 6,478 行 / **77 条顶层命令**；`modes/` 交互层 `interactive-mode.ts` 5,988 行 + 22 controller + 94 component + theme 4,921 行 + wizard 2,102 行；`keybindings.ts` 750 行；`packages/tui` 27,938 行。Gyre `repl.rs` 2,190 行 / **33 条命令** + `main.rs` rustyline 循环 + `tree_ui.rs`(444)/`agents_view.rs`(188)/`markdown.rs`(321)。

| 项 | omp 位置 | omp 用途 | Gyre 现状 | 类型 | 影响 | 建议 | 优先级 |
|---|---|---|---|---|---|---|---|
| Esc 分级中断 | `input-controller.ts:326-463` | 优先级链式中断 | 无 raw 模式 | 缺失 | **运行中无法中断轮次** | crossterm raw + Esc 分发 | 高 |
| Ctrl-C 语义 | `input-controller.ts:1101-1132` | 清编辑器/二次 shutdown/`exit(130)` | `main.rs:1324` 仅 `continue`；运行期无 SIGINT | 偏差 | 提示符下无法退出；运行中直接杀进程 | 进程级 ctrl_c + 分级 | 高 |
| 消息队列 | `queue-input.ts`、`/queue` | 排队/出队/枚举拆分 | `main.rs:2802-2824` 仅单行 steering | 缺失 | 无法批量/撤回输入 | `VecDeque` + dequeue 键 | 高 |
| 可定制键位 | `config/keybindings.ts`（39 动作） | remap 到 yml | 无 | 缺失 | 无法改键 | action 表 + yml 加载 | 高 |
| `/clear` | `builtin-lifecycle.ts:120` | 原地清上下文保 session | 无（只能 `/fresh` 换 id） | 缺失 | 丢会话连续性 | `context.clear()` 保 id | 高 |
| `/todo` 可写 | `builtin-session.ts:141-178` | 11 子命令 | `repl.rs:360-364` 只读渲染 | 部分 | 只能在 REPL 看不能改 | 至少 append/start/done/drop/rm | 高 |
| `/resume` 选择器 | `builtin-lifecycle.ts:302-324` | 无参弹选择器 + `@claude/@codex` 导入 | 无参只打用法 | 偏差 | 手抄 id | 交互选择器 | 高 |
| 44 条命令缺失 | 见 omp 各 `builtin-*.ts` | `/settings`/`/hotkeys`/`/context`/`/usage`/`/jobs`/`/retry`/`/new`/`/drop`/`/rename`/`/memory`/`/handoff`/`/shake`/`/login`/`/logout`/`/export`/`/share`/`/join`/`/leave`/`/theme`… | `repl.rs:166-202` 无对应 | 缺失 | REPL 功能面 ≈ omp 43% | 分批：先 `/settings`+`/hotkeys`+`/context`+`/usage`+`/jobs`+`/retry`+`/new` | 高/中 |
| `/compact` 模式 + focus | `builtin-lifecycle.ts:140` | soft/remote/snapcompact + focus | `repl.rs:402` 单一流程 | 部分 | 无模式/focus | 解析 `[mode] [focus]` | 中 |
| `/plan` 评审流 | `builtin-modes.ts:228,249` | 评审 overlay | 仅 `SwitchMode(Plan)` | 偏差 | 无批准流 | 加 overlay | 中 |
| 主题 token 化 | `modes/theme/`（4,921 行） | 语义色/亮暗/符号集 | ANSI 硬编码 | 缺失 | 浅色终端不可读 | Theme trait + dark/light | 中 |
| 首启向导 | `setup-wizard/`（2,102 行） | provider/主题/模型选择 | 无（缺配置直接报错） | 缺失 | 新用户无法自助 | 交互式 provider/model 选择 | 中 |
| 历史持久化 | `interactive-mode.ts:5634` | 历史落盘 | `DefaultHistory` 不落盘 | 部分 | 重启丢历史 | `FileHistory` | 中 |
| 参数补全 | `builtin-completions.ts:1-291` | 子命令/enum/目录 | 仅 7 命令 | 部分 | 覆盖面窄 | 由命令元数据生成 | 中 |
| 文件路径补全 | omp `@` 补全 | `@file` 提及 | 无（非 `/` 开头返回空） | 缺失 | 手打路径 | 加路径候选 | 中 |
| 图片粘贴键位/回显 | `input-controller.ts:503-508`、`image-references.ts` | Ctrl+V 贴图 + `[Image #N]` | 仅 `/paste` 命令，无回显/缩放 | 部分 | 需打命令；大图浪费 token | 挂 Ctrl+V + 回显 + 缩放 | 中 |
| ACP 命令目录 | `acp-builtins.ts:41-63`（60+ 动态） | 编辑器命令面板 | `main.rs:229-252` 硬编码 15 | 偏差 | 面板缺项 | 由注册表生成 | 中 |
| 有意不迁移 | `packages/tui` 27,938 行 | 差分渲染/94 component/OSC 5522 | — | 不迁移 | — | 需要时引 crossterm 局部 | — |
| 超集 | — | — | `/enhance` `/suggest` `/review` `/swarm` `/lang` `/diff` `/github` `/mode`；i18n 4 语言；流式 Markdown；TOML shell hooks | 超集 | 保留 | — | — |

### 4.11 外围功能模块

omp `coding-agent/src/` 下的独立子系统逐项判定（行数为实测）：

| omp 模块 | 规模 | 用途 | Gyre 对应 | 类型 | 优先级 |
|---|---|---|---|---|---|
| `registry`（agent 注册表） | `registry/agent-registry.ts:117`、`agent-lifecycle.ts:89`、`persisted-agents.ts:500` | 进程级注册/寻址/park/revive/roster 扫描 | **完全缺失**；最近似 `crates/supervisor/src/registry.rs:41`（仅只读观测） | 缺失 | 高（hub/irc 的地基） |
| `sdk.ts` | `sdk.ts:830,918,1287` | 程序化嵌入 SDK（单入口装配 + `discover*` 助手） | 无 SDK crate；装配在 `cli/main.rs:2105`、`server/lib.rs:974`、`cli/rpc.rs:1376` **三处重复且已漂移** | 部分 | 高 |
| `discovery` | 28f/9,056L | ~20 provider（claude/codex/cursor/gemini/opencode/windsurf/vscode/agents/plugins）+ `at-imports.ts` + `builtin-rules` | `crates/discovery/src/lib.rs:71` 仅 4 源；无 at-imports/builtin rules；`server/lib.rs:1274` 未接 discovery | 部分 | 高 |
| `irc`（agent 总线） | `irc/bus.ts:66` | agent↔agent 邮箱：回执/唤醒/broadcast/await-stopped | `crates/core/src/hub.rs:47`；**仅 "main" 入册**（`cli/main.rs:999`、`server/lib.rs:1230`），`:12` 自述缺 ACK/唤醒 | 部分 | 高 |
| `goals` | 4f/812L | objective 生命周期 + 续跑 prompt + `goal` 工具 | 仅预算 `crates/agent/src/lib.rs:48,71`（`engine.rs:873`）；`grep GoalTool\|goal_continuation` → 0 | 部分 | 高 |
| `advisor` | 10f/3,280L | 独立只读评审 + 投递守卫 | `crates/advisor/src/lib.rs:66`，生产调用 1 处（`agent/src/engine.rs:305`）；仅 env 开关、复用主 provider、独立简化脱敏器 | 部分 | 中 |
| `web` | 114f/27,260L | 23 搜索 provider + 75 scraper + fetch | `crates/tools/src/web_search.rs:869`：4 provider + `extract_site:640` 4 站点；**无 fetch 工具** | 部分 | 中 |
| `capability` | 18f/1,814L | 统一 capability 注册表（priority/去重/禁用/内省） | 无统一抽象（`skills/registry.rs:11`、`discovery/lib.rs:33`、`tools/lib.rs:324` 各自实现） | 偏差 | 中 |
| `commit` | 50f/7,582L | agentic 提交（conventional/hunk 拆分/changelog） | 无 | 缺失 | 中 |
| `blob-broker` | 26f/8,678L | 图片外置 URL + 上传目的地（Files API） | 无（仅 `server/lib.rs:67 ImageRef` 内联 base64） | 缺失 | 中 |
| `markit` | 8f/822L | pdf/docx/pptx/xlsx/epub → markdown | 无（`fs.rs:1344-1354` 仅 zip/tar/sqlite/ipynb） | 缺失 | 中 |
| `debug` | `debug/index.ts:576`、`report-bundle.ts:85` | 调试菜单/报告包/raw SSE 探针 | 无（仅 `crates/telemetry`） | 缺失 | 中 |
| `internal-urls` | 22f/5,082L | 15 种内部 scheme 路由 | `crates/tools/src/fs.rs:301-310` 硬编码分派 9/15；缺 `rule://`/`security://`/`vault://`/`history://`/`agent://`/`omp://` | 部分 | 中 |
| `security` | 20f/4,668L | 扫描协调/校验/云扫描/SARIF | `security_scan` 单工具（`:10` 自述无 SARIF/plan/远程） | 部分 | 中 |
| `ssh` | `ssh/connection-manager.ts:734`、`file-transfer.ts:70` | ControlMaster + 远端文件 I/O + sshfs + host CRUD | `crates/tools/src/ssh.rs:45` BatchMode oneshot（`:48` 自述不长连接） | 部分 | 中 |
| `exec` | `exec/bash-executor.ts:454`、`direnv.ts:116` | 持久会话/direnv/user-shell/PTY | `shell.rs` oneshot；**`crates/pty/src/session.rs:177 PtyShell` 零生产调用方**；无 direnv | 部分 | 中 |
| `lsp` | `lsp/tool.ts:170`、`lspmux.ts:132` | LSP 群 + ledger + lspmux + Biome/SwiftLint | `crates/lsp` + `lsp_tool.rs`（11 动作）+ `lsp_write_effect.rs` 真实接线；无 lspmux/部分动作 | 部分 | 中 |
| `dap` | `dap/session.ts:284`、`defaults.json` | 完整 DAP 客户端 | `crates/dap/src/manager.rs:404` 14 动作；`lib.rs:66` 3 适配器（omp 14） | 部分 | 中 |
| `eval` | `eval/index.ts`、`tools/eval.ts:307` | 多语言持久内核 + 编排桥 | `crates/eval/src/manager.rs:33` 仅 py/js；无 `__agent__` 桥 | 部分 | 中 |
| `plan-mode` | `plan-mode/approved-plan.ts:156`、`plan-files.ts:8` | 计划文件审批→切片执行 + 压缩保护 | 仅 `Mode::Plan` 写守卫（`config/rules.rs:96`） | 部分 | 中 |
| `memory-backend` / `mnemopi` / `hindsight` | `memory-backend/resolve.ts:20`、`mnemopi/embed-client.ts:103`、`hindsight/backend.ts:38` | 后端抽象 + 嵌入 worker + bank + 远端 Hindsight | `core/memory.rs:55` + `config.rs:863`（仅 Local/Structured）；嵌入进程内；无 Hindsight | 部分/缺失 | 中 |
| `priority.json` | `priority.json:1`、`model-resolver.ts:31` | smol/slow/designer 模糊优先链 | 无（仅显式角色 `config.rs:22,233`） | 缺失 | 中 |
| `secrets` | `secrets/index.ts:247`、`obfuscator.ts:62` | 出向脱敏 + 入向还原 + secrets.yml | `crates/core/src/secrets.rs:240,360`，生产 2 处；无 secrets.yml/友好名占位 | 部分 | 中 |
| `workspace-tree.ts` | `workspace-tree.ts:52,89` | 目录树渲染（prompt + read 变体） | 无（`list_tool.rs:59` 仅扁平路径） | 缺失 | 中 |
| `telemetry-export*.ts` | `telemetry-export.ts:68`、`-otlp.ts:116` | OTLP 引导 + trace/log/metric 三信号 | `crates/telemetry/src/lib.rs:16,48` 仅 span exporter（tonic） | 部分 | 低 |
| `tts` | 12f/2,410L | 语音合成（sherpa-onnx） | 无（`crates/ttsr` 是流规则引擎，同名异义） | 缺失 | 低 |
| `stt` | 10f/2,091L | 语音识别 | 无 | 缺失 | 低 |
| `live` | 6f/1,502L | Codex realtime 语音会话 | 无 | 缺失 | 低 |
| `export` | 6f/1,808L | HTML 导出 + 加密分享 | 无（TTSR 部分已超集独立成 `crates/ttsr`） | 缺失 | 中 |
| `vibe` | 3f/1,735L | director + 持久 worker | 无（`crates/swarm` 是 swarm-extension，语义不同） | 缺失 | 低 |
| `tiny` | `tiny/title-client.ts:183` | 端上小模型（标题/抽取/停止分类） | 无（无自动标题） | 缺失 | 低 |
| `activity` | 1f/476L | 子 agent activity 时间线索引 | 仅 `supervisor/model.rs:101` 单行状态 | 缺失 | 中 |
| `sharpshooter` | 8f/1,224L | transcript→决策 delta | 无 | 缺失 | 低 |
| `autolearn` | 2f/407L | 自动学习→managed SKILL.md | 无（`learn` 只落记忆 `memory_tool.rs:477`） | 缺失 | 低 |
| `cleanse` | `cleanse/index.ts:50` | 诊断→修复→复验闭环 | 无 | 缺失 | 低 |
| `autoresearch` / `if-bench` / `exa` / `cursor.ts`+`cursor-bridge-tools.ts` / `startup-splash.ts` | 3–8f / 1.1k 行 | 实验循环/基准/Exa/Cursor IDE 桥/首屏 | 多数无；`if-bench` 与 `subprocess` 判为不迁移 | 混合 | 低 |
| `launch` | 11f/2,845L | daemon broker + attach + presence | `crates/supervisor/process.rs:269`（`:4-9` 自述无 broker/持久化/PTY） | 偏差（有意） | 低 |
| `memories` | `memories/index.ts:123,1332` | local 记忆两阶段合并流水线 | `crates/memory/src/store.rs:26,86`；无 SQLite 租约/watermark（`:17` 自述有意偏差） | 部分 | 低 |
| `jsonrpc` | `jsonrpc/message-framing.ts:83` | LSP/DAP 共用分帧（含 resync） | 两份分叉实现（`crates/dap/src/frame.rs:21` vs `crates/lsp/src/transport.rs:363`） | 偏差（技术债） | 低 |
| `subprocess` | `subprocess/worker-client.ts:210` | ONNX worker 子进程隔离 | 无（进程内 fastembed） | 不迁移 | 低 |
| 超集 | — | — | `crates/swarm`（YAML DAG 编排）、`crates/ttsr`（流规则）、`crates/i18n`、`crates/proxy`（SOCKS5）、`crates/snapcompact`、`crates/hashline` 增强、`crates/server`+`web/c5-ui` | 超集 | 保留 |

**同名异义纠正（避免误判）**：Gyre `crates/ttsr` = 流规则引擎（对应 omp `export/ttsr.ts` + `session/ttsr-coordinator.ts`），**非** omp `tts` 语音合成；`crates/browser` 移植 omp `tools/browser.ts`，**非** `web/`；`crates/search` 是 ripgrep/glob 代码搜索，**非** web 搜索；`crates/iso` 移植 omp `crates/pi-iso`；`crates/hashline`→omp `edit/hashline/`；`crates/snapcompact`→omp `packages/snapcompact`；`crates/ast`→omp `tools/ast*`；`crates/prompt`/`i18n`/`pty` 在 omp 侧无同名模块。

**技术债（重复实现）**：两套 `SecretObfuscator`（`crates/core/src/secrets.rs:240` vs `crates/advisor/src/obfuscator.rs:9`）；两套 Content-Length 分帧（`crates/dap/src/frame.rs:21` vs `crates/lsp/src/transport.rs:363`）。

**"原语存在但零生产调用方"全仓唯一实质案例**：`crates/pty/src/session.rs:177 PtyShell`（`RunPtyTool` 走独立的 `run_pty_command`，不经 `PtyShell`）。其余已实现 crate 均有真实装配点。

### 4.12 服务端 / Web / collab

**结论**：omp **没有 Web 宿主**（TUI-only；`collab-web` 仅为浏览器 guest，relay 生产实现不在本仓库）。Gyre `crates/server`（axum 27 路由 + WS + rust-embed SPA）与 `web/c5-ui`（React18+TS+Tailwind+Vite+4 语言 i18n）是**纯超集**。差距集中在 collab 互操作与前端质量门。

| 项 | omp 位置 | omp 用途 | Gyre 现状 | 类型 | 影响 | 建议 | 优先级 |
|---|---|---|---|---|---|---|---|
| collab proto | `wire/src/index.ts:397` `COLLAB_PROTO=3`，不符即拒 | 握手协商 | `server/lib.rs:3432` 写死 `proto:1`，桥不校验 guest hello | 偏差 | 与 omp guest/host 互斥；协商形同装饰 | 定义校验或明确不兼容 | 高 |
| 线协议封套 | `wire/index.ts:406-407` `[4B peerId][sealed]` | 中继定向路由 | `lib.rs:3535-3554` 整包广播；`frame.rs` 无 peer 字段 | 缺失 | 无 per-peer 定向；不兼容 omp relay | 加 4B 头 + peer 路由 | 高 |
| 帧 schema | `wire/index.ts:324-380`（`t` + kebab） | 帧判别 | `crates/collab/src/frame.rs:9-118`（`type` + snake） | 偏差 | 帧集合不同（缺 entry/event/state/bus/agents） | 对齐或宣布自有协议 | 高 |
| collab 审批 | `host.ts:175-210` `ui-request`/`ui-request-end` | guest 答审批 | collab 无（仅控制台 `WebApprovalPolicy`，`lib.rs:312-353`） | 缺失 | 房间内不能审批 | 加 ui-request/response + 定向 | 高 |
| write token 比较 | `host.ts:365-391` 定时安全校验 | 权限 | `crates/collab/src/relay.rs:143` 普通 `t == expected`（`constant_time_eq` 已在 `server/lib.rs:3191` 却未用）；token 走 URL query | 部分 | 时序侧信道；token 进访问日志 | 复用 constant_time_eq；改 header | 中 |
| 工具卡渲染 | `collab-web/src/tool-render/registry.ts` 30 renderer | 富工具视图 | `web/c5-ui/.../Transcript.tsx:439-490` 单一 `ToolBlock` | 部分 | 展示粒度低 | 按工具名分派 renderer | 中 |
| 快照语义 | `host.ts:433-463` `{entries[],final}` 512 KiB | transcript 分页 | `snapshot.rs:17,27-65` 48 KiB 文本块（渲染后 transcript） | 偏差 | guest 无法富渲染 | 改结构化 entry 批 | 中 |
| CLI guest `/join` | `commands/join.ts`、`collab/guest.ts` | CLI 加入会话 | `repl.rs:1279-1301` 仅 `/collab` 生成链接 | 缺失 | CLI 不能加入 | 复用 collab_relay | 中 |
| 前端测试/lint | `collab-web/test/` 9 测试 + oxlint/oxfmt/tsgo | 前端质量门 | `web/c5-ui` 0 测试 0 lint；CI 仅 `npm ci && npm run build` | 缺失 | 前端无护栏 | vitest + eslint + reducer 单测 | 中 |
| collab guest 形态 | React SPA（Transcript/AgentsPanel/Composer/Banners） | 浏览器 guest | `collab_guest.html` 336 行纯文本 chat | 部分 | guest 为 MVP | 增强或独立 React guest | 中 |
| relay 控制消息/关闭码 | `socket.ts:15-20`（4001/4004/4009/4029） | 房间状态语义 | `relay.rs` 进程内 broadcast，无角色/控制消息/关闭码 | 缺失 | guest 不知 host 离线/房间满 | 托管 relay 时补 | 中 |
| 超集 | — | — | REST 27 路由、30+ WS 帧、会话级事件、20 s 心跳 + 限流、rust-embed 单二进制、ACP over HTTP+SSE、relay 200 帧重放环、快照聚合/落盘、前端 i18n | 超集 | 保留 | — | — |
| 不迁移 | `collab-web/scripts/mock-host.ts` 等 | 离线联调 | — | 不迁移 | — | 可选 fixture | 低 |

### 4.13 构建 / 打包 / 发布 / 供应链

**结论**：Gyre CI 骨架可用（fmt / clippy `-D warnings` / nextest / doctest / cargo-deny / 5 腿构建 + tag 触发 Release + 发布后 SHA256 校验），依赖合规部分**反超** omp（硬门跑 `licenses bans sources`，omp 仅 `licenses sources`）。**整个发布编排层缺失**。

| 项 | omp 位置 | omp 用途 | Gyre 现状 | 类型 | 影响 | 建议 | 优先级 |
|---|---|---|---|---|---|---|---|
| 一键 release 脚本 | `scripts/release.ts:1-486` | 预检→版本→锁文件→changelog→check→commit→tag→push→watch | 无 | 缺失 | 手工易漏步骤/锁文件 | `scripts/release.sh` 或 xtask | 高 |
| version bump 一致性门 | `release.ts:281-334`、`sync-versions.ts` | tag↔清单 lockstep | 单一来源已有（33/33 `version.workspace`），无 bump 脚本/校验 | 部分 | 无一致性门 | bump 子命令 + tag 校验 | 高 |
| CHANGELOG/release notes | `fix-changelogs.ts`、`ci-release-notes.ts` | 自动提升 + body_path | 手工 291 行；Release body 为静态文本 | 缺失 | Unreleased 堆积；notes 无内容 | 至少做 Unreleased 提升 | 中 |
| 发布矩阵 | `ci-release-build-binaries.ts:31-81`（7 目标） | darwin x64/arm64、linux gnu x64/arm64、musl x64/arm64、win x64 | `rust.yml:140-179` 5 腿；缺 `x86_64-apple-darwin`、`aarch64-unknown-linux-gnu` | 部分 | 两类用户无产物 | 补 2 腿并同步 expected 列表 | 中 |
| musl 硬门 | `ci.yml:593-604` 非实验 | — | 两 musl 腿 `experimental:true` + continue-on-error | 部分 | musl 失败不阻塞 | 稳定后移除 | 中 |
| 发布附法律资产 | `ci.yml:797-814` | LICENSE + THIRD-PARTY-NOTICES 进资产 | `rust.yml:301-303` 只传二进制；`:354-355` 明确排除 NOTICES；**无 THIRD-PARTY-NOTICES.txt** | 偏差 | 无法履行再分发许可义务 | `cargo about generate` + 加入资产 + diff 门 | 中 |
| MACOS 签名/公证 | `ci-macos-sign.sh`、`macos-entitlements.plist` | Developer ID + notarize | 无（`rust.yml:366-370` 自认） | 缺失 | Gatekeeper 拦截 | 签名脚本 + secrets 门 | 中 |
| curl\|sh 安装器 | `scripts/install.sh:1-334` | 一行安装 + 校验 | 无（README 要求 clone+编译） | 缺失 | 无二进制安装路径 | 补 install.sh + checksum | 中 |
| advisory 硬门 | omp 不跑 advisories | — | `rust.yml:124-126` continue-on-error；`deny.toml:12-15` 软门 | 部分 | 已知通告不阻断 | 逐条 triage 后转硬门 | 中 |
| MSRV 一致性 | omp 未声明 | — | `Cargo.toml:11` 声明 1.85，但 4 个 vendor fork 要求 **1.88**，6/33 crate 未继承 | 偏差 | 声明失真 | 提到 ≥1.88 或删除；6 crate 补继承 | 中 |
| 零消费者 workspace 依赖 | — | — | `ab_glyph`/`lsp-types`（本地版本未用 workspace）、`schemars`/`agent-cli`/`pi-shell`/`pi-walker`（零消费者） | 偏差 | 清单腐化 | 删 4 条 + 2 条改 workspace | 中 |
| vendor 非 workspace 成员 | omp 把 fork 列为成员 | fork 纳入 lint/test | `members=["crates/*"]`，4 fork 全不在 → fmt/clippy/nextest 不覆盖 | 偏差 | fork 回归不可见 | 加入成员或独立 job | 中 |
| 构建未冻结锁文件 | omp `--frozen-lockfile` | 可复现 | clippy/nextest/doctest/build 均无 `--locked`（仅 deny 有） | 偏差 | 锁文件漂移不报错 | 加 `--locked` | 中 |
| action SHA pin | `ci.yml:564,641,716,771,873,920` pin commit | 供应链防篡改 | `rust.yml` 全用可变 tag（@v4/@v1/@v2） | 偏差 | tag 被移动即引入未审代码 | pin 到 commit | 中 |
| 工具链漂移 | omp 固定 nightly+bazel | 固定 | `rust-toolchain.toml:4` 固定 1.97.1，但 `rust.yml:206` 用 `stable` 覆盖 | 偏差 | 发行产物不可复现 | 移除覆盖 | 中 |
| 脚本/task runner | `scripts/` 60+ 文件、70+ npm scripts | 统一入口 | 无 scripts/、无 justfile/Makefile | 缺失 | 命令散落 README | 建 scripts/ + justfile | 中 |
| 其余 | Dockerfile/Dockerfile.robomp、flake.nix、Homebrew、install-tests、canary、.gitattributes、.cargo/config.toml、actionlint | — | 全无 | 缺失 | 分发路径窄 | 按需 | 低 |
| 超集 | — | — | deny `[bans]`、`about.hbs`、三语 README、`config.example.toml`、发布后下载校验、job 级最小权限 | 超集 | 保留 | — | — |
| 不迁移 | bazel、自托管 runner、npm 分发、robomp | — | — | 不迁移 | — | — | — |

### 4.14 文档与测试

**量化**：omp docs 137 文件（130 .md；per-tool 31、toolconv 12、mcp-* 4、settings.md 840 行、environment-variables.md 632 行、skills 文档 + 示例），测试 2,365 文件 / ~24,700 用例；Gyre docs 16 文件（14 篇为分析报告，唯一用户文档 `docs/rpc.md`），测试 1,441 `#[test]` + 12 集成文件。

| 项 | omp 位置 | omp 用途 | Gyre 现状 | 类型 | 影响 | 建议 | 优先级 |
|---|---|---|---|---|---|---|---|
| per-tool 文档 | `docs/tools/*.md`（31） | 参数/示例/边界 | 无 | 缺失 | 只能读源码 | 按工具名建 `docs/tools/` | 高 |
| settings/env 文档 | `docs/settings.md`(840)、`environment-variables.md`(632) | 配置参考 | 无（仅 292 行示例 TOML） | 缺失 | 配置可发现性差 | 由 serde 结构 + env 抽取生成 | 高 |
| MCP 文档 | `docs/mcp-*.md`(4) | 配置/传输/生命周期 | 无 | 缺失 | 重点功能零文档 | 至少补 config + lifecycle | 高 |
| README 三语新功能 | omp README 提及 OAuth/hooks/slash | 首屏总览 | 三语 README 对 OAuth/hook/会话树/模型角色/斜杠/子代理 **全为 0 命中** | 缺失 | 已实现功能不可见 | 补 6 节 | 高 |
| task_tool 测试 | `test/task/` 37 文件 | 委派生命周期 | `task_tool.rs` 621 行 **0 测试** | 缺失 | 最易回归路径无护栏 | 补 spawn/嵌套/预算/取消 | 高 |
| web 前端测试 | `collab-web/test/` 9、`tui/test/` 82 | 前端回归 | `web/c5-ui` 0 测试 0 lint | 缺失 | 双前端无护栏 | vitest + testing-library | 高 |
| server WS 协议测试 | `rpc-frame.test.ts`、`wire-schema.test.ts` | 帧编解码/背压 | `crates/server` 20 测试全纯函数；无 WS 连接测试 | 缺失 | 前端唯一通道无 e2e | tokio-tungstenite 测 `handle_socket` | 高 |
| ACP conformance | `test/acp-*.test.ts` 14 文件 | 握手一致性 | `crates/acp` 47 单元测试；无 `tests/` | 部分 | 握手全流程未验证 | 增 `tests/conformance.rs` | 高 |
| CLI 斜杠分发测试 | `test/slash-commands/` 29 文件 | 每命令行为 | `repl.rs` 28 测试多为补全/help | 部分 | 命令存在但无验收 | 每命令一条 `handle_command` 断言 | 中 |
| RPC e2e | `rpc*.test.ts` 17 + Python 4 | 真实管道 | `rpc.rs` 35 单测强，无跨进程 e2e | 部分 | 背压/管道未验证 | `tests/rpc_e2e.rs` | 中 |
| collab 会话语义 | `test/collab/` 14 文件 | host↔guest 复制 | `crates/collab` 27 测试聚焦 crypto/relay | 部分 | guest 视图可能漂移 | 补 replication/welcome 分片 | 中 |
| bench 执行门 | omp 亦无 CI bench | — | 4 个 criterion bench 从不在 CI 跑 | 偏差 | 性能回归无门 | `cargo bench --no-run` job | 中 |
| 根 `tests/` 未进 CI | omp 全仓扫描 | 回归 | `tests/` 仅 3 文件、无 Cargo.toml | 偏差 | 黄金基线永不校验 | 迁入 `crates/*/tests` | 中 |
| CONTRIBUTING / AGENTS | `CONTRIBUTING.md`(101)、`AGENTS.md`(27 KB) | 贡献规范 | 均不存在 | 缺失 | 无贡献流程 | 补两份 | 中 |
| crate README | 16 个 `packages/*/README.md` | 模块职责 | 0 个 crate README | 缺失 | 33 crate 无入门 | 每 crate 一篇 | 中 |
| 文档分层 | omp user/internals 混合 | 读者分层 | 16 篇中 14 篇为分析报告 | 偏差 | 外部读者无入门 | `docs/analysis/` 分层 | 中 |
| skills 编写文档 | `docs/skills/authoring-*.md` + examples | 教写 skill/hook | 无（仅 1 个 understand skill） | 缺失 | 生态无入口 | 移植 hooks authoring + example | 中 |
| 项目级 skill/命令资产 | `.omp/skills/`(3)、`.omp/commands/`(5) | 固化工作流 | `.agent/` 仅本机 session jsonl（被 gitignore） | 缺失 | 团队工作流未固化 | 建 `.agent/skills|commands` | 低 |

---

## 五、分级缺失清单（汇总）

> **修复状态图例**（2026-09-11 第二轮修复，详见 §九）：
> ✅ 本轮已落地（附证据） · 🟡 部分落地（列出剩余） · ⬜ 未动。
> 下表「现状」列为**修复前**的审计快照（保留以便追溯）。

### 5.1 高优先（正确性风险 / 平台承重 / 生态入口）

| # | 状态 | 项 | omp 锚点 | Gyre 现状（修复前） | 建议 |
|---|---|---|---|---|---|
| H1 | ✅ | 未知 CLI flag 静默成 prompt（计费风险） | `args.ts:352-360` | `main.rs:46-50` | 收窄 `allow_hyphen_values`；未知 flag 报错 exit 2 |
| H2 | ✅ | 子命令 help 失效 | `utils/cli.ts:445,468-477` | `manage.rs` 无 help 分支 | route 首查 `--help`/`help` |
| H3 | 🟡 | REPL 无 Ctrl-C/SIGINT；无 Esc 分级中断 | `input-controller.ts:326-463,1101-1132` | 全仓仅 `rpc.rs:1914` | 进程级 ctrl_c + crossterm raw 按键层（**本轮完成 SIGINT 优雅中止；raw 按键层/Esc 分级仍缺**） |
| H4 | ✅ | 消息队列 / follow-up / dequeue | `queue-input.ts`、`/queue` | 队列（多消息解析 + 入队/出队/取回/删除/清空）、`/queue` 命令、Ctrl-Y 取回键、轮末自动续跑均已落地（见 §二十九） | `VecDeque` + dequeue 键 |
| H5 | ✅ | 可定制键位系统 | `config/keybindings.ts` | action 表（10 个 `app.*`）+ `[keybindings]` 覆盖 + `/hotkeys` 展示生效绑定已落地（见 §三十） | action 表 + yml（Gyre 配置为 TOML；行编辑层动作集） |
| H6 | ✅ | RPC `negotiate_protocol` + v1/v2 门控 | `rpc-types.ts:30`、`rpc-frame.ts:288-306` | 无处理；无条件分片 | 实现协商并门控分片 |
| H7 | 🟡 | RPC 命令面 12 vs 42 | `rpc-types.ts:34-93` | 12 | 先 steer/follow_up/set_todos/get_session_stats/bash |
| H8 | 🟡 | RPC 事件映射宽度（tool id/result/子代理/session） | `rpc-mode.ts:979-981` | 6 类投影，丢 result/id | 透传结构化事件 |
| H9 | ✅ | RPC id 类型 / 响应信封不兼容 | 客户端字符串 id；`{type:"response",…}` | `rpc.rs:58` u64；自定义信封 | 改 Value + 对齐信封 |
| H10 | ✅ | ACP 权限请求形状非标准 | `protocol.ts:130-134` | `acp/rpc.rs:411-426` 缺 `toolCall` | 改标准字段 |
| H11 | ✅ | ACP `stopReason` 恒 `end_turn`（HTTP 非法值） | `acp-agent.ts:1635-1661` | `acp/rpc.rs:360-366`、`http.rs:242` | 补映射 |
| H12 | ✅ | ACP `session/load` 无回放；list/resume/fork 缺 | `acp-agent.ts:712-750,2245-2507` | 无 | 透出 server 能力 + 三态回放 |
| H13 | ✅ | ACP `available_commands` 用请求而非通知，且硬编码 15 条 | `acp-agent.ts:2094-2100` | `acp/rpc.rs:305-323`、`main.rs:229-252` | 改推送 + 由注册表生成 |
| H14 | ✅ | `openai-responses` 无适配器 → Codex 死链 | `providers/openai-responses.ts:924` | `llm/openai.rs:30-35` | 新增适配器 |
| H15 | ✅ | MCP capabilities 未解析 → resources-only server 连接失败 | `mcp/client.ts:214-216` | `mcp/client.rs:441-449`、`tool.rs:984-990` | 解析 capabilities + 能力门控 |
| H16 | ✅ | MCP server→client 请求（ping/roots）无应答 | `mcp/client.ts:55-68` | 三传输均丢弃 | 实现请求分发 |
| H17 | ✅ | MCP tools/call 丢 image/resource/isError | `tool-bridge.ts:207-239` | `client.rs:487-499` | 保真内容块 |
| H18 | 🟡 | 子代理定义化（发现/spawn/只读/预算/worktree 隔离） | `task/*` 26 文件 | `task_tool.rs` 单文件；iso 零调用 | `.gyre/agents/*.md` + 白名单 + 预算 + 接 iso |
| H19 | ✅ | 四段行为章节未注入 rpc/server | `system-prompt.md:103-246` | 仅 `main.rs:836-839` | 抽公共构造，三处统一 |
| H20 | 🟡 | Hook 事件面 3 vs 26 + continuation/cancel | `hooks/types.ts:388-403,481-510` | `hook.rs:11-31` | 扩枚举 + engine 挂点 |
| H21 | ✅ | 插件/市场/扩展/自定义工具（生态入口） | `extensibility/*` 58 文件 | **决策：MCP-only**（ADR 0001），并把五类既有扩展点固化成文档（见 §三十三） | 定子进程扩展协议或 MCP-only 策略 |
| H22 | ✅ | skill frontmatter 严格校验 + 描述转义 | `helpers.ts:396-401`、`managed-skills.ts:62-69` | `render.rs:30-40` 仅折行 | 校验 + 转义 |
| H23 | 🟡 | Settings 运行时单例/热更新/内省 + config get-set | `settings.ts:540,811-824`、`config-cli.ts:243-424` | `get`/`show`/`set`/`check`（含掩码/回滚，§十八）+ **未知键诊断**与 **`config keys` 逐键内省**（§四十一）；**仍缺**：运行时热更新（`Settings` 单例 + reload/watch——当前仍是启动一次加载）与逐键元数据（类型/默认值/文档，现只有路径+形态） | 进程级句柄 + reload + get/set |
| H24 | ✅ | 自定义 provider 键面（headers/compat/auth/modelOverrides） | `models-config-schema-bundle.ts:298-337` | `headers`+`quirks`（§十六）＋**`auth` 三态**（`api_key`/`none`/`oauth`）＋**omp 风格 `compat` 段映射**（`supportsUsageInStreaming`/`supportsToolChoice`/`supportsReasoningEffort`/`supportsReasoningParams`/`maxTokensField`）＋`maxTokensField` 落到 wire（§四十三）；**决策记录**：不追 omp 的 104 位 compat 全表（Gyre 只实现能真正生效的子集，未收录旗标接受但忽略并由 `agent config check` 点名），`modelOverrides` 由 `[[models]]` 等价表达（Gyre 每个 profile 即一个模型） | 扩 provider 段（已按决策收敛） |
| H25 | ✅ | 静态模型目录 + 运行时发现 | `catalog/src/models.json` | 26 条内置目录（补缺语义）+ `agent models catalog` 命令已落地；运行时发现（`models list`）此前已具备（见 §三十二） | 引入精简内置目录 |
| H26 | ✅ | 自动压缩触发策略 + token 家族校正 | `compaction.ts:337,362`、`tokenizer.ts` | 三级阈值策略 + provider 上报下限（§二十八）+ 可声明 tokenizer 家族（§三十四）均已落地 | 绝对/百分比/reserve + 家族编码 |
| H27 | ✅ | job 生命周期接线（方法已备、0 调用） | `agent-session.ts:1989,2047` | 关闭点 dispose（§十七）+ watch 释放与投递恢复（§三十八）全部落地 | 关闭点接入 cancel/drain/wait |
| H28 | ✅ | todo eager prelude + 完成提醒续跑 | `todo-tracker.ts:133,204` | 首轮注入 + 停止边界提醒续跑（§二十二）+ 异步唤醒护栏 + mid-run nudge（§三十五）全部落地 | engine 首轮注入 + 停止分支提醒 |
| H29 | ✅ | goal loop（objective/状态机/工具/续跑） | `goals/runtime.ts` | 状态机/工具/续跑（§二十二）+ 跨会话持久化 + `goal_updated` 结构化事件（§三十六）全部落地 | goal 工具 + continuation prompt |
| H30 | ✅ | memory 后台 rollout/lease + structured append 竞态 | `memories/index.ts:345-601` | O_APPEND 原子追加 + rename 原子替换 + 合并租约 + 可选后台执行均已落地（见 §三十一） | 迁 jobs + O_APPEND |
| H31 | ✅ | 会话 header 字段 + `--continue`/breadcrumb | `session-entries.ts:35-54`、`args.ts:241` | header 仅 3 字段；仅 `--resume <id>` | 扩 header + `.last` |
| H32 | 🟡 | 工具契约兼容（`edit.input`/`ast_grep.pat`/`task.outputSchema`/`ask.questions[]`/`todo` op/`memory_edit` op/`github` op） | 各工具 schema | `edit.input`/`ast_grep.pat`/`ask.questions[]`/`todo` op（§H32 轮）＋**`memory_edit` `update`/`invalidate`**＋**`github` 的 `op` 字段与 omp 只读 op**（§四十四）已对齐；**仍缺**：omp `edit` 的 mode 体系（hashline/replace/apply_patch 三形态）与 `*** Begin Patch` 语法；github 的 `pr_checkout`/`pr_push`/`run_watch`（worktree 隔离与长驻订阅，属 H18 范畴） | 逐项对齐或加别名（收尾中） |
| H33 | ✅ | `glob` 四参数 + `read` 行限 + `run_command.pty` | `glob.ts:43-53`、`streaming-output.ts:9`、`bash.ts:316` | 三项全部落地：`glob` 四参数与 `run_command(pty)`（§十九）＋**`read` 行限口径复核**（§四十四）：omp `readSchema`（`read.ts:530-534`）只有 `path`、行选择器写在 path 内联后缀，Gyre `read_file` 同口径（`path` + `:N`/`:N-M`/`:N+K`/`:N-`/多区间/`:raw`），仅额外可选 `summary` —— 无需再加 `offset`/`limit` 第二套口径，已加契约护栏测试钉住 | 逐项补（已完成） |
| H34 | ✅ | 交互命令族（`/clear`/`/todo` 可写/`/resume` 选择器/`/settings`/`/hotkeys`/`/context`/`/usage`/`/jobs`/`/retry`/`/new`） | 各 `builtin-*.ts` | 行内 10 项全部落地（见 §二十四） | 分批（**本轮完成清单内全部命令**） |
| H35 | 🟡 | 发布编排 + NOTICES 生成发布 + 矩阵补 2 腿 | `release.ts`、`ci.yml:797-814` | NOTICES 生成+漂移门禁、release preflight、CI 矩阵 5→7 腿已落地（见 §二十六）；**缺**版本 bump/tag/publish 自动编排 | xtask release + cargo-about（**本轮完成 notices/release-check + 2 腿**） |
| H36 | ✅ | `.env` 加载 + `${VAR:-default}` | `utils/env.ts:240-259`、`helpers.ts:464-495` | 已落地（见 §二十三）：dotenv 5 源优先级 + POSIX `:-` 默认值 | dotenv + 默认值 |
| H37 | 🟡 | 回归测试网：task_tool / server WS / web 前端 / ACP conformance | 各 test 目录 | 4 面均已补最小行为测试（见 §二十五）；**缺** web 组件级 JS 测试（仓库无 JS 测试运行器） | 每面最小行为测试 |
| H38 | 🟡 | 用户文档：per-tool / settings / MCP + README 三语新功能 | `docs/*` | 工具参考（生成+漂移门禁）、MCP 指南、三语 README 新增能力段已落地（见 §二十七）；**缺** settings 逐项参考 | 分批生成（**本轮完成 tools/MCP/README**） |
| H39 | ✅ | collab 互操作 proto=3（产品决策） | `wire/index.ts:397` | **决策：保持 proto=1 独立**（ADR 0002）+ 显式版本拒绝（见 §三十三） | 决策后对齐或宣布独立 |
| H40 | ✅ | 上下文文件双通道去重 / `@import` / SYSTEM.md / `--append-system-prompt` | `system-prompt.ts:425,451,487-505,387` | 四项全部落地（见 §二十三）：包含去重 + `@import` 展开 + SYSTEM.md 覆盖链 + 追加参数 | 统一收集器 + 覆盖链 + 追加参数 |
| H41 | ✅ | Agent 注册表（寻址/park/revive/roster）——hub·irc 的地基 | `registry/agent-registry.ts:117`、`agent-lifecycle.ts:89` | 名册/park/revive/`agents()`（§二十一）+ 容量预拒（§三十七）；名册**有意不落盘**（进程内可寻址关系，理由见 Hub 文档） | `AgentRegistry` + park/revive + roster 扫描 |
| H42 | ✅ | 统一 SDK 装配入口（消除三处漂移，server 漏接 discovery） | `sdk.ts:830,918,1287` | 三处重复装配 → **收敛到 `agent-sdk`**：行为四段/项目上下文/外来配置（§三十二）、可选工具提示词、**可选组开关推导 + 内置工具注册**（§四十二）都只此一份；cli/rpc/server 全部改调用，server 不再无条件注册 ast/image/lsp、也不再漏注册 hashline/pty/debug/ssh/browser | 抽 `agent-sdk` crate，三前端改调用 |
| H43 | ✅ | hub/irc 子 agent 寻址与回执/唤醒 | `irc/bus.ts:66` | 寻址/广播/唤醒/ACK（§二十一）+ `unwatch` 撤销与回执可观测（§三十七）；回执**有意为进程内**（与消息同一生命周期） | 子 agent 以 task id 入册 + broadcast + 回执 |
| H44 | ✅ | 外来配置发现广度（Codex/Gemini/OpenCode/Windsurf 指令继承 + 内置语言规则） | `discovery/index.ts:44-83`、`builtin-rules/index.ts:46` | 发现面 **4 → 8 源**（+Codex 用户级 / Gemini / OpenCode / Windsurf，含遗留 `.windsurfrules`），并按来源优先级稳定排序；**内置语言规则**（27 条 Go/Rust/TS 约定，最低优先级 + 同名覆盖 + `builtin_rules`/`disabled_rules` 开关）随二进制分发；移植中修掉两个规则解析正确性缺陷（逗号分隔 scope 标量、YAML 双引号转义）——见 §四十 | 补 provider + server 统一入口（server 接线见 §三十二；provider 广度 + 内置规则见 §四十） |
| H45 | ✅ | `internals` 重复实现收敛（SecretObfuscator ×2、Content-Length framer ×2）与 `PtyShell` 接线 | `secrets/obfuscator.ts`、`jsonrpc/message-framing.ts:83`、`exec/bash-executor.ts:454` | 三项全部落地：分帧/脱敏收敛为共享实现（§二十）＋`run_pty_command` 接 `run_command(pty:true)`（§十九）＋**`PtyShell` 持久会话接入为 `shell_session` 工具**（§三十九） | 抽共享 crate + 接线 `PtyShell` 作持久 cwd |

### 5.2 中优先（能力纵深 / 体验差距，按组）

- **配置组**：模型选择语法（`:thinking`/glob/`@role`/模糊）、采样参数（top_p/top_k/min_p/penalties）、thinking 档位与预算、`--config` overlay、profile、fallback 冷却/回切/角色链、retry 参数化、多账号粘性与 block 状态、`disabledProviders`、`workspace.additionalDirectories`、unknown key 告警、凭据掩码、`tools.approvalMode` 默认取舍、逐工具开关命名空间统一、`read.*`/`bash.*`/`lsp.*`/`github.cache.*` 配置族。
- **协议组**：RPC 宿主工具/URI/扩展 UI 子协议、`get_messages` 结构化返回与分页、`compact` 返回结果、`get_state` 字段补齐、子代理帧 + 订阅级别、ACP `initialize` 能力（sessionCapabilities/sse/真实名）、`session/set_config_option`、`current_mode_update`、ACP fs/terminal 桥、tool_call `rawInput`/`locations`、`plan`/`user_message_chunk` 变体。
- **工具组**：`edit` 多模式或 `input` 别名、`read` PDF/归档族/SQLite 查询参数、`ast_edit` 批量、`lsp` 动作与 `apply`、`eval` rb/jl、`browser` tab/CDP、`debug` 26 动作、`hub` 参数面、`memory retain` 批量、`learn` 字段、`manage_skill`、`inspect_image`、隐藏 `yield/goal/think`、`xdev` 挂载、工具名别名、lsp_apply 接线、默认开启 image 组。
- **运行时组**：会话版本迁移链、resume 前缀/跨目录解析、列表状态与标题、fork 语义、压缩记录 `CompactionEntry` 回放、保留窗口按 token + findCutPoint、工具输出保护 `artifact://`、snapcompact 工具调用序列化/多形状/CJK/preserveData、统一 OutputSink、auto-background、yield queue、unexpected-stop 分类器、tool-choice queue、auto-title、steering mode/source/clear、mid-run 模型覆盖、plan mode 四件套、失败轮耐久丢弃、持久化图片外置、`.bak` 孤儿恢复。
- **扩展组**：hook continuation/可取消 session_*/输入改写/tool 参数改写、插件包与市场、自定义工具目录、自定义命令 `$ARGUMENTS` 替换与覆盖优先级、skills 10 provider/managed/containRoot/热重载/`enabled:false`、命令 `argumentHint` 补全。
- **提示词组**：per-tool 提示词文件化、子代理角色模板、goals/memories/plan/advisor/security/review 模板族、工作区树、rules alwaysApply/`rule://`、通知模板文件化、`<file path>` 包装 + auto-loaded 语义、personality 预设。
- **外围组**：advisor 配置化/独立模型/统一脱敏器、internal-urls 路由层、security 扫描三段、web provider 扩容与 scrapers、blob-broker、plan-mode、commit、markit、autolearn controller、snapcompact。 
- **服务端/前端组**：collab 审批、快照结构化、CLI `/join`、前端测试/lint、工具卡 renderer、前端 e2e。
- **构建组**：vendor 纳入 lint/test、`--locked`、action pin、工具链去覆盖、MSRV 统一、advisory triage、changelog 自动化、macOS 签名、install.sh、release notes 生成。
- **文档/测试组**：CLI 斜杠分发测试、RPC e2e、collab 会话复制、bench 执行门、根 tests 接入 CI、CONTRIBUTING/AGENTS、crate README、docs 分层、skills authoring 文档。

### 5.3 低优先（长尾 / 按需拉动）

语音栈（tts/stt/live，纯绿地）· computer 桌面操控 · vibe 长驻 worker · export/share 加密密封 · Cursor/Devin/GitLab Duo wire · Bedrock/Vertex/Azure · 额度报告 · ServiceTier · cache keep-warm 调度 · OpenAI 24h 保留 · 心智模型 per-model history · managed skills · cleanse · autoresearch/if-bench/exa · activity · markit · TUI 差分渲染 · 主题自动探测 · splash/向导 · 桌面通知 · OSC 5522 · emoji/github-ref/internal-url 补全 · inline hint · Ctrl-Z/external editor · prompt i18n · `--version` 格式 · 保留词文案 · `get_tree` 分页 · `MessageUpdate` 死变体 · workspace 死条目 · `statusLine.*`/`tui.*`/`images.urls.*` 等 TUI 专属键。

---

## 六、Gyre 超集与有意不迁移（避免误判为缺失）

**超集**：i18n（4 locale × 338 key，omp 无）· `crates/swarm`（YAML DAG 编排）· `crates/ttsr`（流规则引擎）· `crates/proxy`（SOCKS5）· `crates/server` 27 REST + 30+ WS 帧 + rust-embed 单二进制 · ACP over HTTP+SSE（omp 仅 stdio）· `crates/supervisor` 子代理总线（7 态 + REST/WS）· 活跃叶子 sidecar · 记忆 CJK 分词/MMR/中文时间/L1 嵌入/多 bank · goal hard_stop · 守卫中断上限 · MCP 重连熔断半开 + 语义指纹缓存 + 父子共享动态工具源 · hashline 增强（`conflict://`、`xd://`）· `read_image`/`image_gen` · 命令拦截/minimizer/fuzzy/生成物守护 · `[[hooks]]` shell 规则真实执行（omp 该面未接线）· `/review` `/swarm` `/enhance` `/suggest` `/diff` `/github` `/lang` · 三语 README + `config.example.toml` + `about.hbs` · deny `[bans]` · 发布后下载校验。

**明确不迁移（维持）**：`packages/tui` 27,938 行差分渲染器与 94 component · TS 进程内扩展 1:1 · npm 分发/robomp/Python omp-rpc 客户端 · bazel/自托管 runner · omp legacy pi shim · auth-broker/auth-gateway 远端凭据 · host services（`*-server.ts`）· collab-web React guest（Gyre 维持单文件 `collab_guest.html`）。

---

## 七、不确定与需进一步确认

1. **omp `prompts/tools/` 装载面**：目录 53 篇，源码 `with { type: "text" }` 命中 43 处；未逐篇确认最终装载点。
2. **omp hooks 模块活跃性**：`hooks/loader.ts:220-243` 的 `discoverAndLoadHooks` 全仓无调用点，hook 工厂实际经 extensions loader 绑定；`HookAPI` 契约是否仍活跃未做运行时验证。
3. **omp 声明式 `hooks/pre|post/*.sh` 是否真无执行路径**：其唯一消费者按 `.ts/.js` 过滤；未排除仓外 SDK 调用或运行时代码生成。
4. **ACP `session/available_commands` 的规范归属**：依据 omp vendored `connection.ts` 无此服务端方法判定为非标准；未对照 agentclientprotocol.com 现行草案（是否已升级为请求方法存疑）。
5. **Gyre 运行期 Ctrl-C 行为**：静态证据为 REPL 无 `tokio::signal::ctrl_c()`；未在 tty 实机验证（推断默认 SIGINT 终止进程）。
6. **Gyre `[models.roles]` 是否真正驱动子代理**：仅见 `cli/src/main.rs:2209` 回退路径，未做全链路验证。
7. **`openai-codex-responses` 在 `models.json` 的计数**：两次抽取得 8 与 16（口径差异），不影响结论。
8. **Gyre `/mcp` 与 `route_mcp_*` 行为完整度**：仅核对路由名集合，未逐行读执行层。
9. **`[tools.enabled]` 文档键与代码默认不一致**：`config.example.toml:238-246` 与 `main.rs:748-760` 逐键默认需以代码为准。
10. **Gyre `crates/server` 的 `/api/pause`、`/api/resume`** 在前端无消费者，是否供 ACP/CLI 使用未追踪。
11. **musl 双腿 `experimental`**：rusqlite bundled 交叉编译链未实跑；首次真实 tag 前无法消除风险。
12. **`cargo deny check advisories` 实际通过/失败清单**未实跑（需联网）；`ignore=[]` 是否已掩盖真实通告未知。
13. **Gyre 是否有意裁剪 Python 客户端/前端测试**未知；若产品定位不含则相关行可降为"不适用"。
14. **snapcompact 帧 token 误算**判定为真实偏差，未跑端到端复现"压缩后误再触发"。
15. **omp mnemopi 打分/向量数学与 hindsight** 部分为二级通读（置信中）。
16. **`crates/server/src/lib.rs:1274` 未接 `agent_discovery`**：已复核为真，但属"遗漏"还是"Web 端有意不注入外来配置"无文档证据。
17. **Gyre 是否存在"有意不迁移"的书面决策记录**：未核查 `plans/`、`docs/` 中是否有此类记录，故 `hindsight`/`launch`/`memories`（SQLite 两阶段队列）的"有意偏差"判断仅基于代码内注释自述（`crates/supervisor/src/process.rs:4-9`、`crates/memory/src/lib.rs:17`）。
18. **`conflict://` 是否为 Gyre 自加**：不在 omp router 的 15 个 scheme 内；工作区依赖集内无 PDF 文本提取 crate，`markit` 的 PDF 支持成本未评估。
19. **Gyre `restore_json`（`crates/core/src/secrets.rs:483`）是否已在工具参数分发路径被调用**：仅确认 `engine.rs:896` 的 assistant 还原点，未追到底。
20. **`crates/memory/src/structured.rs` 融合权重与 mnemopi 打分一致性**：未逐行比对。

---

## 八、路线图建议（顺序约束）

1. **立即（正确性/风险）**：H1 未知 flag 收窄 → H2 子命令 help → H3 Ctrl-C/按键层架构决策 → H15/H16/H17 MCP 双向与保真 → H14 openai-responses → H22 skill 校验/转义 → H6/H9 RPC 握手与信封。
2. **第一梯队（宿主可用性）**：H7/H8 RPC 命令面与事件宽度 → H10–H13 ACP 完整面 → H19 四段装配统一 → H23/H24/H25 配置与 provider 面 → H27/H31 job 与会话生命周期 → **H42 SDK 装配收敛**（消除三处漂移）。
3. **第二梯队（Agent 纵深）**：H18 子代理定义化 + iso 接线 → H26 压缩触发/token → H28/H29 todo/goal → **H41 Agent 注册表 → H43 hub/irc 寻址**（注册表是 hub 的地基，须先行）→ H20 扩展事件面 → H21 扩展形态决策 → H32/H33 工具契约 → H44 发现广度 + H45 技术债收敛。
4. **第三梯队（体验与交付）**：H34 交互命令族 → H35 发布链 → H36 配置加载细节 → H37/H38 测试与文档网 → H39 collab 决策 → H40 提示词链。
5. **持续**：中优各组按需；把"声明-代码偏差"作为固定回归类目跟踪。

**工作量粗估**：高优 40 项 ≈ 55–80 人日；中优 ~60 组项 ≈ 80–110 人日；低优按需。架构决策点 3 处：raw 模式按键层、扩展宿主形态（子进程协议 vs MCP-only）、collab 互操作。

---

## 九、修复轮次（2026-09-11 · 第二轮 · 「立即」梯队 + 装配收敛）

> **范围**：§8 路线图第 1 梯队（正确性/风险）全部 + 第 2 梯队中的 **H42 SDK 装配收敛** +
> 第 2 梯队的 **H44 server 接线**。共 **14 项**（12 项完整、2 项部分）。
> **方法**：每项先直读两侧实现确认偏差，再落地 + 加回归测试；批次间跑 `cargo check --all-targets`，
> 最后跑全仓 `cargo test --workspace --all-targets --no-fail-fast`。

### 9.1 已修复项与证据

| # | 状态 | 修复内容 | 证据（新增/改动落点） | 回归测试 |
|---|---|---|---|---|
| H1 | ✅ | argv 预检：未知 flag 一律 stderr 报错 + **exit 2**，绝不沉入 prompt；已知 flag 出现在任务文本之后同样报错（clap `trailing_var_arg` 会静默当 prompt 文本）；`--` 为显式转义出口；纯数字 token（`-1`）仍按 prompt 词 | `crates/cli/src/main.rs:159` `validate_argv_flags`、`:331` `is_meta_flag`、`:178` `scan_lang_flag`（报错跟随 `--lang`） | `main.rs` `argv_flag_guard::*`（6 例）；实测 `agent --model x --smoke-test` → exit 2 |
| H2 | ✅ | 子命令帮助路由：`agent <verb> --help｜-h｜help`、`agent help [verb]`、`agent mcp add --help` 两级主题；返回 `Route::Help` → stdout + exit 0；`help me fix this` 一类真实 prompt 不拦截 | `crates/cli/src/manage.rs:225` `help_topic`、`:257` `help_text`、`Route::Help`；`main.rs` 分发分支 | `manage.rs::subcommand_help_is_routed`；实测 `agent models --help` 打印用法 |
| H3 | 🟡 | **进程级 SIGINT 路由器**：turn 运行中 Ctrl-C → 取消该轮令牌（流式中断 + 在途工具取消，会话保留）；空闲态 → exit 130。**仍缺**：crossterm raw 按键层 / Esc 分级中断 / 消息队列（H4/H5） | `crates/cli/src/main.rs:2805` `SigintRouter`、`SigintAction`、`SigintTurnGuard`；`run_turn`/`run_turn_message` 改建 `run_with_cancel`/`run_message_with_cancel` | `main.rs::sigint_router_cancels_active_turn_then_reports_idle` |
| H6 | ✅ | 实现 `negotiate_protocol`（v2 成功即启用分片；非法版本 → `success:false`）；**分片门控**：未协商 v2 绝不分片（v1 客户端会把 `rpc_chunk` 当未知类型丢弃），超限回退 `rpc_frame_error` | `crates/cli/src/rpc.rs:369` `RPC_V2_NEGOTIATED`、`:381` `OmpResponseFrame`、`:2073` 协商分支、`encode_frame_lines` 门控 | `rpc.rs::omp_negotiate_response_envelope_shape`、`chunking_is_gated_on_negotiated_v2`；实测管道握手成功 |
| H9 | ✅ | 请求 `id` 改为任意 JSON 值（字符串/数字/缺失）并**原样回带**（原 `u64` 会把 omp 官方客户端的字符串 id 整帧判为 `invalid json request`）；omp 风格命令回 `{type:"response",command,success,data\|error}` 信封；`text` 增加 omp 的 `message` 别名 | `crates/cli/src/rpc.rs` `RpcRequest.id: Option<Value>`、`:157` `req_id`、`RpcEnvelope.id: Value`、`OmpResponseFrame` | `request_accepts_string_and_numeric_ids`、`string_id_is_echoed_verbatim_in_envelope` |
| H10 | ✅ | ACP `session/request_permission` 补齐规范必填 `toolCall`（`toolCallId`/`title`/`kind`/`status`/`rawInput`），options 改为 `{optionId,name,kind}` 三字段；`prompt` 降为附加扩展（ACP 无自由文本回答原语） | `crates/acp/src/rpc.rs:476` `permission_request`、`permission_options`、`acp_tool_kind` | `permission_request_serializes_acp_v1_envelope`、`permission_request_maps_command_kind_to_execute` |
| H11 | ✅ | `stopReason` 全量映射：cancel→`cancelled`、`Length`→`max_tokens`、provider refusal→`refusal`、`Aborted`→`cancelled`、其余→`end_turn`；**HTTP 路径不再返回非法的 `"timeout"`**（改 `cancelled`） | `crates/acp/src/rpc.rs:381` `stop_reason_with`；`stdio.rs` 记录 `TurnEnd` 消息 + cancel 标志；`http.rs` 同步 | `rpc.rs::stop_reason_maps_acp_vocabulary` |
| H14 | ✅ | 新增 **Responses API 适配器**（`{base}/responses`）：`instructions`/`input` 条目/扁平 `tools`/`reasoning.effort`；SSE 事件 → 增量事件（文本、推理摘要、`function_call_arguments.delta`、`output_item.done` 权威参数、`response.completed` usage）；Codex 后端自动加 `originator`/`chatgpt-account-id` 头 + `store:false` + `include:["reasoning.encrypted_content"]`；`effective_base_url()` 让 `agent auth login codex` 后免配置可用 | `crates/llm/src/openai_responses.rs`（717 行，新建）、`crates/llm/src/plugin.rs` 注册、`crates/config/src/config.rs:413` `effective_base_url`；`cli`/`rpc`/`server` 三处装配同步 | `openai_responses` 8 例（含 `supports_responses_wire`、`codex_backend_adds_store_and_include`）；`config::effective_base_url_prefers_explicit_then_api_default` |
| H15 | ✅ | 解析 server `capabilities` 并按能力门控 `tools/list`/`resources/list`/`prompts/list`（未声明即不发请求；未声明任何能力时按方法探测）；`-32601` 容错为空清单 → **纯 resources/prompts server 不再因 `tools/list` 失败被判连接失败并 `close()`** | `crates/mcp/src/client.rs:152` `ServerCapabilities`、`supports()`、`is_method_not_found()`、`list_tools`/`list_prompts`/`list_resources` 门控 | `capability_gating_skips_unsupported_lists`、`absent_capabilities_fall_back_to_probing`、`method_not_found_detection` |
| H17 | ✅ | `tools/call` 保真：解析完整内容块（text/image/resource/未知）；`isError` → `ToolResult::Error`（不再伪装成功文本）；**纯单图结果 → `ToolResult::Image`**（真实像素入多模态链路）；resource 渲染 `[Resource: uri]\n text`；多 text 块按 omp 语义空行分隔；无内容时回退渲染 `structuredContent` | `crates/mcp/src/client.rs:550` `call_tool_full`、`McpContent`、`McpToolCallOutcome`、`:275` `first_image`；`crates/mcp/src/tool.rs` 桥接改写 | `parses_tool_call_content_blocks`、`first_image_only_when_exclusively_images`、`structured_content_is_rendered_when_content_is_empty` |
| H19 | ✅ | 四段行为章节（Tool Policy / Delegation / Workflow / Delivery）与外来配置段、security_scan 指引、MCP 指引**统一由 `agent_sdk::compose_context_files` 产出**；RPC 与 server 由此补齐此前缺失的全部段落 | `crates/sdk/src/lib.rs:132` `compose_context_files`、`:99` `optional_tool_sections`；`cli/src/main.rs`、`cli/src/rpc.rs`、`server/src/lib.rs` 三处改调用 | `agent-sdk` 4 例（含行为四段顺序、MCP 段置尾、可选段启用态） |
| H22 | ✅ | skill 描述/名称**转义**（控制/格式字符、`<>`、反引号、`~~~` 围栏折叠）→ `</skills>` 注入闭环；frontmatter 严格校验：`enabled:false` 跳过、描述缺失跳过（对齐 omp `requireDescription`） | `crates/skills/src/render.rs:43` `sanitize_prompt_text`（读侧）、`crates/skills/src/scan.rs:68` `load_skill_file`、`frontmatter.rs` `enabled` 三态 | `description_cannot_break_out_of_skills_section`、`sanitize_is_idempotent`、`skips_disabled_and_descriptionless_skills` |
| H42 | 🟡 | 新建 **`crates/sdk`（`agent-sdk`）** 作为装配侧单一事实来源；三前端共用上下文组装 + 可选工具提示清单（消除「工具注册面与提示词面漂移」）。**仍缺**：tool/provider/context 的全量装配入口收敛（三处仍有 Provider/Registry/Agent builder 重复） | `crates/sdk/src/lib.rs`、`Cargo.toml`（workspace 注册 + cli/server 依赖）；`agent-discovery` 从 cli 依赖移除（改由 sdk 承载） | 同 H19 |
| H44 | 🟡 | server 侧接入外来配置继承（`agent_sdk::compose_context_files` 内含 `foreign_sections`）→ Web GUI 会话开始继承 Codex/Cursor/Cline 指令。**仍缺**：provider 广度（Gemini/OpenCode/Windsurf）与内置语言规则 | `crates/server/src/lib.rs:1274` 起 | 同 H19（`foreign_sections` 语义） |

### 9.2 验证

| 项 | 结果 |
|---|---|
| `cargo check --workspace --all-targets` | **exit 0**，仅 1 条既有 vendor `proc-macro-error2` future-incompat 警告 |
| `cargo test --workspace --all-targets --no-fail-fast` | **55 个测试套件 ok / 3,544 用例**；唯一失败为 vendor `pi-shell` `process::tests::kill_process_group_refuses_self_pgroup`（`getpgid(0)` 在沙箱内返回 0，环境限制，与本轮改动无关，且属第三方 vendor 代码） |
| RPC 握手 E2E | 实测管道输入：旧二进制对字符串 id 回 `{"type":"error","id":0,"message":"invalid json request"}`（即 H9 缺陷现场）→ 新二进制回 `{"type":"ready",...}` / `{"id":"n1","type":"response","command":"negotiate_protocol","success":true,"data":{"protocolVersion":2}}` / 非法版本 `success:false` |
| CLI 行为 E2E | `--smoke-test` → stderr 报错 + exit 2；`models --help` → 用法 + exit 0；`do it --model x` → 报错提示 flag 前置或用 `--` |
| 新增测试用例 | cli bin 101→**106**、cli lib 33、config 94、skills 33→**35**、mcp lib 44→**50**、acp 47→**49**、llm 117→**125**、sdk **4**（新） |

### 9.3 本轮未动（「立即」梯队剩余与后续梯队）

- **H16**（MCP server→client 请求 ping/roots 分发）：需改三条传输的读循环分发模型，与 H15 同域，建议紧接 H15 之后单独一轮。
- **H4/H5**（消息队列 / 可定制键位）：与 H3 的 raw 按键层同属一个架构决策点，须先定 crossterm 方案。
- **H7/H8**（RPC 命令面与事件宽度）、**H12/H13**（ACP 回放/通知面）、**H18**（子代理定义化）、
  **H20/H21**（hook 事件面/扩展形态）、**H23–H41**、**H43/H45**：见 §8 路线图后续梯队。

### 9.4 新增/变更的结构性事实（供下轮基线）

- workspace 新增成员 **`crates/sdk`（`agent-sdk`）**：`cli`、`server` 依赖之；`agent-discovery` 不再是 `cli` 的直接依赖。
- `crates/llm` 新增 `openai_responses.rs`；`collect_providers` 自荐注册表新增一条（装配层零改动）。
- `RpcRequest`/`RpcEnvelope` 的 `id` 类型从 `u64` 变为任意 JSON 值——**下游若解析 Gyre RPC 响应的 `id` 字段需容忍字符串**（既有数字 id 语义不变）。
- `crates/mcp` 新增 `base64` 依赖；`crates/cli` 依赖集新增 `agent-sdk`。
- i18n 新增词条 8 组 × 4 语言（`cli.error.*`、`manage.help.*`、`repl.interrupted`、`repl.interrupt_exit`）。
- RPC v1 客户端行为**收紧**：超 1 MiB 的出站帧不再自动分片，而是回 `rpc_frame_error`（v2 协商后行为同 omp）。

---

## 十、修复轮次（2026-09-11 · 第三轮 · H16 + H7 首批）

> 承接 §九：本轮完成「立即」梯队最后一项 **H16**，并开始第一梯队 **H7**（RPC 命令面）。

### 10.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H16 | ✅ | **MCP server→client 请求应答**（规范要求客户端必须应答 `ping`，否则 server 判对端失联）：新增 `ServerRequestHandler` 分发层，三条传输全部接线——stdio（读 task 与请求路径共享写端）、legacy SSE（读循环 POST 回 message 端点）、Streamable HTTP（流内请求即时应答并 `base_request` 回发）。应答面：`ping`→`{}`；`roots/list`→已登记工作区根；`sampling/createMessage` / `elicitation/*` 与未知方法→**显式 `-32601`**（诚实拒绝让 server 降级，而非静默丢弃导致永久挂起）。`initialize` 只在**登记了根**时声明 `roots` 能力；重连换装传输后重挂该处理器 | `crates/mcp/src/client.rs`：`McpRoot`、`ServerRequestHandler`、`answer_server_request`、`with_transport`、`set_roots`、`initialize` 能力声明、`install_transport` 重挂；`crates/mcp/src/stdio.rs`（共享写端 + 请求分支）、`sse.rs`（`dispatch_message` 返回待回发帧）、`http.rs`（`take_sse_response` 内应答）；`McpLoadOptions.roots` + `ConnectSeam` 传参；cli/rpc/server 三处登记 `McpRoot::file(cwd,"workspace")` | 单测 `server_request_handler_answers_ping_and_roots`、`initialize_omits_roots_capability_without_roots`、`mcp_root_file_uri_shape`；**端到端** `stdio_answers_server_initiated_requests`（python 子进程先发 `ping`+`roots/list`，并把 `tools/list` 响应**延后**到两次应答都到达——不应答即超时失败） |
| H7 | 🟡 | RPC 命令面 12 → **17**：新增 `steer`、`follow_up`（投递到运行中 turn 的 steering 队列；空闲时入队，无 Agent 则显式报错）、`abort`（`cancel` 的 omp 别名 + 成功响应帧）、`get_session_stats`（omp `SessionStats` 形状子集：会话身份 + user/assistant/tool 计数 + `contextUsage`）、`get_available_commands`（omp 形状：`name` **不带**前导 `/`，带 `source` = builtin/file/mcp）。omp 命名的命令统一走 `OmpResponseFrame` 信封（承接 H9） | `crates/cli/src/rpc.rs`：`steer_running_turn`、运行中分支（steer/follow_up/abort）、空闲分支、`dispatch_session_command` 两个新命令、`SessionControl.cwd` | `get_session_stats_reports_counts_and_usage`、`get_available_commands_lists_builtins`、`session_command_whitelist_covers_new_commands`、`steer_requires_non_empty_message` |

### 10.2 验证

| 项 | 结果 |
|---|---|
| `agent-mcp` | 53 lib 单测 + 2 stdio 集成（含新端到端）全绿 |
| `agent-cli` | bin **110** 用例、lib 33 用例全绿 |
| RPC E2E（实测管道） | `get_session_stats` 回 omp 形状；`get_available_commands` 回 34 条（`source` 正确区分 builtin/file）；`steer` 空消息 → `success:false` + 明确原因；`negotiate_protocol` 仍 v2 成功 |

### 10.3 H7 剩余（下一轮候选）

`set_todos`（需把 `Arc<TodoState>` 接进 `SessionControl` 并统一 RPC 侧 todo 句柄）、`bash`（会话内 shell 执行）、
`set_fast_mode` / `set_steering_mode` / `set_follow_up_mode` / `set_interrupt_mode` / `set_auto_compaction` /
`set_auto_retry` / `cycle_model` / `get_available_models` / `new_session` / `switch_session` / `branch` /
`get_branch_messages` / `get_last_assistant_text` / `set_session_name` / `get_messages_page` / `get_login_providers` /
`login` / `export_html` / `abort_and_prompt` / `abort_bash` / `get_subagents` / `get_subagent_messages` /
`set_subagent_subscription` / `set_host_tools` / `set_host_uri_schemes` / `handoff`（≈25 条），
以及 **H8**（事件映射宽度：tool id/result、子代理帧、session 帧透传）。

---

## 十一、修复轮次（2026-09-11 · 第四轮 · H8 + H7 续批）

### 11.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H8 | 🟡 | **事件映射宽度**：①旧式（v1）事件补齐此前丢失的 `id`（`tool_call_id`）与结构化 `result`，并新暴露 `turn_start`/`turn_end`/`message_start`/`message_end`/`tool_update`；②**omp 形状（v2 对端）**：裸帧顶层 `type` = omp `AgentEvent` 判别字段（`tool_execution_start/update/end` 用 `toolCallId`/`toolName`/`args`/`result`/`isError`，`turn_*`/`message_*`/`message_update`/`agent_end`/`usage`/`status`，会话事件直接扁平外发）；③`prompt` 的**终止帧**在 v2 下改为 omp `{type:"response",command:"prompt",success,data:{usage,turns}}`，闭合 omp 客户端的 prompt 往返。**仍缺**：子代理帧（`subagent_lifecycle/progress/event`，需接 supervisor 订阅）与扩展 UI 帧 | `crates/cli/src/rpc.rs`：`map_event`（补 id/result/生命周期）、`map_event_omp`、`write_agent_event`、`run_turn_rpc` 收尾分支；`hides_lifecycle_events` 契约更新（生命周期不再隐藏，Done/Error/StateChanged 仍隐藏） | `tool_events_carry_id_and_structured_result`、`omp_event_shape_uses_camel_case_and_top_level_type`、`event_line_shape_follows_negotiated_mode` |
| H7 | 🟡 | 命令面 17 → **22 可执行** + **28 条显式不受理**：新增 `set_todos`（omp `phases` 与 Gyre `items` 双形状；分组名展平并回 note）、`get_todos`、`get_available_models`（来自配置，非网络）、`get_last_assistant_text`；对**已识别未实现**的 28 条 omp 命令回 omp 信封 `{success:false,error:"…已识别但 Gyre 未实现"}`（客户端可降级，而非被当作协议错误）。**附带工具面修复**：RPC 前端此前完全没有 `todo`/`ask`/`checkpoint`/`rewind`/`security_scan` 五个工具（REPL 与 server 都有）→ 已补齐并共享状态句柄 | `crates/cli/src/rpc.rs`：`todo_items_from_payload`、`todo_list_json`、`unsupported_omp_command` + `OMP_RECOGNIZED_UNSUPPORTED`、`OmpResponseFrame.command: String`、`SessionControl.{todo,checkpoints}`、工具装配补 5 个；`crates/tools/src/todo.rs` `TodoState::replace`（id 重排 + 单活跃规整 + 落盘）、导出 `TodoItem`/`TodoList` | `set_todos_accepts_omp_phases_and_reads_back`、`set_todos_accepts_gyre_items`、`get_available_models_and_last_assistant_text`、`recognized_unsupported_omp_commands_answer_with_omp_envelope`、`todo::tests::replace_renumbers_normalizes_and_persists` |

### 11.2 验证

| 项 | 结果 |
|---|---|
| `cargo clippy -p agent-cli -p agent-tools -p agent-mcp` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| 测试 | agent-cli **117** bin + 33 lib、agent-tools **251**、agent-mcp 53+2 |
| RPC E2E（实测管道，v2 已协商） | `set_todos`（omp phases→Gyre items + note）→ `get_todos` 回读一致；`get_available_models` 列出配置 profile；`get_last_assistant_text` 回 `{text}`；`bash` → `{command:"bash",success:false,error:"…已识别但 Gyre 未实现"}` |

### 11.3 剩余

- **H8 尾项**：子代理帧（`subagent_lifecycle`/`subagent_progress`/`subagent_event`）需把 `crates/supervisor` 的事件总线接进 RPC 事件流；扩展 UI 帧依赖扩展宿主形态决策（H21）。
- **H7 尾项**：`bash`（会话内 shell 执行）、`abort_and_prompt`（需把「取消后接续新 prompt」串到主循环返回值）、`set_thinking_level` 等命名映射、宿主工具/URI 子协议、子代理订阅、`export_html`/`login`/`handoff`/会话管理族（28 条显式不受理清单）。
- 下一梯队：**H12/H13**（ACP 回放与通知面）→ **H18**（子代理定义化）→ **H20/H21**（hook 事件面 / 扩展形态）→ **H23–H25**（配置与 provider 面）→ **H27/H31**（job 与会话生命周期）。

---

## 十二、修复轮次（2026-09-11 · 第五轮 · H12 + H13 · ACP 会话面）

### 12.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H12 | ✅ | **ACP 会话面补全**：①`session/load` 现在**回放历史**（用户消息→`user_message_chunk`、助手文本→`agent_message_chunk`、思考→`agent_thought_chunk`、工具调用→`tool_call`、工具结果→`tool_call_update` 含 `status: completed/failed` + `rawOutput`）；②新增 `session/list`（`{sessions:[{sessionId,cwd,title?,updatedAt}], nextCursor?}`，20 条/页 + cursor 分页，`updatedAt` 为自实现 ISO-8601 UTC）；③新增 `session/resume`（恢复但**不**回放）；④新增 `session/fork`（复制为新会话）；⑤`initialize` 声明 `sessionCapabilities:{list,resume,fork,close}` 与 `mcpCapabilities.sse`（两个传输都推通知：stdio 直写 stdout；HTTP 经新增 SSE 扇出，**响应先于通知**——omp 记录的客户端竞态）。**仍缺**：`session/set_config_option`、`current_mode_update`、ACP fs/terminal 桥、`plan`/`user_message_chunk` 之外的变体（§5.2 协议组） | `crates/acp/src/types.rs`（`UserMessageChunk`/`AvailableCommandsUpdate`/`SessionInfoUpdate`/`AvailableCommand`）；`crates/acp/src/rpc.rs`：`handle_session_list/resume/fork`、`post_dispatch_notifications`、`replay_history`、`active_path_messages`、`unix_to_iso8601`、`available_commands_notification`、能力声明；`crates/acp/src/stdio.rs`（分发后推通知）；`crates/acp/src/http.rs`（`SSE_FANOUT` 扇出 + select 合并）；`crates/acp/src/adapter.rs`（`tool_kind`/`tool_title` 提升为 `pub(crate)` 复用） | `session_load_replays_history_before_bootstrap`、`session_new_pushes_bootstrap_notifications`、`session_list_pages_and_resume_skips_replay`、`session_fork_creates_distinct_session`、`unix_to_iso8601_matches_known_instants`、`fanout_delivers_to_subscribers_only` |
| H13 | ✅ | **`available_commands` 改推送**：bootstrap（`session/new|load|resume|fork`）推 `available_commands_update` 通知（ACP 现行草案只有通知形态，无请求方法）；**清单不再硬编码 15 条**，改从 REPL 同一注册表派生（内置 + 项目自定义命令 → 实测 **34 条**），描述取既有 `help.*` 词条（别名经映射复用：`?`/`h`/`help`→`help.h`、`quit`/`exit`→`help.exit`、`skills`→`help.skill`、`resume`→`help.session`），无词条者留空而不编造。`session/available_commands` 请求方法**保留**为 Gyre 扩展（向后兼容既有客户端），文档标注非标准 | `crates/cli/src/main.rs`：`acp_command_catalog(cwd)` 重写 + `help_entry_description`；`crates/acp/src/rpc.rs` `available_commands_notification` | 同 H12（bootstrap 测试断言 `availableCommands[0].name == "review"` 无前导 `/`）+ ACP E2E 实测 34 条 |

### 12.2 验证

| 项 | 结果 |
|---|---|
| `cargo clippy -p agent-acp -p agent-cli` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| 测试 | agent-acp **55**、agent-cli **117** bin + 33 lib 全绿 |
| ACP stdio E2E（实测管道） | `initialize` 回 `sessionCapabilities{list,resume,fork,close}` + `mcpCapabilities.sse:true`；`session/new` → **响应先行**，随后 `available_commands_update`（34 条、名字无前导 `/`）+ `session_info_update`（`updatedAt:"2026-09-11T14:35:54Z"`）；`session/list` 列真实会话；`session/load` 保持请求 id；`session/fork` 产生新 id；`session/resume` 保持 id 且不回放 |

### 12.3 剩余

- **H12 尾项**：`session/set_config_option`、`current_mode_update` 通知、ACP fs/terminal 桥、tool_call `locations`/`rawInput` 富字段、`plan` 变体（§5.2 协议组）。
- 下一梯队：**H18**（子代理定义化）→ **H20/H21**（hook 事件面 / 扩展形态）→ **H23–H25**（配置与 provider 面）→ **H27/H31**（job 与会话生命周期）→ **H32/H33**（工具契约）。

---

## 十三、修复轮次（2026-09-11 · 第六轮 · H18 首批：子代理定义化）

### 13.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H18 | 🟡 | **命名子代理定义**（移植 omp `task/{discovery,agents,read-only-policy}` 可用子集）：①**发现**：`<cwd>/.agent/agents/*.md`（最近祖先项目级）→ `<config_dir>/agents/*.md`（用户级）→ **内置** `task`（通用全工具）/`scout`（只读侦察），同名先到者胜；②**frontmatter**：`name`/`description`/`tools`/`model`（`@` 前缀剥离）/`thinking`；③**只读判定**：`tools` 非空且全部命中只读白名单 → 只读代理（未知工具一律判非只读，fail-safe，对齐 omp `isReadOnlyAgent`）；④**能力面裁剪**：`agent_tools::FilteredRegistry` 按白名单裁剪子 Agent 注册表——只读代理**根本拿不到**写/执行工具（不是"注册后被审批拦住"）；⑤`task` 工具新增 `agent` 参数（未知名字 → **明确报错并列出可用名**，不静默回退全工具）；⑥定义可覆盖模型别名（装配层提供别名→模型表）与思考档位（`low/medium/high`→预算）；⑦角色正文以 `<agent_definition>` 段**追加**进子代理系统提示；⑧父级提示词新增 `<subagents>` 清单段（名称 + 描述 + `[只读]` 标记）；⑨监督器标签带定义名（仪表盘可区分 scout/reviewer）。**仍缺**：`spawns` 嵌套派生策略、`output` 契约化、`autoloadSkills`、`prewalk`/`advisor`、**worktree 隔离**（`crates/iso` 仍未被 task 调用）、软 token 预算 | 新增 `crates/agent/src/agent_def.rs`（定义/发现/只读/内置/catalog）、`crates/tools/src/filter.rs`（`FilteredRegistry`）、`crates/config/src/frontmatter.rs`（通用 frontmatter 解析，供后续 skills 复用）；`crates/agent/src/task_tool.rs`（`with_agents`/`with_model_overrides`/`resolve_agent`/`build_sub_agent` 覆盖/`thinking_from_level`/schema）；装配四处接线：`cli/src/main.rs`、`cli/src/rpc.rs`、`cli/src/review.rs`、`server/src/lib.rs`（均 `discover_agents(&cwd)` + `<subagents>` 注入 + 模型别名表） | `agent_def` 5 例（解析/只读判定/缺 name 拒绝/优先级去重/catalog）、`filter` 1 例（specs 与 get 双裁剪）、`frontmatter` 4 例、`task_tool` 3 例（命名解析与未知名报错、思考档位、schema 暴露 `agent`） |

### 13.2 验证

| 项 | 结果 |
|---|---|
| `cargo clippy`（agent/tools/config/cli/server） | 清零（修掉 1 处新引入的 `rc_buffer`：`Arc<Vec<_>>` → `Arc<[_]>`） |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| 测试 | agent **122**、agent-tools **252**、agent-config **98**、cli 117+33、server/acp/mcp/llm/skills 全绿 |
| 冒烟 | 在 cwd 放置真实 `.agent/agents/{scout,reviewer}.md` 后 `agent --rpc` 正常启动（发现/解析不阻断启动），`get_state` 正常 |

### 13.3 剩余与下一轮

- **H18 尾项**：worktree 隔离（`crates/iso` 接线）、`spawns` 嵌套策略与深度守卫、软 token 预算、`output` 契约、`autoloadSkills`/`prewalk`/`advisor`。
- 下一梯队：**H20/H21**（hook 事件面 3→26 / 扩展宿主形态）→ **H23–H25**（配置与 provider 面）→ **H27/H31**（job 与会话生命周期）→ **H32/H33**（工具契约兼容）。

---

## 十四、修复轮次（2026-09-11 · 第七轮 · H20：Hook 事件面）

### 14.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H20 | 🟡 | **事件面 3 → 17 个可订阅事件**（全部**真实发射**，无空壳词条）：①`HookEvent` 新增通用 `Named { event, payload }` 变体 + `name()` / `named()` / `to_payload()`，既有 3 个变体保持兼容（`tool_call` / `tool_result` / `stop` 为线协议名）；②**引擎发射点**：`before_agent_start`（携带初始提示词）、`agent_start`、`agent_end`（成功/取消/失败四处早退全覆盖）、`turn_start`、`turn_end`（携带 `message` + `willContinue`）——此前 turn/message 生命周期对 hook 完全不可见；③**会话级结构化事件镜像**：所有 7 处 `SessionEvent` 发射点同步 `fire_hook_session`，因此 `auto_compaction_start/end`、`auto_retry_start/end`、`retry_fallback_applied/succeeded` 自动进入 hook 事件面（新增变体无需再改引擎，`SessionEvent::to_hook_event()` 读 serde `type` 判别字段）；④**配置面**：`HookEventKind` 扩到 17 个（`as_str()` / `is_decidable()`），旧名 `before_tool`/`after_tool` 经 serde alias 继续可用；⑤**shell 钩子改为按事件名匹配**（此前是三个变体的 match），stdin 负载统一带 `event` 字段；`tool_call` 只走决策通道，`on_event` 不再重复触发同一命令；⑥新增 `HOOK_EVENT_NAMES` 常量作为**运行期事件名唯一事实源**，并由配置侧测试与之逐项比对（防词表漂移）。**仍缺**：omp 的 session/tree/plan 类事件（`session_before_switch`/`session_before_branch`/`session_before_compact`/`session_before_tree` 等**可决策**钩子，需先在引擎暴露对应拦截点）、`context` 事件的内容改写、`todo_reminder`、扩展宿主（H21） | `crates/core/src/hook.rs`（`Named`/`name`/`named`/`to_payload`/`HOOK_EVENT_NAMES`）、`crates/core/src/session_event.rs`（`to_hook_event`）、`crates/config/src/config.rs`（17 个 `HookEventKind` + `as_str`/`is_decidable` + 别名）、`crates/agent/src/lib.rs`（`fire_named_hooks`/`fire_hook_session`）、`crates/agent/src/engine.rs`（14 处发射点）、`crates/cli/src/hooks_cfg.rs`（按名匹配 + 统一负载） | agent：`named_hook_events_cover_lifecycle_and_session_events`（两轮真实运行，断序 + 计数）、`session_event_maps_to_same_named_hook_event`；config：`hook_event_names_match_core_vocabulary`（逐项比对 + 每个名字都能从 TOML 解析 + 旧别名兼容）；cli：`named_event_matches_by_name_and_writes_payload`、`tool_call_event_does_not_double_fire`（真实 shell 子进程校验 stdin 负载） |

### 14.2 验证

| 项 | 结果 |
|---|---|
| `cargo clippy`（agent/core/config/cli） | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| 测试 | agent **124**、agent-config **99**、cli lib **35**，其余套件全绿 |

### 14.3 剩余与下一轮

- **H20 尾项**：可决策的会话钩子（`session_before_switch`/`session_before_branch`/`session_before_compact` —— 需要引擎/会话层暴露「可拒绝/可改写」的拦截点，而不只是通知）、`context` 事件改写、`todo_reminder`。
- **H21**（插件/市场/扩展宿主）是一个**架构决策点**（子进程扩展协议 vs MCP-only），需要产品决策而非纯实现；报告 §六 已把它列为「不迁移 TS 进程内扩展」的边界，下一轮将先补一份决策建议 + 最小可落地形态（复用 MCP 作为扩展通道），再进入 **H23–H25**（配置与 provider 面）。

---

## 十五、修复轮次（2026-09-11 · 第八轮 · H23：配置内省与写回）

### 15.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H23 | 🟡 | **`config` 子命令面 2 → 5**：①`agent config get <key>`（点分键读**合并后**生效值）；②`agent config show`（整体生效配置 TOML）；③`agent config set <key> <value>`（写用户 `config.toml`，**保留注释与排版**，写入后必须通过完整加载管线，否则**自动回滚**）；④**密钥掩码**：`api_key`/`api_keys`/`password`/`token`/`secret`/`*_key`/`*_secret`/`*_token`/`*_password` 在 `get`/`show` 一律显示 `***`（`show` 顶部附提示行）；⑤值按 TOML 字面量推断（bool/int/float/数组/引号串/裸串），`[a, b]` 这类裸词列表按字符串元素宽容解析；⑥**明确的不可写判定**：`[[models]]`/`[[hooks]]` 等数组表拒绝点分键写入并给出编辑提示；⑦新增 `ConfigError::Write`（此前写失败被误标为「读取配置失败」）。**仍缺**：运行时热更新（`Settings` 单例 + 逐键 watch/reload，仍是一次性加载）、unknown key 告警、逐键内省元数据（类型/默认值/文档） | `crates/config/src/config.rs`：`config_candidates()` / `load_merged_value()`（分层顺序单一事实源，供 `load`/`load_raw`/`set_key` 共用）、`Config::load_raw()`、`Config::set_key()` → `set_key_at()`（路径注入便于测试）、`scalar_to_item()`、数组表守卫；`crates/core/src/error.rs` `ConfigError::Write`；`crates/cli/src/manage.rs`：`config_show`/`config_get`/`config_set` + `is_secret_key`/`mask_secrets` + 路由三动词；`crates/cli/src/main.rs` 分发；i18n 新增 4 键 × 4 语言 | config：`set_key_writes_preserves_comments_and_rolls_back`（注释保留 / 五种值类型回读 / 非法值回滚 / 数组表拒绝 / 空键拒绝）、`scalar_to_item_infers_types`、`config_candidates_are_user_then_project`；cli：`config_get_and_show_mask_secrets`（掩码 + 不泄漏明文 + 未知键提示）、`config_set_reports_failure_without_panic` |

### 15.2 验证

| 项 | 结果 |
|---|---|
| `cargo clippy`（config/cli/core） | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| 测试 | agent-config **102**、cli lib **38** + bin 117，其余套件全绿 |
| E2E（真实二进制，`XDG_CONFIG_HOME` 指向可写临时目录） | `config get default_model.id` → `gpt-4o-mini`；`config get default_model.api_key` → `***`；`config get nope.zzz` → 未找到键提示；`config set agent.max_turns 77` → 写盘并在用户 config 中新增 `[agent]` 表且**保留原注释**；`config get agent.max_turns` → `77`；`config set agent.context_window_guard 9.0` → `写入后被校验拒绝，已回滚`，随后 `get` 确认该键未被写入 |

### 15.3 剩余与下一轮

- **H23 尾项**：运行时热更新（配置单例 + 文件 watch → 生效值变更通知）、unknown key 告警、逐键元数据（类型/默认值/文档，支撑 `config get` 的自解释输出）。
- 下一轮：**H24**（自定义 provider 键面：headers / compat / modelOverrides）→ **H25**（精简模型目录）→ **H27/H31**（job 与会话生命周期）→ **H32/H33**（工具契约兼容）。

---

## 十六、修复轮次（2026-09-11 · 第九轮 · H24：自定义 provider 键面）

### 16.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H24 | 🟡 | **自定义请求头 + 兼容开关**：①`ModelProfile.headers`（`BTreeMap<String,String>`，`${ENV}` 展开）注入 `ProviderCallContext.headers`，**六个适配器全部接线**（openai-completions / openai-responses / anthropic / gemini / glm / deepseek），应用顺序在**内置鉴权头之后**——同名头以用户配置为准（网关需要非 Bearer 方案时可直接给 `Authorization`），非法头名/值只告警跳过而不让整次请求失败；②新增 `ProviderQuirks`（`omit_temperature` / `omit_stream_options` / `omit_tool_choice` / `omit_reasoning`）并接入 openai-completions 请求体构建——**默认全关**（标准 wire 不变），仅当网关明确拒绝参数时才开；`deny_unknown_fields` 让拼错的开关**报错而非静默失效**；③四处装配点（cli 主路径 / cli 热切换 / rpc / server）统一读取 profile 的 headers+quirks；④`config.example.toml` 补两段带注释示例。**仍缺**：`compat` 全量表（omp 104 位：provider 级参数重命名、`/v1` 路径拼接策略、reasoning 字段族差异等）、`auth` 段（多账号/自定义 OAuth 流程）、`modelOverrides`（按模型 id 覆盖 provider 参数） | `crates/core/src/llm.rs`（`ProviderQuirks` + `ProviderCallContext.{headers,quirks}`）、`crates/llm/src/lib.rs`（`apply_custom_headers`）、`crates/llm/src/openai.rs`（`build_body_with_quirks` + 四个开关落点）、其余五个适配器（headers 接线）、`crates/config/src/config.rs`（`ModelProfile.{headers,quirks}`）、cli/main+rpc+server 装配点、`config.example.toml` | llm：`quirks_omit_parameters_on_demand`（默认齐备 / 全开逐项省略 / 核心字段不受影响 / `build_body` 默认语义不变）、`custom_headers_apply_and_skip_invalid`（真实 `RequestBuilder` 构建，非法头跳过且不影响其它头）；config：`model_profile_headers_and_quirks_parse`（TOML 解析 + 默认空 + 未知 quirks 键报错） |

### 16.2 验证

| 项 | 结果 |
|---|---|
| `cargo clippy --workspace --all-targets` | **全仓清零**（顺带修掉 3 处测试期告警：`slice::from_ref`、测试串行锁改 `tokio::sync::Mutex` 以免 std guard 跨 await） |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| 测试 | agent-llm **127**（+2）、agent-config **103**（+1），cli 117+38、memory 等全绿 |
| E2E | 在 e2e 配置加入 `[default_model.headers]` + `[default_model.quirks]` 后：`config get default_model.headers` 正确回显（含 `${ENV}` 原样保留待装配层展开）、`config get default_model.quirks` → `omit_stream_options = true`、`agent --rpc` 正常启动（装配未因新字段报错） |

### 16.3 剩余与下一轮

- **H24 尾项**：`compat` 全量表、`auth` 段、`modelOverrides`。
- 下一轮：**H27**（job 生命周期接线：`core/jobs.rs` 已有方法但生产零调用）与 **H31**（会话 header 字段 + `--continue`/breadcrumb）——两者都在「第一梯队 宿主可用性」，随后 **H32/H33**（工具契约兼容）。

---

## 十七、修复轮次（2026-09-11 · 第十轮 · H31 + H27）

### 17.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H31 | ✅ | **会话 header 身份字段 + `--continue`**：①`SessionHeader` 从 3 字段扩到 8：新增 `id` / `cwd` / `parent_session` / `previous_session_files` / `provider_prompt_cache_key`（**全部可选**，旧文件原样可读；`with_path()` 从 `<cwd>/.agent/sessions/<id>.jsonl` 反推 id 与 cwd）；②`PersistentContext::open_with_header` 让装配层注入身份（CLI 主路径与 RPC 均已注入 cwd / parent / prompt-cache key）；`rewrite_jsonl` 重写时**保留**既有身份字段（只刷新版本与创建时间）；③新增 `.last` 面包屑（`SessionStore::{mark_last,resolve_last,last_breadcrumb_path}`）——`--continue` 优先面包屑（用户最后一次**实际使用**的会话），失效则回退 mtime 最新，非法 id（路径穿越）不采信；④CLI 新增 `--continue`（无历史会话时明确报错而非静默新建）；CLI / RPC / server 三处创建会话后写面包屑 | `crates/context/src/persistence.rs`（header 字段 + `SessionHeader::{new,with_path}` + `open_with_header` + 面包屑 API + 重写保留身份）、`crates/cli/src/main.rs`（`--continue` 解析 + header 注入 + 面包屑）、`crates/cli/src/rpc.rs`、`crates/server/src/lib.rs`、i18n 3 键 × 4 语言 | context：`header_carries_identity_fields_and_reads_legacy`（新字段落盘 + 旧 3 字段 header 仍可读）、`resolve_last_prefers_breadcrumb_then_mtime`（空目录 / mtime 回退 / 面包屑优先 / 失效回退 / 非法 id）；CLI E2E 实测 |
| H27 | 🟡 | **后台作业生命周期接线**：会话关闭路径此前完全不碰 `AsyncJobManager`——作业任务**活过会话**（进程还在、token 继续烧），且「已完结未投递」结果被静默丢弃。现 `Session` 持有 `jobs: Option<Arc<AsyncJobManager>>`（`build_agent` 返回值扩为 4 元组），`Session::shutdown()` 先对未投递结果**告警**，再 `dispose(3s)`（取消 → 等终结 → 清空，与库内 dispose 同一语义；3s 未清则再告警）。**仍缺**：`unwatch_jobs` / `acknowledge_deliveries` / `resume_deliveries` 仍无生产调用（属 hub 等待接管语义，需要 hub 侧接入 watch 生命周期）、`at_capacity` 未用于前置拒绝（现依赖 `register` 内建容量检查） | `crates/server/src/lib.rs`：`Session.jobs` 字段 + `shutdown()` 的告警/dispose + `build_agent` 返回作业管理器 | core：`dispose_cancels_running_jobs_and_settles`（dispose 取消在跑作业、不悬挂、清空后拒绝新注册——关闭竞态防护） |

### 17.2 验证

| 项 | 结果 |
|---|---|
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| 测试 | agent-context **86**（+2）、agent-core（+1）、server/cli 全绿 |
| E2E | 新会话 header 实测：`{"type":"header",...,"id":"18d44cd6d91bff0a-0","cwd":"target/e2e-rpc2","provider_prompt_cache_key":"18d44cd6d91bff0a-0"}`；旧 header（3 字段）仍可读；`--continue` 首次按 mtime 解析 → 写入 `.last` → 再次精确命中同一会话 |

### 17.3 剩余与下一轮

- **H27 尾项**：hub 侧 `unwatch_jobs`/`acknowledge_deliveries`/`resume_deliveries` 生命周期接线；`at_capacity` 前置拒绝（当前由 `register` 内建容量检查兜底）。
- **H31 尾项**：`title`/`titleSource` 进 header（Gyre 现用 sidecar 存标题，双源需先收敛）；`previousSessionFiles` 仅在重写路径保留、尚无「会话迁移」写入方。
- 下一轮：**H32/H33**（工具契约兼容：`edit.input` / `ast_grep.pat` / `ask.questions[]` / `glob` 四参数 / `read` 行限 / `run_command.pty`）→ H28/H29（todo eager prelude / goal loop）→ H41/H43（Agent 注册表 → hub/irc 寻址）。

---

## 十八、修复轮次（2026-09-11 · 第十一轮 · H33：工具契约兼容）

### 18.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H33 | 🟡 | ①**`glob` 四参数对齐 omp `find`**：新增 `path`（`pattern` 的 omp 别名；可为 glob/文件/目录，**分号分隔多模式**）、`hidden`（默认 **true**，对齐 omp；此前恒 false）、`gitignore`（默认 true）、`limit`（默认 200、钳 1..=200；此前硬编码 100）；`pattern`/`path` 都缺省时回退工作区根扫描（不再报错）；②**`run_command(pty: true)`**：新增 `agent_tools::PtyExecutor` **端口**（`PtyExecOptions`/`PtyExecOutcome`）+ `set_pty_executor` 全局注入 + `agent_pty::PtyAdapter` **适配器**（复用既有 `run_pty_command`，不重复 PTY 实现——同时消除 H45 里「PTY 能力零调用」的一半），cli 主路径 / rpc / server 三处装配注入；语义细节：与 `async` **互斥**（PTY 需前台交互）、`timeout` 秒→毫秒（0 = 不限时）、非零退出码与超时都在结果里显式标注、**未接入执行器时给明确错误**而非静默降级为管道执行。**仍缺**：`read` 行限（omp 的 `readSchema` 同样只有 `path` + 行内选择器，与 Gyre 一致——该项需重新核对口径）、`edit.input` / `ast_grep.pat` / `ask.questions[]` / `todo` op / `memory_edit` op / `github` op 的字段别名 | `crates/search/src/lib.rs`（`glob_match_opts`：多模式 + 扫描开关；`glob_match` 保持旧默认）、`crates/tools/src/search.rs`（GlobTool schema/解析）、`crates/tools/src/shell.rs`（`PtyExecutor` 端口 + `pty` 分支）、`crates/tools/src/lib.rs`（导出）、`crates/pty/src/lib.rs`（`PtyAdapter` + `executor()`）、cli/rpc/server 注入、`config.example.toml` 注释 | tools：`glob_accepts_omp_four_params`（`path` 别名 / limit 截断 / 分号多模式 / `gitignore:false` 命中被忽略文件 / `hidden` 默认含隐藏与显式排除 / `pattern` 优先 / 全缺省不报错）、`pty_flag_delegates_and_reports_unavailable`（未接入 → 明确错误；`pty`+`async` → 互斥错误；端口契约透传 cwd/env/timeout） |

### 18.2 验证

| 项 | 结果 |
|---|---|
| `cargo clippy`（tools/pty/search/cli/server） | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| 测试 | agent-tools **254**（+1）、agent-pty 18、agent-search 3，cli/server 全绿 |
| 冒烟 | 二进制重建后 `agent --rpc` 正常（三处 PTY 注入不阻断启动） |

### 18.3 剩余与下一轮

- **H33 尾项**：`read` 行限口径复核（omp `readSchema` 与 Gyre 同为 `path` + 行内选择器，疑为报告口径偏差——下轮直接比对两侧选择器语法差异）。
- **H32**：`edit.input` / `ast_grep.pat` / `ask.questions[]` / `todo` op / `memory_edit` op / `github` op 的字段别名（下一轮主项）。
- 之后：**H28/H29**（todo eager prelude / goal loop）→ **H41/H43**（Agent 注册表 → hub/irc 寻址）→ **H45**（内部重复实现收敛，本轮已消掉 PTY 一半）。

---

## 十九、修复轮次（2026-09-11 · 第十二轮 · H32：工具契约字段别名）

### 19.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H32 | 🟡 | 逐项对齐 omp 字段名/取值（**双侧皆收，Gyre 原字段优先**，不破坏既有提示词）：①**`apply_hashline` 接受 `input`**（omp `edit` 的 patch 模式字段名）作为 `patch` 别名，缺字段时报错文案同时提示两个名字；②**`ast_search` / `ast_rewrite` 接受 `pat`**（omp `ast_grep.pat` / `ast_edit.ops[].pat`）；③**`todo` op 对齐 omp `TodoOperation`**：新增别名 `init`→`write`、`done`→`complete`、`drop`→`abandon`、`rm`→`remove`，并**新增两个真实 op** `append`（保留既有条目、编号接续）与 `remove`（按 id 删除 + 编号重排连续）；条目字段同时接受 omp 的 `status`（=`phase`）与 `blocker`（=`blocked_reason`）；④**`ask_user` 支持 omp `questions[]` 多问形态**：逐条提问、按脚本顺序取答、`Q/A` 编号汇总（`question`/`prompt` 二选一，`question` 优先）；单问路径行为完全不变（抽为 `ask_single` 复用）。**仍缺**：`memory_edit` 的 `update`/`invalidate` op（Gyre 现为 `search/forget/banks/clear`，需先扩存储 API）、`github` 的 omp op 命名、omp `edit` 的 mode 体系（hashline/replace/apply_patch 三形态）与 `apply_patch` 的 `*** Begin Patch` 语法 | `crates/hashline/src/tool.rs`（`input` 别名 + schema）、`crates/tools/src/ast_tool.rs`（`pat` 别名 ×2）、`crates/tools/src/todo.rs`（op 别名归一 + `append`/`remove` + `status`/`blocker` 字段）、`crates/tools/src/ask.rs`（`questions[]` 分派 + `ask_single` 抽取） | hashline：`accepts_input_alias_for_patch`（别名生效 + 真实落盘 + 缺字段错误文案）；tools：`ast_tools_accept_pat_alias`（搜索/重写/缺字段）、`omp_todo_op_aliases_and_append_remove`（init/done/drop/rm 别名 + append 保留既有 + remove 重排 + 删除不存在 id 报错）、`multi_question_array_asks_each_and_aggregates`（两问逐条送达 + 编号汇总 + 单问回归 + 缺字段错误） |

### 19.2 验证

| 项 | 结果 |
|---|---|
| `cargo clippy`（tools/hashline） | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| 测试 | agent-tools **257**（+3）、agent-hashline **63**（+1），其余套件全绿 |

### 19.3 剩余与下一轮

- **H32 尾项**：`memory_edit` 的 `update`/`invalidate`、`github` op 命名、omp `edit` 三形态（含 `apply_patch` 语法）。
- 下一轮：**H45**（内部重复实现收敛：SecretObfuscator ×2、Content-Length framer ×2；PTY 一半已在 H33 消掉）与 **H41/H43**（Agent 注册表 → hub/irc 寻址）。

---

## 二十、修复轮次（2026-09-11 · 第十三轮 · H45：内部重复实现收敛）

### 20.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H45 | 🟡 | ①**Content-Length framer 收敛为唯一实现**：新增 `agent_core::jsonrpc_frame`（`encode_frame` / `encode_str_frame` / `decode_frames` / `decode_frames_with_limit` / `parse_content_length` + `MAX_BODY_LEN`/`MAX_HEADER_LEN`），统一了此前两侧各不相同的上限与行尾兼容语义（`\r\n\r\n` 与裸 `\n\n` 都接受、body 512 MiB / 头 64 KiB）；`crates/dap/src/frame.rs` 变薄为**错误类型映射包装**（公共 API 不变），`crates/lsp/src/transport.rs` 的读循环从「`read_line` + `read_exact` 逐行解析」改为**缓冲区 + 共享解码器**（坏帧丢弃缓冲而非卡死读循环；响应/通知路由逻辑不变），LSP 本地 `format_frame`/`parse_content_length` 复制实现删除、委托共享编码器；②**正则脱敏实现收敛**：`agent_core::PatternRedactor`（不可逆 `<redacted>` 模式掩码，内置 8 组 pattern + 可追加）成为唯一实现，`crates/advisor/src/obfuscator.rs` 从 115 行实现改为**类型别名再导出**（`pub use agent_core::PatternRedactor as SecretObfuscator`），advisor 公共 API 不变、语义不变；并**明确记录**它与 core `SecretsObfuscator`（可逆 HMAC 占位符）用途不同、不应合并——避免把「同名」误判为重复；③**PTY 能力已无零调用**（H33 已接 `run_pty_command`；`PtyShell` 持久会话仍无生产调用，属剩余项）。**仍缺**：`PtyShell` 接入（持久 cwd/环境的 `run_command` 会话） | `crates/core/src/jsonrpc_frame.rs`（新，含 4 例单测）、`crates/core/src/secrets.rs`（`PatternRedactor` + 内置 pattern 常量）、`crates/advisor/src/obfuscator.rs`（改为再导出）、`crates/dap/src/frame.rs`、`crates/lsp/src/transport.rs`、`crates/core/src/lib.rs` 导出 | core：`round_trip_and_incremental_decode`（分片逐字节喂入的边界断言）、`accepts_bare_newline_header_terminator`、`rejects_bad_header_and_oversized_body`、`trailing_partial_frame_is_not_consumed`、`pattern_redactor_masks_builtin_and_custom_patterns`（五类样本 + 不误伤 + 自定义 + 幂等）；DAP 18 例、LSP 69 例、advisor 11 例全绿 |

### 20.2 验证

| 项 | 结果 |
|---|---|
| `cargo clippy --workspace --all-targets` | 清零（顺带修掉 ask 测试里 std guard 跨 await 的告警） |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| 测试 | core **+5**（framer 4 + redactor 1）、DAP 18、LSP 69、advisor 11、tools 全绿 |

### 20.3 剩余与下一轮

- **H45 尾项**：`PtyShell`（持久 shell 会话）接入执行面作为「同会话保持 cwd/env」的后端 → **已在 §三十九 闭合**（实现为独立 `shell_session` 工具，而非 `run_command` 的新参数——设计取舍见 §39.1）。
- 下一轮：**H41/H43**（Agent 注册表 → hub/irc 子代理寻址与唤醒）——第 2/3 梯队里唯一的「平台承重」缺口；随后 **H28/H29**（todo eager prelude / goal loop）。

## 二十一、修复轮次（2026-09-11 · 第十四轮 · H41 + H43：Agent 注册表与 hub/irc 寻址）

### 21.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H41 | 🟡 | **hub 侧注册表/生命周期/名册落地**（此前只有 `register`/`unregister`/`send`/`peers` 四个最薄 API，无状态、无标签、无积压、无唤醒）：①`AgentStatus{Running,Idle,Parked}` + `AgentInfo{id,label,status,inbox_len}`（`hub.rs:48,71`）作为「可寻址」语义的注册表记录；②`register_handle(id, label, status) -> HubHandle`（`hub.rs:190`）取代裸 `UnboundedReceiver`，`HubHandle{rx, wake: Arc<Notify>, inbox_len: Arc<AtomicUsize>}`（`hub.rs:127`）暴露 `try_recv`/`inbox_len` 并**在消费时递减积压**（`hub.rs:147-162`）；③生命周期 API `set_status`/`park`/`revive`/`wake`（`hub.rs:228,243,251,256`，未注册返回 `false` 而非静默成功；`wake` 触发 `Notify` 并置 `Running`）；④`Hub::agents()`（`hub.rs:274`）按 id 排序返回完整名册（含自己），`broadcast(from, body)`（`hub.rs:297`）投递给除发送者外的全部在册代理并返回成功投递数（收件箱已关闭者跳过，尽力而为）；⑤`HubSupervision` 端口（`hub.rs:520-528`）把「在途作业快照/取消」与 hub 解耦。**仍缺**：名册持久化（跨进程恢复）与 `at_capacity` 预拒（容量上限语义）。 | `crates/core/src/hub.rs`（`AgentStatus` :48、`AgentInfo` :71、`HubHandle` :127、`register_handle` :190、`set_status/park/revive/wake` :228-270、`agents` :274、`broadcast` :297、`send_tracked` :341、`ack` :394、`HubSupervision` :520）；消费面 `crates/tools/src/hub_tool.rs`（`agents` op :376、`broadcast` op :396，`register` 改走 `register_handle`） | core 7 例：`roster_status_and_wake`（名册按 id 排序/标签/状态/park→revive→wake 状态迁移）、`broadcast_skips_sender_and_dead_inboxes`、`ack_lifecycle`、`register_send_recv_unregister`、`reregister_replaces_inbox`、`send_after_drop_reports_gone`、`supervision_port_dispatches`；tools 2 例（本轮新增）：`agents_reports_roster_status_and_backlog`（3 条名册含 `[running] 标签=Scout 积压=2`、`[parked]`、自己标记、排序断言）、`broadcast_skips_sender_and_dead_inboxes`（投递数 1、发送者不在收件人内、缺 `message` 报错） |
| H43 | 🟡 | **hub/irc 子 agent 寻址、唤醒与回执通道**（此前只有 `main` 入册、子 Agent 完全不在总线上，父 Agent 无法寻址子任务，也没有任何 ACK/唤醒语义）：①**子代理入册**：`TaskTool::register_in_hub`（`task_tool.rs:251`）把每个子代理以 `task-<n>`（`task_seq` 自增，`task_tool.rs:76`）注册进 hub，并起一个 **inbox → steering 转发任务**（收件箱消息在轮次边界注入子代理，投递即 ACK），`HubRegistration` 的 `Drop` 中止转发任务并注销（`task_tool.rs:716-725`）——子代理生命周期与在册状态一一对应，无泄漏；②**回执（ACK）**：`Hub::send_tracked` 签发 `ack_id` + `watch` 通道，`Hub::ack` 兑现（`hub.rs:341,394`），`wait_ack` 支持「ack 先于 wait」路径（避免二次成功）；③**名册/广播 op**：`hub_tool` 新增 `agents`（名册 + 自己标记 + 状态/标签/积压）与 `broadcast`（群发排除自己、返回投递数）两个 op（`hub_tool.rs:376,396`）；④**三前端一致**：`crates/cli/src/rpc.rs:2376` 补上此前**完全缺失**的 `HubTool` 装配（CLI REPL :1262 / server :1282 已有），否则 RPC 前端下 TaskTool 注册的子代理无任何可寻址通道（单向黑洞）；同时补 `hub_processes`（`:2124`，`.with_processes` `:2378`）使进程托管 op 与另两前端对齐。**仍缺**：`unwatch`（退订）与跨会话回执投递（resume/重连后补投）。 | `crates/agent/src/task_tool.rs`（`:76` `task_seq`、`:125` `with_hub`、`:251` `register_in_hub`、`:319-321` 入册调用、`:716` `HubRegistration`+`Drop`）、`crates/core/src/hub.rs`（`HubMessage.ack_id` :84、`AckError` :96、`send_tracked` :341、`ack` :394）、`crates/tools/src/hub_tool.rs`（`agents`/`broadcast` op）、`crates/cli/src/rpc.rs:2124,2376`、`crates/cli/src/main.rs:1262`、`crates/server/src/lib.rs:1282` | core `ack_lifecycle`（签发→兑现→重复 ack 为 false；ack 先于 wait 只成功一次）、tools `broadcast_skips_sender_and_dead_inboxes` 与 `agents_reports_roster_status_and_backlog`、tools `send_recv_list_roundtrip`（drop tool 后 hub 仅剩 `task-1`，验证注销）、`wait_from_filter_buffers_mismatched_messages`（不匹配消息缓冲而非丢弃）；`cargo check -p agent-cli -p agent-server --all-targets` 0 error |

### 21.2 验证

| 项 | 结果 |
|---|---|
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零（仅剩 `proc-macro-error2` 的 future-incompat 提示，与本次改动无关） |
| `cargo test -p agent-core -p agent-tools -p agent -p agent-cli -p agent-server --all-targets` | 6 套件全绿，共 **630 passed / 0 failed**（1 ignored），含 core 124、tools 93、cli 259 |
| tools `hub_tool` 子集 | 20 例全绿（含本轮新增 2 例） |

### 21.3 剩余与下一轮

- **H41 尾项**：名册持久化（跨进程/restart 恢复；omp `agent-registry.ts` 的注册表是**文件持久**的）与 `at_capacity` 预拒（在入册前拒绝而非投递后才失败）。
- **H43 尾项**：`unwatch`（退订语义）与跨会话 ACK 补投（`resume`/重连后未兑现回执的处置）。
- 下一轮：**H28/H29**（todo eager prelude / goal loop）——第二梯队剩余的 Agent 纵深项；其后 **H34–H40**（第三梯队）与 **H21**（扩展形态决策）。

## 二十二、修复轮次（2026-09-11 · 第十五轮 · H28 + H29：todo 循环续跑与 goal loop）

### 22.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H28 | 🟡 | **todo eager prelude + 完成提醒续跑落地**（此前 `todo` 只有静态 system prompt 提示，模型可以不建清单、也可以在清单未完成时直接停止，没有任何循环侧接线）：①**只读端口**：新增 `agent_core::todo_loop`（`TodoLoopEntry`/`TodoLoopSnapshot{entries,incomplete}`/`TodoLoopSource` 端口 + `render_incomplete`/`is_done`），`agent-tools::TodoState` 实现它（`crates/tools/src/todo.rs:178-201`）——引擎只读、工具仍是唯一写入者（单活跃不变量/落盘/id 重排都在工具层），不存在第二份待办状态；②**eager prelude**：`run_loop` 在注入用户消息前判定「`[todo] eager` 非 off + `todo` 工具在场 + 上下文尚无用户消息 + 清单为空 + 首条 prompt 非问句/感叹」→ 追加先规划再动手的 `<system-reminder>`，`eager = "always"` 时首轮 request 带 `tool_choice = Hard(Function{todo})`（`engine.rs` 的 `eager_todo_prelude` / `eager_tool_choice`）；③**完成提醒续跑**：停止边界在 steering/aside/followUp/goal 之后、正常停止之前，若未完成待办非空、提醒次数 < `reminders_max`、且末轮不是向用户提问（`is_awaiting_user_answer`：末行问号 + 非 ASCII/英文疑问词，或「请确认 / let me know」类响应提示）→ 注入提醒消息并 `will_continue: true` 续跑；④**防刷屏**：同一提醒在模型未产生实质动作（任一工具调用）前保持静默（`todo_reminder_awaiting`），达上限后按正常停止；⑤**配置与三前端**：新增 `[todo] eager / reminders / reminders_max`（`crates/config/src/config.rs` `TodoConfig`，`config.example.toml` 有文档），`AgentBuilder::todo_loop_source/todo_loop_config`，CLI/RPC/server 三处均把**同一个** `Arc<TodoState>` 交给 `TodoTool` 与引擎。**仍缺**：omp 的 `hasPendingAsyncWake()` 护栏（后台作业在途时不提醒——Gyre 引擎侧无 job 句柄）与 `takeMidRunNudge`（12 次变更工具后中途对账）、压缩后 eager nudges。 | `crates/core/src/todo_loop.rs`（新，端口 + 2 例单测）、`crates/tools/src/todo.rs:178`（`impl TodoLoopSource for TodoState`）、`crates/agent/src/engine.rs`（`eager_todo_prelude`/`todo_reminder_text`/`is_awaiting_user_answer` + 首轮注入点 + 停止边界注入点 + `eager_tool_choice`）、`crates/agent/src/lib.rs`（`TodoEager`/`TodoLoopConfig` + builder）、`crates/config/src/config.rs`（`TodoConfig`）、`config.example.toml:278`、三前端装配（`main.rs:1449`、`rpc.rs:2512`、`server/lib.rs:1364,1369`） | agent 5 例（`todo_reminder_continues_up_to_max`（首轮+工具轮+2 次续跑 = 4 次 provider 调用、2 条提醒入上下文）、`todo_reminder_skips_clear_list_and_user_question`、`eager_todo_prelude_injects_once_and_forces_first_tool_choice`（第二个 run 不重复注入 + `tool_choice` 逐轮记录 `[true,false]`））、core 2 例、tools 1 例（端口快照与清单一致：未完成=2、已完成不入提醒） |
| H29 | 🟡 | **goal loop 落地**（此前只有「预算记账 + 超限提醒」的 `GoalState`，没有 objective、没有状态机、没有工具、没有续跑）：①**状态机**：`GoalStatus{active,paused,budget-limited,complete,dropped}` + `GoalState{objective,status,token_budget,max_continuations,continuations,...}`，迁移规则对齐 omp（`create` 拒绝已有未终结目标、`replace` 仅活跃目标、`complete` 唯一完成入口且拒绝重复/已放弃、`resume` 拒绝 complete/dropped、`pause` 拒绝终结态、`drop_goal` 清目标但保留记账；`note_usage` 超限时 `active → budget-limited`）；②**目标级预算**：`token_budget` 优先、缺省回落会话 `[goals]` 预算，`remaining_tokens`/`effective_token_budget` 供提示渲染；③**三类提示**（`render_prompt`，objective 经 `escape_objective` 转义，防把用户文本当标签注入）：`active`（每 run 起首注入常驻目标上下文）、`continuation`（停止边界续跑，含「complete 前必须核对当前仓库状态/核验范围=声明范围/预算耗尽≠完成」门规）、`budget-limit`（收尾，不续跑）；④**续跑**：停止边界若目标 `active` → 注入 continuation 并 `note_continuation()`（默认上限 8，达上限明确告警停止）；⑤**用户中断**：cancel 分支把活跃目标转 `paused`（对齐 omp `onTaskAborted`，不静默丢目标）；⑥**`goal` 工具**：`create|get|replace|pause|resume|complete|drop`（`crates/agent/src/goal.rs`，capability=ReadOnly），三前端均注册；⑦**目标模式默认可用**：`goal_state` 三前端恒在场（预算 0 = 不限），不再要求先配 `[goals]`；⑧**`/goal` 命令扩展**：`pause|resume|complete|drop` 子命令 + 目标状态展示，i18n 四语（zh/en/ja/ru）补 8 键。**仍缺**：目标持久化（`goal`/`goal_paused` 落盘，重启恢复）与 `goal_updated` 事件面（RPC/ACP 推送）、guided goal interview、`goal-todo-context` 提示。 | `crates/agent/src/goal.rs`（新：状态机 + 提示 + `GoalTool` + 6 例单测）、`crates/agent/src/lib.rs`（`pub mod goal` + 再导出，移除旧 `GoalState`）、`crates/agent/src/engine.rs`（run 起首 active 注入、停止边界 continuation、cancel→pause）、`crates/cli/src/main.rs:1261,1443`、`crates/cli/src/rpc.rs:2369,2506`、`crates/server/src/lib.rs:1274,1376`（`goal_state` + `GoalTool` + builder）、`crates/cli/src/repl.rs`（`handle_goal` 子命令）、`crates/i18n/locales/{zh,en,ja,ru}.json` | goal.rs 6 例（`create_requires_no_unfinished_goal_and_rejects_bad_budget`、`status_transitions_and_resume_rules`、`goal_budget_flips_to_budget_limited_and_prompts_follow_state`、`session_budget_is_fallback_and_continuations_are_capped`、`prompts_escape_objective_and_carry_budget`、`goal_tool_ops_roundtrip`）；agent 3 例（`goal_continuation_runs_until_cap`（2 次续跑 + 达上限告警 + 3 次 provider 调用）、`goal_complete_via_tool_stops_continuation`（经 `goal` 工具 complete 后不续跑）、`goal_pauses_on_cancel`（取消后 0 次 provider 调用、状态 paused））；旧预算用例 `goal_soft_budget_injects_once_then_stops` / `goal_hard_budget_stops_without_injection` 保持绿（向后兼容） |

### 22.2 验证

| 项 | 结果 |
|---|---|
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零（顺带修掉 `unnecessary_option_map_or_else`、两处 `double_must_use`、`needless_character_iteration`） |
| `cargo test --workspace --all-targets --no-fail-fast` | **55 套件全绿**（agent 132、tools 251+、cli 260、i18n 7、config 等），仅剩既有环境性失败 `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)==0`，与本次改动无关） |
| 本轮新增用例 | core 2（todo 端口）、tools 1（端口一致性）、agent 5（H28 循环）、goal/agent 9（H29 状态机与工具） |

### 22.3 剩余与下一轮

- **H28 尾项**：`hasPendingAsyncWake()` 护栏（需要引擎可见的后台作业句柄——可与 H27 job 生命周期接线合并做）与 `takeMidRunNudge`（中途对账 nudge）、压缩后 eager nudges。
- **H29 尾项**：目标持久化（会话 header/`.gyre` 落盘 + 恢复）+ `goal_updated` 事件（RPC/ACP 推送）+ guided goal interview。
- 下一轮：**H34–H40**（第三梯队：会话/上下文/发现广度）与 **H21**（扩展形态决策）；中优先组剩余 **H26**（压缩触发策略）、**H30**（memory 后台 rollout）、**H25**（模型目录）仍在清单内。

## 二十三、修复轮次（2026-09-11 · 第十六轮 · H36 + H40：配置加载细节与提示词链）

### 23.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H36 | ✅ | **`.env` 加载 + `${VAR:-default}`**（此前 `expand_env` 只认 `${VAR}`，未设置即空串，没有任何 dotenv 来源）：①**dotenv 模块**（`crates/config/src/dotenv.rs`）：`parse_env_line` 逐行对齐 Bun/omp 语义（可选 `export ` 前缀、整行 `#` 注释、未引号值的「空白 + `#`」行内注释、单/双/反引号包裹且 `#` 在引号内保留、转义引号不算闭合、未闭合引号取到行尾）；`DotEnv::from_files` 多文件**先到先得**并过滤不安全名值（含 `=`/`\0`）与 macOS `MallocStackLogging*`；②**来源与优先级**：`$GYRE_ENV_FILE`（`:` 分隔）> `<cwd>/.env` > `<cwd>/.agent/.env` > `<config_dir>/.env` > `$HOME/.env`，`GYRE_DOTENV=off|0|false` 关闭；③**不写回 `std::env`**（crate `#![deny(unsafe_code)]`，且运行期改环境在多线程下不安全）：值存进程级只读表，`expand_env` 按「真实环境 → dotenv → 默认值」查询，因此 shell 导出的值永远优先、`.env` 只补缺；④**`${VAR:-default}`**：POSIX `:-` 语义（未设置**或为空**取默认）；`${VAR}` 未设置仍为空串（保持既有兼容）；非法名字（含 `:`/`$`）整个占位符原样保留；⑤**启动接入**：CLI `main` 在 `Config::load` 前 `load_dotenv(&cwd)` 并打印一行来源摘要（server/RPC/ACP 都经同一条 CLI 启动路径，无需重复接线）；⑥文档：`config.example.toml` 头部说明展开语法、dotenv 优先级与关闭开关。 | `crates/config/src/dotenv.rs`（新：`parse_env_line` :88、`DotEnv::from_files` :33、`candidate_paths` :163、`load_dotenv` :186、`dotenv()` :197）、`crates/config/src/env.rs`（`expand_env` :17、`expand_env_with` :23、`split_name_default` :58、`dotenv_lookup` :66）、`crates/config/src/lib.rs`（`pub mod dotenv` + 再导出）、`crates/cli/src/main.rs:722-737`（启动加载 + 摘要）、`config.example.toml:1-7`（语法与优先级文档） | config：`parses_plain_export_quoted_and_commented_lines`、`from_files_first_wins_and_skips_unsafe`、`candidate_paths_put_cwd_before_user_scope`、`escapes_are_respected_when_finding_closing_quote`、`default_value_applies_when_unset_or_empty`（未设置/空/有值/显式空默认/含 `}` 默认/非法名字六路）、`multiple_placeholders_in_one_string`，加上既有 `expands_known_var` / `missing_var_becomes_empty` / `unclosed_is_passthrough` 共 9 例 |
| H40 | ✅ | **上下文文件双通道去重 / `@import` / SYSTEM.md / `--append-system-prompt` 四项全部落地**（此前 `discover_context_files` 只是「读用户级 + walkup 读项目级」并直接拼接，无去重、不展开 `@import`、无 SYSTEM.md、无追加参数）：①**`@import` 展开**（`crates/config/src/at_imports.rs`，移植 omp `discovery/at-imports.ts`）：行首/空白后的 `@path` 内联，相对**引用文件**目录解析、`~/` 相对 HOME，围栏代码块与行内代码跨度内的 token 原样保留（`user@example.com`、`npm install @types/node` 不误伤），尾部句读标点随 token 一起替换，递归上限 5 跳且 visited 跨文件共享（环静默断开），读不到的文件保留原 token；②**双通道包含去重**（`crates/config/src/context_files.rs`）：`split_blocks` 按空行分段但围栏内不分段，`blocks_contain` 判连续子序列，`dedupe_contained` 按 depth 降序——被**更权威**（更近 cwd / depth 更小）文件完整包含的段落整体丢弃（用户级 = `u32::MAX`），避免同一规则在用户级与项目级重复占用上下文；③**`SYSTEM.md` 覆盖链**：项目 `<cwd>/SYSTEM.md` 覆盖用户 `<config_dir>/SYSTEM.md`，内容同样走 `@import`；`agent-sdk::compose_context_files` 把它作为**第一段**注入（`<system-prompt-customization>`，先于行为四段），三前端共用同一装配函数；④**`--append-system-prompt <TEXT\|FILE>`**：新增 CLI flag（+ `[agent] append_system_prompt` 配置键，flag 优先），取值语义对齐 omp `resolvePromptInput`（含换行 = 字面文本；否则可读文件则读其内容；否则字面文本），经 `AgentBuilder::append_system_prompt` 在所有章节**最末尾**注入（宿主最终覆盖指令）。 | `crates/config/src/at_imports.rs`（新：`MAX_AT_IMPORT_DEPTH` :20、`expand_at_imports` :38、`expand_at_imports_with_home` :51）、`crates/config/src/context_files.rs`（新：`collect_context_files` :34、`system_prompt_file` :76、`dedupe_contained` :98、`split_blocks` :125、`blocks_contain` :159、`render_context_file` :169）、`crates/config/src/config.rs:1399,1409`（`discover_context_files` 委托 + `discover_system_prompt`）、`crates/config/src/config.rs:671`（`AgentConfig.append_system_prompt`）、`crates/sdk/src/lib.rs:132-140`（SYSTEM.md 段最先注入）、`crates/agent/src/lib.rs:821`（builder）、`crates/agent/src/engine.rs`（末尾注入）、`crates/cli/src/main.rs`（flag + `resolve_prompt_input` :`cli.help.arg.append-system-prompt`）、三前端装配（`main.rs` / `rpc.rs` / `server/lib.rs`）、`crates/i18n/locales/{zh,ja,ru}.json`、`config.example.toml:97-100` | at_imports 5 例（相对导入、邮箱/围栏/行内代码不展开、尾部标点与缺失文件保留、环引用与 5 跳上限、`~/` 指定 HOME）；context_files 5 例（分段保围栏、包含去重且按 depth 而非位置、depth 双向往例、端到端发现+`@import`+渲染、SYSTEM.md 项目覆盖用户）；sdk +1（`system_md_customization_precedes_behavior_sections`：定制段最先、`@import` 在 SYSTEM.md 内也展开、行为四段紧随）；agent +1（`append_system_prompt_reaches_provider_system`：注入且位于 system 末尾） |

### 23.2 验证

| 项 | 结果 |
|---|---|
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | **55 套件 ok / 2,774 例通过**；唯一失败仍是既有环境性 `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)` 失败，与改动无关） |
| 本轮新增用例 | config +15（dotenv 5 含端到端 `load_dotenv→expand_env` + env 2 新增 + at_imports 5 + context_files 5，既有 2 例保留）、sdk +1、agent +1 |

### 23.3 剩余与下一轮

- H36 已闭合（`${VAR}` 未设置仍为空串是与 omp「保留字面量」的有意差异，已在 `env.rs` 文档注明；如需对齐可后续单独决策）。
- H40 已闭合（未做 omp 的 `customPrompt` 整段替换模式与 `dedupeAlwaysApplyRules`——Gyre 无 always-apply 规则面）。
- 第三梯队剩余：**H34**（交互命令族）、**H35**（发布编排）、**H37**（回归测试网）、**H38**（用户文档）、**H39**（collab 决策）；第二梯队余项 **H21**（扩展形态决策）与中优各组仍待处理。

## 二十四、修复轮次（2026-09-11 · 第十七轮 · H34：交互命令族）

### 24.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H34 | ✅ | **清单内 10 个交互命令全部落地**（此前缺 `/clear`、`/settings`、`/hotkeys`、`/context`、`/usage`、`/jobs`、`/retry`、`/new`，`/todo` 只读，`/resume` 无参只打 usage）：①**`/clear`**：原地清空对话上下文（逐条 `ContextManager::delete_message_at(0)`，保留会话 id / 历史文件 / 系统提示词），需 async 故以 `CommandOutcome::Clear` 交回 `main`（`handle_command` 仍是同步纯函数）；②**`/new`**：新会话（与 `/fresh` 同义，对齐 omp「Start a new session」）；③**`/retry`**：`CommandOutcome::Retry` + `last_user_prompt()` 取最近一条**真实**用户输入（跳过 `<system-reminder>` / `[system` / `⚠` 开头的注入提醒与压缩提示），重发为下一轮 prompt；④**`/context`**：`CommandOutcome::Context` + `context_breakdown()` 按角色统计活跃路径消息数，并打印上下文 token 占用 / 上限 / 百分比与压缩阈值；⑤**`/usage`**：会话累计 token（输入/输出/缓存读/缓存写）、**计费口径**（input + cache_write + output，与 goals 记账一致）、缓存命中率、累计成本、上下文占用；⑥**`/jobs`**：后台作业列表（id / 类型 / 状态 / 已运行时长 / 标签，`recent_jobs` 最近优先）与 `/jobs cancel <id>`；`[agent] async_enabled=false` 时明确提示而非空白——为此 `CommandContext` 新增 `jobs: Option<&Arc<AsyncJobManager>>`（`main` 传入会话级单例）；⑦**`/settings`**：当前生效设置摘要（模型/模式/审批/压缩阈值与 max_turns、todo eager·reminders、goals 预算与软硬停、异步作业、子 Agent、压缩后端、可选工具与 github 开关、语言与 UA）；⑧**`/hotkeys`**：行编辑/运行期快捷键表；⑨**`/todo` 可写**：`add <内容>` / `start|done|drop|rm <id>` / `clear`（id 接受 `t3`、`3`、`#3`；与 `todo` 工具同一状态，`TodoState::replace` 内部保证单活跃不变量与落盘）；⑩**`/resume` 选择器**：无参打印带序号的候选列表 + 提示，`/resume <n>` 按序号解析为会话 id，越界时给出范围错误，非数字仍按会话 id 处理。全部新命令进入 `builtin_commands()` + `HELP_KEYS`（帮助覆盖测试自动校验不漂移），i18n 四语（zh/en/ja/ru）各补 51 键（已校验四语键集一致）。 | `crates/cli/src/repl.rs`（`CommandOutcome::{Clear,Context,Retry}` :127-131、`CommandContext.jobs` :171、`context_breakdown` :857、`last_user_prompt` :876、`print_usage` :910、`handle_jobs` :971、`print_settings` :1018、`print_hotkeys` :1116、`handle_todo` :1139、`normalize_todo_id` :1203、`print_session_choices` :1209、`builtin_commands`/`HELP_KEYS` :182-189/:208-）、`crates/cli/src/main.rs`（`jobs: job_manager.as_ref()` :1680、`Clear` :1857、`Context` :1875、`Retry` :1918）、`crates/i18n/locales/{zh,en,ja,ru}.json`（`help.usage|context|clear|new|retry|jobs|settings|hotkeys` + `usage.*`/`context.*`/`jobs.*`/`settings.*`/`hotkeys.title`/`todo.*`/`resume.*`） | repl 5 例新增 + 既有帮助覆盖测试：`h34_new_clear_retry_context_are_outcomes`（`/clear`→Clear、`/new`→Fresh、`/retry`→Retry、`/context`→Context；`/usage`·`/settings`·`/hotkeys`·`/jobs`→Handled）、`h34_todo_is_writable_from_slash_command`（add→start→done 状态迁移、`1`/`#1`/`t1` 三种 id 写法、未知 id 不改状态、`clear` 清空）、`h34_resume_numeric_selector_maps_to_session_id`（越界→Handled、无参→Handled、非数字→Resume(id) 透传）、`h34_last_user_prompt_skips_injected_reminders`（注入提醒回退到原话 / 正常取最后一条 / 空上下文 None）、`h34_context_breakdown_counts_roles`（user/assistant/tool/other 计数与空快照）、`help_covers_all_builtin_commands`（既有：新增 8 个帮助词条全部被覆盖）；cli 122 例全绿 |

### 24.2 验证

| 项 | 结果 |
|---|---|
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test -p agent-cli --all-targets` | 37 + 122 例全绿（含新增 5 例 H34 用例与既有帮助覆盖测试） |
| `cargo test --workspace --all-targets --no-fail-fast` | **55 套件 ok / 2,779 例通过**；唯一失败仍为 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱内 `getpgid(0)` 失败，与本次改动无关）。其兄弟用例 `kill_builtin_signals_every_process_in_a_jobspec_pipeline` 在并行高负载下偶发（子进程同样受限 → 退出码 101），单独运行 `-p pi-shell --lib` 两次均为 862 passed / 1 failed（仅上述环境性用例） |

### 24.3 剩余与下一轮

- H34 行内的 10 项已闭合。omp 的其余斜杠命令（`advisor`/`export`/`trace`/`share`/`copy`/`pin`/`handoff`/`memory`/`cleanse`/`btw`/`tan`/`force`/`live`/`pause`/`debug`/`ssh`/`join`/`leave`/`browser`…）分别归属其他行：collab 互操作（H39）、扩展/插件面（H21）、会话管理（H31 已完成的 header/breadcrumb 之外的中优组）。
- 第三梯队剩余：**H35**（发布编排）、**H37**（回归测试网）、**H38**（用户文档）、**H39**（collab 决策）；第二梯队余项 **H21**（扩展形态决策）与中优各组。

## 二十五、修复轮次（2026-09-11 · 第十八轮 · H37：回归测试网）

### 25.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H37 | 🟡 | **四个面的最小行为测试网**（此前 `task_tool` 仅 3 个解析类单测、server 只有 HTTP 路由/帧映射单测、ACP 无 dispatch 级协议一致性用例、web 前端交付契约零覆盖）：①**task_tool 执行面**（`execution_tests`，6 例）：单任务文本回传、`tasks` 并行分节聚合（含各自文本与序号标题）、未知命名代理**在调用 Provider 前**拒绝、H18 只读白名单让子 Agent 的模型侧看不到 `write_file`/`run_command`（能力面裁剪而非审批拦截，经捕获 `CompletionRequest.tools` 断言）、`output_schema` 非法输出→带纠错反馈重试一次后返回紧凑 JSON、H43 子代理运行期以 `task-1` 在册且结束后注销（在 Provider 回调时刻抓名册快照）；为此新增可编排桩 Provider（记录每轮工具面与 hub 名册）+ 三工具注册表 + ToolContext 夹具。②**ACP 协议一致性**（5 例）：未知方法 → `-32601` 且回显方法名；**能力声明 ↔ 实现一致**（`initialize` 声明的 `loadSession` 与 `sessionCapabilities{list,resume,fork,close}` 逐个 dispatch，缺失参数须得 `-32602` 而非 `-32601`，防「声明了没实现」的协议漂移）；`authMethods: []` 下 `authenticate`/`logout` 快速失败（`-32603`）；客户端反向调用 `session/request_permission` → `-32600`（方法有效、方向错误）；旧别名 `newTask`/`cancel` 仍路由到标准处理器。③**server WS 面端到端**（新增 dev-dep `tokio-tungstenite`，锁文件已有该版本无需新解析）：绑定 `127.0.0.1:0` + `axum::serve`，真实客户端握手——缺失/错误 token 得 401 拒绝，正确 token 建连后**首帧必须是 `usage_snapshot`**（前端累计态基线，SET 语义）且内嵌 `Usage` 五个计数键齐备。④**web 前端交付契约**（`serve_embedded` 直测）：`/` 返回 `index.html`（`text/html` + `no-cache`，含 `<div id="root">` 挂载点与哈希 bundle 引用）、`index.html` 引用的**全部** `/assets/*` 逐个 200 + `immutable` + 正确 MIME（至少一个 JS bundle）、未知前端路由 SPA 兜底回 `index.html`、路径穿越 `/../Cargo.toml` 不泄漏仓库文件。 | `crates/agent/src/task_tool.rs`（`mod execution_tests` :797；6 例 :984-1129）、`crates/acp/src/rpc.rs`（:1516-1592 五例；复用 `manager_with_session`/`rpc_req` 夹具）、`crates/server/src/lib.rs`（`next_frame_json` :4033、`ws_requires_token_and_sends_usage_snapshot_on_connect` :4061、`body_text` :4121、`embedded_frontend_surface_serves_spa_contract` :4130）、`crates/server/Cargo.toml:47`（`tokio-tungstenite` dev-dep） | 本轮新增 **13 例**（task_tool 6、ACP 5、server 2），全部通过；`cargo test -p agent --lib task_tool::execution_tests` 6/6、`cargo test -p agent-acp` 60/60、`cargo test -p agent-server` 22/22 |

### 25.2 验证

| 项 | 结果 |
|---|---|
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | **2,791 例通过**；唯一失败仍为 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`，与本轮无关），该套件 862 passed / 1 failed，其余套件全绿 |
| 新增测试面 | task_tool 执行 6 例、ACP dispatch 一致性 5 例、server WS 端到端 1 例、web 交付契约 1 例 |

### 25.3 剩余与下一轮

- **H37 尾项**：web 前端**组件级/逻辑级** JS 测试——仓库当前未安装测试运行器（`web/c5-ui` 只有 vite/tsc/tailwind，无 vitest/jest），离线环境无法新增依赖；可行路径是先落地 `tsc --noEmit` 类型门禁或引入 vitest 后补 `src/lib` 单测。
- 第三梯队剩余：**H35**（发布编排：xtask release + cargo-about/NOTICES）、**H38**（用户文档：per-tool / settings / MCP + 三语 README）、**H39**（collab 互操作 proto 决策）；第二梯队余项 **H21**（扩展形态决策）与中优各组。

## 二十六、修复轮次（2026-09-11 · 第十九轮 · H35：发布编排与 NOTICES）

### 26.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H35 | 🟡 | **发布链从「全手工」变为「有门禁的 xtask」**（此前 NOTICES 完全缺失，`about.toml` 只写了手工 `cargo about generate` 步骤且注明「接入 CI 为后续项」；CI 矩阵 5 腿）：①**新 `xtask` crate**（`crates/xtask`，workspace 成员）：`notices [--check] [--output]` 从 `cargo metadata --all-features --locked` 生成**离线**的 `THIRD-PARTY-NOTICES.txt`（无需安装 cargo-about）；输出确定性（包按 `(name,version)` 排序、许可证全文按内容去重编号），每个包列「名称 版本 — 许可证 — 上游链接」，全文集中在文末并标注使用方。②**vendor fork 不漏**：Cargo 会把 `vendor/` 下的 path 依赖自动入册为 workspace 成员，若照搬「排除 workspace 成员」会漏掉 `pi-shell`/`pi-builtins`/`pi-walker`/`brush-core` 的 MIT 义务——过滤规则改为「排除 `<root>/crates/*` 下的自有 crate」，四个 fork 因此入列（`about.toml` 注释点名的正是这四项）。③**生成物**：`THIRD-PARTY-NOTICES.txt`（725 个第三方包 / 388 份去重许可文本，1.27 MB，含 MIT 与 Apache-2.0 正文）已生成并提交；`--check` 实测「已是最新」，两次生成字节一致（确定性验证）。④**`release-check` 预检**：版本一致性（`Cargo.toml` workspace vs `Cargo.lock` 的 `agent`）、`CHANGELOG.md` 含 `## [Unreleased]`、三语 README 齐备、NOTICES 与当前依赖图一致——四项逐一打印，任一失败即非零退出。⑤**CI 接入**：lint job 增加 `Release preflight (xtask)` 步骤（承担 NOTICES 漂移门禁）；发布矩阵 **5 → 7 腿**（补 `x86_64-apple-darwin`（Intel runner `macos-15-intel`）与 `aarch64-unknown-linux-gnu`），并同步 `release_sums` 的资产完整性清单与 release notes 二进制列表（该清单有「新增/删除 matrix 目标必须同步」的硬门注释）。⑥**docs**：`about.toml` 的 CI 说明更新为「xtask 负责离线门禁、cargo-about 仍是发布资产权威生成器」。**仍缺**：版本 bump / tag / publish 的自动编排（`xtask release` 目前只有 `release-check`；omp `release.ts` 的 bump→changelog 定稿→commit/tag/publish→新 `[Unreleased]` 仍未移植）。 | `crates/xtask/Cargo.toml`（新）、`crates/xtask/src/main.rs`（子命令路由）、`crates/xtask/src/notices.rs`（`collect_packages` :45、`parse_metadata` :80、`render` :137、`license_files` :239、`workspace_root` :279、`run` :290）、`crates/xtask/src/release.rs`（`run` :14、`check_versions` :63、`check_changelog` :99、`check_readmes` :110）、`crates/xtask/tests/notices_artifact.rs`（生成物结构契约）、`THIRD-PARTY-NOTICES.txt`（新，26,036 行 / 725 包 / 388 文本）、`.github/workflows/rust.yml`（preflight 步骤 :70、Intel-darwin 腿 :186、gnu-aarch64 腿 :194、资产清单 :371）、`about.toml`（CI 说明） | `cargo test -p xtask` 7 例：`parse_metadata_excludes_workspace_members_and_sorts`、`parse_metadata_keeps_vendor_forks_that_are_workspace_members`（防 fork 漏列回归）、`parse_metadata_rejects_missing_sections`、`render_lists_packages_by_license_and_undeclared_group`、`version_check_reads_real_workspace_and_agrees`（仓库自身版本/CHANGELOG/README 三项预检）、`missing_entries_are_reported_per_item`、`notices_artifact_is_self_consistent`（生成物：头部声明、统计行与明细行数一致、四个 vendor fork 在列、MIT+Apache 正文、去重文本数）；`cargo xtask notices --check` 与两次生成 diff 为空（确定性）；`cargo xtask release-check` 四项全过 |

### 26.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | **清零**——顺带修掉工作区 189 处既有格式漂移（CI lint job 的 `cargo fmt --all --check` 此前必然失败；`vendor/` 无需改动） |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | **2,798 例通过**；失败仅 vendor `pi-shell` 两项环境性用例（`kill_process_group_refuses_self_pgroup` 恒失败于沙箱 `getpgid(0)`；`kill_builtin_signals_every_process_in_a_jobspec_pipeline` 在并行高负载下偶发，单独运行 `-p pi-shell --lib` 只剩前者） |
| 发布预检 | `cargo xtask release-check` → 版本 0.1.0 一致 / CHANGELOG 有 `[Unreleased]` / 三语 README 齐备 / NOTICES 与依赖图一致（725 包 / 388 文本） |

### 26.3 剩余与下一轮

- **H35 尾项**：`xtask release`（版本 bump + changelog 定稿 + commit/tag/push + 新 `[Unreleased]` 段）、cargo-about 产物与 `xtask notices` 的一致性比对（需 CI 安装 cargo-about）、gnu-aarch64 腿的运行期冒烟（需 arm64 sysroot + QEMU）。
- 第三梯队剩余：**H38**（用户文档：per-tool / settings / MCP + 三语 README 新功能）、**H39**（collab 互操作 proto 决策）；第二梯队余项 **H21**（扩展形态决策）与中优各组。

## 二十七、修复轮次（2026-09-11 · 第二十轮 · H38：用户文档）

### 27.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H38 | 🟡 | **工具参考改为「从真实工具面生成」+ MCP 指南 + 三语 README 新增能力段**（此前 `docs/` 只有设计/分析类文档，没有任何面向用户的工具或 MCP 参考；README 三语均未记录本轮梯队的实际新增能力）：①**`cargo xtask docs --tools`**（`crates/xtask/src/docs.rs`）：用**与前端相同的构造器**装配核心工具/全部可选工具组/会话工具/宿主工具（`core_tools_with_jobs`、`ast_tools`、`image_tools`、`lsp_tool`、`HashlineTool`、`RunPtyTool`、`DebugTool`、`SshTool`、`BrowserTool`、`GithubTool`、`TodoTool`/`AskUserTool`/`CheckpointTool`/`RewindTool`/`SecurityScanTool`/`HubTool`、五个 memory 工具、`GoalTool`、`TaskTool`），逐个读 `Tool::name/description/capability/schema` 渲染 Markdown——概览表（工具/分组/能力档/启用开关/摘要）+ 逐工具参数表（参数/类型/必填/说明，枚举取值展开）；②**生成物**：`docs/tools.md`（498 行，**33 个工具**：核心 7 / 可选 13 / 会话 11 / 宿主 2），可选工具标注 `[tools] <key> = true` 启用方式；③**漂移门禁**：`--check` 可比对模式，并已并入 `xtask release-check`（CI lint job 的 release preflight 覆盖），工具 schema/描述变更后忘记重生成会直接失败；④**`docs/mcp.md`**（70 行）：三种接法（TOML `/` `mcp.json` `/` CLI）与优先级、三种传输（stdio / Streamable HTTP / legacy SSE）、能力门控（tools/prompts/resources 及「只有 resources 的 server 也能连」）、roots 与 server→client 请求应答、deferred 连接语义、排障表与相关代码索引（字段级 schema 指向 `config.example.toml`，避免两处漂移）；⑤**README 三语**（`README.md` / `README_ZH.md` / `README_RU.md`）：新增「参考文档」段（工具参考/MCP 指南/NOTICES/发布预检四项入口）与「近期新增能力」（goal 模式、todo 循环、`.env`+`${VAR:-default}`、`SYSTEM.md`/`@import`/`--append-system-prompt`、10 个新交互命令）。**仍缺**：`docs/settings.md`（逐项设置参考）——配置字段当前以 `config.example.toml` 注释为事实源，尚未做「schema ↔ 文档」生成与校验。 | `crates/xtask/src/docs.rs`（`collect_tools` :59、`render` :311、`default_output` :389、`run` :399）、`crates/xtask/src/main.rs:26`（子命令 help）、`crates/xtask/src/release.rs:56`（release-check 第 5 项门禁）、`crates/xtask/tests/docs_artifact.rs`（生成物结构契约）、`docs/tools.md`（新，498 行 / 33 工具）、`docs/mcp.md`（新，70 行）、`README.md` / `README_ZH.md` / `README_RU.md`（参考文档 + 新增能力段） | `cargo test -p xtask` **11 例**（lib/bin 9 + docs_artifact 1 + notices_artifact 1；其中 docs.rs 3 例：`params_extract_type_required_and_description`（必填/枚举展开）、`params_empty_when_no_properties`、`render_groups_and_escapes_table_cells`（竖线转义）；`docs_artifact.rs`：`tools_doc_covers_groups_and_flagship_tools`——头部声明、统计行与概览表行数一致、四个分组都在、10 个关键工具（read_file/write_file/run_command/grep/glob/todo/goal/task/hub/ask）有详情段、可选工具标注启用开关、read_file.path 为必填 string）；`cargo xtask docs --tools --check` 实测「已是最新（33 个工具）」 |

### 27.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | **2,803 例通过**（58 个套件 ok）；唯一失败仍为 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`，与本轮无关） |
| 文档门禁 | `cargo xtask docs --tools --check` 最新；`cargo xtask release-check` 五项全过（版本 / CHANGELOG / 三语 README / NOTICES / docs/tools.md） |

### 27.3 剩余与下一轮

- **H38 尾项**：`docs/settings.md`——按 `Config` 结构逐项生成（section/key/类型/默认值/说明），并加同款 `--check` 门禁；`config.example.toml` 可退化为「示例 + 指向 settings 参考」。
- 第三梯队仅剩 **H39**（collab 互操作 proto 决策：对齐 proto=3 或宣布 proto=1 独立并界定互操作边界）；第二梯队余项 **H21**（扩展宿主形态决策：子进程协议 vs MCP-only）与中优各组。

## 二十八、修复轮次（2026-09-11 · 第二十一轮 · H26：自动压缩触发策略）

### 28.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H26 | 🟡 | **压缩触发从「单一 0.8 百分比」升级为三级策略 + provider 上报下限**（此前 `context_window_guard` 是唯一的触发口径，无法表达「大窗口留固定余量」或「绝对阈值」；且判定只看本地估算，出向压缩/脱敏让 provider 上报值偏小时真实历史可逃过触发）：①**`agent_core::CompactionPolicy`**（移植 omp `resolveThresholdTokens` / `resolveBudgetReserveTokens` 的三级优先级）：**绝对阈值** `threshold_tokens`（钳到 `[1, window-1]`）＞**预留余量** `reserve_tokens`（阈值 = `window - reserve`，配置余量 ≥ 窗口时按窗口 15% 兜底——「不可能默认值」回退）＞**百分比** `guard`（`floor(window × pct%)`，pct 取整到 `[1,99]` 避免 `f32 0.01 → f64` 精度少 1 token；下界钳到 1，否则极小窗口 `floor(0.8×1)=0` 会等于关闭压缩）；`window == 0` → 阈值 0 = 不触发。`TokenUsage::reaches(threshold)` 用 `>=` 与既有 `near_limit` 同语义（保持既有测试与行为兼容）。②**配置**：新增 `[agent] compaction_threshold_tokens` / `compaction_reserve_tokens`（`config.example.toml` 有三行说明与优先级顺序），`AgentBuilder::compaction_policy_from_config(guard, threshold, reserve)` 单点组装避免三前端优先级漂移；`context_guard(...)` 在未给绝对/余量时同步百分比（既有 API 语义不变）。③**引擎**：`run` 起首按 `model.max_input_tokens` 解析一次阈值（`CompactionPolicy::threshold_for`），三处压缩级联判定改用 `over_compaction_threshold`；每轮结束后记下 provider 上报的 prompt token（`input + cache_read + cache_write`）并作为**判定下限**（本地估算与上报值取较大者，移植 omp `compactionContextTokens`）。④**清理**：`Agent.context_guard` 字段已无读取方（阈值由 policy 承载），随之下线，仅保留 builder 参数以兼容既有调用。**仍缺**：按 provider 家族的 tokenizer 选择（当前仅按 model id 在 o200k/cl100k 间选择，非 OpenAI 家族仍以 cl100k 近似；omp 由 catalog 的 `tokenizer` 家族字段驱动）。 | `crates/core/src/context.rs`（`TokenUsage::reaches` :39、`CompactionPolicy` :57、`threshold_for` :89、4 例单测 :315-）、`crates/agent/src/lib.rs`（Agent/Builder 字段 :306/:527、`compaction_policy_from_config` :776、builder 方法 :799）、`crates/agent/src/engine.rs`（`over_compaction_threshold` :31、阈值解析 :195、`provider_context_tokens` 状态 :405、每轮更新 :1093、三处判定 :553/:573/:599）、`crates/config/src/config.rs:682,686`、`config.example.toml:94-96`、三前端装配（`main.rs:1442`、`rpc.rs:2486`、`server/lib.rs:1453`） | core 4 例（`absolute_threshold_wins_and_is_clamped_into_window`、`reserve_threshold_subtracts_and_falls_back_when_impossible`、`percent_is_the_default_and_never_disables_compaction`、`reaches_uses_inclusive_comparison_and_zero_means_unlimited`）；agent 2 例（`absolute_compaction_threshold_overrides_percent`：同一 1M 窗口下 99% 百分比不触发、绝对阈值 1 触发、余量 `window-1` 触发；`provider_reported_prompt_tokens_floor_the_trigger`：本地估算仅几百 token、阈值 90k，靠 provider 上报的 100k 下限触发压缩级联）；既有 `near_limit_emits_structured_compaction_events` 保持绿（向后兼容） |

### 28.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零（顺带修掉 1 处 `double_must_use`） |
| `cargo test --workspace --all-targets --no-fail-fast` | **2,809 例通过**（58 套件 ok）；唯一失败仍为 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`，与本轮无关） |

### 28.3 剩余与下一轮

- **H26 尾项**：tokenizer 家族选择——配置侧加 `tokenizer`（`o200k` / `cl100k` / `heuristic[:chars_per_token]`）并让 `TokenCounter` 按声明（而非仅 model id）选编码/近似比，覆盖非 OpenAI provider（Claude/Gemini/DeepSeek/Qwen 等）。
- §5.1 剩余 ⬜：**H4**（消息队列/`/queue`/dequeue）、**H5**（可定制键位）、**H21**（扩展宿主形态决策）、**H25**（静态模型目录 + 运行时发现）、**H30**（memory 后台 rollout/lease + structured append 竞态）、**H39**（collab 互操作 proto 决策）。

## 二十九、修复轮次（2026-09-11 · 第二十二轮 · H4：消息队列与 dequeue）

### 29.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H4 | ✅ | **运行期消息队列替代「只有单行 steering」**（此前运行中键入的每一行都立即 `steer` 给运行中的 agent，没有排队、没有可视化、没有取回，也没有「一轮结束后接着跑排队的活」的语义）：①**队列实体**（新 `crates/cli/src/queue.rs`）：`MessageQueue`（`VecDeque`）提供 `enqueue`/`enqueue_many`/`pop_front`（= 下一条执行）/`pop_back`（撤销最近一次）/`remove(1 基序号)`/`list`/`clear`/`len`；②**多消息输入解析**（移植 omp `modes/queue-input.ts`）：顺序枚举列表拆条——十进制 / 字母 / **规范**罗马数字编号（`I`-`MMMCMXCIX`，解析后回写一致才接受）、必须同级缩进 + 同一收尾标点（`.` 或 `)`）+ 严格递增，续行归属当前项（`1. 第一段\n   续行\n2. 第二段` → 2 条）；不满足则整段算一条；`-> ` / `=> ` 简写 = 只入队不打断；③**`/queue` 命令**（空闲态）：`list`（带序号）/`add <文本>`（支持列表一次入多条）/`pop`（取队首并**作为下一轮任务**执行 → `CommandOutcome::Inject`）/`undo`/`rm <n>`/`clear`，越界与缺参数给可读错误；④**运行中键入分派** `handle_running_input`：普通行仍即时 steering（既有语义不变），`->`/`=>` 入队并在状态行显示 `[queued #N]`，`/queue …` 由**宿主**消费（不再被当作 prompt 投给模型计费），运行中 `/queue pop` = 取出并立即投递给运行中的 agent；⑤**dequeue 键**：`Ctrl-Y` 经 `rustyline::ConditionalEventHandler` 把队首插入当前编辑行（空队列 = 无操作）；⑥**轮末自动续跑**：每轮结束后 `pop_queued` 把队首排为 `pending_task`，下一轮直接执行（不再等输入）；⑦**帮助/i18n**：`/queue` 进入 `builtin_commands` + `HELP_KEYS`（帮助覆盖测试自动防漂移），四语各补 11 键；README 三语把 `/queue` 与 `Ctrl-Y` 写进交互命令清单。 | `crates/cli/src/queue.rs`（新：`MessageQueue` :17、`parse_queue_shorthand` :96、`split_queued_messages` :252、4 例单测 :307-）、`crates/cli/src/lib.rs`（`pub mod queue`）、`crates/cli/src/repl.rs`（`CommandContext.queue` :173、`handle_queue` :922、`handle_running_input` :1009、`RunningInput` :1082、`lock_queue` :1092、builtin/HELP_KEYS 增 `/queue`+`help.queue`）、`crates/cli/src/main.rs`（队列表 :1639、`pending_task` :1649、`pop_queued` :3154、`DequeueHandler` :3159 + `bind_sequence(Ctrl-Y)`、运行中分派 :3372、`run_turn`/`run_turn_message`/`consume_stream` 增加队列参数）、`crates/i18n/locales/{zh,en,ja,ru}.json`（`help.queue` + `queue.*` 11 键）、三语 README | queue.rs 4 例（FIFO/LIFO 撤销/按序删除/清空；简写仅取消息体；十进制+字母+罗马列表拆条与续行归属；非连续/单条/混用标点/`1.5` 数字/空输入一律单条）；repl 2 例（`h4_queue_command_enqueues_lists_pops_and_clears`：列表入队 2 条、`pop` 返回 `Inject("先跑测试")` 且出队、`undo`/`rm`/越界/`clear`/非法子命令；`h4_running_input_queues_shorthand_and_steers_plain_text`：普通输入 → `Steer`、简写 → `Queued`、空行 → `Ignored`、运行中 `list` 不改队列、`add` 入队、`pop` → `Steer` 且出队）；`help_covers_all_builtin_commands` 覆盖新命令 |

### 29.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | **2,815 例通过**（58 套件 ok）；唯一失败仍为 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`，与本轮无关） |
| 分面 | `cargo test -p agent-cli --all-targets` → 41（lib，含 queue 4 例）+ 124（bin，含 repl 2 例）全绿 |

### 29.3 剩余与下一轮

- §5.1 剩余 ⬜：**H5**（可定制键位系统——本轮已用上 `rustyline` 自定义绑定，可作为 action 表的落点）、**H21**（扩展宿主形态决策）、**H25**（静态模型目录 + 运行时发现）、**H30**（memory 后台 rollout/lease + structured append 竞态）、**H39**（collab 互操作 proto 决策）。

## 三十、修复轮次（2026-09-11 · 第二十三轮 · H5：可定制键位系统）

### 30.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H5 | ✅ | **键位从硬编码变为声明式 action 表 + 配置覆盖**（此前唯一的自定义绑定是上一轮硬编码的 Ctrl-Y；没有任何 action 表、配置面或生效展示）：①**action 表**（新 `crates/cli/src/keybindings.rs`）：10 个动作，wire 名与 omp `app.*` 对齐——`app.interrupt` / `app.clear` / `app.exit` / `app.history.search` / `app.complete` / `app.message.dequeue`（H4 取回队首）/ `app.retry` / `app.session.new` / `app.command.queueList` / `app.command.settings`；每个动作带默认键（`None` = 默认不绑定）、插入文本（插入型动作 = 把 `/retry` 等命令文本写进编辑行）与中文说明；②**键位解析** `parse_key`：`ctrl-/alt-/shift-` 组合（顺序不限）、具名键（`enter`/`tab`/`esc`/`backspace`/`delete`/`insert`/方向/`home`/`end`/`pgup`/`pgdn`/`space`）、`f1..f12`、单字符；`ctrl-<字母>` 归一为大写（与 rustyline 控制字符约定一致，否则绑不到内部 `Ctrl-Y` 槽位）；非法输入给可读错误；③**覆盖解析** `resolve`：配置覆盖默认（默认键随之释放）、**同键冲突后者胜**（按动作表顺序，先前的绑定被移除，避免一键双绑）、非法键位回退默认并诊断、未知动作忽略并上报；④**配置面**：`[keybindings]`（`KeybindingsConfig`，`#[serde(flatten)]` 直接以动作名为键），`config.example.toml` 列出可绑定动作与示例；⑤**绑定到编辑器** `apply_keybindings`：原生动作用 `Cmd::ClearScreen` / `Cmd::ReverseSearchHistory` / `Cmd::Complete`，取回用带队列状态的 `ConditionalEventHandler`，插入型动作 `Cmd::Insert`，中断/退出交由信号与 EOF 路径；启动时逐条打印诊断（未知动作/非法键位）；⑥**`/hotkeys` 展示生效绑定**（`render_table`：键 + wire 名 + 说明，含被忽略项的诊断），固定键位（Enter/上下/ Esc）单列；README 三语补「可定制键位」。 | `crates/cli/src/keybindings.rs`（新：`Action` :22、`from_wire` :122、`resolve` :152、`parse_key` :205、`render_key` :267、`render_table` :304、5 例单测 :328-）、`crates/config/src/config.rs:1586`（`KeybindingsConfig`）+ `Config.keybindings`、`crates/cli/src/main.rs:1642`（解析 + 诊断）/`:3168`（`apply_keybindings`）、`crates/cli/src/repl.rs:175`（`CommandContext.keybindings`）/:1307（`print_hotkeys` 打印生效表）、`config.example.toml`（`[keybindings]` 段）、`crates/i18n/locales/{zh,en,ja,ru}.json`（`hotkeys.fixed`） | keybindings 5 例：`parses_modifiers_named_keys_and_function_keys`（组合修饰键/具名键/F 键/大写归一/三类错误）、`every_action_has_a_unique_parseable_default`（默认键可解析且互不冲突、wire 名可反查）、`resolve_applies_overrides_and_reports_bad_input`（覆盖生效且默认键释放、非法键回退默认、未知动作上报、插入型动作出现）、`duplicate_keys_last_action_wins`（同键仅保留最后动作）、`renders_readable_table_and_keys`；repl `h5_hotkeys_reflects_configured_bindings`（`alt-d` 覆盖 `app.message.dequeue` → 生效表出现 `Alt-D` 且 `Ctrl-Y` 消失、`ctrl-t` 绑定插入型动作、`app.nope` 进未知列表、`/hotkeys` 可执行）；`cargo test -p agent-cli --all-targets` 46 + 125 全绿 |

### 30.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | **2,821 例通过**（58 套件 ok）；唯一失败仍为 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`，与本轮无关） |

### 30.3 剩余与下一轮

- H5 的边界：Gyre 是 rustyline 行编辑层，omp 的 TUI 专属动作（模型轮换/工具展开/会话树导航/剪贴板等）在 Gyre 由斜杠命令承担，未映射为键位；配置为 TOML（Gyre 全仓配置单一来源）而非 omp 的 JSONC/YAML。
- §5.1 剩余 ⬜ 4 项：**H21**（扩展宿主形态决策）、**H25**（静态模型目录 + 运行时发现）、**H30**（memory 后台 rollout/lease + structured append 竞态）、**H39**（collab 互操作 proto 决策）。

## 三十一、修复轮次（2026-09-11 · 第二十四轮 · H30：记忆并发与后台沉淀）

### 31.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H30 | ✅ | **记忆落盘竞态与「多会话同时沉淀」两个真实缺陷**（此前 structured 后端是「读全文 → 拼接 → 整体写回」的 RMW，并发 retain 会互相覆盖；`local` 后端虽用 O_APPEND 但**行与换行分两次 write**，并发下会粘行；整文件替换用 `std::fs::write`，读者可能看到半截 JSONL；两个合并 hook 都在停止边界**内联 await** LLM 沉淀，且没有互斥——CLI/Web/ACP 同时结束任务会重复付费并互相覆盖记忆）：①**原子原语**（新 `crates/memory/src/atomic.rs`）：`append_line` 走 `O_APPEND` 且**行+换行一次 write**（O_APPEND 只保证单次 write 原子——分两次正是并发粘行的根因）；`write_atomic` 走「同目录临时文件（pid+纳秒）+ `sync_all` + `rename`」原子替换，失败清理临时文件；②**结构化后端接入**：`rewrite_records` → `write_atomic`，记录追加 → `append_line`（消灭 RMW）；③**合并租约**（新 `crates/memory/src/lease.rs`）：`<root>/.<name>.lease` 以 `create_new` 独占创建（写入 pid + 时间戳），带 TTL（默认 300s）——持有者崩溃留下的陈旧锁（或无法解析的锁）可被抢占，`LeaseGuard` drop 即释放；`structured.consolidate` / `local.consolidate` / `local.consolidate_mental_models` 各自持锁（structured 与心智模型各一把，互不阻塞），拿不到锁时**直接跳过**（`ConsolidateReport.skipped = true`，零 LLM 调用）；④**后台执行（可选）**：新增 `[memory] background_consolidate`（默认 `false` = 保持内联语义）；开启后两个 hook（`ConsolidateHook` / `StructuredSleepHook`）把沉淀 `tokio::spawn` 到后台任务，停止边界不再被 LLM 阻塞，日志区分「租约被他人持有而跳过」与「真正完成」；⑤**收口**：`ConsolidateReport` 增加 `skipped` 字段（CLI/RPC 两处 hook 装配同步）。 | `crates/memory/src/atomic.rs`（新：`append_line` :19、`write_atomic` :42、2 例单测 :73-）、`crates/memory/src/lease.rs`（新：`DEFAULT_LEASE_TTL` :17、`LeaseGuard` :21、`acquire` :43、3 例单测）、`crates/memory/src/structured.rs`（租约 :724、`rewrite_records` :1062、追加委托 :1076、2 例单测 :1211/:1239）、`crates/memory/src/store.rs`（租约 :99/:191、原子替换 :134/:146/:224、单次 write 追加 :308）、`crates/memory/src/consolidate.rs:25`（`skipped`）、`crates/config/src/config.rs:1146` + `config.example.toml`（`background_consolidate`）、`crates/cli/src/main.rs:1532,1542,2958,3045`、`crates/cli/src/rpc.rs:2580,2590`（hook 装配与后台 spawn） | memory 新增 7 例：`append_is_concurrency_safe`（8 线程 × 50 行并发追加 → 400 行且每行合法 JSON——该用例在修复前**确实失败**，暴露了「两次 write 粘行」）、`write_atomic_replaces_and_leaves_no_temp_file`（替换 + 无残留临时文件 + 深层目录）、`lease_is_exclusive_and_released_on_drop`（互斥/不同名独立/drop 释放/释放后可再取）、`stale_lease_is_stolen_after_ttl`（TTL 内不抢占、过期可抢且内容更新为当前 pid）、`corrupt_lease_file_counts_as_stale`、`consolidate_skips_when_lease_held_by_another_session`（他人持锁 → `skipped`、零改动、释放后可正常沉淀 2 条）、`concurrent_retain_keeps_all_records_parseable`（6 任务 × 20 条并发 retain → 120 行全合法 JSON）；`cargo test -p agent-memory` 83 + 7 全绿 |

### 31.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | **2,828 例通过**（58 套件 ok）；唯一失败仍为 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`，与本轮无关） |

### 31.3 剩余与下一轮

- H30 边界：租约是**单机文件锁**（多机共享目录不在范围内）；后台沉淀在进程退出前可能被截断（已在配置文档注明，租约 TTL 到期后可被下次接手）；未迁到 `AsyncJobManager`（当前用 `tokio::spawn`，job 面可见性留待 H27 尾项）。
- §5.1 剩余 ⬜ 3 项：**H21**（扩展宿主形态决策）、**H25**（静态模型目录 + 运行时发现）、**H39**（collab 互操作 proto 决策）。

## 三十二、修复轮次（2026-09-11 · 第二十五轮 · H25：内置模型目录）

### 32.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H25 | ✅ | **静态模型目录（离线近似）+ 目录命令**（此前没有任何内置目录：未显式配置时上下文窗口恒为 `128_000`、最大输出恒为 `4096`，且 `supports_thinking` 只由全局 `enable_thinking` 决定——`gpt-4o` 这类不支持思考的模型也会被塞思考配置；运行时发现只有 `agent models list` 拉 provider `/models`，缺「开箱即用的合理默认」）：①**目录**（新 `crates/config/src/model_catalog.rs`）：26 条记录覆盖 OpenAI（gpt-5/4.1/4o/o1/o3/o4-mini）、Anthropic（sonnet-4/opus-4/haiku-4/3-7-sonnet/3-5-sonnet/3-5-haiku）、DeepSeek（reasoner/chat）、Google（gemini 2.5/2.0/1.5）、智谱 GLM（4.5/4）、以及 qwen3/qwen/kimi-k2/grok-4/grok-3/llama-3/mistral-large，每条含匹配模式（`*`/`?` 通配）、上下文窗口、最大输出、是否支持思考与建议线协议；`lookup` 按声明顺序首个匹配（具体模式在前，如 `deepseek-reasoner*` 先于 `deepseek*`），`search` 支持按 id/api 过滤；②**只补缺语义**（`ModelProfile`）：`effective_max_input_tokens` / 新增 `effective_max_output_tokens` = 显式配置 > 目录 > 内置兜底（128_000 / 4096）；新增 `effective_supports_thinking` = 全局总开关 **且** 目录未声明不支持——**未收录的模型行为与引入前完全一致**（既有配置/测试零影响）；③**命令面**：`agent models catalog [filter]`（别名 `cat`）离线打印目录表（id 模式 / 建议 api / 上下文 / 最大输出 / 思考），无匹配时给「用 `agent models list` 查真实清单」的可执行提示；④**文档**：`config.example.toml` 说明「留空时按内置目录补缺」、i18n 四语的 `manage.help.row.models` 补 catalog 用法、README 增「内置模型目录」条目。 | `crates/config/src/model_catalog.rs`（新：`CatalogEntry` :22、`CATALOG` :36（26 条）、`lookup` :229、`search` :241、4 例单测）、`crates/config/src/config.rs`（`effective_max_input_tokens` :600、`effective_max_output_tokens` :608、`effective_supports_thinking` :617、`to_model` 接入、集成测试 :2400）、`crates/cli/src/manage.rs`（`ManageCmd::ModelsCatalog` :31、路由 :185-186、`models_catalog` :609、命令测试 :2125）、`crates/cli/src/main.rs:589`、`config.example.toml:24-25`、`crates/i18n/locales/{zh,en,ja,ru}.json`、`README.md` | catalog 4 例：`lookup_matches_known_families_and_reports_each_field`（gpt-4o/claude/deepseek 具体模式优先、大小写空白不敏感）、`unknown_models_and_empty_ids_do_not_match`、`catalog_entries_are_well_formed`（模式非空可匹配示例 id、窗口/输出为正且输出 < 窗口）、`search_filters_by_id_and_api`；config `model_catalog_fills_unknown_defaults_only`（claude-sonnet-4 → 200k/64k/支持思考；gpt-4o-mini → 128k/16k/不支持思考（`to_model(true).supports_thinking == false`）；显式配置覆盖目录；未知模型 `my-local-model` → 128k/4096/总开关透传）；manage `models_catalog_lists_and_filters`（全量 26 条、按家族过滤、无匹配提示）；CLI 实测 `models catalog` / `models catalog deepseek` / `models catalog nosuch` 三种输出 |

### 32.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零（顺带修掉 1 处 `write_literal` 与 1 处重复 `#[test]` 属性） |
| `cargo test --workspace --all-targets --no-fail-fast` | **2,834 例通过**（58 套件 ok）；唯一失败仍为 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`，与本轮无关） |
| CLI 冒烟 | `agent models catalog`（26 条）、`... deepseek`（2 条）、`... nosuch`（提示）全部符合预期 |

### 32.3 剩余与下一轮

- H25 边界：目录是**手工维护的离线近似**（不生成自上游 `models.json`），数值不保证与上游同步——需要精确控制时在配置里显式写窗口；未包含定价/模态字段（成本估算与多模态路由仍是后续项）。
- §5.1 剩余 ⬜ 2 项：**H21**（扩展宿主形态决策）、**H39**（collab 互操作 proto 决策）——两者都是架构/产品决策项。

## 三十三、修复轮次（2026-09-11 · 第二十六轮 · H21 + H39：两个架构决策 + 一处协议护栏）

### 33.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H21 | ✅ | **扩展宿主形态定案：MCP-only（ADR 0001）**（此前该行只有「无」与「定子进程扩展协议或 MCP-only 策略」两个选项，没有决策、也没有把既有扩展点写下来，用户无从知道第三方能力该往哪儿接）：新增 `docs/adr/0001-extension-host.md`——**决策**：v1 不自建插件宿主、不做进程内第三方代码加载、不做插件市场与扩展 UI 子协议；**把五类既有扩展点固化成表格**（MCP server → 工具/提示词/资源，三种传输；自定义斜杠命令 `.agent/commands/*.md` + 用户级；Skill（含 frontmatter 校验）；命名子代理 `.agent/agents/*.md`（受限工具面）；Hook（17 类事件）；工作区约定 `AGENTS.md`（含 `@import`）/`SYSTEM.md`），每项都给出代码位置；**明确不做**与理由（信任链/版本兼容/沙箱/崩溃隔离四项长期负担）；**四个重评触发条件**（无法用 MCP 表达的需求 ≥3 个、有维护者、MCP 规范提供宿主 UI 标准、进程内状态成为硬阻塞）与三个备选方案未采纳理由。README 增「Extending Gyre」条目指向 ADR。 | `docs/adr/0001-extension-host.md`（新）、`README.md`（参考文档段）；既有扩展点代码位置：`crates/mcp/`、`crates/config/src/config.rs`（`discover_commands` :1367）、`crates/skills/`、`crates/agent/src/agent_def.rs`、`crates/core/src/hook.rs`、`crates/config/src/context_files.rs`、`docs/mcp.md` | 决策文档 + 既有 51 例 MCP/命令/skill/子代理/hook/上下文测试继续作为这些扩展点的行为契约（`cargo test --workspace` 全绿）；无新增测试（决策项） |
| H39 | ✅ | **协同线协议定案：保持自有 proto=1 并做显式版本拒绝（ADR 0002 + 可执行护栏）**（此前版本号是硬编码字面量 `1`，既没有常量、没有版本校验，也没有任何「版本不同怎么办」的约定——guest 收到不兼容的 `welcome` 会当没事发生继续渲染）：①**决策文档** `docs/adr/0002-collab-protocol.md`：列出 omp proto=3 与 Gyre proto=1 的**帧集合差异表**（快照分块/bye/error 为 Gyre 独有），判定「改版本号≠可互操作」，因此**保持独立**并声明不承诺跨版本兼容；给出 host 桥密钥盲视导致「协商方向只能是 guest 校验 host」的已知边界；②**单一事实源**：`agent_collab::PROTO_VERSION = 1` 常量（`frame.rs`），host 侧 `seal_host_welcome` 与 guest 页面 `PROTO_VERSION` 常量同步使用（不再有裸字面量）；③**可执行护栏**：`proto_compatible(remote)` 只接受完全相等；新增 `CollabClient::decode_checked` ——解封后校验 `Welcome.proto`，不匹配返回 `CollabError::ProtoMismatch { remote, local }`（错误消息含两端版本与「刷新/升级」建议，由 `proto_mismatch_message` 生成）；**非握手帧不做校验**（版本只在握手帧声明）；普通 `decode` 不受影响（诊断/日志路径仍可解封）；④**guest 页面**（`collab_guest.html`）：`welcome` 分支比对 `PROTO_VERSION`，不一致时红色状态 + 系统提示并停止交互，不再静默降级；⑤`CollabError` 新增 `ProtoMismatch`（`thiserror` 消息复用同一函数）。 | `crates/collab/src/frame.rs`（`PROTO_VERSION`、`proto_compatible`、`proto_mismatch_message`）、`crates/collab/src/relay.rs`（`decode_checked`）、`crates/collab/src/error.rs`（`ProtoMismatch`）、`crates/collab/src/lib.rs`（导出）、`crates/server/src/lib.rs`（`seal_host_welcome` 用常量）、`crates/server/src/collab_guest.html`（常量 + 版本校验分支）、`docs/adr/0002-collab-protocol.md`（新）、`README.md` | collab 新增 2 例：`decode_checked_rejects_incompatible_protocol_version`（proto=3 的 `Welcome` → `ProtoMismatch{3,1}` 且消息含两端版本；本端版本通过；`Chat` 等非握手帧不受校验影响；普通 `decode` 仍可解封）、`proto_compatibility_is_exact_match`（`0/2/3/99` 全部不兼容）；`cargo test -p agent-collab` 29 例全绿 |

### 33.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零（顺带修掉 1 处 `clone_on_copy`） |
| `cargo test --workspace --all-targets --no-fail-fast` | **2,836 例通过**（58 套件 ok）；唯一失败仍为 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`，与本轮无关） |

### 33.3 收束：§5.1 高优先清单状态

至此 **§5.1 的 H1–H45 不再有 ⬜**（§四十四 后为 ✅ 35 + 🟡 10 + ⬜ 0）：

- **✅ 已闭合 35 项**：H1、H2、H4、H5、H6、H9–H17、H19、H21、H22、H24、H25、H26、H27、H28、H29、H30、H31、**H33**、H34、H36、H39、H40、H41、H42、H43、H44、H45。
- **🟡 已落地主体但仍有明确尾项 10 项**（每项的「仍缺」都写在 §5.1 表格里，并有 §九–§四十四 的轮次证据）：
  - 架构/协议面：H3（仅进程级 SIGINT，未做 raw 按键层）、H7（28 条命令显式不支持）、H8（无子代理/扩展 UI 帧）、H20（17 事件；无 decidable session hooks）、H23（**仍缺**运行时热更新与逐键元数据；未知键诊断/内省见 §四十一）。
  - Agent 纵深：H18（无 worktree 隔离/spawns/预算/输出契约）。
  - 工具/文档/交付：H32（omp `edit` 的 mode/`apply_patch` 语法；github worktree/watch op）、H35（版本 bump/tag/publish 自动编排）、H37（web 组件级 JS 测试）、H38（`docs/settings.md`）。
- **中优（§5.2 各组）**：配置组/协议组/工具组/运行时组/扩展组仍未系统展开——它们是独立的后续工作流，不属于本目标的第 1–3 梯队。
- **验证基线**（随每次轮次刷新，最新见 §四十四）：`cargo fmt --check` 清零、`cargo check --workspace --all-targets` 0 error/0 unused、`cargo clippy --workspace --all-targets` 清零、`cargo test --workspace --all-targets --no-fail-fast` **3,748 passed / 1 failed（59 套件，58 ok）**（唯一失败为 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup` 的环境性用例），`cargo run -p xtask -- release-check` 五项全过（版本/CHANGELOG/三语 README/NOTICES/工具文档生成物一致）。

## 三十四、修复轮次（2026-09-11 · 第二十七轮 · H26 尾项：可声明的 tokenizer 家族）

### 34.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H26（尾项） | ✅ | **tokenizer 家族从「只能按 model id 推断」变为「可显式声明」**（上一轮补齐了三级压缩阈值与 provider 上报下限；尾项是家族校正——非 OpenAI provider 没有公开 BPE 词表，此前一律按 cl100k 近似，CJK 语料的分词密度被系统性低估，压缩触发偏晚）：①**家族类型**（`crates/context/src/token.rs`）：`TokenFamily { O200k, Cl100k, Heuristic { chars_per_token } }` + `TokenFamily::parse` 接受 `o200k` / `o200k_base` / `cl100k` / `cl100k_base` / `heuristic`(=4) / `heuristic:<正数>` / `chars:<正数>`（大小写与空白不敏感；非法串返回 `None`）；②**声明优先、id 推断兜底**：`TokenCounter::family_for(model_id, spec)` —— 合法声明直接采用（claude 也能声明 `o200k`、gpt-4o 也能声明 `cl100k`），非法/缺省回落既有 id 推断（o200k 家族 vs cl100k），**笔误不改变计数口径**；③**计数路径**：新增 `count_text_with_family` / `count_context_with_family`（旧 `count_text_for` / `count_context_for` 保留为 `None` 特例，签名不变、行为不变）；启发式分支 `heuristic_chars = ceil(chars / chars_per_token)`（空文本 0、非空至少 1，比例下限 0.1 防除零）；④**配置面**：`ModelProfile.tokenizer` → 运行时 `Model.tokenizer`（`#[serde(default)]`），`config.example.toml` 说明取值与建议（CJK ≈ 1.5、英文 ≈ 4）；⑤**上下文接线**：`InMemoryContext`/`PersistentContext` 的状态里缓存 `last_tokenizer`（与 `last_model_id` 同时更新），`build_provider_context` 与 `token_usage()` 两处计数都走 `count_context_with_family`——**声明在 build 之后仍对 `token_usage()` 生效**（UI 与压缩判定口径一致）。 | `crates/context/src/token.rs`（`TokenFamily` :15、`parse` :32、`family_for` :101、`count_text_with_family` :118、`count_context_with_family` :148、`heuristic_chars` :213）、`crates/core/src/model.rs:106`（`Model.tokenizer`）、`crates/config/src/config.rs:579`（`ModelProfile.tokenizer`）+ `to_model` 透传 + 测试 :2408、`crates/context/src/lib.rs`（`last_tokenizer` :76/:231/:254、build 时记录 :472、两处计数 :479/:639）、`config.example.toml:27-30` | token 4 例：`token_family_parses_supported_specs`（六种合法写法 + 三类非法）、`declared_family_overrides_id_inference`（claude 声明 o200k、gpt-4o 声明 cl100k、非法串回落）、`declared_heuristic_ratio_changes_counting`（同 12 字：比例 4 → 3 token、比例 1.5 → 8 token；空文本 0）、`declared_family_applies_to_context_counting`（上下文计数随比例变化，且与「角色开销 4 + 文本 + priming 3」逐项一致）；config `tokenizer_declaration_reaches_runtime_model`（声明透传到 `Model`，未声明为 `None`）；`cargo test -p agent-context -p agent-config` 全绿 |

### 34.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused（`Model`/`ModelProfile` 新增字段的 13 处构造点已同步） |
| `cargo clippy --workspace --all-targets` | 清零（顺带删除失去调用方的 `encoder_for`） |
| `cargo test --workspace --all-targets --no-fail-fast` | **2,841 例通过**（58 套件 ok）；唯一失败仍为 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`，与本轮无关） |

### 34.3 剩余与下一轮

- H26 已闭合。**未做**：按 provider 家族**自动**选 tokenizer（omp 由 catalog 的 `tokenizer` 字段驱动）——Gyre 的 H25 目录没有上游分词器数据，凭空填比例会引入更难查的偏差；当前策略是「id 推断 + 用户显式声明」，已在 `config.example.toml` 写明。
- 下一轮候选（🟡 尾项，按正确性收益排序）：**H28**（异步唤醒护栏 / mid-run nudge）、**H29**（目标持久化 + `goal_updated` 事件）、**H41/H43**（名册持久化 / `unwatch` / 跨会话回执）、**H45**（`PtyShell` 持久会话）、**H23**（配置热重载与未知键告警）。

## 三十五、修复轮次（2026-09-11 · 第二十八轮 · H28 尾项：异步唤醒护栏 + 中途对账 nudge）

### 35.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H28（尾项） | ✅ | **补齐 omp `TodoTracker` 的两个门控**（§二十二 落地了 eager prelude 与完成提醒；尾项是「什么时候**不该**提醒」与「中途提醒」）：①**异步唤醒护栏**（移植 omp `hasPendingAsyncWake`）：新增端口 `agent_core::AsyncWakeProbe { has_pending_async() }`，并直接为 `AsyncJobManager` 实现（`running_jobs(None)` 非空 **或** `has_pending_deliveries(None)`）——停止边界注入完成提醒前先探测，**有后台作业在途时保持静默**（它们完成会经投递 sink / hub 唤醒循环，此时再催一轮会打断正常的「等结果 → 继续」节奏）；`AgentBuilder::async_wake(probe)` + 三前端注入各自进程级作业管理器（CLI main / RPC / server）；②**中途对账 nudge**（移植 omp `takeMidRunNudge`）：`MUTATING_TOOLS` 集合（`write_file`/`run_command`/`run_pty_command`/`ast_rewrite`/`replace_block`/`apply_hashline`/`eval`/`ssh`），在**工具批执行之后、下一次模型调用之前**统计——本轮触碰 `todo` 则清零，否则累加变更类调用数；累计 ≥12 且清单仍有未完成项时注入一次对账提醒（`<system-reminder>` 列出未完成项，提示标记完成/补充新项），每次 run **上限 2 次**（阈值/上限为引擎常量，与 omp 一致）；③**配置面**：`[todo] mid_run_nudge`（默认 `true`，可关闭）；`config.example.toml` 注明阈值语义（连续 12 次变更、每次 run 上限 2）。 | `crates/core/src/jobs.rs`（`AsyncWakeProbe` :1127、`impl … for AsyncJobManager` :1132）、`crates/core/src/lib.rs`（导出）、`crates/agent/src/lib.rs`（`TodoLoopConfig.mid_run_nudge` :89、`from_config` :110、Agent/Builder 的 `async_wake` :316/:540 + builder 方法）、`crates/agent/src/engine.rs`（`MUTATING_TOOLS` :44、阈值 :55、`mid_run_nudge_text` :60、探测捕获 :218、计数状态 :432、护栏判定 :1686、本轮统计 :1868-1871、批后注入 :2160-）、三前端装配（`main.rs`/`rpc.rs`/`server/lib.rs` 的 `async_wake`）、`crates/config/src/config.rs`（`TodoConfig.mid_run_nudge`）、`config.example.toml` | agent 新增 2 例：`todo_reminder_skipped_while_async_jobs_in_flight`（探针报告在途 → 0 次提醒且不续跑；无在途 → 1 次提醒并续跑）、`mid_run_nudge_fires_after_mutating_tools_without_todo_touch`（同一轮 12 次 `write_file` 且未触碰 todo → 恰 1 次中途提醒并继续下一轮；`mid_run_nudge=false` → 0 次）；既有 5 例 todo 循环用例保持绿 |

### 35.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | **2,843 例通过**（58 套件 ok）；唯一失败仍为 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`，与本轮无关） |

### 35.3 剩余与下一轮

- H28 已闭合。阈值（12）与上限（2）按 omp 取值写为引擎常量；若需要按项目调整可再提为配置项（当前刻意不暴露，避免把内部启发式变成用户契约）。
- 下一轮候选（🟡 尾项）：**H29**（目标持久化 + `goal_updated` 事件）、**H41/H43**（名册持久化 / `unwatch` / 跨会话回执）、**H45**（`PtyShell` 持久会话）、**H23**（配置热重载与未知键告警）。

## 三十六、修复轮次（2026-09-11 · 第二十九轮 · H29 尾项：目标持久化 + `goal_updated` 事件）

### 36.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H29（尾项） | ✅ | **目标跨会话持久化 + 客户端可见的状态变更事件**（§二十二 落地了状态机、工具、续跑与三类提示；尾项是「重启后目标丢了」与「客户端只能从文案猜状态」）：①**快照与落盘**（`crates/agent/src/goal.rs`）：`GoalSnapshot { objective, status, token_budget, continuations, max_continuations, updated_at_unix }`（serde，只存身份与状态，不存进程内用量记账）；`goal_file(cwd)` = `<cwd>/.gyre/goal.json`（与 `todo.json` 同目录约定）；`save_goal` 原子写（同目录临时文件 + `rename`，失败仅告警不影响本轮）；目标被放弃时**删除文件**（下次启动不复活已 drop 的目标）；`load_goal` 对缺失/损坏文件返回 `None`（不阻断启动）。②**恢复语义**（`GoalState::restore`）：`active` **恢复为 `paused`** —— 重启不自动续跑（对齐 omp `onThreadResumed` 的保守语义，自动继续一个可能过期的目标比让用户显式 resume 更危险）；`budget-limited`/`complete`/`dropped` 原样恢复，预算与续跑进度保留；③**写入点**：`goal` 工具每次成功变更后落盘（cwd 取自 `ToolContext.workspace`）、引擎在**续跑**与**中断转 paused** 两处落盘，并在停止边界做状态变化检测；④**`goal_updated` 结构化事件**：新增 `SessionEvent::GoalUpdated { status, objective, continuations }`（wire `type = "goal_updated"`）+ `to_status()` 展示投影；引擎在停止边界比对 `last_goal_status`，变化时**双通道成对发射**（`Session(event)` + 由事件派生的 `Say`），并追加一条 `<system-reminder>` 让模型按新状态收尾/继续——工具驱动的变更（complete/pause 等）在同一停止边界一并捕获。RPC（`kind: "session"`）与 server WS（`type: "session"`）**泛型转发**新增变体即可达客户端；ACP 侧展示文本经配对的 `Say` 进入 `agent_message_chunk`（结构化事件无 ACP 对应 update 类型，与既有 Session 帧策略一致）。⑤**三前端启动恢复**：CLI main / RPC / server 构造 `GoalState` 时 `load_goal` 并 `restore`，CLI 额外打印一行恢复摘要（`active` 恢复时提示需 `goal resume`）；`.gitignore` 增 `**/.gyre/goal.json`；引擎测试改用临时工作区（不把 `.gyre/` 落到仓库目录）。 | `crates/agent/src/goal.rs`（`GoalSnapshot` :30、`goal_file` :55、`save_goal` :63、`load_goal` :89、`snapshot` :304、`restore` :323、工具落盘 :667、测试 :870）、`crates/core/src/session_event.rs`（`GoalUpdated` :157、`to_status` 分支 :216）、`crates/agent/src/engine.rs`（`last_goal_status` :435、状态变化检测与双通道发射 :1655-、续跑落盘 :1699、中断落盘 :458）、`crates/cli/src/main.rs:1234`、`crates/cli/src/rpc.rs:2304`、`crates/server/src/lib.rs:1278`、`.gitignore:30` | agent 新增/加强 2 例：`goal_snapshot_roundtrip_and_restore_pauses_active`（create+续跑 → 落盘字段保真；恢复后 `active→paused` 且预算/续跑数保留；`complete` 原样恢复；drop 后文件被删；损坏 JSON 返回 `None`）、`goal_complete_via_tool_stops_continuation`（新增强断言：发出 `goal_updated{status:"complete", objective:"做完即停"}`，且 `目标已更新` 的展示文本必须由结构化事件派生）、`goal_tool_ops_roundtrip`（新增：`create` 后文件落盘且字段正确）；`cargo test -p agent --lib` 144 例全绿 |

### 36.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | 全量 **3,704 passed**（57 套件 ok）；失败仅 vendor `pi-shell`：隔离运行 `-p pi-shell --lib` 两次均为 **862 passed / 1 failed**（`getpgid(0)` 环境性），全量并行高负载下其兄弟用例 `kill_builtin_signals_*` 偶发（与本轮无关）；本轮无新增失败 |
| 卫生 | `git status` 无 `.gyre/` 残留（测试已改用临时工作区，`.gitignore` 补 `**/.gyre/goal.json`） |

### 36.3 剩余与下一轮

- H29 已闭合。**未做**：guided goal interview（`guided-goal-interview.md` 的交互式目标澄清）与 `goal-todo-context` 提示——它们依赖 omp 的交互式 UI 流程，Gyre 侧以「用户在对话里给目标 + `goal` 工具创建」替代。
- 下一轮候选（🟡 尾项）：**H41/H43**（名册持久化 / `unwatch` / 跨会话回执）、**H45**（`PtyShell` 持久会话）、**H23**（配置热重载与未知键告警）、**H27**（job 生命周期关闭点接线）。

## 三十七、修复轮次（2026-09-11 · 第三十轮 · H41 + H43 尾项：容量预拒与回执撤销）

### 37.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H41（尾项） | ✅ | **`at_capacity` 注册前预拒 + 明确定案「名册不落盘」**（§二十一 落地了名册/标签/状态/park/revive；尾项是「注册表可无限增长」与「持久化名册」）：①**容量**：`Hub::set_capacity` / `capacity` / `at_capacity`（0 = 不限），`Hub::try_register_handle` 在满员时返回 `HubError::AtCapacity { limit }` 且**不创建收件箱**（`register`/`register_handle` 保持不限，供宿主内部与测试用）；重名注册视为**替换**不占新名额；注销后腾出名额。②**派生前预拒**：`TaskTool::execute` 在拆分单/多任务之前检查 `hub.at_capacity()`，满员时直接返回可执行错误（提示「等现有子代理结束或调大 `[subagent] max_registry`」），`register_in_hub` 另用 `try_register_handle` 兜底（失败仅告警不中断委派）。③**配置与装配**：新增 `[subagent] max_registry`（0 = 不限，**不含父 agent**），三前端 `hub.set_capacity(max_registry + 1)`；`config.example.toml` 有说明。④**名册不落盘的定案**：`Hub` 文档写明理由——在册 id 描述的是**进程内**可寻址关系（`main` 是当前前端进程、`task-<n>` 是运行中的子代理），跨进程恢复只会得到永远收不到消息的幽灵条目；需要「跨重启可见的代理历史」应由宿主另行记录，而不是让总线假装代理还在。 | `crates/core/src/hub.rs`（`HubError::AtCapacity` :35、`Hub` 文档「为什么名册不落盘」:137、`set_capacity` :224、`capacity` :231、`at_capacity` :237、`try_register_handle` :251、`contains` :267、测试 :720）、`crates/agent/src/task_tool.rs`（`try_register_handle` :258、预拒 :643）、`crates/config/src/config.rs:1527`（`max_registry`）、`crates/cli/src/main.rs:906`、`crates/cli/src/rpc.rs:2139`、`crates/server/src/lib.rs:1032`、`config.example.toml` | hub `capacity_pre_rejects_registration_and_allows_replacement`（2/2 满员→新 id 预拒且名册长度不变、重名替换不占名额、注销腾位、0 = 不限复位） |
| H43（尾项） | ✅ | **`unwatch` 撤销 + 回执可观测 + 重复确认不静默**（§二十一 落地了 `send_tracked`/`ack`/`wait_ack`；尾项是「发送方无法放弃等待（只能等到超时）」与「回执状态不可观测」）：①**三态回执**：内部 `AckState { Pending, Acked, Revoked }`（`watch<AckState>`），`ack()` 只接受 `Pending`——**重复确认返回 false**（不静默成功），已撤销的回执不可再确认；②**`revoke_ack`**（= `unwatch`）：移出表项并置 `Revoked`，**正在 `wait_ack` 的等待者立即收到 `AckError::Revoked`**（新错误变体，不再白等到超时）；③**`pending_acks`**：只统计**未兑现**回执（已确认但尚未被 `wait_ack` 消费的不计），作为可观测性口径；④**工具面**：新增 op `acks`（查看未兑现数）/`ack <ack_id>`（确认）/`unwatch <ack_id>`（撤销，重复撤销与未知 id 报错）与 `send tracked=true`（签发回执并回显 `ack_id`），工具描述同步；⑤**跨会话回执投递同样定为进程内**：回执与消息共享同一生命周期（进程结束即消失），不为「resume 后补投」引入跨进程持久化队列——与 H41 的名册定案一致。 | `crates/core/src/hub.rs`（`AckState` :115、`AckError::Revoked` :110、`revoke_ack` :501、`pending_acks` :519、`wait_ack` 三分支、`ack` 重复语义、测试 :765）、`crates/tools/src/hub_tool.rs`（描述 :201、schema :227、`tracked` 发送 :313、`ack` :422、`unwatch` :435、`acks` :448、测试 :1745） | hub `revoke_ack_wakes_waiter_and_prunes_registry`（等待中被撤销→立即 `Revoked`、`pending_acks` 归零、撤销后 `ack` 失败、重复撤销 false、未确认则 `Timeout` 且表项保留可再撤）；tools `tracked_send_ack_and_unwatch_ops`（默认不签发回执、`tracked=true` 回显 `ack_id`、`acks` 计数、`ack` 成功且重复失败、`unwatch` 成功且重复失败、缺参报错） |

### 37.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | 全量 **3,709 passed**（58 套件 ok）；失败仅 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`，与本轮无关） |
| 顺带修复的测试隔离缺陷 | `agent-i18n::tests::init_sets_active_locale` 曾在本轮全量并行下偶发失败（进程级 locale 被另一测试改成 `ja`）：已用进程内 `Mutex` 串行化所有 `init(...)` 测试（`crates/i18n/src/lib.rs:213`），隔离运行 3/3 通过、全量通过 |
| 生成物 | `cargo xtask docs --tools` 已重生成（hub 工具描述含新 op），`release-check` 五项全过 |

### 37.3 剩余与下一轮

- H41/H43 已闭合（名册持久化与跨会话回执两项均以「进程内可寻址关系」定案并写明理由）。
- 下一轮候选（🟡 尾项）：**H45**（`PtyShell` 持久会话）、**H23**（配置热重载与未知键告警）、**H27**（job 生命周期关闭点接线）、**H24**（compat/auth/modelOverrides）、**H32/H33**（工具契约复核）、**H44**（provider 发现广度）、以及 H3/H7/H8/H18/H20/H35/H37/H38/H42。

## 三十八、修复轮次（2026-09-11 · 第三十一轮 · H27 尾项：watch 释放与投递恢复）

### 38.1 已修复项与证据

| # | 状态 | 修复内容 | 证据 | 回归测试 |
|---|---|---|---|---|
| H27（尾项） | ✅ | **修掉一处真实的结果静默丢失**（§十七 已接会话关闭路径的 `dispose`；尾项是 `unwatch_jobs` / `resume_deliveries` 无生产调用）：`wait ids` 会 `watch_jobs` 抑制自动投递，而 `enqueue_delivery` 在抑制状态下**直接丢弃不入队**——此前的超时/取消返回路径**不释放 watch**，于是「watch 期间完结的作业结果」永久丢失（模型既没拿到 `wait` 结果，也不会收到异步通知）。修法：①`wait_jobs` 拆成「薄包装 + `wait_jobs_inner`」，**所有返回路径**（取到结果 / 全部已消费 / 超时 / 取消）统一执行 `unwatch_jobs`（解除抑制，之后完结的作业正常入队）**+ `resume_deliveries`**（把已完结但被抑制的结果重新入队），并把超时提示改成「watch 已释放——作业完成后结果会以异步通知投递（也可再次 `wait ids` 取回，先到者消费、不会重复）」；双通道去重由既有 `consume_job_results`/`is_job_result_consumed` 保证（投递成功即标记已消费，后到的 `wait ids` 报「均已消费」）。②**会话/Agent 重建时恢复投递**：新增 `AsyncJobManager::suppressed_job_ids(owner)`，三前端在 `register_delivery_sink` 之后调用 `resume_deliveries`——否则「重建期间没有活 sink」的投递会死信、被抑制的投递会一直躺在抑制集合里。③**容量前置拒绝**已由 `register` 在**创建作业行之前**完成（`running_count >= max_running_jobs` → 明确错误），shell 工具把它转成 `ToolError::Execution` 交给模型，因此无需再在调用方做一次预检（`at_capacity()` 作为探测 API 保留）。 | `crates/core/src/jobs.rs`（`suppressed_job_ids` :616、`register` 容量拒绝 :333）、`crates/tools/src/hub_tool.rs`（`wait_jobs` 包装 + 释放 :576-593、`wait_jobs_inner` :596、超时提示 :688、测试 :1769）、`crates/cli/src/main.rs:1114`、`crates/cli/src/rpc.rs:2217`、`crates/server/src/lib.rs:1222` | tools `wait_ids_timeout_releases_watch_and_requeues_result`：注册一个「等信号才完成」的作业 → `wait ids` 超时返回并提示已释放 → 断言抑制集合为空 → 放行作业完成 → 结果**确实投递到 sink**（1 秒内）且被标记 `consumed`（修复前该断言失败：结果既不投递也不可观测）；既有 `wait_ids_returns_result_once_and_suppresses_delivery` 仍绿（watch 期间的抑制语义未变） |

### 38.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | 全量 **3,710 passed**（58 套件 ok）；失败仅 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`，与本轮无关） |

### 38.3 剩余与下一轮

- H27 已闭合（`unwatch_jobs`/`resume_deliveries` 接入生产路径；`at_capacity` 由 `register` 前置拒绝）。
- 下一轮候选（🟡 尾项）：**H45**（`PtyShell` 持久会话）、**H23**（配置热重载与未知键告警）、**H24**（compat/auth/modelOverrides）、**H32/H33**（工具契约复核）、**H44**（provider 发现广度）、**H20**（decidable session hooks）、**H18**（worktree 隔离等）、**H42**（SDK 收敛）、**H35/H37/H38**（发布/测试/文档尾项）、**H3/H7/H8**（协议与按键层）。

## 三十九、修复轮次（2026-09-11 · 第三十二轮 · H45 尾项：`PtyShell` 持久会话接入）

### 39.1 修复

H45 的最后一项是「`PtyShell` 零生产调用」：§二十 已把 `run_pty_command`（**一次性** PTY）接进 `run_command(pty: true)`，
但 `PtyShell`（**持久** shell：跨命令保持 cwd / 环境变量）自 §9 引入后没有任何生产调用点。
本轮把它接成 **`shell_session` 工具**，并在此过程中修掉两个只有在真实接线后才暴露的执行正确性缺陷。

| 项 | 状态 | 修复内容 | 证据（双侧 file:line） | 测试 |
|---|---|---|---|---|
| H45（尾项） | ✅ | ①**新增 `shell_session` 工具**（`op = run/status/close`）：按工作区根**惰性**启动持久 `PtyShell`；`cd`/`export` 等状态在后续 `run` 中继续生效（omp `bash-executor.ts` 用 `PtyShell` 作持久执行后端的语义）；`status` 报告「未启动 / 存活 / 失效」，`close` 显式回收；capability = `Execute`（默认需审批），未知 `op` 与缺 `command` 都返回 `InvalidArgs` 而非静默执行。②**会话生命周期硬化**：单命令墙钟超时（默认 `PTY_RUN_TIMEOUT` = 1 min）→ kill shell + `poisoned` 标记（fail-closed，**不复用**可能仍在阻塞读上的会话）；`is_poisoned()` + `ensure_session()` 实现**自愈**（失效会话在下次 `run` 时自动重建）；`Drop` 里 `kill + wait` 回收子进程（不留僵尸）。③**修掉两个真实缺陷**（见下「缺陷 A/B」）。④**装配**：CLI/REPL 与 JSON-RPC 共用的 `assemble_builtin_tools` 在 `pty` 组里注册两个工具；**server 此前完全没有 pty 工具**（PTY 能力只能经 `run_command(pty: true)` 触达），本轮补上受 `[tools].enabled.pty` 控制的可选组与对应提示词注入；xtask 文档生成器同步（`docs/tools.md` → 34 个工具）。 | `crates/pty/src/tool.rs`（`ShellSessionTool` :118、`ensure_session`/自愈 :131、`name`/`schema`/`capability` :149/:157/:170）、`crates/pty/src/session.rs`（`spawn` :192、`is_poisoned` :248、`run_with_timeout` :259、超时置失效 :293、未命中 marker 置失效 :307、`reap_exit_code` :319、`read_until_marker` :338、行首锚定 :359）、`crates/pty/src/lib.rs`（`PROMPT_SECTION` 含两个工具的用法）、`crates/cli/src/main.rs:2610-2614`（CLI/RPC 注册）、`crates/server/src/lib.rs:1270` 起（server 注册）与 `:1383` 起（`<pty>` 提示词注入）、`crates/xtask/src/docs.rs:93-94`、`docs/tools.md` | pty：`shell_session_persists_cwd_across_runs`（`cd` 到临时子目录后第二条命令的 `pwd` 仍在子目录、且不回到工作区根）、`shell_session_status_close_and_exit_code`（未启动 → `exit 3` 透出**真实** `[exit 3]` → 状态失效 → `close` 回收 → 未启动；未知 op / 缺参报错）、`shell_session_timeout_poisons_then_self_heals`（超时杀会话 + 置失效 + 下次 `run` 自动重建）、`shell_exit_reports_child_code_and_poisons`（session 层：`exit 7` → `Some(7)` + 失效 + 复用报错，**不死锁**）；cli：`assemble_enables_pty_group_with_persistent_session`、`assemble_defaults_to_core_only`（默认两个 pty 工具都不注册）；sdk：`optional_prompts_follow_enabled_state`（`<pty>` 指引随开关出现/消失，且含 `shell_session`） |

**缺陷 A：回显被误判为完成 marker（导致持久会话「只回显、无输出、退出码 None」）。**
`PtyShell` 用「命令后追加 `printf '\n<marker>%d' $?`，读到 marker 即完成」的协议。原实现按**子串**搜索 marker，
而持久 shell 的命令行本身会被终端回显，回显内容里含 `printf '\n<marker>...`，于是「回显」被当成「完成」——
`run` 在命令真正执行**前**就返回，表现为输出只有回显、`exit_code` 恒为 `None`（工具层显示 `-1`）。
初始化里的 `stty -echo` 在 bash/readline 下**不保证**关闭回显（readline 会重置终端模式），因此不能依赖它。
修法：marker 匹配改为**行首锚定**（搜索 `\n` + marker，见 `session.rs:359-372`）——真正的 marker 前面必有 `printf` 输出的换行，
而回显行里的 marker 前缀前是空格。

**缺陷 B：`exit N` 结束 shell 时退出码丢失（退化为 `-1`）。**
`exit N` 会终止 shell，`printf` 永远不会执行 → 既没有 marker，也没有换行；旧的 `read_until_marker` 读到 EOF 后返回 `exit = None`，
工具层把它显示成 `-1`，调用方无法区分「命令失败」与「shell 已退出」。
修法：`read_until_marker` 返回 `(正文, 退出码, 是否命中 marker)`（`session.rs:338`）；未命中时
①用 `reap_exit_code()`（有界重试 `try_wait`，`session.rs:319`）从子进程回收**真实**退出码；
②置 `poisoned` —— 无论 EOF（shell 死）还是输出超过 `PTY_MAX_OUTPUT` 上限（shell 活着但流已失步，残余输出会被下一条命令误读），
都必须让会话失效（fail-closed，`session.rs:307`）。

**设计取舍（记录，避免被当成缺陷）**：`PtyShell` 没有做成 `run_command(session: true)` 之类的开关，而是独立工具 `shell_session`。
理由：`run_command` 的契约是**无状态**执行（每条命令独立进程 + 独立超时 + 可 `async`），把持久会话塞进同一工具会让
「超时/取消/审批/输出截断」的语义在两个模式间分叉；`shell_session` 把「同一个 shell 里连续执行」显式暴露给模型，
生命周期（惰性启动 / 超时置失效 / 显式 close）也能独立演进。两者与 `run_command` 形成互补：一次性管道执行 / 一次性 TTY 执行 / 持久 TTY 会话。

### 39.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零（顺带修掉新写的 `clippy::op_ref`） |
| `cargo test --workspace --all-targets --no-fail-fast` | **3,715 passed / 1 failed / 59 套件（58 ok）**；唯一失败仍是 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)` 受限，与本轮无关）。相对上一轮基线 **+5 例**：pty +4（3 个工具用例 + 1 个 session 退出码用例）、cli +1（pty 组注册） |
| `cargo run -p xtask -- release-check` | 五项全过：版本一致 0.1.0 / CHANGELOG `[Unreleased]` / 三语 README / NOTICES（725 包 · 388 许可）/ `docs/tools.md` 与工具面一致（**34** 个工具） |
| 覆盖说明 | server 侧的 pty 组注册是 `build_agent` 内联块（需完整 `Config`/MCP/审批桩才可构造），**无专属用例**：其正确性由「同一工具集契约」的 CLI 用例（`assemble_enables_pty_group_with_persistent_session`）+ 工具实现自身的 pty 用例 + `cargo check`/`clippy` 覆盖；且该块受 `[tools].enabled.pty`（默认 `false`）约束，默认路径的注册表与提示词不变 |

### 39.3 剩余与下一轮

- H45 已完全闭合；§5.1 的 🟡 剩 14 项。
- 下一轮候选（🟡 尾项，按正确性收益排序）：**H23**（配置热重载与未知键告警）、**H24**（compat/auth/modelOverrides）、**H32/H33**（工具契约复核）、**H44**（provider 发现广度）、**H20**（decidable session hooks）、**H18**（worktree 隔离/预算/输出契约）、**H42**（SDK 统一装配——server 的 hashline/ssh/browser/debug 可选组仍缺）、**H35/H37/H38**（发布/测试/文档尾项）、**H3/H7/H8**（raw 按键层 / 未支持 RPC 命令 / 子代理·扩展 UI 帧）。

## 四十、修复轮次（2026-09-11 · 第三十三轮 · H44：外来配置发现广度 + 内置语言规则）

### 40.1 修复

H44 的两条尾项：**provider 发现广度**（Gemini/OpenCode/Windsurf/Codex 指令继承）与**内置语言规则**
（omp 的 27 条 Go/Rust/TS 约定，随二进制分发、最低优先级）。两者都属「开箱即得」类能力：
用户既有的其他工具配置与 omp 语言约定无需迁移即可生效。

| 项 | 状态 | 修复内容 | 证据（双侧 file:line） | 测试 |
|---|---|---|---|---|
| H44 | ✅ | ①**发现面 4 → 8 源**：新增 `~/.codex/AGENTS.md`（Codex 用户级；项目级 AGENTS.md 已由通用源覆盖）、`GEMINI.md`/`.gemini/GEMINI.md`（walkup）+ `~/.gemini/GEMINI.md`、`~/.config/opencode/AGENTS.md`（OpenCode 用户级）、`.windsurf/rules/*.md`（walkup，frontmatter 解析）+ `~/.codeium/windsurf/memories/global_rules.md` + 遗留 `.windsurfrules`。②**顺序与去重语义显式化**：`discover` 现按 `Source::priority` **升序**（通用 → 具体，越具体越靠后注入）稳定排序再做「来源+路径」去重——此前文档声称已排序但代码只是按硬编码顺序拼接。③**可测性**：新增 `discover_with_home(cwd, home)`，用户级扫描不再依赖进程级 `HOME`（并行测试安全）。④**内置规则集**：`crates/ttsr/rules/*.md` 内嵌 27 条规则（Go 8 / Rust 6 / TypeScript 13，全部 `interruptMode: never`：命中只折叠为工具结果前导提醒、**不打断模型流**），`builtin_rules()` 解析并做**工具名映射**（omp `tool:edit` → Gyre `tool:apply_hashline`、`tool:write` → `tool:write_file`），`load_rules()` 以「内置在前、同名项目规则替换」装配；`[ttsr] builtin_rules = false` 关闭整套、`disabled_rules` 逐条剔除（对内置同样生效）。⑤**三前端统一**：cli 主路径 / rpc / server 全部改用 `load_rules`，加载提示改为「内置 N 条 + 项目 M 条（项目名列表）」而不再刷屏 27 条。⑥**移植中修掉两个真实解析缺陷**（见下「缺陷 A/B」） | `crates/discovery/src/lib.rs`（`Source` :29、`discover` :97、`discover_with_home` :105、`discover_codex` :178、`discover_gemini` :190、`discover_opencode` :210、`discover_windsurf` :223、`parse_rule_markdown` :375）、`crates/ttsr/src/builtin.rs`（`BUILTIN_RULE_SOURCES`、`builtin_rules` :100、`map_tool_name` :113）、`crates/ttsr/rules/*.md`（27 条，MIT 归属见模块文档）、`crates/ttsr/src/lib.rs`（`load_rules` :44）、`crates/ttsr/src/rule.rs`（`split_scope_tokens` :290）、`crates/ttsr/src/frontmatter.rs`（`unescape_double_quoted` :164）、`crates/config/src/config.rs`（`TtsrConfig.builtin_rules` :1575）、`crates/cli/src/main.rs:1419-1429` + `report_ttsr_rules` :2639、`crates/cli/src/rpc.rs:2450`、`crates/server/src/lib.rs:1555`、`config.example.toml`（`[ttsr]` 段） | discovery：`discovers_gemini_user_and_project`（用户级+项目级、无 home 时用户级不可见）、`discovers_codex_and_opencode_user_levels`、`discovers_windsurf_rules_and_legacy`（frontmatter globs + global_rules + 遗留文件）、`discover_sorted_by_priority`（优先级单调 + 首尾来源）；ttsr：`all_builtin_rules_parse_and_are_non_interrupting`（27 条全部可解析、条数、名字唯一、全为 never）、`tool_names_map_to_gyre_tools`（映射后工具名全部落在真实工具面）、`scope_globs_survive_mapping`、`parses_comma_separated_scope_scalar`、`split_scope_tokens_drops_blanks`、`double_quoted_scalars_unescape_yaml_escapes`、`load_rules_merges_builtin_and_project_with_name_override`（同名覆盖不增条目、`builtin_rules=false` 只剩项目规则）、`disabled_rules_filter_builtins_through_coordinator`（启用 → `Reminders`；禁用该条 → `None`；关整套 → `None`）；config：`ttsr_builtin_rules_defaults_and_parse` |

**缺陷 A：逗号分隔的 `scope` 标量被当成单个 token（glob 被吞、静默失效或解析失败）。**
omp 的内置规则写成 `scope: "tool:edit(*.rs), tool:write(*.rs)"`（YAML 标量 + 逗号分隔），而 Gyre 的
frontmatter 解析把整串当作**一个** scope token：`RuleScope::parse` 找到第一个 `(` 后一路取到最后一个 `)`，
glob 变成 `*.rs), tool:write(*.rs` —— 19 条规则「解析成功但作用域全错」（静默失效），8 条因 glob 非法直接失败。
修法：`split_scope_tokens()` 按**括号深度 0** 的逗号切分（`rule.rs:290`），因此 glob 里的 `{ts,tsx}` 不会被误切。

**缺陷 B：YAML 双引号标量未解转义（正则里的 `\(` 编译失败）。**
规则文件写 `condition: "import\\("`（YAML 双引号语义：`\\` → `\`，得到正则 `import\(`），而 Gyre 的 `unquote`
只剥引号不解转义，正则拿到 `\\(` 直接编译失败（8 条规则被跳过）。
修法：`unescape_double_quoted()`（`frontmatter.rs:164`）实现 YAML 双引号转义（`\\`/`\"`/`\n`/`\t`/`\r`/`\/`/`\0`），
**未知转义原样保留**（正则的 `\b`/`\d` 不受影响，宽松优先）；单引号按 YAML `''` → `'` 处理。

**设计取舍（记录）**：①内置规则以**最低优先级**参与装配而非硬编码进 prompt——命中才注入，零上下文成本，
且用户同名规则覆盖内置副本（与 omp `builtin-defaults` provider 一致）；②**不做「全部 27 条默认中断」**：
全部规则本身声明 `interruptMode: never`，Gyre 尊重该声明（折叠为工具结果提醒），因此默认启用不会造成
「编辑被拒/流被反复打断」的意外；③工具名映射只在**作用域名**上做，未识别名字保持原样——宁可不命中，
也不误映射到无关工具（有测试钉住映射后全部落在 Gyre 真实工具面）。

### 40.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | **3,728 passed / 1 failed / 59 套件（58 ok）**；唯一失败仍是 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`，与本轮无关）。相对上一轮 **+13 例**：discovery +4、ttsr +8、config +1 |
| `cargo run -p xtask -- release-check` | 五项全过：版本一致 0.1.0 / CHANGELOG `[Unreleased]` / 三语 README / NOTICES（725 包 · 388 许可）/ `docs/tools.md` 与工具面一致（34 个工具） |
| 许可归属 | 内置规则正文取自 oh-my-pi（MIT，Copyright © 2025 Mario Zechner / 2025-2026 Can Bölük / 2026 Stencil Labs, Inc.），来源路径与许可写在 `crates/ttsr/src/builtin.rs` 模块文档；`THIRD-PARTY-NOTICES.txt` 由 `cargo metadata` 生成（不含内嵌文本资源），故归属记在源码头 |

### 40.3 剩余与下一轮

- H44 已闭合；§5.1 的 🟡 剩 13 项：H3、H7、H8、H18、H20、H23、H24、H32、H33、H35、H37、H38、H42。
- 下一轮候选（按正确性收益排序）：**H23**（配置热重载 + unknown key 告警 + 逐键内省）、**H24**（compat 全量表 / auth 段 / modelOverrides）、**H42**（SDK 统一装配：server 的 hashline/ssh/browser/debug 可选组）、**H32/H33**（工具契约别名复核）、**H20**（decidable session hooks）、**H18**（worktree 隔离/预算/输出契约）、**H35/H37/H38**（发布/测试/文档尾项）、**H3/H7/H8**（raw 按键层 / 未支持 RPC 命令 / 子代理·扩展 UI 帧）。

## 四十一、修复轮次（2026-09-11 · 第三十四轮 · H23 尾项：未知键诊断 + 逐键内省）

### 41.1 修复

H23 的三条尾项里，本轮落地**未知键告警**与**逐键内省**（热更新留待后续）。
关键设计：**不给配置结构体加 `Serialize`、不改加载语义**——schema 由一份覆盖全部配置字段的
TOML 样例（`SCHEMA_FIXTURE`）解析成 `toml::Value` 得到，未知键只告警、仍被忽略。

| 项 | 状态 | 修复内容 | 证据（双侧 file:line） | 测试 |
|---|---|---|---|---|
| H23（未知键诊断） | ✅（子项） | ①新增 `agent_config::schema`：`SCHEMA_FIXTURE`（覆盖全部字段的样例，含数组表元素与 map 占位）+ `unknown_keys()`（递归比对，**只报 schema 里没有的键**）+ `suggest()`（同层编辑距离 ≤ 2 的「是否想写 X？」）。②**绝不误报**：`OPEN_PATHS`（`models_roles`/`keybindings` 的 `flatten` 映射、`mcp.servers` 的 untagged stdio/http 联合、`tools.enabled`/`agent.tools.approval` 自由 map、`*.headers`）整体跳过；`KEY_ALIASES` 消化 `serde(alias)`（`transport` → `type`）。③`Config::load_with_warnings()`（新）与 `load()`（= 加载 + `tracing::warn!` 每条）；**未知键不改变加载结果**（有测试断言被忽略）。④CLI：`agent config check` 打印逐条告警（带建议），启动路径打印一行摘要 + 明细（stderr，不污染 stdout 管道）；i18n 新增 3 键 × 4 语言。⑤**漂移护栏**：两个单测直接扫 `config.rs` 源码，要求每个 `pub` 字段名与 `serde(rename)` 都出现在 fixture 中、每个 `serde` 别名都在 `KEY_ALIASES`/显式允许表里——新增配置字段时会立刻失败而不是静默漏诊 | `crates/config/src/schema.rs`（新，`SCHEMA_FIXTURE`、`OPEN_PATHS`、`KEY_ALIASES`、`unknown_keys`、`known_keys`、`children_of`、`levenshtein`）、`crates/config/src/config.rs`（`load_with_warnings` :126 起、`load` 包装）、`crates/config/src/lib.rs`（导出）、`crates/cli/src/manage.rs`（`config_keys`、`config_check_warnings`、`report_config_warnings`）、`crates/cli/src/main.rs`（`config check` 分支与启动提示）、`crates/i18n/locales/{en,zh,ru,ja}.json` | config：`fixture_parses_and_has_no_unknown_keys_of_its_own`、`schema_fixture_covers_all_fields`（源码扫描漂移护栏）、`schema_key_aliases_are_declared`、`detects_typo_with_suggestion`（`agent.max_turn` → `max_turns`）、`open_paths_arrays_and_aliases_do_not_warn`（开放子树/数组表/别名零误报）、`known_keys_and_children_shape`、`load_with_warnings_reports_unknown_keys`（含「未知键不生效」断言）；cli：`config_keys_lists_known_keys_and_filters_by_prefix`、`config_check_warnings_render_suggestion`、`route_config_shapes`（keys 三形态） |
| H23（逐键内省） | ✅（子项） | `agent config keys [<prefix>]`：列出 schema 中的已知键与形态（`table`/`scalar`/`array`/`array-of-tables`/`open-map`），支持前缀过滤（含前缀本身的逐层展示）；未知前缀给明确提示而非空输出。这是「逐键内省」的第一步：**路径 + 形态**已可查询（类型/默认值/文档仍缺，见 §41.3） | `crates/config/src/schema.rs`（`known_keys`/`children_of`/`key_info`）、`crates/cli/src/manage.rs`（`ManageCmd::ConfigKeys` + `config_keys`）、`crates/cli/src/main.rs` 分发、`manage.usage.config` 四语更新 | 同上 + 端到端实测（见 §41.2） |

### 41.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零（顺带修掉 `clippy::needless_lifetimes`） |
| `cargo test --workspace --all-targets --no-fail-fast` | **3,737 passed / 1 failed / 59 套件（58 ok）**；唯一失败仍是 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`）。相对上一轮 **+9 例**：config schema +6、config 加载 +1、cli +2 |
| `cargo run -p xtask -- release-check` | 五项全过 |
| 端到端（真实环境） | `agent config check` 在本机用户配置上**零误报**地报出 3 个真实失效键：`default_model.context_window_guard`、`models.context_window_guard`（`context_window_guard` 的正确位置是 `[agent]`，`config.example.toml:98`）与 `skills.enable_commands`（全仓无任何读取点）；`agent config keys agent.commands` 正确列出 12 条（含 `array-of-tables`/`open-map` 形态），`agent config keys nope` 给出明确提示。加载结果不受影响（`exit=0`） |

### 41.3 剩余与下一轮

- H23 仍是 🟡：**运行时热更新**（omp `Settings.reloadFromDisk` 语义：进程级句柄 + 原子换入 + 按 mtime 指纹判定重载）与**逐键元数据**（类型/默认值/文档，可顺带交付 H38 的 `docs/settings.md`）尚未落地。
- 下一轮候选（按正确性收益排序）：**H24**（compat 全量表 / auth 段 / modelOverrides）、**H42**（SDK 统一装配：server 的 hashline/ssh/browser/debug 可选组 + 三处装配收敛）、**H32/H33**（工具契约别名复核）、**H23 热更新 + 元数据**（可与 **H38** `docs/settings.md` 合并交付）、**H20**（decidable session hooks）、**H18**（worktree 隔离/预算/输出契约）、**H35/H37**（发布/测试尾项）、**H3/H7/H8**（raw 按键层 / 未支持 RPC 命令 / 子代理·扩展 UI 帧）。

## 四十二、修复轮次（2026-09-11 · 第三十五轮 · H42：三前端装配收敛到 `agent-sdk`）

### 42.1 修复

H42 的最后一块是**内置工具装配**的三处漂移：CLI 侧有 `assemble_builtin_tools`（rpc 复用），
server 直接调 `agent_tools::builtin_tools_with_pool_and_jobs` 并**无条件**注册 ast/image/lsp，
而 hashline/pty/debug/ssh/browser 与其提示词在 server 完全缺席；可选组开关的推导逻辑也在
cli 与 rpc 各写了一遍。

| 项 | 状态 | 修复内容 | 证据（双侧 file:line） | 测试 |
|---|---|---|---|---|
| H42 | ✅ | ①**新增 `agent_sdk::assembly`**：`OPTIONAL_TOOL_KEYS`/`is_known_optional_key`、`optional_tool_switches(cfg)`（唯一开关推导：`OPTIONAL_TOOL_PROMPTS` 默认 + `hashline` 默认 true + `pty`/`debug`/`ssh`/`browser` 默认 false，显式 `[tools].enabled.<key>` 覆盖）、`assemble_builtin_tools(...)`（唯一注册实现：核心工具 + 8 个可选组 + github，返回共享 `LspPool`）、`new_snapshot_store()`/`SnapshotStore`（`read_file` 记录与 `apply_hashline` 回放共享同一实例，调用方再经 `AgentBuilder::snapshot_store` 交给引擎）。②**CLI/REPL 与 rpc**：删除本地 `assemble_builtin_tools`、`OPTIONAL_TOOL_KEYS`、开关拼装与快照存储构造，改为 `pub use agent_sdk::{…}` + `agent_sdk::optional_tool_switches` + `agent_sdk::new_snapshot_store()`（`crate::assemble_builtin_tools` 调用点与既有测试不受影响）。③**server**：改为同一入口——`optional_tool_switches` → `assemble_builtin_tools`；**行为对齐**（此前 server 无条件开 ast/image/lsp、且没有 hashline/pty/debug/ssh/browser）；提示词也改为与注册同源（`optional_tool_sections(&optional, github_enabled)`），不再只注入 github 段；快照存储注入引擎（`assemble(...).snapshot_store(...)`）。④`agent_tools::builtin_tools_with_pool_and_jobs` 保留但加文档警示（无条件注册，不适合做前端入口），避免后来者再走老路 | `crates/sdk/src/assembly.rs`（新：`new_snapshot_store` :26、`OPTIONAL_TOOL_KEYS` :31、`optional_tool_switches` :48、`assemble_builtin_tools` :71 起）、`crates/sdk/src/lib.rs`（导出）、`crates/sdk/Cargo.toml`（+`agent-dap`、dev `toml`）、`crates/cli/src/main.rs`（再导出 + `optional_tool_switches` :1068、`new_snapshot_store` :1072）、`crates/cli/src/rpc.rs:2167/:2170`、`crates/server/src/lib.rs`（统一装配 :1267、快照存储 :1269、提示词同源 :1386、`assemble` 新 `snapshots` 形参 + `.snapshot_store` :1508）、`crates/tools/src/lib.rs:385`（文档警示） | sdk：`optional_tool_switches_defaults_and_overrides`（默认值 + 显式覆盖 + 白名单/github 排除）、`assemble_registers_only_enabled_groups`（默认：核心在、`apply_hashline` 在、`replace_block`/`lsp` 不在、`LspPool` 为 None；打开 ast/lsp/pty + github 后：四者齐备且池非空）；cli 既有 `assemble_defaults_to_core_only`/`assemble_enables_ast_group`/`assemble_enables_pty_group_with_persistent_session`/`assemble_github_independent_of_optional_map` 现在直接覆盖收敛后的实现 |

**行为变更（有意，需记录）**：server 侧 ast/lsp/image 由「无条件注册」改为「随 `[tools].enabled.<key>`」
（默认关，与 CLI/`docs/tools.md` 的「可选」语义一致）；需要它们的部署在配置里显式打开。
`lsp` 未启用时 `LspPool` 为空，且 `LspWriteEffect` 本身也受同一开关门控（`server/lib.rs` `assemble` 内 lsp writethrough 分支），
因此不存在「空池写效果」的悬空状态。

### 42.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零（顺带修掉 rpc 的 `unused_mut`） |
| `cargo test --workspace --all-targets --no-fail-fast` | **3,739 passed / 1 failed / 59 套件（58 ok）**；唯一失败仍是 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`）。相对上一轮 **+2 例**（sdk assembly 用例） |
| `cargo run -p xtask -- release-check` | 五项全过 |
| 覆盖说明 | server 的 `build_agent` 仍是内联装配块（需完整 `Config`/MCP/审批桩才可构造），**无专属用例**；其正确性由「同一实现」的 sdk 用例 + cli 既有装配用例 + `cargo check`/`clippy` 打底（与 §三十九 的 pty 注册同一策略） |

### 42.3 剩余与下一轮

- H42 已闭合；§5.1 的 🟡 剩 12 项：H3、H7、H8、H18、H20、H23、H24、H32、H33、H35、H37、H38。
- 下一轮候选（按正确性收益排序）：**H24**（compat 全量表 / auth 段 / modelOverrides——自定义 provider 互操作）、**H32/H33**（工具契约别名复核）、**H23 热更新 + 逐键元数据**（可与 **H38** `docs/settings.md` 合并交付）、**H20**（decidable session hooks）、**H18**（worktree 隔离/预算/输出契约）、**H35/H37**（发布/测试尾项）、**H3/H7/H8**（raw 按键层 / 未支持 RPC 命令 / 子代理·扩展 UI 帧）。

## 四十三、修复轮次（2026-09-11 · 第三十六轮 · H24：鉴权三态 + omp `compat` 映射）

### 43.1 修复

H24 的剩余面是「自定义 provider 键面」：`auth` 段、omp 风格 `compat` 表与 `modelOverrides`。
本轮落地 `auth` 与 `compat`（含真正落 wire 的 `maxTokensField`），并对 `modelOverrides`
与 compat 全表给出**明确的决策**（不再留作模糊尾项）。

| 项 | 状态 | 修复内容 | 证据（双侧 file:line） | 测试 |
|---|---|---|---|---|
| H24 | ✅ | ①**鉴权三态 `auth`**（`agent_core::AuthMode`：`api_key`（默认）/`none`/`oauth`，兼容 omp 的 `apiKey` 写法）：`auth = "none"` 时**不发送**内置鉴权头（本地 vLLM/Ollama/免鉴权网关），`oauth` 时**只用** OAuth 凭据存储（缺失即报错并提示 `agent auth login`，不再静默回退 env/auth.toml）；`ProviderCallContext::builtin_api_key()` 统一判定「`auth = none` 或 key 为空 → 不发头」，**六个适配器**（openai-completions / openai-responses / anthropic / gemini / glm / deepseek）与模型发现 `list_models` 全部改用它——顺带修掉「配了空 key 却发 `Authorization: Bearer `」的旧行为。解析入口 `resolve_profile_api_key()` 收敛到一处（CLI 主路径 / CLI `--model` 热切换 / RPC / server 四处共用）。②**omp 风格 `compat` 段**（`CompatConfig`，camelCase 形状）：`supportsUsageInStreaming=false`→`omit_stream_options`、`supportsToolChoice=false`→`omit_tool_choice`、`supportsReasoningEffort|supportsReasoningParams=false`→`omit_reasoning`、`maxTokensField`→`ProviderQuirks.max_tokens_field`；与 `quirks` **取并集**（`omit_*` 保守取真；`max_tokens_field` 显式 `quirks` 优先）。③`max_tokens_field` 真正落 wire：openai-completions 请求体按它选择 `max_tokens` / `max_completion_tokens`（默认保持既有 wire）。④**容错**：`compat` 里未实现的 omp 旗标**不报错**（否则既有 omp 配置直接加载失败），由 H23 未知键诊断在 `agent config check` 里点名提示。⑤`config.example.toml` 补三态鉴权 / compat / maxTokensField 注释 | `crates/core/src/llm.rs`（`AuthMode` :385、`MaxTokensField` :403、`ProviderQuirks.max_tokens_field` :440、`builtin_api_key` :480）、`crates/llm/src/oauth/mod.rs`（`resolve_oauth_only` :267、`fresh_or_refreshed` :279、`resolve_profile_api_key` :318）、`crates/llm/src/{openai,openai_responses,anthropic,gemini,glm,deepseek}.rs`（鉴权头按 `builtin_api_key()` 条件添加）、`crates/llm/src/model_discovery.rs`（空 key 不发头）、`crates/config/src/config.rs`（`ModelProfile.auth` :618 / `.compat` :624、`CompatConfig` :639、`to_quirks` :655、`effective_quirks` :681）、四处装配点（cli main / rpc / server）、`config.example.toml`、`crates/config/src/schema.rs`（fixture 补 `auth`/`compat`，漂移护栏支持 `rename_all = "camelCase"`） | core：`auth_mode_parses_omp_and_gyre_spellings`、`builtin_api_key_respects_auth_mode_and_empty_key`、`max_tokens_field_names`；config：`model_profile_auth_and_compat_mapping`（默认/三态解析/compat→quirks 映射/并集与优先级/**未实现旗标不报错**）；llm：`openai::tests::quirks_omit_parameters_on_demand` 扩展 `max_tokens_field` 切换 wire 字段名、`oauth::tests::profile_api_key_follows_auth_mode`（`none`→`None`、`api_key`→配置值、`oauth` 缺凭据→可操作错误） |

**决策记录（避免把「未实现」当 bug）**：
1. **不追 omp 的 104 位 `compat` 全表**。那些旗标对应 omp 九个 SDK 适配器的参数重命名/路径拼接策略；
   Gyre 的六个适配器没有那套机制，收录后只能是「接受但无效」——比不收录更危险。因此只映射
   **能真正生效**的子集，其余旗标照常接受（不破坏 omp 配置加载）并由 `agent config check` 点名。
2. **`modelOverrides` 不需要独立实现**：omp 的 provider 级 `modelOverrides` 是「一个 provider 带多模型时
   逐个打补丁」；Gyre 的模型面本来就是 `[[models]]` **一条一个 profile**，per-model 的 headers/quirks/
   compat/auth/extra_body/token 上限都已可直接写在对应条目上——语义等价，无需再引入一层覆盖表。

### 43.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | **3,744 passed / 1 failed / 59 套件（58 ok）**；唯一失败仍是 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`）。相对上一轮 **+5 例**：core +3、config +1、llm +1 |
| `cargo run -p xtask -- release-check` | 五项全过 |
| 行为变更 | ①空 API key 不再发 `Authorization: Bearer `（改为不发鉴权头）；②`auth = "oauth"` 缺失凭据从「静默回退」变为**启动期明确报错**（CLI/RPC/server 三处装配） |

### 43.3 剩余与下一轮

- H24 已闭合；§5.1 的 🟡 剩 11 项：H3、H7、H8、H18、H20、H23、H32、H33、H35、H37、H38。
- 下一轮候选（按正确性收益排序）：**H32/H33**（工具契约别名复核——纯契约面，收益直接）、**H23 热更新 + 逐键元数据**（可与 **H38** `docs/settings.md` 合并交付）、**H20**（decidable session hooks）、**H18**（worktree 隔离/预算/输出契约）、**H35/H37**（发布/测试尾项）、**H3/H7/H8**（raw 按键层 / 未支持 RPC 命令 / 子代理·扩展 UI 帧）。

## 四十四、修复轮次（2026-09-11 · 第三十七轮 · H32 尾项 + H33：记忆可编辑与 github op 面）

### 44.1 修复

H32 剩「`memory_edit` 的 `update`/`invalidate`、`github` 的 omp op 命名」，H33 剩「`read` 行限口径复核」。
本轮把这三块一起收掉：记忆从「只能查/删」变成「可改写/可失效」，github 工具接受 omp 的
`op` 字段与只读 op 名，`read` 的口径复核结论落到契约护栏测试。

| 项 | 状态 | 修复内容 | 证据（双侧 file:line） | 测试 |
|---|---|---|---|---|
| H32（`memory_edit`） | ✅（子项） | ①`MemoryStore` 新增两个**默认实现**（不破坏 local 等无稳定记录 id 的后端）：`update(id, content?, importance?)` 与 `invalidate(id, replacement_id?)`；②`StructuredMemoryStore` 实现就地改写与**失效标记**——失效写进既有 `metadata`（`superseded_by`/`superseded_at`），**不新增 schema**，且 `recall_in`/`search_in` 统一过滤失效记录（`is_superseded`）：失效 ≠ 删除，`stats` 仍计数、仍可追溯；③`memory_edit` 工具新增 `update`（`content`/`importance` 至少给一个，否则明确报错）与 `invalidate`（可选 `replacement_id`）两个 op，`id` 提取抽成 `required_id` 并让错误文案带上 op 名 | `crates/core/src/memory.rs`（trait 默认 `update` :136 / `invalidate` :152）、`crates/memory/src/structured.rs`（`is_superseded` :84、`update`/`update_in` :638/:651、`invalidate`/`invalidate_in` :677/:685、trait impl :997/:1012、recall/search 过滤）、`crates/tools/src/memory_tool.rs`（`required_id` :294、`op_update` :350、`op_invalidate` :386、schema/描述/dispatch） | tools：`memory_edit_update_and_invalidate`（update 改 content+importance 后**存储层**核对、空 update 报错、invalidate 后 recall **与** search 都不再命中、`stats` 仍为 1、未知 id 友好提示） |
| H32（`github`） | ✅（子项） | ①**`op` 字段别名**：`action`（Gyre 原名）优先、`op`（omp 原名）次之，`action_of()` 统一归一；`pr_create` → `create_pr`（唯一语义等价的命名差异，映射表附注释说明为何不假装支持 worktree/watch op）；②**新增 6 个只读/搜索动作**对齐 omp `gh` 的只读面：`repo_view`（`GET /repos/{repo}`）、`file_read`（contents API + `branch` 可选，**base64 内容解码为纯文本**）、`search_issues`/`search_prs`（Search API，自动补 `is:issue`/`is:pr` 与 `repo:` 作用域）、`search_code`、`search_repos`（按 omp 语义忽略 `repo`）；③schema：`required` 由 `["action","repo"]` 放宽为 `["action"]`（`search_repos` 无需 repo，其余动作仍强制），并补 `op`/`path`/`branch` 属性；④写门槛判定改用 `action_of`（别名同样受 `allow_write` 约束） | `crates/tools/src/github.rs`（`normalize_action` :81、`action_of` :92、`READ_ACTIONS` 扩展、schema `op`/`path`/`branch` 与 required、`file_read`/`search_*` 请求构造、`decode_contents_body` :233）、`docs/tools.md`（重新生成） | tools：`action_alias_and_op_field`（`pr_create` 归一、`op` 字段、`action` 优先、空值报错、未登记名原样）、`file_read_decodes_base64_contents`（含换行的 base64、目录数组、非 base64、非法 JSON 全部按契约处理）、`schema_enumerates_all_actions` 扩展（新动作在枚举内、`op` 同枚举、`repo` 不再 required） |
| H33（`read` 行限） | ✅ | **复核结论**：omp `readSchema` 只有 `path` 一个字段、行选择器写在 path 内联后缀（`third/oh-my-pi/.../tools/read.ts:530-534`），Gyre `read_file` 同口径（`path` + `:N`/`:N-M`/`:N+K`/`:N-`/`:a-b,c-d`/`:raw`），唯一额外属性是可选 `summary`——**不存在需要补齐的第二套 `offset`/`limit` 口径**；H33 其余两项（`glob` 四参数、`run_command(pty)`）已在 §十九 落地 | `third/oh-my-pi/packages/coding-agent/src/tools/read.ts:530-534` ↔ `crates/tools/src/fs.rs:26-35`（schema）、`crates/tools/src/fs.rs` 测试模块 | tools：`read_schema_matches_omp_path_only_contract`（`required == ["path"]` 且属性恰好 `{path, summary}`——后续若加第二套行限参数会立刻失败） |

### 44.2 验证

| 项 | 结果 |
|---|---|
| `cargo fmt --all --check` | 清零 |
| `cargo check --workspace --all-targets` | 0 error / 0 unused |
| `cargo clippy --workspace --all-targets` | 清零 |
| `cargo test --workspace --all-targets --no-fail-fast` | **3,748 passed / 1 failed / 59 套件（58 ok）**；唯一失败仍是 vendor `pi-shell::process::tests::kill_process_group_refuses_self_pgroup`（沙箱 `getpgid(0)`）。首轮并行运行另有一次 vendor `kill_builtin_signals_every_process_in_a_jobspec_pipeline` 超时（负载相关的既有 flake，**单独运行通过**，见 §44.3 复核）；相对上一轮 **+4 例**（tools：memory 1 + github 2 + read 契约 1） |
| `cargo run -p xtask -- release-check` | 五项全过；期间发现并修复 `docs/tools.md` 漂移（github/memory_edit 描述变更后按生成器重跑） |

### 44.3 复核记录（vendor flake）

首轮全量并行运行有 2 个 vendor `pi-shell` 失败；其中
`kill_builtin_signals_every_process_in_a_jobspec_pipeline` 报 `pipeline did not stop: Elapsed(())`。
单独运行该用例通过（`cargo test -p pi-shell --lib kill_builtin_signals…` → ok），第二轮全量并行运行也通过——
属**负载相关**的既有 flake（与本轮改动无关：本轮未触碰 `vendor/` 与 shell 执行路径）。

### 44.4 剩余与下一轮

- H32 仍有尾项：omp `edit` 的 mode 体系（hashline / replace / apply_patch）与 `*** Begin Patch` 语法；
  github 的 `pr_checkout`/`pr_push`/`run_watch`（worktree 隔离与长驻订阅，归 H18/后续批次）。
- §5.1 的 🟡 剩 10 项：H3、H7、H8、H18、H20、H23、H32、H35、H37、H38。
- 下一轮候选（按正确性收益排序）：**H23 热更新 + 逐键元数据**（可与 **H38** `docs/settings.md` 合并交付）、**H20**（decidable session hooks）、**H18**（worktree 隔离/预算/输出契约）、**H3/H7/H8**（raw 按键层 / 未支持 RPC 命令 / 子代理·扩展 UI 帧）、**H35/H37**（发布/测试尾项）。
