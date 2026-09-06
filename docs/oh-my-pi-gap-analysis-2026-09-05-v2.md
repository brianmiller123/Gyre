# Gyre × oh-my-pi 差距分析报告（2026-09-05 · 第二轮）

> 调研对象：`third/oh-my-pi`（fork of badlogic/pi-mono，MIT；本地 checkout，v18.1.2 系快照）vs 本仓库 Gyre（Rust workspace，33 crates + vendor 4 fork crate，HEAD `b840e4f`，41 个未提交文件全部位于 `web/`——**以当前工作树为唯一事实源**）。
> 方法：10 路并行只读 scout 分域逐文件精读（Cmds / Tools / Config / Ext / Prompts / Proto / RuntimeCore / FeatureMods / Build / PriorDocs），每条结论要求 file:line 级证据；main 对承重结论直读复核六项（MCP 版本出价、openai-responses 适配面、RPC 命令面、read 选择器、update_tx 生产者、压缩 pinning 语义），并对 09-05 晨间报告做增量重验。

> **⚡ 修复落地（同日晚，HEAD 后）**：本报告高优清单中 10 项已在本轮修复并通过全仓
> fmt / clippy(`-D warnings`) / test 门禁——#1 hashline 锚定闭环（read 段头+共享快照库+
> stale 恢复）；#2 read 行选择器族（`:N/:N-M/:N+K/:N-/多区间/:raw`，`:img` 除外——由
> read_image 覆盖）；#5 update_tx 生产者（写工具+apply_hashline 逐区段 partial，逐调用
> tool_call_id 贯通）；#3 RPC ready 握手+v2 分片（命令面扩容仍缺）；#8/#9 部分——MCP
> 通知消费（tools/list_changed 刷新+回调）与 server instructions 注入 system prompt
> （重连/熔断/OAuth/工具缓存未动）；#12 部分——web_search recency（provider 链未动）；
> #24 部分——保留顶层词防误注入+task 位置参数收全词（子命令层未动）；#25 部分——ACP
> 真实 tool_call_id+kind/title 推断（authenticate/session 扩容未动）；#6 部分——空/意外
> 停止有界重试 3 次（分类器/冷却未动）；小修批：i18n goal.* 七键（en/ru）、
> enable_commands 死键移除、根 Cargo.toml tree-sitter 死条目移除、max_turns 文档修正。
> 另：`task` 位置参数改为 trailing 收集（`agent list all my files` 未加引号整句作 prompt，
> 对齐上游）。
>
> **⚡ 修复落地 · 第二轮（同日晚，续）**：高优清单再落地 9 项——#4 RPC 命令面最小集
> （get_state/set_model/set_thinking/get_messages/compact/get_usage，6 命令+协议文档）；
> #8 MCP 重连退避+爆发熔断（常量对齐 manager.ts，半开探活自动恢复，状态 API 就绪）；
> #12 web_search API provider 链（Tavily/Brave + recency，401/429 fallback）；#9 hub
> 监督面（wait/inbox/jobs/cancel 四 op + supervisor 共享句柄贯通 TaskTool/review/hub；
> 进程托管 op 待 async job manager）；#14 系统提示词四段（Tool Policy/Delegation/
> Workflow/Delivery+Critical，项目上下文之前注入）；#16 会话文件 header+版本化+
> migrations 骨架（legacy 无 header 兼容）；#19 部分——secrets 管线级双向脱敏（per-install
> HMAC key + 出向占位/入向还原，provider 边界生效，GYRE_SECRETS=off 直通；OAuth/凭据
> 存储未动）；#22 skills 跨工具 provider（.claude/.codex/OpenCode/.github + [skills.providers]
> 开关 + 同名去重）；#28 部分——发布链（cargo-deny 门 + about/NOTICES 生成链 + 矩阵
> 2→5 目标 + SHA256SUMS+下载验证 + CI 硬化；musl 目标 continue-on-error 待 fastembed
> 验证，advisories 5 项 RUSTSEC 待 triage）。
>
> **⚡ 修复落地 · 第三轮（同日晚，三）**：剩余高优再落地 9 项——RPC 反向通道
> （`--rpc-forward-ask`：审批/追问 request 帧外发 + 宿主 response 回路由，默认关闭兼容）；
> 会话树导航用户面（REPL `/tree` `/branch` + RPC `get_tree`/`switch_branch`，handoff
> 摘要交接）；模型角色 `[models.roles]` + `/model <角色>`；运行时模型发现
> （`list_models`：OpenAI 兼容/DeepSeek/Anthropic + REPL `/models` + RPC `list_models`）；
> 文件式 hook `[[hooks]]` + ShellHook（before_tool deny/allow）；认证链分层
> （auth.toml 0600 → GYRE_<PROVIDER>_API_KEY env，config 优先）；ACP
> `session/available_commands`（serve+stdio 注入 15 命令）；MCP 工具清单磁盘缓存
> （启动落盘 + 未连上 stale 回填告警）+ legacy HTTP+SSE 传输回退。
>
> **不确定项**：`switch_branch` 的 handoff 依赖会话构建时注入 summarizer（未注入 →
> 明确报错，不做静默降级）；MCP 缓存刷新时机为启动 load 后，运行期工具变更不落盘；
> 内置静态模型目录（models.dev 快照）未做——运行时发现已覆盖主要 provider，静态目录
> 价值存疑，降级为低优候选。
> 与 `docs/oh-my-pi-gap-analysis-2026-09-05.md`（同日晨）的关系：本报告为其**增量更新 + 全量扩深**。晨间报告后工作树新增两个修复提交（`fdf92c9` mcp+llm、`667a25f`+`b840e4f` 前端审计 P0/P1）；本轮以全新独立比对为主，晨间报告仅作先验交叉。
>
> **⚡ 修复落地 · 第五轮（09-06，续）**：**认证链 OAuth 流落地**——引擎
> （PKCE S256 / 回环回调服务器含 state 校验与手动粘贴赛跑 / RFC 8628 设备轮询）、
> oauth.toml 凭据存储（0600 原子写，对齐 omp AuthStorage 字段语义）、
> `agent auth login|logout` CLI、三流（anthropic 回环 PKCE 54545 + rotation 刷新 +
> 30 天绝对寿命提醒；openai-codex 固定 1455 PKCE + JWT 身份 + 刷新；zai-coding-plan
> 9999 精确回调 + 铸 `gyre` 长效 key）、运行时解析链
> config → oauth.toml（先刷后用）→ auth.toml → env 贯通交互/RPC/Web 三路径、
> anthropic 适配器 `sk-ant-oat` Bearer+beta 头分支。**附带修复**：RPC 会话引导此前
> 只读 profile 内联 key——auth.toml/env 在 RPC 路径从未生效，本轮随 OAuth 链一并贯通。
> 真实二进制 + 本地 stub 全链 e2e（授权 302 → 回调 → 信封 → 铸 key → 落盘 →
> list/logout）通过。
>
> **OAuth 暂缓项**（有意，凭证层完备性优先于适配器完备性）：github-copilot
> （需 GH→copilot 运行时令牌交换 + 专有头）、google-gemini-cli/antigravity（Code Assist
> 协议与现有 generativelanguage 适配器不匹配，token 不可直用）、openai-codex 的
> ChatGPT 后端请求头（originator/account-id）属 responses 适配器增强、其余长尾流
> （kimi/xai/cursor/gitlab-duo 等 19 个，模式同前三类）。
>
> **⚡ 修复落地 · 第六轮（09-06，续二）**：**剩余清单最高优 MCP OAuth 落地**——
> 发现链（RFC 9728 受保护资源元数据 → authorization_servers → RFC 8414/OIDC 授权
> 服务器元数据，网关子路径候选序逐条对齐 omp `buildWellKnownUrls`、issuer §3.3
> 校验）、RFC 7591 DCR（失败带端点+状态详情）、授权码 + PKCE 回环流（复用上移
> `agent-core::oauth_callback` 的回调服务器）、RFC 8707 资源指示（公告保留/兜底
> 同源剥离/授权 URL 内嵌优先）。凭据含刷新物落 `oauth.toml`（`mcp_oauth:<url>` 行，
> omp `MCPAuthConfig` 持久化语义）。HTTP 传输连接装载 Bearer：过期先刷后用、
> 401 单飞刷新 + 一次性重试、invalid_grant 清凭据并指引 `agent mcp login`；
> `[mcp.servers.<name>.oauth]` 手工子表覆盖白名单制服务商。验证：axum 仿真
> 授权服务器三条链路 e2e（发现+DCR+交换 / 预刷新 / 401 重试+清除）+ 真实二进制
> stub 冒烟（login URL→302→落盘→logout）。
>
> **剩余高优**（截至第六轮后）：omp 其余 CLI 子命令（usage/token/plugin 动作/
> setup/update/auth-broker/auth-gateway）、macOS 签名/brew 包。

---

## 〇、结论速览

1. **晨间报告两处正确性 bug 已在 `fdf92c9` 修复并经本轮直读确认**：MCP 协议版本出价改 `2025-11-25` + server 协商版本回传（`crates/mcp/src/client.rs:129`、`http.rs:35-36,87-88,235-237`）；openai-responses 伪适配 SUPPORTED 收窄为 `OpenAiCompletions+OllamaChat` 并附回归测试（`crates/llm/src/openai.rs:29-35,562-568`）。
2. **本轮新增一处正确性级缺口（工具域）**：hashline 锚定闭环断裂——omp 的 read 输出带 `[path#hash]` 段头 tag 并把读取版本写入快照库、edit 凭 hash 做 stale 校验；Gyre read_file 输出裸行号、hashline parser 把段头 hash 当可选、快照仅覆盖编辑历史（`crates/tools/src/fs.rs:110-115` vs `packages/coding-agent/src/tools/read.ts:1495-1509`）。
3. **晨间高优清单中 H16'（update_tx 生产者）、H4'（RPC 握手）经本轮直读确认仍未落地**（`crates/tools` 全部 `update_tx: None`；`crates/cli/src/rpc.rs` 仅 prompt/cancel/ping 三消息）。
4. 新一轮比对把缺口粒度下沉到 **~210 条**（此前 ~95 条），新增大量此前未记录的面：CLI 保留字防误注入、ACP availableCommands 与 tool_call id 状态机、collab 线协议整体分叉（proto=1 vs proto=3）、会话文件无 header/版本化、RPC 仅 4 命令 vs omp ~50、模型目录/发现/角色全缺、设置无运行时单例、Esc 分级中断/键位系统整缺等。
5. 结构口径：Gyre 33 crates + vendor 4；web/c5-ui 前端约 1 万行（前端审计 P0×7 + P1×16 已修，主包 489KB→376KB）；omp TS 1,453k 行 + Rust 223k 行不变。

---

## 一、结构与模块映射（现状）

### 1.1 入口与二进制

| | oh-my-pi | Gyre |
|---|---|---|
| CLI 入口 | `packages/coding-agent/src/cli.ts`（`bin: omp`），`cli-commands.ts` 注册 **42 个顶层子命令** + RESERVED_TOP_LEVEL_WORDS 防误注入 | `crates/cli`（bin `agent`）单一 flag 式入口：task 位置参数 + 14 flags；无 `#[command(subcommand)]` |
| TUI/交互 | `modes/interactive-mode.ts`（227KB，自绘 TUI + 可定制键位） | rustyline REPL（`repl.rs`，30 个斜杠命令 + `/skill:` 前缀 + .md 自定义命令） |
| RPC | `modes/rpc/`（~50 条 RpcCommand + ready/分帧协商 + host 反向通道） | `crates/cli/src/rpc.rs`：prompt/cancel/ping 三消息（本轮直读确认） |
| ACP | `modes/acp/acp-agent.ts`（@zed-industries 全量方法面） | `crates/acp`（stdio+HTTP/SSE 双传输；方法子集，见 §2.6） |
| Server | —（collab-web 静态站 + relay） | `crates/server`（axum，27+ REST 路由 + WS + collab 桥 + rust-embed 前端）——Gyre 超集面 |

### 1.2 模块映射（相对晨间报告无大变，摘要）

omp→Gyre 已对齐/超集：agent 循环、tools 核心、llm 五家适配器、skills/mcp(双传输)/discovery 四源、collab（自有协议）、snapcompact、hashline、memory 双后端、ttsr、advisor（简化）、eval 内核（py/js）、web 前端 + i18n 四语（Gyre 独有超集）。
omp→Gyre 有对应物但子集/偏差：acp、rpc、plan-mode、goals、advisor、ssh、browser、github/lsp/eval 工具、prompts 体系、config 面。
omp→Gyre 无对应物：见 §四 分级清单（扩展系统、市场、语音、computer、vcs、voice、export、blob-broker、markit、commit、autoresearch/cleanse/compress/if-bench、pi-natives 长尾、pi-iso 8 后端、pi-ast 语言面等）。

---

## 二、分域差距明细

> 每域格式：omp 位置（用途）→ Gyre 现状 → 影响/建议。status：❌missing / 🟡partial / ⚠divergent。标 ✅M 的为 main 直读复核项。

### 2.1 命令面（Cmds，28 条）

omp 权威口径：斜杠命令 72 主名 + 5 别名（六组注册表，`slash-commands/builtin-*.ts`）；CLI 42 子命令（`cli-commands.ts`）；可定制键位 `keybindings.yml` + 双击 Esc 回退 + Esc 分级中断。

**CLI 层**：
| # | 项 | omp | Gyre | 级 |
|---|---|---|---|---|
| 1 | 子命令层整体 | 42 子命令（models/config/gc/usage/stats/worktree/plugin/completions/render/git…） | ❌ 单层 flag 入口 | 高 |
| 2 | 保留字防误注入 | RESERVED_TOP_LEVEL_WORDS：`agent models` 提示管理命令而非发模型（防 #4845 类事故） | ❌ 位置参数一律进 LLM（`main.rs:993-997`） | 中 |
| 3 | shell 补全生成 | completions/__complete | ❌ | 中 |
| 4 | 插件/市场 CLI | `omp plugin` 动词族 | ❌（依赖扩展域） | 中 |

**斜杠命令层**（Gyre 现有 30 项外）：
| # | 族 | 缺失命令 | 级 |
|---|---|---|---|
| 5 | 会话生命周期 | /new /clear /drop /rename /move /pin /handoff /shake（Gyre 仅 /fresh /sessions /session） | 高 |
| 6 | 转录分支 | /branch /fork /tree（Gyre 无消息树消费面） | 中 |
| 7 | 认证 | /login /logout（会话内 OAuth） | 高 |
| 8 | 模型控制 | /switch /fast /extended-context /prewalk /vision /computer | 中 |
| 9 | 流程模式 | /plan-review /vibe /loop /queue /guided-goal /security /settings /setup；/plan 语义分歧（omp toggle+prompt，Gyre 单向切入） | 中 |
| 10 | 运行控制 | /force /pause /live | 中 |
| 11 | 协作导出 | /collab 应为 start\|view\|stop\|status（Gyre 仅打印房间 key⚠）；/join /leave /share /advisor /export /trace /dump /copy ❌ | 中 |
| 12 | 记忆维护 | /memory（stats/gc/rebuild；Gyre 有 memory 库无入口） | 中 |
| 13 | 状态仪表盘 | /usage /jobs /context /changelog /hotkeys /stats /extensions /hub；⚠/status 语义冲突（omp=扩展中心，Gyre=模型/用量汇总）；⚠/agents 语义冲突 | 中 |
| 14 | /mcp 管理 | omp add/list/remove/test 子命令；Gyre 仅列工具 🟡 | 中 |
| 15 | /todo 修改 | omp append 子命令+模糊匹配；Gyre 只读 🟡 | 中 |
| 16 | /goal 语义冲突 | omp=自治目标模式切换；Gyre=token 预算 set/extend ⚠ | 中 |
| 17 | /fresh 语义错位 | Gyre /fresh ≈ omp /new；omp /fresh（重置流状态）Gyre 无 | 低 |
| 18 | /resume 交互 | omp 无参开选择器 + @claude/@codex 导入；Gyre 必须显式 id 🟡 | 中 |
| 19 | 技能暴露方式 | omp 每 skill 一条斜杠命令；Gyre /skill:<name> 前缀 ⚠ | 低 |
| 20 | ACP 命令面 | availableCommands 广播 + ACP 文本模式斜杠拦截，Gyre ACP 完全没有 ✅M | 高 |

**键位/交互**：
| # | 项 | 说明 | 级 |
|---|---|---|---|
| 21 | 可定制键位系统 | omp keybindings.yml（action→chord、profile 级联、/hotkeys、6 测试文件）；Gyre 无 raw 模式按键层（rustyline 固定 Emacs） | 高 |
| 22 | 双击 Esc 回退 | 500ms 双击开转录回退选择器；Gyre 无任何 Esc 处理 | 高 |
| 23 | Esc 分级中断 | 依次取消 mcp-test→压缩→handoff→retry→TTS→子代理→队列→bash→eval→流式回合；Gyre 运行中键盘只能 steering，**无法中止回合**（`main.rs:2239-2318` ✅M） | 高 |
| 24 | 消息队列键位 | Ctrl+Q 入队 / Alt+Up 取回 / /queue；Gyre 用 steer 隐式替代 ⚠ | 中 |
| 25 | 编辑/显示快捷键 | Ctrl+G 外部编辑器、Ctrl+P 模型循环、Ctrl+T 思考可见性等 | 中 |
| 26 | 粘贴通道 | omp Ctrl+V 图像优先 + OSC 5522 + bracketed paste；Gyre /paste 命令 ⚠ | 低 |
| 27 | /git TUI | omp 分屏 diff 审阅+暂存+提交；Gyre /diff 打印 ⚠ | 低 |
| 28 | 行为测试 | Gyre 缺 /diff /lang /session /resume /custom 命令分发与 flag→行为路由测试 🟡 | 中 |

### 2.2 工具面（Tools，27 条）

omp 权威清单：29 BUILTIN + 3 HIDDEN（`builtin-names.ts`）+ CustomTool（generate_image/tts/web_search 双注册）+ `mcp__` 动态。Gyre：核心 7 常开 + 可选组 + 会话级工具 + MCP 动态。测试面：omp `test/tools/` ~150 个行为测试文件 vs Gyre 内嵌单测 + 3 个集成目录。

**❌ 高优（正确性/高频路径）**：
| # | 项 | omp | Gyre | 说明 |
|---|---|---|---|---|
| 1 | hashline 锚定闭环 ✅新 | read 输出 `[path#hash]` tag + 读取版本入快照库（`read.ts:1495-1509`）+ edit 凭 hash stale 校验（`edit/index.ts:386-390`） | read 裸行号、parser hash 可选、快照仅编辑史（`fs.rs:110-115`、`hashline/parser.rs:93-101`） | stale 检测/外部改动检测失效；模型可能编造 hash 触发 mismatch |
| 2 | read 行选择器族 | `:N`/`:N-M`/`:N+K`/`:N-`/逗号多区间/`:raw`/`:raw:N-M`/`:img`（`read-selector.ts:22-77`） | 仅 `:conflicts` + archive/sqlite/ipynb 冒号语法（`fs.rs:884-890` ✅M） | 大文件只能整读 2MiB 头；intercept 把 `sed -n` 重定向回 read_file 形成绕道循环风险 |
| 3 | bash async/pty 参数 | `async:true` 转 job（AsyncJobManager 自动转后台）、`pty:true` 接管终端（`bash.ts:316-330`） | 无 async 参数；PTY 拆独立工具（`shell.rs:61-74`、`pty/tool.rs`） | 长任务占死工具轮 |
| 4 | grep 参数面 | path 分号多值+`:N-M`+内部 URL、case/gitignore/skip 分页、20 文件×20 命中+总 cap 2000、4MB 窗口化+partial-coverage 提示、30s 超时（`grep.ts:82-118`） | pattern/path/highlight；全局 50 硬截断；hidden/gitignore 写死；4MB 整文件跳过（`search.rs:22-33`、`search/lib.rs:46-48`） | 误报无法切大小写、无法翻页、单热文件挤占预算 |
| 5 | web_search 链+recency | recency/limit/max_tokens 等参数 + provider 自动 fallback 链（exa/tavily/kagi/…12+ 家各有测试） | DDG HTML+SearxNG 双链、无 recency（`web_search.rs:489-560`） | DDG 单点限流风险 |
| 6 | hub 12 op | send/wait/inbox/list/jobs/cancel/start/ps/logs/stop/restart/describe（进程全生命周期） | send/recv/list 三 op（`hub_tool.rs:88-96` ✅M） | 多代理「等待/管理」半边缺 |
| 7 | task 参数面 | name?/agent?/effort/outputSchema+schemaMode/isolated/批量每项独立（`task/types.ts:114-190`） | task/tasks[]/output_schema 仅此；审批 ReadOnly vs omp exec | 子代理无角色/隔离/结构化分级 |
| 8 | 隐藏工具 yield/goal/think | HIDDEN_TOOL_NAMES 按需注册（yield=结构化输出终态） | ❌（grep 无实现） | output_schema 缺强制交付通道 |

**❌ 其余缺失**：manage_skill（skill 写路径）、computer（桌面操控）、tts 工具、inspect_image（可并入 read `:img`）、write 设备挂载框架（xd://<tool> 通用 device vs Gyre 仅 xd://pending/resolve/reject ⚠）。

**⚠ 形态偏差（保留 Gyre 语义可，但需记录）**：edit 单 hashline vs omp 5 模式分发（replace/patch/apply_patch/sloppy + auto-repair + settings fuzzy 双轨）；todo op 模型不同（omp 单操作增量+phase 分组 vs Gyre write 全量替换）；ask 单问 vs questions[]；checkpoint{goal}/rewind{report} vs Gyre {status,note}/{confirm}；github 操作集互缺（omp search_*/run_watch/checkout/push vs Gyre graphql+CI logs）；lsp 缺 type_definition/implementation/rename_file/status/reload/capabilities/request；browser 无 tab 会话/observe/aria 快照/relay；eval 缺 rb/jl；glob 单参无开关/排序；memory retain 单条+edit 无 update/invalidate；Gyre 特有 ssh/list_files/replace_block/pty 拆分（无害冗余）。

**🟡 部分对齐**：read 富格式（zip/tar/sqlite/ipynb ✅M 已落地；缺 PDF/SVG 内联/URL reader-mode/profile 摘要/自动 elision footer/重复读环断提示）；工具分级暴露（omp loadMode essential/discoverable + 统一超时表 vs Gyre 全量注册）；**vendor/pi-builtins 内容漂移（新发现，需确认）**：124 文件同名对齐，但抽验 grep.rs 上游 1785 行 vs vendor 1945 行、ls.rs 5279 vs 5389——非晨间报告所记的唯一 xutf 漂移点，需逐文件 diff 定基线。

### 2.3 配置面（Config，24 条）

omp：`settings.ts`（3,255 行 Settings 运行时单例+热更新）+ `settings-schema.ts`（6,338 行，约 700 键）+ models.yml 体系 + `auth-storage.ts`。Gyre：`config.example.toml` + `crates/config/src/config.rs`（1,766 行，serde struct + 启动时一次加载）。

**❌ 高优**：
| # | 项 | omp | Gyre |
|---|---|---|---|
| 1 | Settings 运行时单例 | sync get/set、后台持久化、外部修改检测（mtime generation）、文件锁、SettingSignal 订阅 | 启动加载一次；/model 只切 profile 不重载配置 |
| 2 | 内置模型目录 | models.dev catalog bundled + 每 provider 默认模型 + context/cost/thinking 元数据 + SQLite 缓存刷新 | ❌ 每模型手写 [[models]]；supports_thinking 恒 false |
| 3 | 运行时模型发现 | ollama/llama.cpp/lm-studio/openai-models-list 等 6 类发现 + 上下文窗口探测 + 超时策略 | ❌ Ollama/vLLM 也需手写条目 |
| 4 | 自定义 provider 配置面 | models.yml providers.<name>：headers、40+ compat 兼容位、auth none、keyless、modelOverrides | ⚠ 仅 extra_body 透传；compat 硬编码在各适配器 |

**❌/🟡 中优**：配置键 schema 内省 + `config get/set/list` CLI（❌；未知键静默忽略）；`--config` 一次性 overlay（🟡）；.env 三级自动加载（❌；`${VAR}` 未设静默空串维持）；模型选择语法（`provider/id`、`:thinking` 后缀、模糊/退役别名匹配 🟡）；模型角色 10 个（default/smol/slow/vision/tiny/…❌，advisor/tiny 全走主模型）；fallback 增强（usage-aware 余量、冷却回切、全局 fallbackChains 🟡）；`!cmd` 凭据解析（❌）；OAuth 登录/刷新（❌）；SQLite 凭据存储+多账号+会话粘性+退避记账（🟡 key_ring 环无持久化）；provider 门控 disabledProviders（❌）；采样面 top_p/top_k/tier（🟡 仅 temperature）；重试参数可配置化（❌，行为已有）；per-model thinking 元数据与六档预算表（🟡）；security/secrets/vault 开关键（❌）；语音配置键群（❌，且 Gyre crates/tts、stt 存在但 Config 无段——需先核实现完成度）；workspace.additionalDirectories（❌）；MCP 行为开关 enableProjectConfig/renderMarkdown/notifications（❌）。

**⚠ 低**：max_turns 示例 10000 vs 代码默认 1000（文档一致性）；TUI 外观键群（Gyre 无 TUI 定制面，影响有限）。**测试**：omp 30+ 配置行为测试 vs Gyre 内联单测（解析/合并/链展开），凭据链无测试。

### 2.4 扩展 / 插件 / 技能 / MCP（Ext，20 条）

omp 四层体系：extensions（进程内 TS，ExtensionAPI ~40 事件 + registerTool/Command/Provider/UI）→ hooks（文件式 TS hook）→ discovery（~15 provider 跨工具发现）→ plugins（npm/link 运行时 + marketplace 安装链）。MCP：manager（三传输 + OAuth + tool-cache 延迟连接 + 熔断重连 + prompts/instructions + 多源配置）。

| # | 项 | omp | Gyre | 级 |
|---|---|---|---|---|
| 1 | 进程内扩展系统 | ExtensionAPI/ExtensionFactory 生命周期 | ❌（结构性不可 1:1；建议 Rust Hook 扩展 + 子进程扩展协议替代） | 高 |
| 2 | hook 事件面 | shared-events 全集：session_*/branch/compact*/context/before_agent_start/stop(可要求 continuation)/auto_retry*/tool_call(可改写 input)/tool_result(可改写 content) | HookEvent 仅 BeforeTool/AfterTool/Stop + intercept 仅整体阻止（`core/hook.rs`） | 高 |
| 3 | 文件式 hook 子系统 | 用户可配置 hook 模块发现与加载 | ❌ 仅装配层注入的记忆 hooks | 高 |
| 4 | 技能跨工具 provider | claude/codex/opencode/github + 原生 .omp/.agents，每源独立开关 | 仅 NativeSkillProvider（config_dir/skills + .agent/skills + custom） | 高 |
| 5 | agent-plugins.org 标准包 | plugin.json+skills/+mcp.json、${PLUGIN_ROOT} 展开、containment 校验 | ❌ | 中 |
| 6 | managed skills | auto-learn 自动沉淀技能 provider | ❌ | 中 |
| 7 | 市场安装链 | marketplace registry/fetcher/cache + 双级 installed_plugins.json + /marketplace + auto-update | ❌ | 中 |
| 8 | 插件运行时加载 | npm/link 包内 skills/hooks/tools/commands/mcp.json 接入各发现面 + -e 注入 + doctor | ❌ | 中 |
| 9 | 自定义工具加载 | 用户 tools/ 目录 TS 工具 | ❌（可先做声明式 .md/.json→run_command 模板） | 中 |
| 10 | 自定义命令面 | TS 命令模块 + bundled + 跨工具命令 + 用户覆盖优先级 | 🟡 仅 .md 注入 | 中 |
| 11 | MCP OAuth 全链 | 发现/PKCE/动态注册/凭据刷新/onAuthError | ❌ | 高 |
| 12 | MCP 工具缓存+延迟连接 | agent.db + config hash + 30d TTL + DeferredMCPTool（启动不被慢 server 阻塞） | ❌ 启动串行 connect | 中 |
| 13 | MCP 重连/熔断/通知消费/订阅 | onClose 重连+退避+burst 熔断；tools/resources/prompts_changed 刷新 | ❌ 连接死即失效 ✅M（晨间 H7' 维持） | 高 |
| 14 | MCP prompts + server instructions | prompts/list→斜杠命令、instructions 注入 system prompt | ❌ initialize 仅取版本 | 中 |
| 15 | MCP 配置多源 | settings + .mcp.json 合并 + disabled/enabled 名单 + /mcp add 写回 | 🟡 仅 config.toml 内嵌 | 中 |
| 16 | MCP legacy SSE 传输 | type:"sse" 独立传输 | ❌（主流已迁 Streamable HTTP，存量存在） | 中 |
| 17 | context 发现广度 | windsurf/gemini/vscode/copilot/ssh 远程 + @import 展开 + builtin-rules 27 条 | 🟡 四源（AGENTS/CLAUDE/cursor.mdc/clinerules） | 低 |
| 18 | 技能 frontmatter 严格校验 | 闭合六字段 schema + NFKC name 校验 + 违规跳过告警 | 🟡 宽松解析 | 低 |
| 19 | 运行期热重载 | /reload-plugins + /mcp reload | ❌ 改配置须重启 | 低 |
| 20 | 本域测试 | ~70 个测试文件 | 🟡 skills 4 模块 + mcp client/tool 单测；**http.rs/stdio.rs 零测试** | 中 |

### 2.5 提示词 / 模板 / 主题（Prompts，22 条）

omp：`system-prompt.ts`（1,040 行装配器）+ `prompts/system/system-prompt.md`（251 行：Role/Personality/Runtime/Skills&Rules/Internal URLs/Tool Inventory/Tool Policy/Delegation/Workflow 六阶段/Delivery 四契约/Critical）+ `prompts/` 全树约 200 个 .md（tools/52、system/约 95 运行期模板、memories/9、advisor/5、goals/6、agents/7 角色模板、compaction/15）+ 主题系统（60+ 颜色 token、100+ 内置主题、OSC 11 深浅检测、符号预设）+ 提示词重写管线。
Gyre：7 个 prompts/*.md（五模式角色 5-10 行 + review + compaction-summary）+ `crates/prompt`（{{var}} 简单替换）+ engine.rs 组装（platform/workspace/AGENTS.md 裸文本/mental_models/memories/skills）+ web 前端 light/dark。i18n 四语为 Gyre 独有超集（omp 无 i18n 基建）。

**❌ 高优（行为杠杆段）**：Workflow 六阶段章节；Delivery 契约与 Critical 收尾段；Delegation 委派门控段；Skills & Rules 的 generic-rules/domain-rules 通道（Gyre cursor.mdc 全文注入、globs 丢弃 🟡）；SYSTEM.md 项目覆盖链 + `--append-system-prompt` 通道。
**中优**：人格预设三档 + PERSONALITY.md；Internal URLs 提示词章节（机制已齐仅缺文案，⚠ 形态）；Tool Inventory 渲染（弱模型工具选择）；workstation 环境块细节（GPU/终端/内核，🟡）；上下文文件 `<file>` 包装 + auto-loaded 条款 + 段落包含去重 + @import 展开（🟡）；工作区目录树 + 多根 `<workspace-roots>`；~95 个运行期通知模板族（需逐个 triage：title/thinking-loop/plan-mode 等与 Gyre 功能对应者先做）；52 个 per-tool 提示词模板（⚠ Gyre 内联 schema 描述形态可行，缺 examples 与使用时机）；7 个子代理角色模板（scout/reviewer/…）；goals 提示词族；compaction 提示词族其余 14 个（🟡 compaction-summary 已同构）；TUI 主题系统（Gyre ANSI 硬编码 + web 仅 light/dark）；computer-safety 安全条款块。
**低优**：active-repo-context；review 变体（headless/custom/ci-green 🟡）；memories/sharpshooter 模板精度（⚠）；提示词重写管线；嵌套仓库上下文。
**测试**：omp system-prompt 7 + theme 8 行为测试 vs Gyre 4 个 smoke。

### 2.6 协议 / API / 数据结构（Proto，30 条）

omp：`packages/catalog` KnownApi **14 线协议族** + ~70 KnownProvider + classifyModel 身份分类；`packages/ai` ~40 适配器 + oauth/ 10+ 流 + auth-broker/gateway + dialect/（qwen-xml/harmony/kimi…）+ 严格 schema 校验；`packages/wire` COLLAB_PROTO=3 线语法；`modes/rpc` ~50 命令；`modes/acp` 全量 ACP。
Gyre：Api 枚举 7 值（5+1 实装；OpenAiResponses 无适配器——H3' 修复后为「诚实的未实现」）+ 5 适配器；`crates/cli/src/rpc.rs` 4 命令；`crates/acp` 方法子集；`crates/collab` 自有 proto=1。

| # | 项 | omp | Gyre | 级 |
|---|---|---|---|---|
| 1 | 线协议族 | 14 族（responses/codex/azure-responses/vertex/bedrock/gemini-cli/cursor/devin/gitlab-duo…） | 5+1（anthropic/openai-completions/gemini/glm/deepseek + ollama 兼容） | 高 |
| 2 | openai-responses 真 wire | reasoning items/加密回放/previous_response_id | ❌ 枚举已预留 + 防伪回归测试（`openai.rs:562-568` ✅M） | 高 |
| 3 | OAuth 套件 | oauth/ 10+ provider 流 + 刷新 + sqlite 凭据库 | ❌ 仅 env API key | 高 |
| 4 | 流式事件粒度 | AssistantMessageEvent 13 变体（块边界 start/end） | AssistantEvent 10 变体（无 TextStart/End 等） | 中 |
| 5 | 会话级事件 | auto_compaction_*/auto_retry_*/model_changed/notice/todo_reminder… | ❌ AgentEvent 无会话层；server/前端不可观测自动压缩/重试 | 高 |
| 6 | AgentEvent 形态 | agent_start + message_update 全量 partial 快照 | 无 AgentStart；无 partial 快照 ⚠ | 中 |
| 7 | StopReason wire 拼写 | toolUse (camelCase) | tool_use (snake_case) + 特有 Pause ⚠ | 低 |
| 8 | Usage 字段 | reasoningTokens/cttl 5m-1h/server/credits | 4 token 桶 + cost 🟡 | 中 |
| 9 | 内容块类型 | redactedThinking/ServerToolContent/Fallback/TextSignature | Text/Thinking{signature}/ToolCall 🟡（redactedThinking 回放丢失可 400） | 中 |
| 10 | Thinking 控制模型 | 5 种 mode + effortMap/effortRouting/suppressWhenOff | 仅 budget_tokens（✅M：与 09-05 M27 一致维持） | 高 |
| 11 | OpenAI compat/dialect 层 | ~30 开关 + 方言包 + 流标记修复 | extra_body 透传 + 两家硬编码 ⚠ | 中 |
| 12 | 缓存控制 | CacheRetention + Anthropic 1h TTL + OpenAI prompt_cache_key/显式断点 | Anthropic 多点断点（局部领先）；无 TTL/prompt_cache_key 🟡 | 中 |
| 13 | ServiceTier | auto/flex/scale/priority + per-family | ❌ | 低 |
| 14 | RPC 命令面 | ~50 条（get_state/set_model/set_thinking_level/get_messages_page/branch/handoff/login…） | 4 条（prompt/cancel/ping + model 参数 ✅M）——「prompt 管道」非 headless 驱动面 | 高 |
| 15 | RPC 协商+分帧 | ready{protocolVersion,maxFrameBytes} + rpc_chunk 重组 | ❌ NDJSON 单行（H4' 维持 ✅M） | 高（先握手后扩命令） |
| 16 | RPC 宿主反向通道 | host_tool_call/host_uri_request/extension_ui_request(11 种 UI) | ❌（审批一律拒绝，需 yolo） | 中 |
| 17 | RPC 事件映射 | AgentSessionEvent 全集 + 子代理三帧 | 6 种窄映射（tool_result 仅 ok 布尔） | 中 |
| 18 | ACP 方法面 | list/resume/fork/configOption/extMethod + authenticate 真实现 | 子集；authenticate/logout 空实现（危险：客户端误以为已就绪） | 高 |
| 19 | ACP update 变体 | 9+（plan/available_commands/config_option/session_info/current_mode + tool_call kind/locations/status 机） | 5 变体；**tool_call_id=工具名**（并行同名工具不可区分——正确性缺陷 ✅M） | 高 |
| 20 | ACP 权限协商 | allow/reject × once/always + locations + cacheKey | 纯文本 request_permission + Yes/No | 中 |
| 21 | ACP 客户端 fs 桥 | fs/write_text_file 路由（写进编辑器 buffer）+ 回读校验 | ❌ 全部本地落盘 | 中 |
| 22 | collab 线协议 | wire proto=3：guest 可 prompt/abort/agent-cmd，host 实时镜像 entry/event/state | proto=1 自有聊天广播：guest 只能围观 ⚠✅M | 高（产品决策） |
| 23 | collab envelope/控制面 | [4B peerId] envelope + peer-joined/left/room-closed + 标准分享链接 | 裸密封帧 + 无控制消息 ⚠ | 中 |
| 24 | collab-web | 完整 React guest UI + 30 工具渲染注册表 | 单文件 collab_guest.html ⚠（轻量替代合理） | 低 |
| 25 | SDK | sdk.ts createAgentSession 一站式 | ❌ 装配序列散落 cli/server（可抽 AgentBuilder） | 中 |
| 26 | 模型身份分类 | classifyModel identity.class/revision 集中兼容表 | 各适配器硬编码启发式 🟡 | 低 |
| 27 | 行为级测试 | acp 13 + collab 14 + ai 250+ 文件 | 全内联单测，无 tests/ 集成目录 | 中 |

### 2.7 会话运行时核心（RuntimeCore，21 条）

omp：`session/` 77 文件（session-manager 3,147 行、turn-recovery 108KB、stream-guards、todo-tracker、snapcompact-inline、async-job-delivery、foreign-session-import、exit-diagnostics…）+ `task/` 27 文件（executor 3,649 行、worktree 隔离、persisted-revive、structured-subagent、spawn-policy…）+ `async/job-manager` 924 行。
Gyre：`crates/context`（树/持久化/压缩）、`crates/agent`（engine 1,744 行 + task_tool 619 行 + supervisor 观测）、swarm/ttsr/snapcompact。

| # | 项 | omp | Gyre | 级 |
|---|---|---|---|---|
| 1 | 会话文件 header+版本化 | SessionHeader + CURRENT_SESSION_VERSION=3 + typed entries（ModelUsage/Title/BranchSummary）+ migrations 链 | 裸 SessionNode JSONL + .leaf sidecar；无 header/版本/元数据 | 高（树导航/fork 语义/统计的地基） |
| 2 | fork 语义 | parentSession 关联 + 计费清零 + artifact 目录复制 | 纯文件复制 🟡 | 中 |
| 3 | 树导航用户面 | /tree /branch /navigateTree /discardEntryDurably | tree.rs 纯函数已有**无消费方**；仅 checkpoint/rewind 回卷 🟡 | 中 |
| 4 | resume 完备性 | --continue 最近会话、终端 breadcrumb、悬空 tool_call 诊断修复、Claude/Codex 会话导入 | 必须显式 id；无悬空 tool_call 修复（严格 provider 400 风险）🟡 | 中 |
| 5 | 空/意外停止恢复 | empty-stop 有界重试 + tiny 分类器 yes/no + fallback cooldown + usage-limit 切换（H5' 维持 ✅M） | ❌ 空回复直接「任务完成」 | 高 |
| 6 | 流式守卫 | StreamingEditGuard（patch 预检/生成文件拦截）+ ToolCallLoopGuard | ❌（TTSR 是另一机制，覆盖不到） | 高 |
| 7 | 中断重放安全 | 按「是否已有 tool_call 入上下文」分流：可重放自动续跑 / 不可重放占位+续写 | 🟡 仅保底存文本（persist_interrupted） | 中 |
| 8 | fallback 治理 | usage-aware fallback + selector 冷却 + 到期回切 + 重试留痕 | 🟡 静态链 + 429 退避（无冷却/回切/留痕） | 中 |
| 9 | 失败轮耐久丢弃 | 先落盘再 discard（审计可追） | ❌ 失败轮不落盘 | 低 |
| 10 | 子代理定义化 | agents/*.md 发现 + frontmatter + spawn-policy + DEFAULT_SPAWN_AGENT | ❌ 单一匿名 task（swarm 仅一次性编排）⚠ | 高 |
| 11 | worktree 隔离接线 | baseline→delta 回写 + 所有权标记 | iso/workspace 原语已有**零生产调用方**；engine 收尾仅 diff 🟡 | 高 |
| 12 | 只读子代理工具面 | READ_ONLY_TOOL_NAMES 白名单 | ⚠ 模式级只读存在，子代理默认继承全部工具+放行 | 中 |
| 13 | 子代理软预算 | SOFT_REQUEST_BUDGET + 1.5x 强停 + grace | ❌ 仅 MAX_INFLIGHT=16 | 中 |
| 14 | 持久复活 | parked roster 落盘 + 冷复活重建 | ❌ supervisor 进程内观测 | 中 |
| 15 | yield 工具/组装 | 隐藏 yield 工具 + artifact 落盘超限引用 | 🟡 JSON-in-text 提取 + 2 次重试 + 64KB 截断 | 低 |
| 16 | async job manager | 注册/取消/投递重试/retention/poll ladder + 完成自动投回会话 | ❌ | 高 |
| 17 | todo 追踪 | eager prelude（tool_choice 强制）+ 完成提醒续跑 + mid-run 对账 | 🟡 状态机+持久化有，护栏缺 | 中 |
| 18 | 自动标题 | tiny 模型生成 + 全局索引 + 低信号过滤 | ❌ 仅手动改名 | 中（两域重复确认） |
| 19 | snapcompact inline | 每请求大工具结果→PNG 帧变换 + provider 图像预算 + 节省账本 | ⚠ 仅作压缩后端 | 低 |
| 20 | 统一 OutputSink/artifact | 截断策略统一 + 超限全文 artifact | ❌ 各处截断分散 🟡 | 低 |
| 21 | 行为测试 | task/turn-recovery/streaming 等行为测试族 | engine.rs/task_tool.rs 无内联测试 🟡 | 中 |

### 2.8 外围功能模块（FeatureMods，28 条）

omp 齐全（advisor 完整运行时/记忆四层/goals/plan-mode/security/secrets/auto 系/语音全栈/ssh 深度/export/blob-broker/web 生态/internal-urls 15 scheme/遥测三信号/browser-relay/markit/工具族 utils）。Gyre 覆盖不均。

**高优**：secrets 管线级双向脱敏（omp per-install HMAC placeholder + 出入消息 transform；Gyre 仅 advisor 通道 regex——**工具结果/日志/消息中密钥完全不脱敏**）。
**中优**：advisor 运行时（消息指纹去重/loop-guard/增量转录/失败回滚/专用模型）；goals objective 文本 + paused 状态机 + goal 工具（⚠ Gyre 仅预算记账）；plan-mode propose→批准→handoff→模型切换→计划保护闭环（Gyre 仅写限制 🟡）；SSH 深度（ControlMaster 复用/文件传输/sshfs/ssh:// 协议；Gyre v1 每命令冷连 🟡）；export 会话导出+加密分享（❌）；web 搜索生态（27 provider + 查询规划 + 80 站点 scrapers；Gyre 双链+4 站点 🟡）；internal-urls 补 scheme（缺 agent://、history://、omp://、rule://、security://、vault://、ssh:// 及补全 🟡）；memories 后台 rollout 整理管线（两阶段 + lease/heartbeat；Gyre 任务末单次合并 ❌）；autolearn 自动 capture 回路（🟡 有 learn 工具无控制器）；commit 提交生成模块（❌）；markit 文档→markdown（pdf/docx/pptx/xlsx/epub ❌）；telemetry metrics+logs 三信号（🟡 仅 span）。
**低优**：记忆后端注册表（五后端 vs 两内置 ⚠）；mnemopi SQLite/embed worker/bank 分域（⚠ fastembed 单体）；hindsight 远程全量（🟡 mental-models 已移植）；security findings 生命周期+SARIF（⚠ 仅扫描工具）；autoresearch/cleanse/compress/if-bench（❌）；语音栈全缺（stt/tts/live/pi-voice ❌）；blob-broker（❌）；browser-relay 扩展中继（⚠ 自研 CDP 启动）；collab replication-shrink（🟡）；qrcode/session-color（❌）；图像加载降采样/SVG 光栅化 + shell snapshot 别名过滤（🟡）；auto-thinking（⚠ 已移植，仅 effort 映射差异）。

### 2.9 构建 / CI / 发布（Build，20 条）

omp：947 行 CI 20 job（7 目标发布矩阵 + 签名公证 + release_github_verify 下载验证 + npm/brew + install_methods + concurrency/paths/dispatch）+ scripts/ 55 文件（release.ts 编排/install.sh 三模式/session-stats 语料分析/脚本自带测试）+ Docker 四阶段 + nix + bazel + deny.toml/about.toml/NOTICES。
Gyre：rust.yml 唯一 workflow（lint+test+build 2 目标）；无 scripts/、无任务运行器、无 Dockerfile、无 deny/NOTICES、无安装器。

| # | 项 | 级 |
|---|---|---|
| 1 | 一键发布编排（版本重写+changelog+原子 tag+CI watch；Gyre 手工改版本打 tag） | 高 |
| 2 | 发布目标矩阵 7 vs 2（缺 linux-arm64/musl×2/darwin×2/win32） | 高 |
| 3 | cargo-deny + about.toml → THIRD-PARTY-NOTICES（vendor 四 fork 的许可证义务） | 中（晨间 H18' 列高，本轮按合规时效性判中高之间，维持高） |
| 4 | SHA256SUMS 汇总 + 发布后下载验证 job | 中 |
| 5 | musl 静态二进制 + 装后冒烟 | 中 |
| 6 | curl\|sh 安装器（三模式）+ install-tests | 中 |
| 7 | CI 硬化（concurrency release 组不取消/paths/dispatch/npm ci——`rust.yml` 现用 `npm install`） | 中 |
| 8 | Docker 多阶段镜像 | 中 |
| 9 | 基准入 CI（criterion 编译门；4 个 bench 无 CI 门）+ eval-bench 跑批 | 中 |
| 10 | 任务运行器/开发者脚本面（justfile 最小四条起步；Gyre 命令只藏 CI yaml） | 中 |
| 11 | session-stats 会话语料分析管线 | 低 |
| 12 | changelog 自动化（🟡 格式已对齐，搬运靠手） | 低 |
| 13 | prompt token 台账 / rewrite-system-prompt 管线 | 低 |
| 14 | macOS 签名公证（secret 缺省跳过模式） | 低 |
| 15 | Homebrew tap | 低 |
| 16 | Nix 打包 | 低 |
| 17 | ⚠ npm 分发面（Gyre 纯二进制路线，合理不移植） | — |
| 18 | ⚠ bazel/自托管基建（规模不匹配，合理不移植） | — |
| 19 | actionlint（omp 自身也未挂 CI 门） | 低 |
| 20 | workspace 死条目（tree-sitter "0.25" 无消费者，晨间已记，维持） | 低 |

---

## 三、相对晨间报告（09-05）的增量

**FIXED（`fdf92c9`，本轮直读确认）**：
- H2' MCP 协议版本出价：`client.rs:129` 出 2025-11-25，`set_protocol_version` 协商降级，HTTP 传输握手后带 `MCP-Protocol-Version`。
- H3' openai-responses 伪适配：SUPPORTED 收窄 + `supports_excludes_unimplemented_responses_wire` 回归测试——「路由谎言」消除，剩余为诚实的「未实现」缺口（Proto #2）。

**PARTIAL**：H1' 提交在途工作——`fdf92c9`+`667a25f`+`b840e4f` 已入库；仍有 41 个未提交文件（全部 `web/`，前端审计后续）。前端审计 P0×7+P1×16 属质量修复，**不触及** M7-M10 功能缺口（per-tool 工具卡/ask options 线协议/消息队列/图像回显维持）。

**PERSISTS（本轮直读/双重确认）**：H16' update_tx 生产者（全部工具 `update_tx: None`）；H4' RPC 握手（rpc.rs 三消息）；H5' TurnRecovery；H6' web_search 链；H7' MCP 通知消费；H8' bash 后台；H9' hub 12op；H10' read 文本选择器（archive/sqlite/ipynb 选择器已落地，`:N-M`/`:raw`/`:img` 仍缺）；H11' ast 语言面；H12' iso 后端；H13' xdev 通用挂载；H14' system prompt 工程化；H15' 认证链/OAuth；H17' task 结构化半边；H18' cargo-deny；H19' 发布矩阵；H20' edit replace 模式。

**NEW（晨间未记录，本轮新发现 ≥ 40 条，择要）**：
1. hashline 锚定闭环断裂（Tools #1，正确性级）。
2. collab 线协议整体分叉 proto=1 vs proto=3，guest 不可操控 host（Proto #22-23，需产品决策）。
3. ACP tool_call_id=工具名致并行同名工具不可区分（Proto #19，正确性）；authenticate 空实现误导致绪。
4. 会话文件无 header/版本化，resume 语义贫瘠（RuntimeCore #1）。
5. RPC 仅 4 命令 vs ~50，且无宿主反向通道——headless 驱动面不成立（Proto #14-17）。
6. CLI 保留字防误注入缺失（Cmds #2，防误计费）。
7. Esc 无法中止进行中回合（Cmds #23，交互安全）。
8. 模型目录/发现/角色/选择语法/采样面整缺（Config #2-8）。
9. 设置无运行时单例/热更新/内省（Config #1-2）。
10. .env 不加载 + `${VAR}` 空串静默（维持，Config 复核）。
11. 子代理定义化/spawn-policy/只读面/软预算/持久复活全缺（RuntimeCore #10-14）。
12. agent-plugins.org 标准包、managed skills、市场链、插件运行时加载（Ext #5-9）。
13. skills 跨工具 provider（claude/codex/opencode/github）缺失（Ext #4）。
14. vendor/pi-builtins 内容级漂移新证据（grep.rs/ls.rs 行数差，方向待确认——见 §五）。
15. 工具分级暴露（loadMode）与统一超时表缺失（Tools）。
16. README/config 文档漂移点：max_turns 示例 10000 vs 默认 1000。
17. 悬空 tool_call 修复、--continue、外部会话导入（RuntimeCore #4）。
18. i18n ru/ja 各缺 13 key 已由前端审计 P0-7 修复（web 端）；CLI 侧 goal.* 7 键缺口维持。

---

## 四、分级缺失清单（汇总）

### 高优先（正确性风险 / 平台承重 / 生态入口，28 项）

| # | 项 | 域 | omp 锚点 | 一句话建议 |
|---|---|---|---|---|
| 1 | hashline 锚定闭环（read 输出 tag + 读取快照 + stale 校验） | Tools | `read.ts:1495-1509` | read 侧接 format+snapshots，apply_hashline 读共享快照 |
| 2 | read 文本行选择器族 `:N-M/:raw/:img` | Tools | `read-selector.ts:22-77` | split_path_selector 后接 parseSel，行区间/绕过渲染/图像分支 |
| 3 | RPC ready 帧+协商+分片（先于命令扩容） | Proto | `rpc-types.ts:144-150` | ~300 行握手协议，NDJSON 兼容 |
| 4 | RPC 命令面扩至会话控制最小集（get_state/set_model/set_thinking_level/get_messages/steer/abort/compact） | Proto | `rpc-types.ts:29-119` | 沿 RpcEnvelope 增 data 字段分批 |
| 5 | ToolExecutionUpdate 生产者（edit/write send partial） | Tools | `tool-execution.ts:1122-1205` | 管道已通，两工具内 send 即通水 |
| 6 | TurnRecovery：empty-stop 有界重试 + 意外停止分类 + fallback cooldown | Runtime | `turn-recovery.ts:735,820,1441` | `agent/turn_recovery.rs` 先两机制 |
| 7 | 流式编辑守卫 + 工具调用循环守卫 | Runtime | `stream-guards.ts` | engine 流式段预检 + args-hash 计数 |
| 8 | MCP server→client 通知消费 + 重连/熔断 | Ext | `manager.ts:302-305,1038-1097` | stdio/http 读循环分发→registry 刷新 |
| 9 | MCP prompts/resources/订阅 + server instructions + server→client 应答 | Ext | `client.ts:458-506,320-354,50-66` | initialize 已有 result 可取 instructions |
| 10 | MCP OAuth 全链（可先 1-2 家验证架构） | Ext | `oauth-flow.ts` | RFC 8414 + PKCE + 回环回调 |
| 11 | MCP 工具缓存 + 延迟连接 | Ext | `tool-cache.ts`、`manager.ts:676-723` | 配置 hash 键缓存 + Deferred 工具 |
| 12 | web_search provider 链扩容 + recency | Tools | `web/search/index.ts` | 接 brave/tavily + after: 映射 |
| 13 | bash async 参数 + async job manager（后台任务簿记） | Tools/Runtime | `bash.ts:824-1102`、`async/job-manager.ts` | `agent/jobs.rs` + supervisor 挂载 |
| 14 | hub 监督面补 wait/inbox/jobs/cancel（+start/ps/logs/stop） | Tools | `hub/index.ts:80-127` | 扩 hub_tool op，映射 supervisor API |
| 15 | 系统提示词工程化：Workflow 六阶段 + Delivery 契约 + Delegation 门控 + Tool Policy 四段 | Prompts | `system-prompt.md` | 中文版移植为共享主体模板 |
| 16 | SYSTEM.md 覆盖链 + `--append-system-prompt` | Prompts | `system-prompt.ts` loadSystemPromptFiles | 配置键 + 装配双路径 |
| 17 | 会话文件 header + 版本化条目模型 | Runtime | `session-entries.ts` | SessionRecord::{Header,Node,Meta} + migrations |
| 18 | 子代理定义化（[[agents]] + spawn policy + 只读工具面 + worktree 隔离接线） | Runtime | `task/discovery.ts`、`worktree.ts` | 复用 iso 原语接 task isolation 配置 |
| 19 | 认证链分层（CredentialSource trait + env 层）+ OAuth 最小集（1-2 家）+ SQLite 凭据存储 | Config/Proto | `auth-storage.ts:5639-5693` | 先 anthropic/zai 高频通道 |
| 20 | openai-responses 真 wire +（按需）vertex | Proto | `providers/openai-responses.ts` | 枚举位已预留，复用 SSE 助手 |
| 21 | 内置模型目录 + 运行时发现（ollama 等）+ 模型角色 10 个 | Config | `model-registry.ts`、`model-discovery.ts`、`model-roles.ts` | 静态描述符表起步 |
| 22 | 技能跨工具 provider（claude/codex/opencode/github）+ per-source 开关 | Ext | `skills.ts`、`discovery/*` | SkillProvider trait 已就绪，补四实现 |
| 23 | hook 事件面扩至 shared-events 全集 + 文件式 hook（声明式 [hooks] 外部命令起步） | Ext | `shared-events.ts`、`hooks/loader.ts` | on_event 返回 Result 枚举化 |
| 24 | CLI 子命令层（clap Subcommand）+ 保留字防误注入 | Cmds | `cli-commands.ts` | models/config/usage 先行；保留字提示一行防误 |
| 25 | ACP 修正面：tool_call 真实 id+kind+status 机、authenticate 实装、session/list\|resume、availableCommands | Cmds/Proto | `acp-agent.ts:629-1202` | 先修 tool_call id（正确性） |
| 26 | 会话级事件通道（auto_compaction/auto_retry/model_changed/notice）+ AgentStart | Proto | `agent-session-events.ts` | 扩 AgentEvent 或 SessionEvent 双通道 |
| 27 | secrets 管线级双向脱敏（HMAC placeholder + 工具结果/消息 transform） | Feature | `secrets/*` | SecretObfuscator 下沉 agent-core 管线 |
| 28 | 发布链：发布编排 + 目标矩阵扩容（arm64/musl/darwin）+ cargo-deny + NOTICES | Build | `release.ts`、`ci.yml:522-670` | xtask release + matrix + deny 全量 check |

**条件性高优**：Esc 分级中断/双击 Esc 回退/键位系统（Cmds #21-23）——需先做 raw 模式按键层架构决策（crossterm），短期可先做「单行 `.`/abort 关键字取消」协议；collab 互操作改造（Proto #22-23）——需产品决策后重写 frame.rs。
**顺序硬约束**：RPC 先握手后扩命令（#3→#4）；会话 header 先于树导航用户面/fork 语义/usage 条目（#17→Runtime 中优族）。

### 中优先（能力纵深 / 体验差距，~70 项，按组）

- **工具组**：edit replace/apply_patch 模式 + settings fuzzy 双轨；grep case/gitignore/skip 分页+上限对齐+单文件命中上限；glob path 列表/gitignore/hidden/limit/mtime 排序；read 富格式（PDF/URL reader-mode/自动 elision footer/环断提示）；task 参数面（name/agent/effort/schemaMode/isolated）；github 补 search_*/run_watch；lsp 补 6 动作；browser tab 会话+observe/aria；eval 补 rb/jl；todo op 语义对齐；ask questions[]；memory retain items[]+invalidate；manage_skill；工具 loadMode 分级+统一超时表；debug 补低频动作（可选）。
- **配置组**：settings 单例+热更新+内省+`config` CLI；`--config` overlay；.env 三级加载+${VAR} 告警；模型选择语法（provider/id、:thinking 后缀）；fallback 冷却回切+usage-aware；`!cmd` 凭据；重试参数可配置；per-model thinking 元数据；采样 top_p/top_k/tier；security/secrets 开关键；workspace.additionalDirectories；MCP 行为开关；provider 门控。
- **CLI/交互组**：会话生命周期命令族（/new /clear /drop /rename /move /pin /handoff /shake）；/branch /fork /tree；/login /logout；/memory；/mcp add|remove|test；/todo append；/resume 选择器；/usage /jobs /context /stats /changelog；/goal 语义改名或对齐；/plan toggle；消息队列；Ctrl+G 外部编辑器；行为测试补齐。
- **协议组**：流式事件块边界（13 变体）；Usage 扩展（reasoning/context/cttl）；内容块补 redactedThinking 等；Thinking 多 mode；OpenAI compat 层（reasoningDisableMode/多 system 合并先行）；缓存 TTL/prompt_cache_key；RPC 宿主反向通道（extension_ui 子集）；RPC 事件映射补全（tool id/结果文本/子代理帧）；ACP 权限选项机+fs 桥；SDK/AgentBuilder。
- **运行时组**：fork 语义；树导航用户面（依赖 #17）；resume 完备性（--continue/悬空修复/外部导入）；中断重放分流；fallback 冷却回切+留痕；子代理软预算；持久复活；todo eager+完成提醒；自动标题；失败轮留痕；统一 OutputSink/artifact；engine/task_tool 行为测试。
- **扩展组**：agent-plugins.org 标准包；市场安装链；插件运行时加载；自定义工具（声明式先行）；自定义命令 TS/bundled/跨工具；MCP .mcp.json 多源+disabled；MCP legacy SSE；managed skills；frontmatter 严格校验；热重载；mcp http/stdio 传输层测试。
- **提示词组**：人格预设；rules generic/domain 通道+去重；Internal URLs 章节；Tool Inventory；workstation 块细节；上下文文件包装+@import；工作区树/多根；运行期通知模板族（triage 后）；子代理角色模板；goals/compaction 提示词族；主题系统（TUI token 化 + 2-3 内置主题）；安全条款块；提示词行为测试。
- **外围组**：advisor 指纹/loop-guard/增量；goals objective+goal 工具；plan-mode propose 批准流；SSH ControlMaster/传输；export/share；web scrapers/provider 补充；internal-urls 补 scheme；memories 后台整理；autolearn 控制器；commit 模块；markit；telemetry metrics/logs；per-tool 工具卡（M8 十个先）；ask options 线协议（M7）；消息队列（M9）；图像回显（M10）；snapcompact CJK '?' 修复（维持）。
- **构建组**：SHA256SUMS+verify job；musl；安装器；CI 硬化；Docker；基准 CI 门；justfile；配置/凭据行为测试；依赖升级批（arboard wayland/image 四格式/tiktoken 0.11）。

### 低优先（长尾 / 按需拉动，~35 项）

语音栈全缺（stt/tts/live/pi-voice）· computer 桌面操控 · pi-vcs · pi-voice · metaharness + typescript-edit-benchmark · browser-relay 扩展 · mnemopi 深语义（SQLite/embed worker/bank）· hindsight 远程全量 · blob-broker · autoresearch · cleanse · compress · if-bench · legacy SSE MCP · read PDF/office（随 markit）· pi-natives 长尾（clipboard/fs-watch/diff 引擎/文本宽度等 ~40 面）· write 归档/SQLite 行写 · inspect_image（并入 :img）· Web 键位重映射/emoji/LaTeX · collab 托管 relay/QR + replication-shrink · qrcode/session-color · Docker/Nix · session-stats · changelog 自动化 · prompt token 台账 · macOS 签名 · Homebrew · actionlint · 嵌套仓库上下文 · model identity 分类 · StopReason wire 拼写 · todo op 对齐 · ServiceTier · /git TUI · 技能暴露方式对齐 · /fresh 语义改名 · i18n goal.* 7 键（一行修） · workspace 死条目 · max_turns 文档修正 · swiftshooter/prewalk · snapcompact inline 每请求变换 · 统一 OutputSink · vendor 版本锚注释更新。

### 明确不迁移（维持 + 本轮新增确认）

- TUI 差分渲染引擎（25k 行，与 rustyline+Web 路线冲突；键位缺口以 raw 模式最小按键层替代）。
- TS 进程内插件 1:1（结构性不可；以 hook 全集 + 子进程扩展协议 + 声明式工具/命令替代）。
- npm 分发面/robomp/Python omp-rpc 客户端（后者随 RPC 握手落地后按需）。
- bazel/自托管 runner 基建（规模不匹配）。
- collab-web React guest（保持单文件形态，渲染协议可选对齐 wire）。
- legacy pi 兼容 shim（无存量生态负担）。
- omp nix/bazel CI 覆盖面。

---

## 五、不确定与需进一步确认（18 项）

**新产生**：
1. **vendor/pi-builtins 漂移方向**（Tools 新证据）：grep.rs 上游 1785 vs vendor 1945 行、ls.rs 5279 vs 5389 行，内容不同但无法只读判定领先/落后；需与上游快照逐文件 diff 定基线后再定同步策略（晨间「唯一漂移点是 xutf」的结论不再完整）。
2. **collab 是否以 omp 互操作为目标**：决定 frame.rs 重写（proto=3/peerId envelope/控制消息）还是文档声明独立协议；当前「移植自 collab-web」注释有误导。
3. **`--rpc` 目标集成方**：决定命令集裁剪（最小集推测 get_state/set_model/set_thinking_level/get_messages/steer/compact）与是否需要 rpc_chunk。
4. **hashline read-tag 接线 vs 维持宽松模式**：产品决策（omp 全闭环 vs 接受 stale 检测弱化）。
5. **ACP fs 桥的 Zed 需求场景**：决定 medium 项排期（依赖客户端能力协商）。
6. **Gyre crates/tts、crates/stt 现有实现完成度未逐一打开**：语音配置键 gap 的工作量取决于此。
7. **SkillsConfig.enable_commands 注释过时**：注释称「/skill:<name> 远期未实现」，实际 repl.rs:423 已实现——删注释或接开关。
8. **Model.supports_thinking 恒 false**：有意保守还是待接目录的占位。
9. **key_rings 轮换触发顺序**：core 消费点未逐行核实（装配已确认传入；「限流/撤销换下一个」来自文档注释）。
10. **rustyline 是否已含 Ctrl+R 历史搜索**（影响键位缺口优先级）。

**沿袭晨间（已复核/仍开放）**：
11. fastembed（ort）在 musl/aarch64 的工具链兼容性——扩发布矩阵前必须先证。
12. checkpoint×压缩 pinning：本轮定论——Gyre 压缩仅保护 skill-read 结果（`tool_protection.rs`），无位置 pinning；**当前即时回卷无数据风险**，仅当实现「turn_end 延迟应用」时成为前置（晨间漏追踪项，状态：条件性缺口）。
13. KeyRing「限流自动切 key」与 engine 429 退避两处口径（晨间 #2 维持，与 #9 合并核查）。
14. RPC id 类型策略（Gyre u64 vs omp 可配 string，跨协议统一待定）。
15. ACP UsageUpdate 是否规范 update 类型。
16. workspace.dependencies 其余死条目（仅 tree-sitter 一项已实锤）。
17. omp prompts/system ~95 运行期模板的逐个 triage（多数依赖上游特有工具面，移植优先级需按 Gyre 功能路线取舍）。
18. `.agent/` 与 `.gyre/` 双目录边界设计意图未文档化（晨间 #14 维持）。

---

## 六、路线图建议（顺序约束）

1. **立即**：提交 41 个在途 web 文件（H1' 收尾）→ hashline 闭环 + read 行选择器（两个工具域高优，各 ≤1 天，测试锁定）→ update_tx 生产者（edit/write 两处 send）→ MCP 版本出价/openai-responses 修复已入库 ✅。
2. **第一梯队（顺序硬约束优先）**：RPC 握手 → RPC 命令面最小集 → MCP 通知消费/重连 → async job manager + bash async → TurnRecovery 两机制 → web_search 链。
3. **第二梯队**：会话 header/版本化 → 树导航用户面 + fork 语义 → 子代理定义化 + 隔离接线 → 系统提示词四段 + SYSTEM.md 链 → 认证链 + OAuth 最小集 → hub 监督面。
4. **第三梯队**：CLI 子命令层（models/config/usage）+ 保留字 → ACP 修正面（tool_call id 正确性先）→ 模型目录/发现/角色 → skills 跨工具 provider + agent-plugins → 配置面中优批 → 提示词模板族 → 发布矩阵 + cargo-deny。
5. **按需**：中低优先各组按用户需求拉动；collab 互操作与 Esc 键位系统两个架构决策先行定夺。

**总量估算**：高优 28 项 ≈ 35-50 人日；中优 ~70 项 ≈ 80-110 人日；低优按需。无架构阻塞（两处决策点：collab 互操作、raw 模式按键层）。

---

## 附：本轮证据方法

- 10 路并行只读 scout（Cmds/Tools/Config/Ext/Prompts/Proto/RuntimeCore/FeatureMods/Build/PriorDocs），每路要求 omp_ref + gyre_ref 双侧 file:line 证据与 missing/partial/divergent 三态；完整结构化产出存档 `agent://Cmds`、`agent://Tools-2`、`agent://Config`、`agent://Ext`、`agent://Prompts`、`agent://Proto`、`agent://RuntimeCore`、`agent://FeatureMods`、`agent://Build`、`agent://PriorDocs`。
- main 交叉验证：`git log/status`（HEAD b840e4f；在途 41 文件全 web/）；六项承重结论直读复核（MCP 出价 `client.rs:129`、SUPPORTED `openai.rs:35,562-568`、rpc.rs 三消息 `:310-321,:832-843`、update_tx 全 None 十七处、fs.rs 选择器 `:52-56,:884-890`、压缩 pinning 语义 `compaction.rs`/`tool_protection.rs`）。
- 晨间报告 20 高优 + 45 中优逐条对照：2 项 FIXED、18 项 PERSISTS（含 3 项 main 直读确认）、若干降级/拆分；PriorDocs 五份历史文档交叉核对（08-23/feature-analysis 已落地声明视为已验证基线，不再重查）。
