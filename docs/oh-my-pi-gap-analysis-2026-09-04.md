# Gyre × oh-my-pi 差距分析报告（2026-09-04）

> 调研对象：`third/oh-my-pi`（fork of badlogic/pi-mono，MIT；本地 checkout @ `18781d8295`，版本 v18.1.2+5，2 天前同步）vs 本仓库 Gyre（Rust workspace，33 crates + vendor 4 crates，另有未提交在途工作：MCP Streamable HTTP、web/c5-ui 重构、config 扩展）。
> 方法：10 路并行只读 scout 分域精读（core-loop / tools / cli+协议 / config+扩展+MCP / TUI vs Web / LLM 提供层 / Rust crates 层 / 构建 CI 脚本 / 外围 packages / prompts+文档+测试+i18n），共约 357 项逐条判定，全部基于今日工作树实际读码；main 对承重结论做二次交叉验证（反向 grep、cargo check、源码抽查）。**源码为唯一事实源**。
> 证据格式：omp 路径相对 `third/oh-my-pi/`，Gyre 路径相对仓库根。判定：❌ 缺失 · 🟡 部分 · ⚠ 偏差/形态不同 · ✅ 对齐或领先。

---

## 〇、结论速览

1. **规模**：Gyre ≈ 256k 行 Rust（471 文件，含 vendor 169k）+ web/c5-ui ≈ 10k 行 TS vs omp ≈ 1,453k 行 TS（4,665 文件）+ 223k 行 Rust（401 文件）。规模比 ≈ 1:6，差距主要在接口纵深、provider 生态与资源密度，不在内核架构。
2. **内核循环高保真对齐**（core-loop 域 ✅22/59）：steering 三通道、250ms 中断轮询、pause gate、Harmony 双计数器、截断续写、瞬时错误恢复、软需求升级、TTSR、advisor、魔法关键词、auto-thinking、记忆三挂点、会话树+shake/summarize/snapcompact/prune 压缩链、task 并行护栏、DAP 14 动作（比 omp 更全）。
3. **上轮（09-03）P1 清单反向 grep 全部零进展**（negotiate、xd://propose、manage_skill、empty_stop、cooldown、fallback_chain、pinned、elicitation、terminal/create、postmortem、pricing、自动标题均无命中）——唯二例外见 #4/#5。
4. **工作树在途已落地 MCP Streamable HTTP 传输**（未提交）：`crates/mcp/src/http.rs`（258 行，Mcp-Session-Id/MCP-Protocol-Version/保留头剥离/头 ${ENV} 展开）+ config 无标签 enum 双形态 + 4 测试。上轮 D2 的传输半边已完成（未提交），剩余 OAuth/prompts/templates/订阅/通知。
5. **本轮新发现 3 处正确性级缺陷 + 2 处行为偏差**（上轮未识别）：
   - **Anthropic thinking 签名与流全丢**：`crates/llm/src/anthropic.rs` 全文无 `signature`/`thinking_delta`/`thinking_start`（grep 零命中）；omp 带签名回传（`catalog/src/types.ts:564-583`）+ `thinking_start/delta/end` 事件（`ai/src/types.ts:1329-1331`）。官方 API 对无签名 thinking 回传 400 → Claude 多轮带思考必崩（main 已复核）。
   - **Gemini thoughtSignature 丢弃**：`crates/llm/src/gemini.rs:172-173,439-441`；Gemini 3 思考上下文每轮重建。
   - **MCP 工具名无 server 命名空间**：`crates/mcp/src/tool.rs:26-28` 直接用 server 提供名 → 跨 server 同名工具静默冲突（main 已复核）；omp 用 `mcp__<server>__<tool>` + sanitize + origin-key 去重（`tool-bridge.ts:395-431`）。
   - 魔法关键词注入顺序相反：Gyre 在用户消息**前**（`engine.rs:126-155`），omp 在**后**（`ultrathink.ts:17-21` + `agent-session.ts:5797-5801`）。
   - `reflect` 语义相反：omp = 带上下文问答（`memory-reflect.ts:8-11`），Gyre = 提炼写入（`memory_tool.rs:233-238`）——同名不同义会误导模型。
6. **误报修正**：RustCrates scout 报「vendor/pi-builtins 缺 bytecount 声明」——`cargo check -p pi-builtins` 通过，`Cargo.toml:55` 已声明 `bytecount 0.6.8 (runtime-dispatch-simd)`，误报；但 **xutf 漂移属实**（omp `wc.rs` 用 xutf 处理 CJK-EAW 宽度，Gyre vendor 用 unicode-width）。Tools scout 存疑的「zip 读取」——`fs.rs:920` 有完整 zip 分支，已支持。
7. **分级清单**：高优先 18 项（含 3 bug）、中优先 32 项、低优先 30+ 项，见 §三。另有 24 项跨 scout 不确定点汇总于 §四。

---

## 一、架构、目录与入口对比

### 1.1 双方结构

**omp（TS+Rust 双栈 monorepo，bun + cargo + bazel + nix + docker）**
- 产品层 `packages/coding-agent`（885k 行，60+ 子模块：session/modes/commands/task/tools/mcp/lsp/dap/slash-commands/extensibility/secrets/plan-mode/compress/goals/activity/async/commit/…），入口 `main.ts`(2,126 行) + `cli.ts`(472) + `cli-commands.ts`(393，42 个子命令注册表) + `sdk.ts`(4,314 行嵌入式 SDK)。
- 引擎层 `packages/agent`（34k）+ 提供层 `packages/ai`（249k：~90 provider 注册、7 层认证、OAuth 12 家）+ 数据层 `packages/catalog`（60k：models.json 11.5MB ~1400 模型 + descriptors ~70 厂商）。
- 原生层 8 个 Rust crate：pi-builtins 90k（~125 命令模块）/ pi-natives 41k（grep/glob/fd/PDF/audio/WebRTC/clipboard/desktop napi）/ pi-shell 38k / pi-vcs 10k（gix+jj-lib）/ pi-walker 6k / pi-ast 4.4k（57 语言）/ pi-iso 4k（8 后端）/ pi-voice 3.9k + vendor brush-core。
- 外围 15 包：utils 57k / tui 56k / mnemopi 31k / omptype 26k / stats 13k / hashline 11k / metaharness 11k / natives 7k / typescript-edit-benchmark 4.7k / collab-web 3.9k / snapcompact 3.8k / browser-relay 0.6k / wire 0.5k / agents / ai。
- 工程：scripts/ 60+（release/install.sh 334 行/install.ps1/session-stats/edit-benchmark/cleanup-scan…）、根 package.json ~110 scripts、ci.yml 20 jobs、7 发布目标+macOS 签名公证、cargo-deny/about.toml/THIRD-PARTY-NOTICES(22.9k 行)、docs/ 130 篇、TS 测试 2,349 个文件。

**Gyre（纯 Rust workspace + 内嵌 Web）**
- 33 crates：tools 14.4k / agent 9.5k / cli 6.8k / llm 5.8k / context 5.5k / memory 5.2k / server 4.3k / hashline 3.9k / core 3.2k / config 3.0k / lsp 2.4k / eval 2.3k / acp 2.1k / swarm 2.0k / browser 1.8k / dap 1.8k / ttsr 1.5k / collab 1.5k / mcp 1.3k / iso 1.3k / 其余 ≤1k ×13；vendor 4（pi-shell/pi-builtins/pi-walker/brush-core）。
- 入口单点 `crates/cli/src/main.rs`（clap flags+位置参数，无子命令机制）；Web 面 `crates/server`（25+ REST + WS）+ `web/c5-ui`；REPL `crates/cli/src/repl.rs`（30 命令）。
- 工程：CI 3 jobs（rust.yml）、2 发布目标、无 scripts/无 justfile/无 deny.toml；docs/ 11 篇（10 篇内部分析笔记）；Rust 测试自研 ≈1,083 个 + vendor 继承 2,024；前端 0 测试。

### 1.2 模块映射（经验证）

| omp | Gyre | 状态 |
|---|---|---|
| packages/coding-agent | crates/cli + agent + context + tools + server + config + prompt + … | 🟡 内核对齐、外围缩水 |
| packages/agent | crates/core + agent | ✅ |
| packages/ai | crates/llm | 🟡 5/90 provider、2/7 认证层 |
| packages/catalog | crates/llm/registry.rs（手工 TOML 清单） | ❌ 无目录数据 |
| packages/tui | web/c5-ui + crates/server（既定边界：不移植 TUI） | 🔄 替代实现 |
| packages/hashline | crates/hashline（12 模块，多 repair） | ✅ |
| packages/snapcompact | crates/snapcompact（v1 单形状，CJK→?） | 🟡 |
| packages/mnemopi | crates/memory（structured/vec/temporal/mmr） | 🟡 ~17 模块未移植 |
| packages/omptype | crates/agent/schema_check.rs（432 行子集校验）+ 手写 schema | ⚠ |
| packages/utils | 分散：acp→crates/acp、walk→vendor/pi-walker、persist→crates/context/persistence | 🟡 缺 HTML→MD/正文抽取 |
| packages/wire | crates/collab（frame/codec/snapshot） | 🔄 |
| packages/stats | crates/server /api/stats 聚合 | 🟡 |
| packages/metaharness | crates/eval（仅执行内核，非基准设施） | ❌ |
| packages/browser-relay | crates/browser（仅自启 headless） | ❌ |
| packages/collab-web | crates/server collab_guest.html（单文件 E2E 聊天室） | 🔄 定位不同 |
| packages/natives | crates/search + pty + shell + vendor/pi-builtins | 🟡 ~2/40 能力面 |
| crates/pi-ast | crates/ast（5 语言 vs 57） | ❌ 大缺口 |
| crates/pi-iso | crates/iso（rcopy 实装 vs 8 后端） | ❌ 大缺口 |
| crates/pi-vcs / pi-voice | 无对应物 | ❌ |

### 1.3 入口形态对比

| 形态 | omp | Gyre |
|---|---|---|
| 交互 TUI | ✅ 默认 | 🔄 REPL（rustyline）+ Web |
| 无头 print/json | ✅ `-p` text/json | ❌ one-shot 仅流式 markdown |
| RPC（NDJSON） | ✅ 42 命令+握手+分片 | 🟡 3 命令无握手（`cli/rpc.rs`） |
| ACP | ✅ stdio | ✅ stdio + HTTP/SSE（Gyre 多远程传输） |
| SDK | ✅ createAgentSession（4,314 行） | ❌（装配逻辑私有于 main.rs） |
| 子命令 | ✅ 42 个 | ❌ 无子命令机制 |
| daemon | ✅ launch/broker | ❌ |

---

## 二、分域差距明细（每域精华；完整表见各 scout 归档 history://）

### 2.1 核心循环与会话（59 项：✅22 🟡10 ⚠9 ❌18）
最大缺口 **TurnRecovery 恢复族**（omp `session/turn-recovery.ts` 2,657 行统一 owner）：empty-stop 重试+丢回合 reparent（`:73,:796-800`）、unexpected-stop tiny 分类器、fallback cooldown/到期回归（`:1441-1454,:1945-1946`）、工具批失败重放（`:2615-2619`）、usage-limit 凭证切换。其余 ❌：xd://propose plan 批准流（`interactive-mode.ts:3161-3185` + `plan-mode/approved-plan.ts`）、async/job-manager 后台任务（924 行：投递重试/retention/poll ladder）、子代理复活（persisted-revive）、子代理软预算（executor.ts SOFT_REQUEST_BUDGET+1.5x 强停）、子代理 fallback 链继承（`executor.ts:158-260`）、角色池 priority.json、自动标题、`/compress` rewrite/approve 协议、sharpshooter/prewalk/cleanse。🟡：会话持久化缺 entry 元数据/迁移链、pins/usage、todo 缺 eager prelude+完成提醒、goals 缺状态机+wall-clock、prune 缺 supersede 分组、summarize 远程超时 10s vs omp 180s、IRC bus 缺 IPC/ACK/唤醒。

### 2.2 工具面（38 行：✅7 🟡16 ⚠8 ❌6 ➕1）
广度接近（~33 vs 29+3），差距在**深度**：
- read：缺选择器族 `:N-M,:N+K,:raw,:img` 多段组合（omp `read.ts:530-538`）、PDF/office 经 markit 转换、目录读=层级树、重复读防环（`read.ts:605-609`）；Gyre 有 `summary`（tree-sitter 结构折叠）为独有。
- edit：缺 replace/patch/sloppy 降级模式（omp `edit/index.ts:67-76` 五模式）；Gyre fuzzy 容错已有。
- bash：缺后台作业（AsyncJobManager：自动转后台/`bash_*` 句柄轮询）与交互 PTY 续聊；Gyre run_pty_command 一次性无 send-keys。
- web_search：❌ 25 家 provider 链（brave/tavily/exa/perplexity/kagi…）+ recency vs Gyre DDG+Searxng 双链无 recency——**单点限流风险最高项**。
- hub：❌ 12 op 监督面（wait/inbox/jobs/cancel/start/ps/logs/stop/restart/describe）vs Gyre send/recv/list 纯消息。
- task：🟡 缺 agent 花名册参数、schemaMode strict、isolated worktree、async/detached、yield 信封协议（hidden 工具）。
- ❌ manage_skill、computer、yield/think/goal（hidden 三件）、xdev 通用 `xd://` 设备挂载（16 discoverable 工具按需唤起；Gyre 仅 3 硬编码 URI）。
- ✅ 已对齐：conflict://、DAP debug、内部 URL 读面（skill/memory/mcp/artifact/issue/pr/http）、进程内 bash（pi-builtins）、grep/glob 引擎路线（纯 Rust vs napi 二进制）。

### 2.3 CLI 入口与协议（40 项：❌22 🟡11 ⚠1 ✅6）
- ❌ 子命令机制（omp `cli-commands.ts:14-260` 42 个：commit/models/config/usage/auth-broker/setup/completions/plugin/marketplace/grep/render/shell/git/worktree/share/join/stats/bench…全部无）。
- launch flags 缺：`--thinking/--system-prompt/--append-system-prompt/--no-tools/--continue/--print(json)/--max-time/--api-key`（omp `cli/args.ts:25-100` 43 flags vs Gyre ~20）。
- slash：**60 个内置命令缺失**（omp 73 vs Gyre 30 内含别名）。缺失名单（按组）：modes 14（security/settings/setup/plan-review/vibe/guided-goal/loop/queue/switch/fast/extended-context/computer/vision/prewalk）、collab 9（advisor/export/trace/dump/share/join/leave/browser/copy）、session 14（jobs/usage/stats/changelog/hotkeys/context/extensions/git/hub/branch/fork/tree/login/logout）、lifecycle 17（ssh/new/clear/drop/shake/handoff/pin/btw/tan/omfg/cleanse/retry/debug/memory/rename/move/add-dir）、marketplace 3、control 3（force/live/pause）。Gyre 结构缺 subcommand/allowArgs/aliases 元数据（喂不了协议面）；RPC/ACP 内无法执行 slash。
- RPC：❌ ready 帧+版本协商（`rpc-types.ts:144-150`）、1MiB/64MiB 帧分片（`rpc-frame.ts`）、42 命令 vs 3、消息粒度事件、会话运行期操作。⚠ **顺序硬约束：先握手后扩命令**。
- ACP：🟡 缺 session/list、setSessionConfigOption（现在重建会话）、authenticate/logout 为 `{}` 存根、extMethod；adapter 仅 5 类 update（tool_call 流式/plan 缺）。
- ✅ Gyre 领先：REST/WS Web 面、ACP HTTP+SSE、`--lang` 四语、SOCKS5。

### 2.4 配置/扩展/MCP/secrets（38 项）
- 配置：10 模型角色（`model-roles.ts:22-31`）❌；retry 策略旋钮（maxRetries/usage-aware fallback/fallbackChains/fallbackRevertPolicy）🟡；.env 分层加载 ❌（`${VAR}` 未设置静默空串有踩坑风险，`env.rs:7-27`）；env 文档 632 行 vs Gyre ~15 个裸变量无文档；运行时写回 🟡；server.auth_token 未走 SecretString 🟡。
- MCP（工作树现状）：stdio+HTTP ✅ 已落地；❌ OAuth 全链（RFC9728 发现/PKCE/动态注册，`oauth-flow.ts:342`）、prompts/list+get、resources/templates+subscribe、server→client 通知（tools/list_changed 等 4 类，`manager.ts:303-305`；Gyre SSE 通知仅 debug 日志丢弃 `http.rs:130-136`）、server→client 请求应答（ping/roots/list）、legacy SSE-only 传输；🟡 `.mcp.json`/`.cursor/mcp.json` 只读兼容缺失。
- 扩展：ExtensionAPI 45 事件/HookAPI 25 事件（ConfigExt 实测修正：非 27）vs Gyre 3 事件 trait；❌ 文件式插件/marketplace/TS 动态加载（既定边界，结构性不可 1:1）；Hook 缺 system prompt 覆盖钩子。
- skills：7+ 源 vs 仅 native；**provider 端口已就绪**（`core/skill.rs:82-97`），补 Claude/Codex provider 即可覆盖迁移大户；manage_skill ❌。
- 上下文文件：8+ 格式 vs 4（缺 GEMINI.md/.cursorrules/windsurf/copilot）；@-import 展开 ❌（`at-imports.ts`）。
- secrets：HMAC keyed 可逆占位符体系 ❌（`secrets/placeholder.ts:1-83`）；Gyre SecretString 底座 ✅ 占优但 advisor obfuscator 仅覆盖快照链路。

### 2.5 TUI vs Web/REPL（26 项：❌10 🟡8 ⚠3 ✅5）
硬缺口（Web 端）：消息队列+dequeue（Ctrl+Q/`->` 简写/Alt+Up 取回；Gyre 忙时仅单条 steer）、per-tool 工具卡（omp renderCall/renderResult，collab-web/src/tool-render/ 30 个 React 视图可基本直接移植）、编辑流式 diff 预览（server `ToolExecutionUpdate` 预留未发 `lib.rs:200-207`）、ask 选项器（线协议 `AskMessage` 无 options 字段 `web/c5-ui/src/lib/agent/types.ts:74-81`）、图像回显（`<img>` 全库无渲染）、键位重映射+/hotkeys、emoji/GitHub-ref/内部 URL 补全。✅ 领先：会话分支树 UI、多模态输入、markdown 高亮、thinking 折叠。

### 2.6 LLM 提供层（36 项：❌17 🟡9 ⚠4 ✅4）
- 覆盖面：14 线协议 vs 7 枚举 5 适配器（缺 openrouter/codex-responses/azure/bedrock/gemini-cli/vertex/cursor/gitlab-duo/devin）；~70 厂商目录 vs 手工 TOML；models.json 11.5MB 定价/能力目录 ❌（cost_usd 无来源）。
- 认证（差距最大域）：7 层解析链、OAuth 12 家+多账号轮换、a/b/c sibling 直轮（403/usage-limit，MAX=64）、跨进程刷新租约（SQLite CAS+lease fence）、持久化限流块——Gyre 仅 RuntimeOverrides+进程内 KeyRing 两层。
- 数据结构：Usage 缺 reasoning_tokens/cttl/cost 分桶；内容块 3 vs 7 种（缺 RedactedThinking/ServerTool/签名）；AssistantMessage 缺 upstreamProvider/responseId/duration/ttft。
- 流式：缺 partial-JSON 修复（omp parseJsonWithRepair；Gyre 坏 JSON 原串塞 arguments）、泄漏清洗中间件、per-host 首事件超时、OpenAI 显式缓存断点、cache TTL 1h/24h。
- ✅ 已对齐：429 同模型退避、fallback 链、DeepSeek/GLM reasoning_content 全链路、稳定前缀缓存断点。
- 最高杠杆顺序：Anthropic thinking 修复 → openai-responses 适配器 → 凭据链+租约 → compat 数据化+定价。

### 2.7 Rust crates 层（25 项：✅4 🟡9 ❌12）
- vendor 四件套：pi-walker/brush-core/pi-shell 与 omp HEAD 同步（差异≈rustfmt 风格）✅；pi-builtins 落后于 xutf/bytecount 宽度与 wc/jq 实现（bytecount 误报已排除，xutf 漂移属实）。
- crates/search vs pi-natives：仅 ~2/40 能力面——缺 fuzzyFind（fd）、流式 grep+binary 检测、clipboard、fs watch（notify）、向量 SIMD topK、diff 结构化输出、文本宽度排版、tokens 多编码表（O200k 等，现单 cl100k 近似）。
- crates/ast：**5 语言 vs 57**，缺 contextual selector/MultipleNode 兜底/parse_cache/node_chain_at——建议直接 vendor omp pi-ast（纯 Rust 无 napi 依赖）。
- crates/iso：**1 后端实装 vs 8**（apfs/btrfs/zfs/reflink/overlayfs/projfs/windows_block_clone/rcopy），native() 恒返回 Rcopy。
- pi-vcs（gix+jj-lib 双后端）、pi-voice（4 平台音频+WebRTC）：无对应物。

### 2.8 构建/CI/脚本/依赖（40 项）
- 工程入口：❌ 无 justfile/xtask（本地/CI 命令漂移）；❌ 发布编排（omp release.ts 486 行原子 push）；❌ 安装器（install.sh 334 行+install.ps1）；❌ 发布后下载验证。
- 供应链：❌ cargo-deny（omp licenses 白名单+yanked=deny+sources 门）；❌ about.toml→THIRD-PARTY-NOTICES；🟡 CI 前端 `npm install` 而非 `npm ci`（lock 已提交未强制）；omp 侧另有 bun 3 天新包冷却。
- CI：❌ concurrency group（release 防取消）、paths 过滤、workflow_dispatch、actionlint；🟡 无 `[profile.ci]/[profile.profiling]`。
- 打包：2 目标 vs 7（缺 linux-arm64/musl×2/darwin×2+签名公证）；❌ musl 冒烟；SHA256 sidecar 🟡（缺汇总 SHA256SUMS.txt）。
- 依赖缺口：**tree-sitter 语法包 workspace 级 0 个**（omp ~50 语言显式声明）→ ast 语言面瘫没的直接原因；ast-grep 注释停用；grep-searcher/-pcre2 未引（自拼 ignore+globset）；rust-toolchain stable 浮动 vs omp nightly pin；tiktoken-rs 0.6 vs 0.11、toml 0.8 vs 1.1、image png-only vs png/jpeg/gif/webp、arboard 无 wayland feature。
- ✅ 合理不移植：bazel/nix 基建（规模不匹配）、npm 发布面、robomp。

### 2.9 外围 packages（19 项）
硬缺口：metaharness（实验管理 REST:4700+基准 runner+看板）、typescript-edit-benchmark（Babel 变异编辑质量基准）、browser-relay Chrome MV3 扩展（驱动用户已登录标签页）、utils HTML→Markdown/正文抽取（marked/turndown/readability）。mnemopi ~17 模块未移植（episodic-graph/triples/Weibull 遗忘/extraction LLM 管线/beam 召回/MCP 暴露——部分为既定边界）。omptype JIT schema 无对等（schemars 已声明零使用）。stats 看板/autolearn/managed-skills 缺。

### 2.10 prompts/资源/文档/测试/i18n（36 项）
- **System prompt 是最大行为差距**：omp 251 行 Handlebars 主模板（Role/ToolPolicy/Delegation 门控/Workflow 六阶段/Delivery 契约/Critical，`prompts/system/system-prompt.md`）+ 1,040 行组装器（并行 prep+5s deadline 降级）vs Gyre system-code.md 10 行 + PromptCatalog 5 模式。prompt 资源 178 md vs 7 md（25×）。委派门控段/安全条款/steering 资源/预置子代理角色 8 个/逐工具 53 篇全部缺失。
- 自定义覆盖链 ❌：SYSTEM.md/PERSONALITY.md/--append-system-prompt 全无。
- 文档：130 篇 vs 11 篇（10 篇内部笔记）；逐工具参考 31 篇 vs 0；用户主题文档（config/session/approval/mcp/memory…）vs 0。
- 测试：TS 2,349 文件 vs 前端 0；Gyre 自研 Rust 1,083 不落下风，但 telemetry 0、supervisor/pty 3；omp python 测试不适用。
- i18n：Gyre 唯一整体领先域（omp 无 i18n），但 en/ru 各缺 7 键（237 vs zh/ja 244）⚠；prompt 正文中文硬编码与 i18n 边界未定义。
- 资产：无 icon/favicon/品牌资产 ⚠。

---

## 三、分级缺失清单

### 高优先（正确性缺陷 + 破坏面最大的缺口，~18 项）

| # | 项 | omp 位置 | Gyre 现状 | 影响 | 建议 |
|---|---|---|---|---|---|
| H1 | Anthropic thinking 签名解析+回传 | `catalog/src/types.ts:564-583`、`ai/src/types.ts:1329-1331` | `llm/anthropic.rs` 零 signature/thinking_delta（已复核） | Claude 多轮带思考 400 必崩——**正确性 bug** | SSE 解析 signature_delta/thinking_delta→落块+回传 |
| H2 | Gemini thoughtSignature 回传 | `ai/src/types.ts:846` | `gemini.rs:172-173,439-441` 丢弃 | Gemini 3 思考上下文每轮重建、费用/正确性 | 落块回传，随 H1 同批 |
| H3 | MCP 工具名 server 命名空间 | `tool-bridge.ts:395-431` `mcp__<server>__<tool>` | `mcp/tool.rs:26-28` 直用原名（已复核） | 跨 server 同名工具静默冲突——**正确性 bug** | 加 `mcp_<server>_` 前缀+冲突告警 |
| H4 | 提交在途工作（MCP HTTP/config/web 重构） | — | 工作树未提交（含新测试） | 工作丢失风险；后续分析基线混乱 | 全量 nextest 后提交 |
| H5 | TurnRecovery 恢复族 | `session/turn-recovery.ts`（empty-stop :73、reparent :796、cooldown :1441、批重放 :2615） | engine.rs 散点自愈，四机制全无 | 长跑会话在脏 API/弱模型下失败率放大 | 建 `agent/turn_recovery.rs` 统一收敛 |
| H6 | RPC ready 帧+版本协商+分片 | `rpc-types.ts:144-150`、`rpc-frame.ts` | `cli/rpc.rs` 无握手无上限 | 协议演进无兼容路径；巨行破坏消费端。**顺序硬约束：先握手再扩命令** | 首行 ready+negotiate+chunk 协议（~300 行） |
| H7 | web_search provider 链 | `web/search/providers/` 25 家+recency | DDG+Searxng 双链 | DDG 限流单点，搜索质量主瓶颈 | 抽象已就绪；接 brave/tavily 2-3 家+recency |
| H8 | MCP 通知+prompts/templates | `manager.ts:303-305`、`client.ts:350-479` | SSE 通知 debug 丢弃（`http.rs:130-136`） | server 工具热更新失效；MCP 提示词模板不可用 | 读循环消费通知→注册表刷新；补 3 方法 |
| H9 | tree-sitter 语法包 + ast 面对面 | omp `Cargo.toml:408-458` ~50 语法、pi-ast 57 语言 | workspace 0 语法包、ast 5 语言 | read summary/ast_search 语言覆盖瘫痪级 | 按 pi-ast（纯 Rust）vendor 或分批引语法 |
| H10 | rust-toolchain pin | omp nightly-2026-08-08 pin | stable 浮动 | CI/本地工具链漂移、lint 突变 | pin 具体 stable 版本 |
| H11 | cargo-deny + 第三方声明 | `deny.toml`、`about.toml`、ci.yml:154-169 | 无 | 许可证/yanked/registry 源零防护；分发合规缺 | 骨架照抄，接 lint job |
| H12 | iso 后端实装 | pi-iso 8 后端（`lib.rs:280-291`） | 仅 rcopy，native() 恒 Rcopy（`iso/lib.rs:9-11`） | 隔离全量拷贝慢+耗盘；task 隔离参数因此缺 | 顺序：reflink→btrfs/zfs→overlayfs→apfs/projfs |
| H13 | hub 监督面（wait/inbox/jobs） | `tools/hub/index.ts:80-83` 12 op | hub_tool 3 op 纯消息 | 多代理编排「等待/管理」半边缺 | 先 wait/inbox/jobs，进程监督次之 |
| H14 | 后台作业（bash async） | `bash.ts:13-15` AsyncJobManager 自动转后台 | run_command 无后台 | 长任务占死工具轮；watcher/服务无法托管 | AsyncJob 等价物+`bash_*` 句柄族 |
| H15 | xdev 通用 `xd://` 设备 | `tools/xdev.ts`（16 discoverable 挂载） | 仅 3 硬编码 URI | 上下文瘦身机制整缺；token 成本随工具组膨胀 | registry 已有 get(name)；通用化派发成本低 |
| H16 | read 选择器族对齐 | `read.ts:530-538`（`:N-M,:N+K,:raw,:img` 多段） | 仅 `:conflicts`+local:// | 编辑行号锚定/大文件导航体验降级 | 纯文本窗口部分低成本先行 |
| H17 | System prompt 模板工程化 | `prompts/system/system-prompt.md` 251 行+178 资源 | 10 行角色描述 | 行为上限受制：无工具纪律/委派门控/交付契约 | 移植最小四段：ToolPolicy/Workflow/Delegation/Delivery |
| H18 | 发布矩阵 arm64+macos 与发布后验证 | ci.yml 7 目标+verify job | 2 目标、无验证 | arm64/mac 用户无产物；坏 release 上线后才知 | 先 aarch64-linux+macos（签名 secret 缺省跳过模式照抄） |

### 中优先（补齐后收敛 ~80% 用户可见差距，~32 项）

| # | 项 | omp 位置 | Gyre 现状 | 建议 |
|---|---|---|---|---|
| M1 | MCP OAuth（授权码+RFC9728） | `mcp/oauth-flow.ts:342` | 无 | 企业/SaaS MCP 入口 |
| M2 | CLI 子命令骨架+models/config/usage | `cli-commands.ts` | 无子命令 | clap Commands enum；`agent models list`+`config get/set` 最优先 |
| M3 | slash 60 缺失命令（分批） | `builtin-registry.ts:26-34` | 30/73 | 先 new/clear/retry/usage/context/hotkeys/login/logout |
| M4 | SlashSpec 元数据化+协议内 slash | `available-commands.ts:48-108`、`rpc-mode.ts:1025` | `&[&str]` 平铺 | name/aliases/subcommands/allowArgs 结构 |
| M5 | `--print` json/`--mode json` | `modes/print-mode.ts` | 无机器可读输出 | 复用 rpc map_event |
| M6 | `--append-system-prompt`/SYSTEM.md 等价物 | `system-prompt.ts` prep | 无覆盖机制 | 嵌入/自动化注入指令 |
| M7 | ask questions[] 选项器 | `ask.ts:63-80`、collab-web ask.tsx | 单问 yes/no/text | 线协议 AskMessage 扩 options/results（server+web 同步） |
| M8 | Web per-tool 工具卡+diff 预览 | `tool-execution.ts:1122-1205`、collab-web tool-render/ | 通用 ToolBlock | 移植 30 个 React 视图；接通 ToolExecutionUpdate |
| M9 | Web 消息队列+dequeue | `queue-input.ts:21-24` | 忙时单条 steer | 纯前端+现成帧 |
| M10 | Web 图像回显 | — | 无 `<img>` | data URI 渲染，纯前端 |
| M11 | edit replace/sloppy 模式 | `edit/index.ts:67-76` | 仅 hashline | replace 最常用 fallback |
| M12 | 模型角色 10 个（@role） | `model-roles.ts:22-31` | 仅 alias | Config `roles: HashMap`→装配层取 profile |
| M13 | provider 目录+定价数据化 | `descriptors.ts`、models.json | 手工 TOML | 提炼 5-8 家+字段判定+每模型定价（cost_usd 落地） |
| M14 | 凭据链+刷新租约+限流块持久化 | `auth-storage.ts:5639-5693,419-449` | 2 层+进程内 KeyRing | CredentialSource trait；rusqlite UPSERT 租约 |
| M15 | OAuth 设备流最小集（1-2 provider） | `oauth/` 12 家 | 无 | anthropic device-code 验证架构 |
| M16 | openai-responses 适配器 | providers/openai-responses | 枚举无适配器 | o1/o3/codex 系入口 |
| M17 | partial-JSON 修复+泄漏清洗中间件 | `anthropic.ts:2325`、dialect/thinking.ts | 坏 JSON 塞原串 | transform 层中间件 |
| M18 | checkpoint 压缩 pinning→turn_end 延迟 | `agent-session.ts:1241-1245` | 同步回卷+压缩会删节点 | **顺序硬约束：先 pinning 后延迟** |
| M19 | plan propose 批准流 | `plan-mode/approved-plan.ts` | xd:// 仅 ast 暂存 | 复用 pending/resolve 骨架 |
| M20 | async/job-manager+子代理复活/软预算 | `async/job-manager.ts`、`executor.ts` | 无 | hub v2 前置件 |
| M21 | 自动标题 | `utils/title-generator.ts` | 仅手工 `_titles.json` | tiny/smol 角色生成 |
| M22 | manage_skill 工具 | `tools/builtin-names.ts:30` | 无 | 技能 CRUD+写回 |
| M23 | skills Claude/Codex provider | `extensibility/skills.ts:173-177` | 端口就绪仅 native | 端口上加 2 provider，覆盖迁移大户 |
| M24 | 上下文文件 GEMINI.md/.cursorrules+@-import | `discovery/gemini.ts:132`、`at-imports.ts` | 4 格式无 @import | 只读 provider+环检测 |
| M25 | .env 分层加载+`${VAR}` 告警 | `$env` 逐层回填 | 无加载、静默空串 | config 加载时读 .agent/.env+用户级 |
| M26 | HMAC 占位符出站脱敏 | `secrets/placeholder.ts:1-83` | 仅 advisor 链 `<redacted>` | 挂 llm transform；执行前反混淆 |
| M27 | Usage/内容块/消息元数据对齐 | `catalog/types.ts:101-166`、`ai/types.ts:707-861` | 5 字段/3 块 | reasoning_tokens/cttl/cost、RedactedThinking、签名 |
| M28 | tokens 多编码+搜索参数对齐 | pi-natives `tokens.rs:31-37`、grep/glob schema | cl100k 单编码；grep 无 skip/case、glob 形状不同 | utok 编码表；工具参数对齐 omp 形状 |
| M29 | retry 策略配置化+usage-aware fallback | `settings-schema.ts:1781-1923` | 固定策略 | `[retry]` 节+用量感知 |
| M30 | telemetry metrics+弱测试 crate | omp-stats | 无 metrics；telemetry 0 测试 | token/cost/时延直方图；补 supervisor/pty |
| M31 | 安装器+发布编排+concurrency/paths | `install.sh`、`release.ts`、ci.yml:76 | 无 | curl\|sh+xtask release+CI 防取消 |
| M32 | docs 用户向 6-8 篇+逐工具 10 篇 | docs/ 130 篇 | 11 篇内部笔记 | CLI/config/session/MCP/approval 顺序 |

### 低优先（长尾/按需，30+ 项）

| # | 项 | 说明 |
|---|---|---|
| L1 | computer/tts/voice/pi-voice | 桌面自动化+语音整域（既定暂缓） |
| L2 | pi-vcs（gix/jj-lib） | git 感知原生加速；shell git 可先顶 |
| L3 | metaharness+typescript-edit-benchmark | agent 级基准/实验设施（L4 snapcompact 前置） |
| L4 | snapcompact 多形状+CJK 修复 | 15 形状+计费感知帧选择；现 CJK→? 硬伤 |
| L5 | mnemopi 深语义（episodic-graph/triples/Weibull/extraction） | ~17 模块；多数既定省略，挑 1-2 按需 |
| L6 | ExtensionAPI/文件式插件/marketplace | Rust 无动态加载，结构性边界；事件面逐步扩 |
| L7 | browser-relay MV3 扩展+多标签+aria 快照 | 现仅 headless 单实例；cdp_url attach 先行 |
| L8 | legacy SSE-only MCP 传输 | 旧版 server 才需要 |
| L9 | ACP session/list+extMethod+elicitation 表单 | 编辑器专有面 |
| L10 | Python omp-rpc 客户端 | 依赖 RPC v2 稳定 |
| L11 | /compress rewrite/approve 协议、sharpshooter、prewalk、cleanse | 产品流长尾 |
| L12 | eval rb/jl 内核 | 四语言缺二 |
| L13 | read PDF/office/SVG-rasterize、rar/7z/iso 归档 | markit 对应物；`?limit/?where` SQL 参数化 |
| L14 | fuzzyFind/clipboard/fs-watch/向量 SIMD/文本宽度/diff 结构化（pi-natives 长尾） | 按工具需求逐个 |
| L15 | write 归档/SQLite 行写 | bash 回退可用 |
| L16 | inspect_image 小模型委派、image_gen 多 provider | 无视觉模型降级 |
| L17 | emoji/GitHub-ref/内部 URL 补全、LaTeX 渲染、主题 token 对齐、键位浮层 | Web 锦上添花 |
| L18 | collab guest 只读 transcript 镜像+托管 relay+QR | 定位决策后 |
| L19 | Docker/Nix 打包 | 有部署需求再立项 |
| L20 | prompt 文风工具/session-stats/cleanup-scan/edit-benchmark 管线 | 工程长尾 |
| L21 | 内部 URL 协议族、daemon broker、profile 多配置、contextPromotion | 场景未现 |
| L22 | goals 状态机+wall-clock、todo eager/提醒、aside commit/discard 协议 | 语义打磨 |
| L23 | 品牌资产 icon/favicon/头图 | 工程资产 |
| L24 | sum 溢出类：think/goal hidden 工具、yield 信封、subagent fallback 继承、角色池 priority.json | 随 hub v2/typed 输出一起 |

### 明确不迁移（既定边界，维持）
TUI 差分渲染引擎（25k 行，与 rustyline+Web 路线冲突）；TS 动态插件 1:1（结构性不可，inventory+事件面替代）；mnemopi 全量深语义（复杂度/收益比）；Node SDK/robomp（语言栈）；bazel/自托管基建（规模不匹配）；覆盖率/变异测试门槛（双方皆无）。

---

## 四、不确定与需进一步确认（跨 scout 汇总 24 项）

**行为语义类**
1. omp HookAPI 事件数：ConfigExt 实测 25 个 `on()`（`hooks/types.ts:481-523`），历史口径 27——待定口径（或含 sendMessage/appendEntry）。
2. omp settings 总条数 ~400 为估算（grep 截断），未精数。
3. omp bash 超时默认值/上限（`tool-timeouts.ts`）未逐值读；Gyre 1-3600s/默认 120。
4. magic-keyword 注入顺序：CoreLoop 判「omp 在用户消息后」基于 `ultrathink.ts`+`agent-session.ts:5797-5801`，建议对照 `runLoopBody` 全链复核后修 Gyre 顺序。
5. `reflect` 语义对齐方向（Gyre 改名 distill 还是改语义为问答）需产品决策。
6. omp ACP listSessions/resumeSession 是否 ACP 规范 stable 集（对 `packages/utils/src/acp/protocol.ts` 或官方规范核实）。
7. Gyre ACP `UsageUpdate` 是否规范 update 类型（若是私有扩展需 initialize capabilities 声明）。
8. RPC id 类型：Gyre u64 vs omp 可选 string——跨协议统一策略待定。

**Gyre 侧待核**
9. crates/agent、crates/tools 运行时是否向 system 数组追加段落未逐行核（影响 system prompt 差距判定口径）。
10. i18n en/ru 各缺 7 键的键名未 diff（仅计数 237 vs 244）。
11. rustyline Ctrl+R 历史反查是否默认可用（未实测 REPL）。
12. Gyre edit 工具 ToolResult 是否已含 diff 正文（影响 M8 工作量）。
13. `/api/stats` 聚合是否有对应 Web 看板页（归 UI 域确认）。
14. llm `max_in_flight` 并发租约有字段无消费者验证——生效性待查。
15. `cost_usd` 消费方与计价来源未追踪（M13 联动）。
16. thinking-loop redirect 在 Gyre 是否以别名存在未全局确认。
17. Gyre CHANGELOG 维护方式（手写/工具）未验证。
18. web/assets 构建产物回写/漂移策略未确认（rust-embed release 从磁盘读 or CI 覆盖）。

**omp 侧待核**
19. omp MCP sampling/elicitation 支持情况未核查（ConfigExt 未计入）。
20. omp bash-interactive 交互续聊协议细节未深挖（H14 设计输入）。
21. `tts.ts/vibe.ts/review.ts` 在 omp tools/ 有 schema 但不在 BUILTIN_TOOL_NAMES——条件注册形态推断，未完全证实。
22. omp collab-web guest 链路是否 E2E 未读 codec.ts（影响 L18 定位对比）。
23. metaharness 对 @stencil-hq/vibemon 依赖深度未查。
24. omp `turn-context` 不在 packages/ai（任务书假设错误），实际属 session 域。

---

## 五、路线图建议（顺序约束）

1. **立即（本周）**：H4 提交在途工作 → H1/H2/H3 三个正确性修复（各 ≤1d，测试锁定）→ H10 toolchain pin + H11 cargo-deny 骨架（合计 ~1d）。
2. **第一梯队（顺序硬约束优先）**：H6 RPC 握手（先于命令扩容）→ M18 压缩 pinning（先于 turn_end 延迟）→ H5 TurnRecovery 收敛 → H8 MCP 通知/prompts → H7 web_search 链。
3. **第二梯队**：H13/H14/H20（hub v2+后台作业+async 任务，依赖关系：async 先于 hub 监督）→ H15 xdev 通用化 → H17 system prompt 模板 → M12-M17 provider/凭据域。
4. **第三梯队**：M2-M11（CLI/slash/Web 交互面）→ H9/H12（ast/iso 重资产）→ M18-M32 均质件。
5. **P2 插空**：低优先表按需求拉动；L3 eval 设施宜早于 L4 snapcompact 深化。

**总量估算**：高优先 ≈ 25-35 人日；中优先 ≈ 60-80 人日；低优先按需。无架构阻塞，全部为可排期增量。

---

## 附：本轮证据方法
- 10 路并行只读 scout（CoreLoop/Tools/CliProto/ConfigExt/UiWeb/LlmLayer/RustCrates/BuildCi/Periphery/ResDocs），每路要求 file:line 证据与逐项判定，共 ~357 项。
- main 交叉验证：omp 快照 `git log`（18781d8295，v18.1.2+5）；Gyre 工作树 `git status/diff`（MCP HTTP/config/web 在途确认）；P1 清单 14 关键词反向 grep（全部零命中，除 in_flight 误命中 3 文件非相关）；`cargo check -p pi-builtins`（bytecount 误报排除）；`vendor/pi-builtins` vs omp `wc.rs` xutf 漂移 grep 复核（属实）；`anthropic.rs` signature 零命中复核；`mcp/tool.rs` 命名复核；`fs.rs:920` zip 分支复核（Tools 存疑解除）。
- 上轮文档 `docs/oh-my-pi-gap-analysis-2026-09-03.md` 用作基线校准，全部结论以今日源码重验。
