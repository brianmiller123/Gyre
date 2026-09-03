# Gyre × oh-my-pi 差距分析报告（2026-09-03）

> 调研对象：`third/oh-my-pi`（fork of badlogic/pi-mono，MIT；本地 checkout @ `18781d8295`，2026-09-02，版本 v18.1.2+5）vs 本仓库 Gyre（Rust workspace，33 crates + vendor 4 crates）。
> 审查基线：**当前工作树**（最后提交 `ffc88c5` "P0 工程基线"，另有 5 文件 +881/−125 未提交改动：shell 执行引擎统一）。
> 方法：以 `docs/oh-my-pi-agent-gap-analysis-2026-08-23.md`（功能矩阵）与 `docs/oh-my-pi-benchmark-2026-09-02.md`（工程维度）为对照基线，本轮 3 路并行只读 scout 分域精读（omp 交互面 / omp 配置与扩展 / Gyre 在途增量），另对全部承重结论做 40 余项 grep/read 直接复核（P1 未动工项反向 grep 零命中、P0 落地项逐处核验、REPL 命令表逐条清点）。**源码为唯一事实源**；对上一轮文档中已过期的计数（Hook 事件数、provider 数、工具数）已按今日源码修正。
> 证据格式：omp 路径相对 `third/oh-my-pi/`，Gyre 路径相对仓库根。

---

## 〇、结论速览

1. **上轮（09-02）P0 工程四件已全部落地**（commit `ffc88c5` 亲验）：vendor/ 入库（415 文件受跟踪，.gitignore 精确豁免）、CI 执法链（fmt+clippy `-D warnings`+nextest+doctest+发布冒烟+SHA256）、README 三处漂移修正（模块树 33 crates、MCP stdio-only 表述与代码一致）、CHANGELOG 建立。
2. **工作树原有一批代码完成的在途工作已落地**（本报告发布当日以 `932399f` 提交）：`run_command` 执行引擎统一（brush 内嵌 shell 为主引擎、rm/mv/ln 扣留回退、`cwd`/`env`/`timeout` 参数、Windows PTY 发现链、非交互环境基线、输出 CRLF 归一与中段省略、21 条破坏性命令拦截、10 个新测试）。对应 `docs/oh-my-pi-bash-port-cross-platform-analysis.md` §5 的 P0 全部 + P1 大部，全量测试通过。
3. **功能矩阵（08-23）经抽验仍然成立**：P1 未动工项（plan propose、RPC v2、MCP HTTP、manage_skill、自动标题、定价表、postmortem、跨进程限流、压缩 pinning、OAuth）经反向 grep 今日全部确认零进展；Phase 0 功能修复（CLI 四工具接线、security_scan 双端、429 Retry-After）亲验在位。
4. **本轮新发现两处小缺陷**：① REPL `print_help` 帮助表仅覆盖 18/30 个已分发命令（缺 `/todo` `/goal` `/diff` `/fresh` `/paste` `/enhance` `/suggest` `/agents` `/plan`），且 help 文案称 4 种 mode 实际接受 5 种（缺 `plan`）——**已修复**（`0d82386`：HELP_KEYS 单源 + 四语补齐 + 覆盖回归测试）；② Gyre 的 Rust `vendor/` 四 fork 仍停在 18.0.0 时代快照，而 omp TS 基线已 v18.1.2+5（bash-port 文档已明示暂缓，维持暂缓但列入台账）。
5. **剩余真实差距五类**（规模对比：Gyre ≈ 86k 行自有 Rust + 169k vendored vs omp ≈ 778k TS + 195k Rust）：接口纵深、provider/账号生态、产品流半成品、恢复语义广度、性能与工程资产（配置 schema 单源、文档面 8 vs 126+ 篇）。
6. **总量估算**：P0 ≈ 1–2 人日；P1 ≈ 40–55 人日（收敛 ~80% 用户可见差距）；P2 ≈ 40–60 人日。引擎与功能内核不必大动，剩余差距全部是可排期的增量工作，无架构阻塞。

---

## 一、oh-my-pi 全景（v18.1.2）

### 1.1 规模与架构

- 三层 + 原生支撑：**产品层** `packages/coding-agent`（AgentSession ~10k 行 + SessionManager；会话树/审批/plan/todo/goals/checkpoint/子代理/记忆/技能/扩展/RPC/ACP/73 个 slash 命令/TUI 模式）→ **引擎层** `packages/agent`（agent.ts 门面 + agent-loop.ts 双循环：内循环工具批 + 250ms steering 轮询；纯函数压缩；AppendOnlyContext）→ **提供层** `packages/ai`（streamSimple 单点 → 14 种 KnownApi 方言、凭证轮换、transient 重试、跨进程限流）→ **数据与原生** `packages/catalog`（71 provider 描述符 + models.json 38.2 万行 ≈ 17MB 模型目录，`packages/catalog/src/models.json`、`src/provider-models/descriptors.ts:72-575`）+ 8 个 Rust crate（pi-shell 38k / pi-natives 25k / pi-walker / pi-iso / pi-ast / pi-voice + vendored brush-core + pi-builtins ~117 命令模块，进程内零 fork）。
- 入口四形态 + SDK：TUI（默认）/ `-p` 无头 / `--mode rpc|rpc-ui`（NDJSON）/ `acp`（编辑器）；`@oh-my-pi/pi-coding-agent` Node SDK（`sdk.ts`、`docs/sdk.md`）+ Python omp-rpc 类型化客户端（`python/omp-rpc/src/omp_rpc/protocol.py`，1807 行）。

### 1.2 功能特性

- **工具面**：29 内置 + 3 隐藏（yield/goal/think）= 32 规范工具（`packages/coding-agent/src/tools/builtin-names.ts`，单点清单），另加 `mcp__<server>_<tool>` 动态前缀；16 个 discoverable 工具挂 `xd://` 设备（`tools/xdev.ts:57` `XDEV_KEEP_TOP_LEVEL` 保顶集合）。AgentTool 接口含 omptype JIT schema（第 3 次调用编译）、intent 注入、onUpdate 流式进度、matcherDigest（TTSR 流式匹配）、approval 声明、renderCall/renderResult 渲染器。
- **子代理**：task 工具（schema 校验 typed 输出、PAL 工作树隔离 8 后端、递归深度护栏）+ IRC 邮箱总线（`task/irc/bus.ts`，ACK/唤醒/复活/`MAILBOX_CAP=100`）+ Agent Hub（Alt+A 全屏：roster/树视图/steer/revive/kill/_transcript_，`docs/agent-hub.md`）+ vibe 模式（director 驱动 fast/good 常驻 worker，`docs/vibe-mode.md`）。
- **上下文与记忆**：AppendOnlyLog + StablePrefix 字节指纹；压缩方法序 remote→snapcompact→handoff→shake→soft + 溢出上下文升级（contextPromotion，`settings-schema.ts:2466`）；snapcompact 像素帧 PNG（按供应商计价公式选帧形状）；记忆四后端（off/local/hindsight/mnemopi SQLite+向量+情景图）+ retain/recall/reflect/memory_edit/learn/manage_skill 工具面 + 压缩前召回。
- **TTSR 时间旅行流规则**：正则命中中断流 → 注入系统提醒 → 同点重试，注入可存活压缩。
- **advisor 第二模型**：逐轮审阅、nit/concern/blocker 内联注入，独立上下文与模型。
- **collab**：`/collab` 中继共享 + QR + 浏览器 guest + `omp join`；端侧密封、`/collab view` 只读链接；guest 命令白名单 11 条（`docs/collab.md:93`）。
- **其它**：browser（Puppeteer/CDP/relay 扩展驱动真实 Chrome 标签页）、computer（桌面窗口/截图/原生输入/AX 树）、eval（持久 Python+Bun 内核 + 工具环回桥）、security_scan（plan/run/inspect/import）、`omp commit`（原子拆分提交 + 依赖排序 + 消息校验）、tts/voice、web_search 23 后端链 + 22 类站点感知正文提取。

### 1.3 命令与交互方式

- **Slash 命令 73 内置**（`slash-commands/builtin-registry.ts:26-33` 六组注册表拼接）+ 捆绑自定义 `/review`（`extensibility/custom-commands/bundled/review/index.ts:476`）、`/green`（`.../ci-green/index.ts:47`）+ 内嵌 `/init`（`task/commands.ts:11-12`）+ 文件发现 7 优先级源（native 100 > omp-plugins 90 > claude 80 > claude-plugins/agents/codex 70 > opencode 55，`docs/slash-command-internals.md`）。分组：Control 4 / Modes 17 / Collaboration 10 / Session 19 / Lifecycle 20 / Marketplace 3。注意 `/commit` **不是** slash 命令（是 `omp commit` CLI 子命令与 `/git` UI）。
- **键位**：22 个可重映射 action ID（`docs/keybindings.md`；`~/.omp/agent/keybindings.yml`）：Ctrl+P 循环角色模型、Alt+A Agent Hub、Alt+Shift+P plan、Ctrl+R 历史搜索、Ctrl+T 思考可见性、Ctrl+Q 排队跟进、Alt+R 重试失败轮等；`/hotkeys` 实时查看。
- **Prompt 控件**：`!cmd`/`!!cmd`（bash，后者不进上下文）、`$python`（`modes/input-controller.ts:860-864,600`）、`@path` 附件；magic keywords `ultrathink`/`orchestrate`/`workflowz`（散文掩码匹配，`modes/magic-keywords.ts:37`，`docs/magic-keywords.md`）。
- **TUI 体验**：工具卡片（renderCall/renderResult，与 HTML 导出/collab-web 共享渲染器）、编辑流式预览 diff（`docs/tools/edit.md:125`）、ask 结构化选项器（含 Other 兜底，`docs/tools/ask.md:81-84`）、Kitty/Sixel/iTerm2 图像协议（`packages/tui/src/terminal-capabilities.ts:21-22`）、JSON 主题热加载（`docs/theme.md`）、slash/`@` 文件/emoji/GitHub 引用/内部 URL 五类自动补全、差分渲染引擎（HistoryBatch 单调握手 + viewport 增量重绘，`docs/tui-core-renderer.md`）。

### 1.4 配置与扩展机制


- **环境变量**：310 表行 / ~320 变量、11 大类（`docs/environment-variables.md`）；dotenv 加载序 process > `<cwd>/.env` > 用户级，`OMP_*` 镜像 `PI_*`。
- **Provider/认证**：models.yml 自定义 provider（9 种 api 方言 + 6 种 discovery 类型 + `!command` 密钥）；**7 层认证解析**（`--api-key` → models.yml → 存储 OAuth（**多账号轮换**）→ 登录 key → env → 其它存储 key → fallback）；OAuth `/login` 12+ 提供方、coding-plan 订阅 5+、keyless 本地引擎；**10 模型角色**（`config/model-roles.ts:17-28`：default/smol/slow/vision/plan/designer/commit/tiny/task/advisor）；retry.fallbackChains 链 + 溢出上下文升级（`settings-schema.ts:1882,2466`）。
- **扩展**：TS 模块动态加载（进程内不沙箱）+ `?mtime` 图级热重载 + marketplace（Claude 插件目录兼容，`docs/marketplace.md`）+ **ExtensionAPI 45 事件（14 个可阻断/改写，含 `before_provider_request` 整包替换）**（`extensibility/extensions/types.ts:1068-1103`）+ **HookAPI 27 事件（10 个可阻断/改写）**（`extensibility/hooks/types.ts:388-403` + `shared-events.ts:152-165`）。注册面：工具/命令/快捷键/flag/渲染器/Provider/文件写删 fallback/`sendMessage` 三通道。
- **MCP**：stdio / Streamable HTTP / legacy SSE 三传输（`src/mcp/transports/{stdio,http,sse}.ts`）+ OAuth discovery 与凭证按 profile 确定性存储 + resources/templates/prompts/订阅富化 + list_changed 通知转发扩展。
- **Skills 与上下文继承**：`<root>/<name>/SKILL.md` 非递归 + 7 源优先级 + `skill://` 协议 + `/skill:<name>` 命令；上下文文件 8 格式继承（`.omp/AGENTS.md`、`.claude/CLAUDE.md`、codex/gemini/opencode/copilot/agents/standalone），按目录深度 shadowing、`@path` 5 跳递归导入、可单文件禁用（`docs/context-files.md`）。

### 1.5 错误处理与恢复

- ai 层：a/b/c 凭证轮换（`auth-retry.ts:81` 上限 64 次；403/usage-limit/账号策略**直接轮换 sibling**）；transient 重试（3 次指数退避、Retry-After>30s 放弃，`oneshot-retry.ts:127-139,227-235`）；**跨进程 in-flight 限流**（文件租约：锁 10s/过期 30s/心跳 5s，`packages/ai/src/stream.ts:177-264,646`）。
- 会话层：TurnRecovery 2,371 行（`session/turn-recovery.ts`）：AIError 分类、empty-stop 重试 + developer 提示 + 丢回合持久化 reparent、usage-limit→凭证轮换、模型 fallback 链 + cooldown、上下文溢出→压缩接管、恢复注解持久化。
- 引擎层：合成 error/aborted、refusal 不重放、Harmony 泄漏自愈、崩溃 postmortem 分类器。
- 安全：审批 tier × 用户 × 模式比较序、secrets HMAC 可逆占位符（密钥字节永不达 provider，执行前 `deobfuscateToolArguments`，`secrets/obfuscator.ts`）、ACP 远程审批门、PAL 8 后端隔离。

### 1.6 测试与工程保障

- TS 23,290 用例（2,365 个 `*.test.ts`）分桶分片 CI + Rust 2,534 走 nextest（`.config/nextest.toml`：fail-fast=false、**retries=0**、slow-timeout 60s）；cargo-deny 许可证/来源双门；**metaharness**（experiment→run→trace + SQLite + REST:4700 基准管理器）+ typescript-edit-benchmark（Babel 变异量化 edit 成功率）+ snapcompact SQuAD recall eval（用 eval 驱动帧形状参数）；release_gate 聚合门 + 7 目标二进制 + macOS 签名公证 + SHA256SUMS + 发布后下载验证；docs/ 126 篇分层文档（根 81 + tools 30 + toolconv 12 + skills 3）。

---

## 二、Gyre 现状基线（2026-09-03 核查）

- 规模：33 crates / 86,249 行自有 Rust + vendor 169k 行（pi-shell / pi-builtins / pi-walker / brush-core，18.0.0 时代快照）；Web 前端 `web/c5-ui` ~10.2k 行 TS/TSX。
- 工程基线（`ffc88c5` 后）：CI 三门（fmt/clippy `-D warnings`/nextest+doctest）+ tag 发布（双平台矩阵 + `--version` 冒烟 + SHA256 sidecar）；`.config/nextest.toml` 在位；workspace lints 全 33 crate 继承（advisor/discovery/shell 已补）；vendor 入库（`git ls-files vendor/` = 415）；CHANGELOG + README 一致性已修复。
- 测试：1,072 个测试函数（上轮 1,060）+ 4 个 criterion bench（ttsr/search/tokenize/compaction）——**bench 仍未进 CI**。
- Phase 0 功能修复亲验在位：CLI 装配 todo/ask/checkpoint/rewind（`crates/cli/src/main.rs` 4 命中）、security_scan 双端装配、429 Retry-After 退避（`crates/llm/src/lib.rs:56-72` + `crates/agent/src/engine.rs:476-523`）。
- 在途未提交：shell 执行引擎统一（见速览 #2，代码完成 + 测试，待提交）。
- 新发现缺陷：REPL 帮助表漂移（9 命令缺席 + mode 文案缺 `plan`，`crates/cli/src/repl.rs:148-181,262-395,502-533` + `crates/i18n/locales/en.json:105-125`）。
- **已对齐/超集项**（无需动作，累计自 08-23 矩阵 ✅ 15 项 + Phase 0）：引擎双循环/三通道 steering/PauseGate/deadline/250ms 批级取消、interruptMode、magic keywords、task typed schema 校验、auto-thinking 分类器、AppendOnlyLog+StablePrefix、压缩降级链+压缩前召回、会话树/分支/JSONL 持久化、细粒度事件流、审批 7 步链（含元字符命令永不白名单 + sidecar 运行时切换，超集）、Harmony 泄漏自愈、合成错误/refusal 过滤、panic 隔离、超时矩阵（MCP/LSP/DAP/CDP/SSE 逐 chunk）、429 退避、i18n 四语（超集）、记忆工具面+AutoRetainHook+中文调优（超集）、todo 五阶段机、ask/checkpoint/rewind/security_scan 装配、eval 双内核+环回桥、collab 本地中继+AES-256-GCM、SOCKS5 出口代理、六边形架构与错误体系（thiserror 分层 + AgentError 总线 + 语义谓词，优于 omp 的 TS Error 类机制）。

---

## 三、差距矩阵（按优先级）

判定：❌ 完全缺失 · 🟡 部分实现 · ⚠ 未接线/不一致。工作量：人日（单人全栈、语义移植口径，沿用前两轮经源码校准的估算）。

### P0 · 立即（1–2 人日）

| # | 差距 | oh-my-pi 实现与位置 | Gyre 现状 | 工作量 | 建议 |
|---|---|---|---|---|---|
| P0-1 | ~~在途工作未提交~~ ✅ 已落地（`932399f`） | —（对照物是自身 bash-port 分析 §7） | 原 5 文件 +881/−125 代码完成含 10 新测试 | 0.1d | 已提交并随附跨平台分析文档；全量测试通过 |
| P0-2 | ~~REPL 帮助表漂移~~ ✅ 已落地（`0d82386`） | omp 命令帮助与注册表同源（builtin-registry 单点）+ `/hotkeys` 运行时内省 | `print_help` 重构为 `HELP_KEYS` 单源循环；9 条命令四语补齐；`help.mode` 补 `plan`；`/enhance` `/suggest` 用法接入 i18n | 0.5d | 已修；`help_covers_all_builtin_commands` 回归测试防再漂移 |
| P0-3 | ~~CHANGELOG 补记~~ ✅ 已落地 | docs/cli-reference.md 与 CHANGELOG 逐版对应 | 两笔提交均已记入 Unreleased 段 | 0.1d | 完成 |

### P1 · 高价值补齐（40–55 人日，1–2 月）

**A. 接口纵深（生态前置，~16–24d）**

| # | 差距 | oh-my-pi 实现与位置 | Gyre 现状 | 工作量 | 建议 |
|---|---|---|---|---|---|
| A1 | RPC 协议 v2 | 42 命令 12 组（`modes/rpc/rpc-types.ts:28-93`）+ ready 帧 `supportedProtocolVersions:[1,2]`（`:144-149`）+ negotiate 握手 + >1MiB `rpc_chunk` 分片 + 机器可读 error `code` + host_tools/host_uris 桥 + extension_ui 11 方法往返（`rpc-mode.ts` 1,547 行） | 🟡 3 命令（prompt/cancel/ping）+ 6 事件，无握手无版本、错误无码（`crates/cli/src/rpc.rs:47-58,110`）；server WS 已有 steering 但 CLI RPC 无（自家三面行为不一致） | 握手+error code+steering 回移 2–4d；扩至 15–20 命令 8–12d | **顺序硬约束：先 ready 帧 + negotiate 再扩命令**（有第三方客户端后加握手即破坏性变更）；命令按 omp 42 条清单裁剪，不盲抄 |
| A2 | Python 类型化客户端 | 手写 TypedDict 协议 1,807 行 + 客户端 2,154 行（`python/omp-rpc/`），无 codegen、运行时校验 | ❌ 无（`docs/rpc.md` 契约已定稿） | 8–12d | 依赖 A1 v2 稳定后启动；复制 omp 手写模式 |
| A3 | ACP 方法纵深 | agent 12 方法 + client 12 方法：fs/read_text_file、terminal/create+output、elicitation/*（`packages/utils/src/acp/connection.ts:243-312`，实现 2,786 行） | 🟡 10 方法 + request_permission；`session/load`/`set_mode` 以重建会话实现（`crates/acp/src/rpc.rs:283-299`） | 2–4d | terminal/fs 路由让编辑器终端/缓冲区成为工具执行面；elicitation 对接 ask 工具 |
| A4 | REST/WS 契约文档 | —（omp 无 REST面；对应物是 RPC/SDK 文档） | 🟡 26 REST 路径 + WS ServerFrame 20 变体零契约文档（`crates/server/src/lib.rs:1558-1594`） | 1–2d | serde tag 已是机器可读来源，手写一页 + 「实现文件」清单防漂移 |

**B. 产品流闭环（~14–23d）**

| # | 差距 | oh-my-pi 实现与位置 | Gyre 现状 | 工作量 | 建议 |
|---|---|---|---|---|---|
| B1 | plan propose 批准流 | `xd://propose` 写设备 → PlanProposalHandler → 四选一批准弹窗 → 合成 plan-approved 提示词 + 可选压缩接管（`plan-mode/approved-plan.ts`） | ❌ 仅 `/plan` 切模式 + architect/plan 写约束（`crates/config/src/rules.rs:30-40`）；`xd://propose` grep 零命中 | 4–6d | 复用现有 `xd://pending/resolve/reject` 两阶段设备骨架（`crates/tools/src/fs.rs:439-457`）；Web 弹窗 + CLI 四选一两端 |
| B2 | checkpoint turn_end 延迟 + 压缩 pinning | rewind 副作用延迟到 turn_end（`session/agent-session.ts:1241-1245`，yield 守卫 `:7303`）；条目树永不删故 rewind 恒安全 | 🟡 rewind=switch_branch_with_handoff 即时应用；**压缩会删节点** → 无 pinning 语义，延迟应用会产生「rewind 到已消失位置」的静默数据错误 | pinning 3–5d（含 turn_end） | **顺序硬约束：先实现 compactor pinned 节点集，再开 turn_end 延迟**；不可倒置 |
| B3 | hub v2（ACK/唤醒/复活/jobs/launch） | IRC 邮箱总线（`task/irc/bus.ts`：ACK、park 唤醒、`MAILBOX_CAP=100`）+ Agent Hub UI + 后台任务句柄/取消 | 🟡 进程内 Hub v1（注册表+定向邮箱，`crates/core/src/hub.rs`）；无 ACK/唤醒/复活/jobs 句柄/Hub UI | 5–8d | 扩展现有 core/hub + supervisor，勿另立；Web 端已有 supervisor dashboard 可挂任务面板 |
| B4 | manage_skill 工具 | 记忆工具面之一：技能 CRUD（`tools/builtin-names.ts:30`，`docs/skills.md`） | ❌ grep 零命中；skills 仅发现+注入+`/skills` | 1–2d | 文件-backed 技能写回 + 前缀校验，量小 |
| B5 | 会话自动标题 | LLM 标题生成（`utils/title-generator.ts`）+ 会话列表标题槽 | ❌ 仅手动 `_titles.json` | 1–2d | 复用 smol/tiny 角色思路：配置里挑最便宜模型生成 |
| B6 | vibe 模式 | director + fast/good 常驻 worker（`docs/vibe-mode.md`，`builtin-modes.ts:260`） | ❌ 无 | 5–8d | 可后置；依赖 B3 hub v2 的 worker 管理面 |

**C. 恢复语义广度（~6–10d）**

| # | 差距 | oh-my-pi 实现与位置 | Gyre 现状 | 工作量 | 建议 |
|---|---|---|---|---|---|
| C1 | 凭证轮换分类 | a/b/c 轮换上限 64、403/usage-limit/账号策略直接轮换 sibling、attemptedKeys 去重（`packages/ai/src/auth-retry.ts:81-122`） | 🟡 KeyRing round-robin 多 key（`crates/agent/src/engine.rs:472-479`）；无 403/usage-limit 直轮分类 | 1–2d | 在现有 fallback 链内加错误分类→直轮即可 |
| C2 | empty-stop reparent + 模型冷却 | TurnRecovery：empty-stop 重试+developer 提示+丢回合 reparent、fallback cooldown、恢复注解持久化（`session/turn-recovery.ts`） | ❌ 瞬时错误白名单续跑已有；empty-stop/冷却/注解无（grep 零命中） | 2–4d | reparent 依赖会话树已有能力，量可控 |
| C3 | 跨进程 in-flight 限流 | 文件租约：锁 10s/过期 30s/心跳 5s/signal 回退 250ms（`packages/ai/src/stream.ts:177-264,646`） | ❌ grep 零命中 | 2–3d | 多实例共享配额场景才需要；可降 P2 若单实例为主 |

**D. Provider 生态（~9–14d）**

| # | 差距 | oh-my-pi 实现与位置 | Gyre 现状 | 工作量 | 建议 |
|---|---|---|---|---|---|
| D1 | 模型目录数据化 | models.json 17MB/71 描述符/4000+ 模型（生成式维护，`bun run gen:models`）+ KnownApi 14 方言（`catalog/src/types.ts:8-22`）+ 模型字段（vision/thinking/context 上限）自动判定 | ❌ 5 适配器（openai/anthropic/deepseek/glm/gemini）+ 手工模型清单 | 4–6d | **不照抄 4409 条**：提炼实际使用 5–8 家 + 字段自动判定，消除手工清单漂移；vision/思考能力判定可反哺 read_image/inspect 路由 |
| D2 | MCP Streamable HTTP + OAuth | 三传输 + OAuth discovery + resources/prompts/订阅（`src/mcp/transports/*`，`docs/mcp-runtime-lifecycle.md:15,93,236`） | 🟡 仅 stdio 4 方法（`crates/mcp/src/client.rs:223-290`）；README 已如实声明 | 5–8d | HTTP 传输先行（远程 MCP 生态入口），OAuth 随后；订阅通知对齐 D1 |
| D3 | OAuth/coding-plan 登录 | `/login` 12+ provider OAuth + 订阅路由 + 凭证保险库 | ❌ 仅 env/models.toml key | OAuth v1（1–2 provider）5–8d → **P2** | 账号基础设施，按需求排期 |

**E. 性能（~7–11d）**

| # | 差距 | oh-my-pi 实现与位置 | Gyre 现状 | 工作量 | 建议 |
|---|---|---|---|---|---|
| E1 | token 计数双输 | 原生 utok 六族词表 + 非消息段实例身份缓存（`packages/coding-agent/src/utils/context-usage.ts:196-225`）+ provider usage 回报做锚点、本地估算仅下限（`session/session-advisors.ts:1573-1585`） | 🟡 每轮全量 BPE×2 重编码 + 非 OpenAI 退 chars/4（`crates/context/src/token.rs:70`） | 3–5d | usage 锚点 + 尾部增量 + 补 claude/deepseek/glm 词表；精度与 O(n) 双收益 |
| E2 | 上下文管线克隆密度 | — | 🟡 `active_path_messages` 全量克隆（`crates/context/src/lib.rs:94-96`）、`snapshot_nodes` 全森林克隆、compact 双份重建 | 3–5d | Arc 化/借用视图，先 bench 后改（bench 已有 4 个） |
| E3 | 无界通道 | — | 🟡 工具更新 `unbounded_channel`（`crates/agent/src/engine.rs:1356`）；steering 以 `len()` 轮询 | 1d | 加上限+满时策略；顺手改消费式轮询 |

**F. 工程与配置资产（~10–16d）**

| # | 差距 | oh-my-pi 实现与位置 | Gyre 现状 | 工作量 | 建议 |
|---|---|---|---|---|---|
| F1 | 配置 schema 单源 | SETTINGS_SCHEMA ~5,500 行驱动运行时内省 `omp config get/set`（`settings-schema.ts:473-6035`）+ 坏文件 `.broken-*` 隔离 | 🟡 `config.example.toml` 28 节逐键注释，但无 schema 无启动校验、2 级覆盖 | 3–5d | 最大结构性 DX 差距；schema 结构驱动校验 + `agent config get/set`；5 层覆盖可后置 |
| F2 | AGENTS.md 工程宪法 | 26KB AGENTS.md（分层规则/「两个实现=bug」/测试反模式/allow 必须带理由） | ❌ 无 | 1–2d | 把 lints 意图、错误设计模式（AgentError 总线）、vendor 策略、CI 门禁成文化 |
| F3 | cargo-deny | deny.toml→about.toml→THIRD-PARTY-NOTICES 合规闭环 + CI 双门（`ci.yml:152-162`） | ❌ 无 deny.toml | 0.5–1d | 骨架直接抄 |
| F4 | bench 进 CI + 体积台账 | metaharness 基准设施 | 🟡 4 criterion bench 已建，CI 不跑 | 1d | 至少趋势记录；release 产物体积入 CHANGELOG |
| F5 | telemetry metrics | OTel 全套 + omp-stats 本地看板（`packages/stats`） | 🟡 OTLP span + usage 聚合；无 metrics 导出、telemetry 零测试 | 2–3d | metrics（token/cost/时延直方图）+ 补测试 |
| F6 | 发布矩阵扩展 | 7 目标（linux x64/arm64/musl、darwin ×2 签名公证、win32）+ SHA256SUMS + 发布后验证 | 🟡 2 平台（linux/win x64）+ 冒烟 + SHA256 | 1–2d | 先补 linux-arm64 + macos（无签名）；签名/公证按需 |

**G. 文档（~5–8d）**

| # | 差距 | oh-my-pi 实现与位置 | Gyre 现状 | 工作量 | 建议 |
|---|---|---|---|---|---|
| G1 | per-tool 文档 | docs/tools/ 每工具一页（30 篇）+ toolconv 12 篇 | ❌ 仅 rpc.md 一篇开发文档 | 3–4d | 核心 10 工具先做；每篇内嵌「实现文件」清单对抗漂移 |
| G2 | authoring 三部曲 + 示例 | adding-a-provider / custom-tools / extension 教程 + 3 个可运行扩展示例 | ❌ 无 | 2–4d | 给「扩框架者」：新增 provider trait、新工具 trait、hook 示例各一 |

**H. 独立小件（~7–10d，互相独立可插空）**

| # | 差距 | oh-my-pi 实现与位置 | Gyre 现状 | 工作量 | 建议 |
|---|---|---|---|---|---|
| H1 | read PDF | `tools/read-pdf.ts` + `markit/converters/pdf/`（结构化 markdown） | ❌ grep 零命中 | 2–3d | `pdf-extract` crate 或复用 markit 思路；URL 场景 arxiv 高频 |
| H2 | cost 定价表 | catalog 携带每模型定价（`packages/catalog/src/types.ts`），usage→cost 本地记账 | ❌ `cost_usd` 恒 0（透传上游） | 2–3d | 提炼版定价表（随 D1 目录数据化一起做最省） |
| H3 | inspect_image 语义 | 视觉模型委派（`/vision` 开关；模型看不到时自动激活） | 🟡 read_image 内联多模态（同名不同机制，功能可达） | 1–2d | 纯文本模型场景补委派路径 |
| H4 | xdev 通用化 | 16 discoverable 工具统一 `xd://` 挂载（`tools/xdev.ts:83-86`），`read xd://` 列文档 | 🟡 仅 3 硬编码路径（pending/resolve/reject） | 2–3d | 随 B1 plan propose 一起把 xd:// 设备抽象成注册表 |
| H5 | read selector 语法 | `:50-200`/`:raw`/`:img`/`:conflicts` 等选择器族 | 🟡 有行区间 + raw + conflicts；缺 img/多段组合 | 1–2d | 按需补；` selector` 是 token 效率特性 |
| H6 | 上下文文件继承 8 格式 | codex/gemini/opencode/copilot/agents 等 8 源 + 深度 shadowing（`docs/context-files.md`） | 🟡 4 来源（AGENTS/CLAUDE/Cursor/Cline，`crates/discovery/src/lib.rs:23-34`） | 1–2d | 纯增量发现源 |

### P2 · 长期（40–60 人日，按资源排期）

| # | 差距 | oh-my-pi 实现与位置 | Gyre 现状 | 工作量 |
|---|---|---|---|---|
| L1 | Hook 事件面 3→10–15 | ExtensionAPI 45 事件（14 可阻断/改写，含 before_provider_request 整包替换）+ HookAPI 27 事件 | 🟡 Hook trait 3 事件 + 拦截/改写 2 方法，仅内部 3 个记忆 hook 注入 | 3–6d |
| L2 | secrets HMAC 参数占位符 | `secrets/obfuscator.ts`：密钥字节永不达 provider，执行前反混淆 | 🟡 SecretString+env 展开+advisor 脱敏；工具参数级占位符无 | 3–5d |
| L3 | 最小 agentic eval 设施 | metaharness（experiment→run→trace+SQLite+REST）+ typescript-edit-benchmark + SQuAD eval 驱动参数 | ❌ 无（Gyre eval crate 是执行内核，非评测设施） | 8–12d |
| L4 | snapcompact 计费感知帧形状 | 15 形状变体 + 按供应商视觉 token 公式选帧 | 🟡 v1 单形状，CJK 折 `?` 硬伤（`crates/snapcompact/src/lib.rs:8-10`） | 5–8d（依赖 L3） |
| L5 | InProcShell 持久会话 | brush 持久会话跨调用存活（`pi-shell`） | 🟡 oneshot 无状态；本次在途改动亦未含（bash-port 文档 P1-7 显式暂缓） | 3–5d |
| L6 | PAL 隔离接线 + 高性能后端 | 8 后端（apfs/btrfs/zfs/reflink/overlayfs/projfs/rcopy/block-clone，`natives/native/index.d.ts:1293-1302`） | ❌ `Workspace::start_isolation` API 零调用方；iso 仅 rcopy | 接线 2–4d；reflink/overlayfs 8–12d |
| L7 | OAuth 设备流 v1 | `/login` OAuth 12+ provider + 凭证保险库 | ❌ | 5–8d |
| L8 | 跨 crate 集成测试 + postmortem | TurnRecovery 注解持久化 + 崩溃 postmortem 分类器 + 26 跨 crate 测试目录 | ❌/🟡 单 crate 内行为测试好，跨 crate 层无 | 8–14d |
| L9 | web_search provider 链 | 23 后端 auto 链 + 22 类站点提取 + 安全库（NVD/OSV/KEV） | 🟡 DDG + Searxng 链（`crates/tools/src/web_search.rs:153-175`） | 5–8d |
| L10 | collab 托管 relay + QR | my.omp.sh 托管 + QR + 浏览器 guest | 🟡 本地 host 中继 + E2E 加密 + guest 页；无托管/QR（grep 零命中） | 3–5d（QR）+ 托管需基础设施 |
| L11 | Rust vendor fork 快进 | omp TS 基线 v18.1.2+5 | 🟡 vendor/ 停在 18.0.0 时代（零本地补丁，bash-port 文档 P0-5 暂缓） | 2–4d（回归窗口） |
| L12 | Docker/Nix 打包 | 4 阶段 Docker（tini）+ Nix flake + Home Manager 模块 + mise | ❌ 仅 cargo + tag CI | 3–5d |
| L13 | computer/tts/voice | desktop 10.6k 行 + pi-voice + sherpa-onnx | ❌（既定暂缓：生态与真机矩阵成本高） | 10–15d（如做 Linux X11 先行） |
| L14 | 会话导入 | `/resume @claude/@codex` 导入 Claude/Codex 会话 | ❌ | 3–5d |

### 明确不迁移（既定边界，README 应明示）

| omp 能力 | 原因 | Gyre 替代 |
|---|---|---|
| TUI 差分渲染引擎（25k 行 TS） | 与 rustyline + Web 双前端路线冲突 | Web 端借鉴 append-only 回滚契约做 diff 推送 |
| TS 动态插件 + marketplace + 7 源 slash | Rust 无运行时模块加载，结构性不可 1:1 | inventory + Hook（事件面 L1 扩展）+ 文件级命令；README 已如实表述 |
| Node SDK / robomp GitHub bot | 语言栈/独立产品 | `--rpc` v2（A1）+ Python 客户端（A2） |
| mnemopi 情景图/三元组/Weibull 遗忘深度语义 | 复杂度/收益比 | structured + 向量 + MMR 中文调优已覆盖主路径且为中文超集 |
| 覆盖率/变异测试门槛 | 双方皆无，不构成对标差距 | nextest + 行为级测试已够 |
| 巨文件执法 | 标杆自身 10,230 行更大 | 择机将 `agent/src/lib.rs` 内联块下沉子模块 |

---

## 四、路线图与顺序约束

1. **本周（P0，1–2 人日）**：提交在途 shell 工作（P0-1）→ REPL 帮助表修复（P0-2，含四语）→ 顺带把 `builtin_commands()` 与帮助表一致性加单测。
2. **第一梯队（P1-A/B 前置件，~3–4 周）**：RPC ready 帧 + negotiate + error code + steering 回移（A1 第一步）→ 压缩 pinning（B2 第一步）。**两个顺序硬约束**：RPC 版本握手先于命令扩充；压缩 pinning 先于 checkpoint turn_end 延迟应用——顺序颠倒分别造成破坏性协议变更与「rewind 到已消失位置」的静默数据错误。
3. **第二梯队（P1 闭环，~4–6 周）**：plan propose（B1）→ checkpoint turn_end（B2 第二步）→ hub v2（B3）→ RPC 命令扩容 + Python 客户端（A1 第二步 + A2）→ MCP HTTP（D2）→ 模型目录数据化（D1，连带 H2 定价表）。
4. **第三梯队（P1 均质件）**：恢复语义 C1/C2 → 性能 E1–E3 → 工程 F1–F6 → 文档 G1/G2 → 小件 H3–H6/B4/B5。
5. **P2 按资源插空**：L1（hook 事件面）与 L3（eval 设施）优先——前者是扩展生态的乘数，后者是 L4/snapcompact 与 edit 工具调参的前提。

**P0+P1（41–57 人日）收敛剩余用户可见差距的 ~80%。**

---

## 五、风险与依赖

- **provider API 漂移**（目录/OAuth/web_search）：trait 抽象 + fixture 单测锁定；omp 目录是生成式资产，提炼维护。
- **checkpoint×压缩互斥**：pinning 语义必须先行（B2），否则延迟应用产出静默数据错误。
- **RPC 协议膨胀**：先 negotiate 版本握手（A1），命令按 omp 42 条裁剪，不盲抄。
- **PAL rcopy 性能**：接线默认 opt-in 配置开关，reflink 后端落地后再默认。
- **secrets 占位符**（L2）：HMAC 密钥本地保管与反混淆时机（执行前、日志后）需对齐 omp 语义，防密钥进日志。
- **在途 shell 改动**：提交前跑全量 nextest；`is_withheld_command` 已成 vendor 语义契约，后续 vendor 快进（L11）需保该门。

---

## 附：本轮审查证据方法

- omp 侧计数今日实测：29 BUILTIN + 3 HIDDEN 工具（`builtin-names.ts` 逐行）、73 内置 slash（`builtin-registry.ts:26-33` 六组逐条）、ExtensionAPI 45 事件/14 可阻断（`extensions/types.ts:1068-1103`）、HookAPI 27 事件/10 可阻断（`hooks/types.ts:388-403` + `shared-events.ts:152-165`，修正 08-23 文档的「25」）、71 provider 描述符（`descriptors.ts:72-575`）、models.json 382,579 行、SETTINGS_SCHEMA `:473-6035`、310 环境变量行、docs/ 根 81 篇。
- Gyre 侧反向 grep 今日零命中确认未动工：`negotiate`、`xd://propose`、`manage_skill`、`streamable`、`empty_stop`、`postmortem`、`pricing|cost_table`、`auto title`、`pinned_nodes`、`elicitation`、`terminal/create`、`fallbackChain`、`in_flight`、PDF、QR。
- P0 落地亲验：`git ls-files vendor/` = 415；`.github/workflows/rust.yml`（lint/test/build 三 job + 冒烟 + SHA256）；`.config/nextest.toml` 在位；advisor/discovery/shell `[lints]` 已补；CLI 四工具 4 命中、security_scan 双端命中；REPL `builtin_commands()` 30 条与 `print_help` 18 条逐条比对（GyreDelta scout）。
- 在途改动特征化：`git diff --stat`（5 文件 +881/−125）+ diff hunk 精读（brush-first 派发、cwd/env/timeout、NON_INTERACTIVE_ENV ~40 变量、CRITICAL_PATTERNS 21 正则、CRLF 归一 + 中段省略、Windows shell 发现链 7 级）。
