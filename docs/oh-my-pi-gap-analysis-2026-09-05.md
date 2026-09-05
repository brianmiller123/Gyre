# Gyre × oh-my-pi 差距分析报告（2026-09-05）

> 调研对象：`third/oh-my-pi`（fork of badlogic/pi-mono，MIT；本地 checkout @ `18781d8295`，v18.1.2+5，**快照与 09-04 报告完全一致**）vs 本仓库 Gyre（Rust workspace，33 crates + vendor 4 crates，工作树有 80 文件未提交在途改动 +3391/−1801，**以工作树现状为唯一事实源**）。
> 方法：8 路并行只读 scout 分域精读（CoreLoop / Tools / CliProto / ConfigExt / LlmLayer / RustCrates / BuildCi / ResDocs），昨日基线 `docs/oh-my-pi-gap-analysis-2026-09-04.md` 仅作先验——每条结论均以今日工作树实读 file:line 重验（FIXED / PERSISTS / NEW）；main 对承重结论二次抽查（伪适配、RPC 命令面、read 选择器、MCP 版本出价、update_tx、empty-stop 六项直读复核全部属实）。
> 证据格式：omp 路径相对 `third/oh-my-pi/`，Gyre 路径相对仓库根。判定：❌ 缺失 · 🟡 部分 · ⚠ 偏差/形态不同 · ✅ 对齐或领先。

---

## 〇、结论速览

1. **omp 侧零漂移**：快照仍为 `18781d8295`（与昨日一致），本轮全部差异来自 Gyre 工作树的在途改动。
2. **昨日高优清单已修 4 项 + 行为偏差 2 项，全部在未提交工作树中**：
   - ✅ H1 Anthropic thinking 签名解析+回传（`llm/anthropic.rs:117-128,271-293,363-377,436-450` + 2 测试）
   - ✅ H2 Gemini thoughtSignature 回传（`llm/gemini.rs:169-207,311-315,519-527,545-556`，含 base64 校验）
   - ✅ H3 MCP 工具名命名空间（`mcp/tool.rs:126` `mcp__<server>_<tool>` + sanitize + origin-key 去重 + 4 测试）
   - ✅ H10 toolchain pin（`rust-toolchain.toml:1-5` `channel = "1.97.1"`）
   - ✅ 魔法关键词注入顺序（`agent/engine.rs:156-161`：通知**后于**用户消息，注释对齐 omp `agent-session.ts:5797-5801`）
   - ✅ `reflect` 语义（`tools/memory_tool.rs:185-207`：只读综合问答，空命中文案与 omp 逐字一致，行为测试 `:614-666`）
   - ✅ 半边落地：ask options（`tools/ask.rs:45-67`）、ToolExecutionUpdate 三层帧发射（`engine.rs:1354-1430` + `server/lib.rs:212-216` + web 帧类型）、stats 看板 + `/api/stats/trend`（`StatisticsPanel.tsx` 291 行）、goals 墙钟（`agent/lib.rs:49,115-124`）、prune per-file supersede（`context/compaction.rs:1070`）、Anthropic 缓存断点 MultiPoint 精确化（`llm/transform.rs:37-100`）、ast-grep-core 启用 + 5 tree-sitter 语法包 + pattern.rs search/rewrite、MCP stdio 防御件 + JSON-RPC batch、ACP session/load/close/set_mode + request_permission 全机制、favicon+品牌字体。
3. **本轮新发现 2 处正确性级风险**（昨日未识别）：
   - **MCP 协议版本出价过旧**：`mcp/client.rs:125` 硬编码 `"2024-11-25"` → 实为 `"2024-11-05"`；omp 出价 `2025-11-25` 且注明 **AWS Bedrock AgentCore Gateway 拒绝 <2025-11-25 的工具调用**（`mcp/types.ts:163-168`）——严格网关下 MCP HTTP 全挂。
   - **openai-responses 伪适配**：`llm/openai.rs:30-34` `SUPPORTED` 含 `Api::OpenAiResponses/OllamaChat`，但 `stream()` 恒 POST `{base}/chat/completions`（`:55`），全文件无按 api 分支——api=openai-responses 的模型被静默降级为 chat wire，Responses 专属能力（reasoning items/encrypted replay/previous_response_id）全失且报错误导排障。
4. **omp 侧口径修正**（基线计数偏差）：79 provider 注册（非 ~90）、19 个 OAuth 流模块（非 12）、41 个 CLI 子命令（非 42）、58 个 flag 令牌（非 43）、77 个内置 slash（非 73）、docs 129 篇（非 130）；cargo-deny CI 实际只跑 licenses+sources（advisories 配置存在但未执行）；actionlint 非 CI 门。
5. **分级清单**：高优先 20 项（含 2 新 bug）、中优先 45 项、低优先 30 项，见 §三。不确定项 17 条见 §四。

---

## 一、结构与模块映射（今日现状）

### 1.1 双方规模

- **omp**：TS 1,453k 行（4,665 文件）+ Rust 223k 行（401 文件）；packages/ 20 + crates/ 8 + vendor brush-core；scripts/ 57 文件；CI 947 行 20 job；docs 129 篇；TS 测试 2,349 文件。
- **Gyre**：Rust ≈256k 行（含 vendor 169k）+ web/c5-ui ≈10k 行 TS；33 crates + vendor 4；无 scripts/、无 justfile；CI 3 job；docs 12 篇；自研 Rust 测试 **1,088**（逐 crate 计数：tools 177 · agent 91 · llm 88 · memory 79 · context 77 · hashline 67 · cli 64 · config 61 · acp 33 · ttsr 32 · collab 27 · core 23 · snapcompact 20 · ast 20 · skills 22 · browser 19 · prompt 18 · dap 18 · server 18 · lsp 17 · proxy 16 · mcp 15 · advisor 15 · eval 13 · shell 12 · swarm 12 · search 11 · i18n 7 · iso 5 · discovery 5 · supervisor 3 · pty 3 · **telemetry 0**）；前端 0 测试。

### 1.2 模块映射状态变化（相对 09-04）

| omp | Gyre | 昨日 → 今日 |
|---|---|---|
| crates/pi-ast | crates/ast | ❌ 5 语言 → ⚠ 新增 pattern.rs 六档严格度 search/rewrite + ast_search/ast_rewrite 工具接线 + 5 语法包（仍 5 vs 57 + 4 子能力缺）|
| packages/coding-agent（协议面）| crates/acp + server | 🟡 → 🟡+ ACP 增 session/load/close/set_mode + request_permission（600s 超时 + ACP v1 四选项 + HTTP 回执路由）；REST 25+ → **27 路由** |
| packages/stats | crates/server /api/stats | 🟡 → ✅+ 前端 StatisticsPanel 看板 + trend 端点落地（autolearn/managed-skills 仍无）|
| crates/pi-iso | crates/iso | ❌ 维持（1/8 后端，`native()` 恒 Rcopy，`iso/lib.rs:82-87`）|
| crates/pi-vcs / pi-voice | 无对应物 | ❌ 维持（gix/jj/webrtc/opus 全库 grep 零命中）|
| vendor 四件套 | vendor/ | pi-walker/pi-shell/brush-core ✅ 同步（仅 rustfmt 行偏移）；pi-builtins ⚠ xutf 漂移维持（omp `wc.rs:1109`/`jq.rs:627` 用 xutf vs Gyre `:1118`/`:640` unicode-width）|

---

## 二、分域差距明细

### 2.1 核心循环与会话（CoreLoop）

**已修**：见速览 #2（注入顺序、reflect、goals 墙钟半边、prune per-file 半边）。

**仍缺（18 条基线 ❌ 重验：15 仍缺、2 半修、1 偏差维持）**：

| # | 项 | omp 位置（用途） | Gyre 现状 | 影响 / 建议 |
|---|---|---|---|---|
| 1 | TurnRecovery 恢复族 | `session/turn-recovery.ts` 2,657 行统一 owner：empty-stop 重试+reparent（`:73,:749-800`）、unexpected-stop tiny 分类器（`:820,:874`）、fallback cooldown/到期回归（`:1441-1454,:1945-1946`）、工具批失败重放（`:2585-2619`）、usage-limit 凭证切换（`:2177-2184`） | `agent|core|context` grep `empty_stop/reparent/cooldown/replay` 零命中；空回复在 `engine.rs:1021-1028` 直接「任务完成」（main 已复核）；fallback 每轮全新尝试（`:506-540`） | 长跑会话在脏 API/弱模型/限流下失败率放大。建 `agent/turn_recovery.rs`，先 empty-stop + cooldown 两机制 |
| 2 | async/job-manager 后台任务 | `async/job-manager.ts` 924 行：投递重试退避 `:3-5`、retention `:6`、并发上限 `:7`、poll ladder `:21-33`、owner-scoped cancel | grep `AsyncJob|JobManager` 零命中 | 长任务占死工具轮。与 H14 bash 后台同批 |
| 3 | 子代理复活 persisted-revive | `task/persisted-revive.ts:41-48` + `registry/agent-lifecycle.ts`（parked→revived 冷复活） | `task_tool.rs` 619 行子代理即弃 | hub v2 前置件 |
| 4 | 子代理软预算 | `task/executor.ts:120-133`（SOFT_REQUEST_BUDGET + 1.5x 强停 + grace 5） | 无请求预算计数 | 成本护栏缺 |
| 5 | 子代理 fallback 链继承 | `task/executor.ts:176-273`（`subagent:<id>` 角色钉链） | `build_sub_agent`（`task_tool.rs:146-186`）仅单 model | 弱模型子代理无 fallback |
| 6 | xd://propose plan 批准流 | `interactive-mode.ts:3161-3185` + `plan-mode/approved-plan.ts` | grep `propose|approved_plan` 仅 ast 暂存命中 | 复用 `pending_rewrites` 骨架（`agent/lib.rs:381`） |
| 7 | 自动标题 | `utils/title-generator.ts:131-190`（tiny 本地/在线/角色回退、`<title>` 标记） | 仅手工 `_titles.json`（`context/persistence.rs:322-385`） | smol/tiny 角色生成 |
| 8 | /compress 模式+focus+rewrite/approve | `session/compact-modes.ts:22-103` + `settings-schema.ts:2515-2517` methodOrder | 固定三连 shake→summarize→prune（`cli/main.rs:1740-1748`） | 模式选择/focus 指令/交互协议全缺 |
| 9 | todo eager prelude + 完成提醒 | `session/todo-tracker.ts:165-330`（强制 tool_choice prelude、remindersMax、mid-run nudge） | `tools/todo.rs` 仅阶段机+单活跃不变量 | 模型合规性护栏缺 |
| 10 | IRC bus ACK/唤醒 | `irc/bus.ts:38-41`（`outcome: injected|woken|revived|failed`）+ `:146-197` parked 复活投递 | `core/hub.rs` 纯内存无回执 | 多代理编排「等待」半边联动 H13 |
| 11 | sharpshooter / prewalk / cleanse / 角色池 | `settings-schema.ts:2949-2986,543-552`、`commands/cleanse.ts`、`config/model-resolver.ts:31` | 全部零命中 | 低优先长尾 |
| 12 | pins / usage 命令 | `session/session-pins.ts:8-50`、`session/session-stats.ts` | grep pin 仅 `tokio::pin!`；repl 无 /usage | 低 |
| 13 | goals 状态机 | `goals/state.ts:3` 五态 + `goals/runtime.ts`（中断 auto-pause、跨重启持久化） | `agent/lib.rs:69-145` 仅预算记账（墙钟半边今日已落地） | 状态机+持久化半边缺 |
| 14 | prune supersede 分组 | `packages/agent/src/compaction/pruning.ts:26-31,67,73-79,183-207,433,45-51`（supersedeKey 分组、`\u0000` 规则、占位符、缓存温区） | per-file 已有（`:1070`），无分组/key 规则/占位符/温区保护 | 低 |
| 15 | summarize 远程超时 | 配置化方法链（`settings-schema.ts:2586-2619`） | `context/compaction.rs:203` 硬编码 10s | 配置化即可 |
| 16 | 持久化 entry 元数据/迁移链 | `session/session-entries.ts` + `session-migrations.ts` 版本链 | `core/context.rs:95-104` SessionNode 仅 3 字段；迁移仅 1 条 | 低 |
| 17 | aside commit/discard 协议 | `packages/agent/src/types.ts:34-50`（thunk 型 AsideMessage + commit/discard symbol） | aside 信道+双 drain ✅；无 inject-or-drop 生命周期（过期诊断照灌） | 低 |

**新发现（基线未记录，6 条）**：
1. **date-cwd 提醒缺失**（中低）：omp `session/date-cwd-reminder.ts:1-43` 挂首条 user turn 保 system 前缀稳定+跨午夜刷新；Gyre 无日期注入（`engine.rs:117` 仅 cwd）——模型无当前时间感知。
2. **provider 图像预算裁剪缺失**（中）：omp `session/provider-image-budget.ts:1-34` 按 provider 限额 LRU 裁剪 tool-result 图像；Gyre 多模态直传——严格 provider 多图长会话 400/429 崩风险（正确性邻近）。
3. **后台预压缩带 + idle compaction 缺失**（中）：omp `session/speculation-lead.ts`（threshold−lead 预启动 summarizer）+ `compaction.asyncEnabled/idleEnabled`；Gyre 全同步轮内触发，近限瞬间才付 summarize 延迟。
4. **ToolChoice 指令队列缺失**（低）：omp `session/tool-choice-queue.ts`（迭代化多连强制指令+回调）；Gyre 仅单发 Soft/Hard。
5. **流式编辑守卫/工具调用环检测缺失**（低）：omp `session/stream-guards.ts`（edit 生成文件守卫、ToolCallLoopGuard）；Gyre 有 TTSR/Harmony 但无此二守卫。
6. **压缩配置面缺失**（低中）：omp `settings-schema.ts:2493-2690` 十余旋钮；Gyre 压缩参数全硬编码。

### 2.2 工具面（Tools）

omp 权威清单 29 BUILTIN + 3 HIDDEN（`builtin-names.ts:6-41`）逐一判定：✅7 🟡/⚠14 ❌11。**昨日 6 条 ❌ 无一今日落地**。

**❌ 仍缺（高优 6 条）**：
| 工具 | omp 位置 | Gyre 现状 |
|---|---|---|
| read 选择器族（H16） | `read.ts:530-538`（`:N-M,:N+K,:raw,:img` 多段内联）、`:579-610` 防环、`:1312-1347` 目录树、`:1414-1441` markit | `fs.rs` schema 仅 path+summary；仅 `:conflicts`（main 已复核 `fs.rs:25-57`）；无文本行选择器/防环/目录树/PDF-office |
| bash 后台作业（H14） | `bash.ts:316-331` async?/pty?、`:824-874` AsyncJobManager、`:1041-1102` 自动转后台、`:1117` 交互 PTY | `shell.rs:54-163` 纯前台；`pty/tool.rs:19-37` 一次性无 send-keys |
| edit 五模式（M11） | `edit/index.ts:44-48` hashline/replace/patch/apply-patch/sloppy | `hashline/tool.rs:45-67` 单 hashline；fuzzy 容错已有 |
| web_search 链（H7） | `web/search/index.ts:43-47` recency schema + providers/ 24 家 | `web_search.rs:53-70` DDG+Searxng 双链无 recency（main 已复核）——单点限流风险最高项 |
| hub 12 op（H13） | `hub/index.ts:80-127` 12 op + `:367-518` wait 统一竞争（消息∪作业∪超时∪steer） | `hub_tool.rs:88-99` 3 op send/recv/list（main 已复核） |
| task 结构化半边 | `task/types.ts:114-154` name/agent/outputSchema/schemaMode/isolated + `yield-assembly.ts` 信封 + `agents.ts:9-16` 花名册 | `task_tool.rs:409-424` 仅 task/tasks/output_schema |

**❌ 其余**：manage_skill（`crates/skills` 无 `impl Tool`）、computer（屏幕操控整缺）、hidden 三件 yield/goal/think、inspect_image（小模型视觉委派）、write 归档/SQLite 行写（L15）。

**⚠ 形态偏差**：github（omp 11 op `gh.ts:74-76` 含 search_issues/search_code/pr_checkout/pr_push/run_watch vs Gyre 10 动作多 CI logs+graphql 缺 search/watch）、glob（参数名 pattern vs path，硬上限 100 无分页）、grep（缺 case/gitignore/skip，硬上限 50 无分页）、lsp（缺 status/capabilities/request/reload/rename_file/工作区 `*`）、todo（动词集不同，omp `:584-593` 有 op 推断修复）、ask（单问 vs questions[]/multi/recommended/超时自动选中）、eval（缺 rb/jl）、browser（缺标签页监督/relay/aria 快照）。

**新发现**：
1. grep cap50 / glob cap100 超限**静默截断**（`search.rs:56,114`）；omp `DEFAULT_FILE_LIMIT=20`+`skip` 分页续读（`grep.ts:96-98,1339-1382`）——并入 M28 一并修。
2. omp bash 三项前端语义（leading `cd` 提取 `:913-926`、direnv preflight `:1115-1125`、client-bridge 终端路由 `:1033-1040`）为 H14 设计输入。
3. Gyre eval 经环回 HTTP 桥做子进程工具回调（`eval/bridge.rs:38-46`）——与 omp eval bridge 同构，基线未记。
4. omp vibe_* 5 件套持久 worker 会话（`vibe.ts:103-252`）——Gyre 无对等，低。
5. Gyre ask 选项渲染为纯文本 prompt（`ask.rs:125-133`）——UI 无法结构化点击，与 M7/M8 耦合。

### 2.3 CLI 入口与协议（CliProto）

**本域无整条修复**；omp 口径修正：41 子命令（`cli-commands.ts:14-226`）、58 flag 令牌（`cli/flag-tables.ts:104-308`：31 string+3 optional+24 boolean）、77 内置 slash（六组 17+10+19+24+3+4）。

| # | 项 | omp | Gyre | 优先级 |
|---|---|---|---|---|
| 1 | CLI 子命令机制（M2） | 41 子命令注册表 + 分发链 `cli.ts:444-453` | `main.rs:33-101` 单层 Cli struct，无 `#[command(subcommand)]`；**41 个全部无**（含 models/config/usage/commit/stats/completions/grep/render/shell/git/worktree…） | 高（`agent models list`+`config get/set` 最优先） |
| 2 | Launch flags | 58 令牌单一事实源 `flag-tables.ts` | `main.rs:43-100` 仅 14 flags；点名 8 缺：`--thinking`/`--system-prompt`/`--append-system-prompt`/`--no-tools`/`--continue`/`--print`/`--max-time`/`--api-key` 全 ❌ | 高 |
| 3 | 内置 slash 62 缺失（M3） | 77 内置（modes 17/collab 10/session 19/lifecycle 24/marketplace 3/control 4） | `repl.rs:147-178` 30 项平铺（26 独有+4 别名），零新增；62 缺失（lifecycle 20/session 13/modes 14/collab 9/marketplace 3/control 3） | 中 |
| 4 | SlashSpec 元数据+协议内 slash（M4） | name/aliases/allowArgs/subcommands/inlineHint（`builtin-registry.ts:59-66`）+ RPC `get_available_commands`（`rpc-mode.ts:1154`）+ ACP `available_commands_update` + RPC 内直跑 slash/skill（`:1021-1023`） | `&[&str]` 平铺；`rpc.rs` 仅 prompt/cancel/ping，协议内无法执行 slash | 中 |
| 5 | RPC ready 帧+协商+分片+42 命令（H6） | ready 帧 `rpc-types.ts:144-150`、1MiB/64MiB 分片 `rpc-frame.ts:7-9`、42 命令 `rpc-mode.ts`、host 工具桥 `set_host_tools/set_host_uri_schemes`（`:1163-1176`）、`--mode rpc-ui` | `cli/rpc.rs` 3 请求类型（两处分发点 `:310-321,:832-843` 交叉确证）；无握手/无上限/无分片 | **高（顺序硬约束：先握手后扩命令）** |
| 6 | `--print` json（M5） | `modes/print-mode.ts:9-11`（-p 文本/--mode json 事件流） | 无 --print/-p/--mode | 中（复用 `rpc.rs map_event` 即得） |
| 7 | ACP 剩余缺口（L9） | listSessions `:712`/resumeSession `:729`/setSessionConfigOption `:772`/extMethod `:1122`（_omp/* 四方法）/authenticate 真实现 `:676`；sessionUpdate 10 类 | `acp/rpc.rs` 今日已增 session/load/close/set_mode + newTask/cancel 兼容 + request_permission 全机制 ✅；仍缺 list/resume/configOption/extMethod；authenticate/logout `{}` 存根；update 5 vs 10 类（缺 plan/session_info/config_option/current_mode/available_commands）；ToolCall 恒 completed 无流式 | 中 |
| 8 | Web 消息队列（M9） | `queue-input.ts` Ctrl+Q/`->`/Alt+Up dequeue | 27 路由无 queue 端点；忙时单条 steer | 中（纯前端可先做） |
| 9 | Python omp-rpc 客户端（L10） | `python/omp-rpc/`（client/protocol/host_tools/host_uris + 测试） | 无 | 低（被 #5 阻塞） |
| 10 | --profile/--alias 多配置、daemon broker（L21） | `flag-tables.ts:107-109` + profile-bootstrap | 无 | 低 |

**✅ Gyre 领先面（今日确认）**：REST/WS 27 路由（`server/lib.rs:1553-1585`：sessions CRUD、skills(+body)、mcp、enhance、upload、history、branches+switch、messages/{line} 删除、models、stats+trend、pause、resume、socks5、approval-mode、workspace、fs、file、collab）；ACP HTTP+SSE 双传输（omp 仅 stdio）；`--lang` 四语、SOCKS5 三 flag、`--otlp`、`--list-sessions`。

### 2.4 配置 / 扩展 / MCP / secrets（ConfigExt）

**已修**：H3（见速览）。**在途增强**：MCP stdio 防御件（4MiB 行上限/stderr 排空/超时清理/kill_on_drop）、HTTP JSON-RPC batch（`http.rs:126-134`）。

**MCP 八项**：
1. ✅ stdio+HTTP 双传输落地并加固（`stdio.rs:31-135` + `http.rs:16-268`：Mcp-Session-Id/协议版本头回传/保留头剥离/${ENV} 头/SSE 响应/DELETE 终止）。
2. ❌ **协议版本出价过旧（新·正确性）**：`client.rs:125` 出价 `"2024-11-05"`；omp `"2025-11-25"` 且 Bedrock AgentCore Gateway 拒绝旧版工具调用（`mcp/types.ts:163-168`）——**高**。
3. ❌ server→client 通知消费（H8 核心）：`http.rs:144-147` 通知 debug 丢弃（昨日 `:130-136` 行号漂移）、GET SSE 监听未实现；stdio 无 id 通知静默落空（`stdio.rs:84-88`）。omp `manager.ts:302-305` 四类通知→注册表刷新+信箱缓冲+GET SSE 断线续传——**高**。
4. ❌ prompts/list+get、resources/templates+subscribe（omp `client.ts:458-506,320-354` + `manager.ts:383-439` epoch 回滚）——高/中。
5. ❌ server→client 请求应答（ping/roots/list，omp `client.ts:50-66`）——中。
6. ❌ OAuth 全链（授权码+PKCE+动态注册+RFC 9728，`oauth-flow.ts:300-342`）——中（M1）。
7. ❌ .mcp.json/.cursor/mcp.json 等 12 源配置兼容（omp `discovery/*` 12 provider）——中（先做根 mcp.json 只读 provider）。
8. ❌ legacy SSE-only 传输（L8）——低。

**配置对照 24 组**（❌ 10 / 🟡 9 / ✅ 5）：模型角色 10 个 ❌（M12）；retry 策略十键 ❌（M29）；bash 超时 🟡（omp 默认 300s/0=禁用/maxTimeout clamp vs Gyre 120s/1-3600s，基线不确定 #3 定论）；主题 ❌、telemetry ❌（M30）、.env 分层 ❌（M25，`${VAR}` 未设置静默空串 `env.rs:17` 维持）、checkpoints/magicKeywords/workspace.additionalDirectories/tier/secrets 开关 ❌；MCP/provider 旋钮/thinking 预算/compaction/skills/memory/edit/循环护栏/auth_token 非 SecretString 🟡；审批模式/i18n/SOCKS5/subagent 节 ✅（后三组 Gyre 领先或自有）。

**skills/上下文/secrets**：skills 7+ 源 vs native 唯一（provider 端口 `core/skill.rs:82-97` 确认就绪，补 Claude/Codex 即覆盖迁移大户）——M23 中；上下文文件 omp 9 provider vs Gyre 4 类（缺 GEMINI.md/.cursorrules/windsurf/copilot/opencode/.omp/.agents）+ @-import 展开 ❌（`at-imports.ts:41,50,69`）——M24 中；secrets HMAC 可逆占位符体系 ❌（`placeholder.ts:42-50,215-233,262-266`，Gyre 仅 advisor regex 脱敏；SecretString 底座 ✅ 占优）——M26 中。

**事件面**：ExtensionAPI 45 / HookAPI 25（今日实测确认，基线不确定 #1 定论：sendMessage/appendEntry 是独立方法非 on() 事件）vs Gyre HookEvent 3 变体 + 3 附加回调 = 6 挂钩点（`core/hook.rs:11-77`）。

**新发现**：omp MCP requestIdFormat 可配（`mcp-json.ts:87-90`）vs Gyre 固定 u64（与 RPC id 类型待定项联动）；omp Smithery 三件 MCP 注册表生态（低）；Gyre `[agent.commands.interceptor/minimizer]` 配置节有 omp pi-shell 对应物（基线漏记 ✅）。

### 2.5 LLM 提供层（LlmLayer）

**已修**：H1/H2/签名入数据模型（`core/message.rs:147-173`）/缓存断点 MultiPoint 精确化（`transform.rs:37-100`，6 测试）。

**仍缺（按杠杆排序）**：
1. **openai-responses 伪适配（新·高）**：`openai.rs:30-34` vs `:55`（main 已复核）——SUPPORTED 收窄或落真 Responses wire。
2. **Provider 覆盖面（高）**：omp 12 线协议（`register-builtins.ts:52-99`）+ 79 家注册 vs Gyre 5 适配器（`plugin.rs:39-53`）+ 7 Api 枚举。全缺：openai-codex-responses、azure-openai-responses、google-gemini-cli、google-vertex、amazon-bedrock（+sigv4/eventstream/credentials/mantle）、cursor、devin、gitlab-duo（×2）、kimi、openai-anthropic-shim、server 侧四件。
3. **认证 7 层链（高）**：omp `auth-storage.ts:5639-5693`（runtime override→config pin→OAuth→login 源→env→stored→custom + session 粘滞）vs Gyre 2 层（RuntimeOverrides+KeyRing；无 env 回退层）。
4. **OAuth 19 流模块全缺（高）**：`registry/oauth/` 19 模块（anthropic/openai-codex/cursor/devin/github-copilot/google×2/gitlab-duo×2/xai/minimax-code/xiaomi/wafer/openrouter/perplexity/zai/kimi/opencode + device-code + pkce/callback-server）——先 1-2 家验证架构。
5. **多账号轮换/兄弟直轮（中）**：`auth-retry.ts:91` MAX=64、`:220-224` 403/usage-limit 兄弟凭证直轮、`:408` rotateSessionCredential。
6. **跨进程刷新租约+限流块持久化（中）**：`auth-storage.ts:419-449`（lease fence + SQLite CAS + MAX 语义）。
7. **模型角色 10 个（中）**：`model-roles.ts:22-44`。
8. **目录+定价数据化（中）**：models.json + descriptors + `models.ts:81-83` calculateCost；Gyre `cost_usd` 字段存在但全库无计价来源，恒 0.0。
9. **Usage/内容块/消息元数据（中）**：Usage 5 vs 丰富分桶（reasoningTokens/cttl 5m-1h/server/credits/cost 5 桶，`catalog/types.ts:101-166`）；内容块 3 vs 7+（缺 RedactedThinking/ServerTool/textSignature/itemId/providerMetadata，`ai/types.ts:707-861`）；AssistantMessage 5 vs ~20 字段（+duration/ttft/completedAt `:1014-1019`）。
10. **partial-JSON 修复（中）**：`anthropic.rs:516-523` 坏 JSON 原串塞 arguments vs `parseJsonWithRepair`（`utils/json-parse.ts:553` + `providers/anthropic.ts:2325`）。
11. **泄漏清洗中间件（中）**：`dialect/thinking.ts:23-31` 七种泄漏成语 + ThinkingInbandScanner（fence/code-span 感知）vs Gyre inband.rs 仅抑制自有 `<tool_call>` 标记。
12. **prefix-binding（omp 新特性，中）**：Anthropic thinking 签名-前缀绑定失配处理（drop_block/error 行为+control betas+自动重试+陈旧签名剥离，`anthropic.ts:2061-2067,2969-2984`、`transform-messages.ts:617-683`）；Gyre 无条件回放签名，前缀漂移 400 无降级路径。
13. **OpenAI reasoning 方言族（新·中）**：`OpenAIReasoningFormat` 8 种 + DisableMode 10 种 + reasoning-fallback 13.9KB；Gyre 仅 reasoning_effort 档位。
14. **OpenAI 显式缓存断点+TTL（中）**：prompt_cache_key + ttl 1h/24h（`openai-completions.ts:1617-1637`、`openai-responses.ts:1168-1184`）；Gyre 仅 ephemeral 无 ttl。
15. **per-host 首事件超时（低）**：全局 360s 单值 vs omp first-event/idle 两段+per-provider LazyStreamLimits。

### 2.6 Rust 原生层（RustCrates）

**vendor drift**：pi-walker/pi-shell/brush-core ✅ 同步（仅 rustfmt 行偏移）；**pi-builtins xutf 漂移属实且为唯一漂移点**（omp `wc.rs:1109`/`jq.rs:627` xutf vs Gyre unicode-width；bytecount 两侧一致，昨日误报结论维持）。

| # | 项 | omp | Gyre | 优先级 |
|---|---|---|---|---|
| 1 | ast 语言面（H9 余量） | pi-ast **57 语言**（`language/mod.rs`）+ workspace 56 语法包 + ast-grep-core 0.39 | 5 语言 + 5 语法包（crate 级）+ 0.34；新增 pattern.rs 六档严格度对齐 omp ops.rs 主体 ✅；工具面 ast_search/ast_rewrite 已接线 | 高（vendor pi-ast 一次性收编，纯 Rust 无 napi） |
| 2 | ast 4 子能力 | contextual selector（`ops.rs:105`）、MultipleNode 兜底（`:111-160,:394-400`）、parse_cache（`parse_cache.rs:206` LRU+字节上限）、node_chain_at（`block.rs:302`） | grep 全部零命中 | 高×2（随 vendor）/中×2 |
| 3 | iso 后端（H12） | 8 后端全实装（`pi-iso/lib.rs:280-291`）+ auto_order | `native()` 恒 Rcopy（`iso/lib.rs:82-87`）、7 STUB、AUTO_ORDER=[Rcopy] | 高（reflink→btrfs/zfs→overlayfs→apfs/projfs） |
| 4 | tokens 多编码（M28b） | utok **10 编码**（O200k 默认/Cl100k/**Claude ctok 精确计数重构**/Qwen3/DeepSeekV3/KimiK2/Glm5，zstd 内嵌表） | tiktoken-rs 0.6 单 cl100k 近似（`search/tokens.rs:8`）——Anthropic/GLM/Qwen/DeepSeek/Kimi 预算估算失真 | 中（短期 tiktoken-rs 0.11 + encoding 参数；长期 vendor utok） |
| 5 | 流式 grep/PCRE2/binary 检测 | `grep.rs:663` BinaryDetection、`:1091` stop-after、三条流式路径、grep-pcre2 | 一次性收集、隐式 binary 跳过、4MB 上限、仅 rust regex（`search/lib.rs:32-93`） | 中（引 grep-searcher/-pcre2 换芯） |
| 6 | fuzzyFind（fd） | `fd.rs:1-13,62` 子序列模糊打分 | `extras.rs:58,80` 子串 contains | 中（~100 行移植） |
| 7 | pi-natives 长尾 | clipboard/fs watch/结构化 diff（**jsdiff v9 位级兼容引擎 1,536 行**）/文本宽度/spelling/pdf/highlight/html/ps/sixel/desktop 等 ~40 面 | ≈7/40 面 | 低（按需） |
| 8 | pi-vcs / pi-voice | gix 0.85 + jj-lib 0.44 双后端；4 平台音频+WebRTC | 零对应（全库 grep 零命中） | 低（既定边界） |

**新发现**：Gyre fs_cache 为 pi-natives fs_cache 精简移植（TTL 1s/16 条目，形态对齐容量窄）；vendor 版本锚 `18.0.0` vs omp 18.1.2（断代标记无用化，建议更新注释）；omp vectors.rs 实为标量位精确循环非 SIMD（基线措辞存疑成立）。

### 2.7 构建 / CI / 打包 / 依赖（BuildCi）

**已修**：H10（pin 1.97.1）；基线三处结论过时：ast-grep-core 已启用、5 语法包已入图（crate 级）、tag 冒烟+产物校验已加（`rust.yml:126-129,135-146,171`）。

**仍缺**：
| # | 项 | omp | Gyre | 优先级 |
|---|---|---|---|---|
| 1 | cargo-deny（H11） | `deny.toml`（yanked=deny+licenses 白名单+sources 门）+ CI `:152-160` | 无文件无 step。⚠ 实施口径修正：omp CI 只跑 licenses+sources，Gyre 接入时应跑**全量** `cargo deny check` | 高 |
| 2 | about.toml→THIRD-PARTY-NOTICES | cargo-about 0.8.2 + 22.9k 行 NOTICES（作为 release 资产） | 均无 | 中 |
| 3 | 发布矩阵（H18 余量） | 7 目标（linux x64/arm64/musl×2/win32 + darwin×2）+ macOS 签名公证（secret 缺省跳过模式）+ **release_github_verify** 下载-验签-冒烟（`ci.yml:818-850`） | 2 目标（linux-gnu/win-msvc）；tag 冒烟仅构建机本机 | 高 |
| 4 | musl 目标+冒烟 | musl×2 + alpine docker 冒烟（`:599-605`） | 无 | 中 |
| 5 | CI 硬化 | concurrency `:74-76`（release 不取消）、paths `:6-58`、workflow_dispatch、bun --frozen-lockfile | 无 concurrency/paths/dispatch；`npm install`（`rust.yml:118`，lock 已提交） | 中（npm ci 一行先行） |
| 6 | profiles | release(fat,1)/ci/local/profiling/dev + `dev.package."*" opt-level=2` | 仅 release(thin,16) | 中 |
| 7 | justfile/xtask/scripts/ | ~110 npm scripts + scripts/ 57 文件 | 无 scripts/、无任务运行器 | 中 |
| 8 | 发布编排+安装器 | release.ts 486 行（原子 push/canary）+ install.sh 334 行 + install_methods job | 无 | 中 |
| 9 | 依赖漂移 | — | arboard 无 wayland、image png-only、tiktoken-rs 0.6、toml 0.8、ast-grep-core 0.34、syntect 无 yaml-load、serde_json 无 preserve_order、portable-pty 0.8 | 中三项（arboard/image/tiktoken 一批升）/低五项 |
| 10 | workspace 死条目（新） | — | `tree-sitter = "0.25"`（`Cargo.toml:79`）无消费者（ast 用 crate 级 0.24，lock 仅 0.24.7 一条） | 低中（删或统一 workspace） |
| 11 | SHA256SUMS.txt 汇总 | `ci.yml:797-814` | 仅 per-artifact sidecar | 低 |
| 12 | actionlint | `.github/actionlint.yaml`（⚠ 非 CI 门，系 cleanse checker） | 无 | 低（口径修正后可选项） |
| 13 | .cargo/config.toml | cmake/Ninja env + nightly unstable | 无——**合理缺失**（无 cmake 系依赖），musl 立项时再补 | 低（无需动作） |

**反向差集（Gyre 有 omp 无，无需动作）**：reqwest/axum/rust-embed/tracing/lsp-types/rusqlite/secrecy/aes-gcm/criterion/fastembed/opentelemetry/ab_glyph 等——omp 对应能力在 TS 层或 Gyre 独有功能面。

### 2.8 资源 / 文档 / 测试 / i18n / 外围（ResDocs）

**已修**：M8 半边（三层帧全链，附回归测试 `agent/lib.rs:3867-3900`）、M7 半边（ask options）、stats 看板+trend（基线不确定 #13 销）、L23 部分（favicon+品牌字体）、§四.18 rust-embed 策略已文档化（`server/lib.rs:1336-1378`）、§四.22 collab-web E2E 确认（codec.ts AES-256-GCM 双方同构）。

**仍缺（要点）**：
1. **H17 system prompt 模板工程化（高）**：omp `prompts/system/system-prompt.md` 251 行（Role/ToolPolicy/Delegation 六门控/Workflow 六阶段/Delivery 四契约/Critical）+ `system-prompt.ts` 1,040 行组装器（并行 prep+5s deadline 降级）vs Gyre 5 角色 prompt 7-10 行中文（`prompts/system-code.md` 10 行）。**H17b 覆盖链（高）**：SYSTEM.md 项目覆盖用户/PERSONALITY.md/--append-system-prompt/custom 整模板切换——Gyre grep 零命中。移植蓝本明确；Gyre `prompts/review.md`/`compaction-summary.md` 的结构化风格可复用。
2. **M8b 流式 diff 无生产者（高）**：管线三层已通但 crates/tools 全部工具 `update_tx: None`（main 已复核 17 处）——edit/write 在 execute 内 send partial 即通水。
3. **M8 per-tool 工具卡（中）**：omp collab-web tool-render/ 30 个 React 视图+registry——直接移植源；Gyre 通用 ToolBlock。
4. **M7 Web 线协议（中）**：`AskMessage` 无 options 字段（`web/c5-ui/src/lib/agent/types.ts:74-81`）；工具侧 options 仅渲染成 prompt 文本。
5. **M9 消息队列（中）/ M10 图像回显（中低）**：transcript 纯文本（纯图片消息仅 placeholder `Transcript.tsx:316-318`）；仅输入侧有 `<img>` 缩略图。
6. **资源密度（中）**：prompts 178 md vs 7；docs 129 vs 12（逐工具参考 31 vs 0）；scripts 57 vs 0。
7. **i18n（低，易修）**：CLI 四语 237/237/244/244 键，en/ru 各缺 zh 独有 7 键 `goal.{disabled,title,status,exceeded,mode,updated,usage}`（键名本轮定论，基线不确定 #10 销）；web 端另有独立四语 ~350 键（领先面更大）；prompt 正文中文硬编码 × i18n 边界待产品决策。
8. **外围 8 包**：metaharness ❌（L3）、typescript-edit-benchmark ❌、browser-relay ❌（L7，cdp_url attach 先行）、mnemopi 🟡（10 模块已移植含新增 intent/mental_models，episodic-graph/triples/Weibull/extraction/beam/MCP 暴露缺，L5）、snapcompact 🟡（单形状，**CJK 仍折叠 '?'** `render.rs:62-63`，L4）、utils HTML→MD ❌（turndown/marked/readability 零对应，中低）、omptype ⚠（schemars 声明仍死）、collab 🟡（只读/可写双链接+wt 门控已有，托管 relay/QR 缺）。
9. **测试面**：1,088 自研 vs 前端 0；telemetry 0 测试（supervisor/pty 各 3）。

---

## 三、分级缺失清单（2026-09-05）

### 高优先（正确性风险 + 破坏面最大，20 项）

| # | 项 | omp 位置 | Gyre 现状 | 影响 | 建议 |
|---|---|---|---|---|---|
| H1' | 提交在途工作 | — | 80 文件未提交（含 H1/H2/H3/H10 修复+测试） | 丢失风险；分析基线混乱 | 全量 nextest 后提交 |
| H2' | MCP 协议版本出价 **新 bug** | `mcp/types.ts:163-168`（2025-11-25 + Bedrock 硬门槛） | `client.rs:125` 出价 2024-11-05 | Bedrock AgentCore 等严格网关工具调用全拒 | 出价 2025-11-25，按 server 返回降级（set_protocol_version 已有） |
| H3' | openai-responses 伪适配 **新 bug** | `providers/openai-responses.ts` | `openai.rs:30-34` 名义支持、`:55` 恒 chat wire（main 已复核） | 路由谎言：Responses 专属能力全失、排障误导 | SUPPORTED 收窄为 OpenAiCompletions+OllamaChat，或落真 /v1/responses wire |
| H4' | RPC ready 帧+协商+分片 | `rpc-types.ts:144-150`、`rpc-frame.ts` | `cli/rpc.rs` 3 命令无握手 | 协议无兼容演进路径；巨帧打爆消费端。**先握手后扩命令** | 首行 ready+negotiate+chunk（~300 行） |
| H5' | TurnRecovery 恢复族 | `session/turn-recovery.ts`（五机制） | 散点自愈，五机制全无（main 复核 empty-stop） | 长跑失败率放大 | `agent/turn_recovery.rs` 先 empty-stop+cooldown |
| H6' | web_search provider 链+recency | `web/search/` 24 家 | DDG+Searxng 双链（main 复核） | 单点限流，搜索质量主瓶颈 | 链抽象已就绪；接 brave/tavily+recency |
| H7' | MCP 通知消费+prompts/templates | `manager.ts:302-305`、`client.ts:458-506,320-354` | `http.rs:144-147` 丢弃、GET SSE 未实现 | server 工具热更新失效；提示词模板不可用 | 读循环通知分发→registry 刷新；补 3 方法 |
| H8' | bash 后台作业 | `bash.ts:824-1102` AsyncJobManager | 纯前台 | 长任务占死工具轮 | AsyncJob+`bash_*` 句柄族（与 async/job-manager 同批） |
| H9' | hub 监督面 | `hub/index.ts:80-127` 12 op | 3 op（main 复核） | 多代理「等待/管理」半边缺 | 先 wait/inbox/jobs/cancel |
| H10' | read 选择器族 | `read.ts:530-538` | 仅 `:conflicts`（main 复核） | 编辑锚定/大文件导航降级 | 纯文本 `:N-M`/`:raw` 先行 |
| H11' | ast 语言面+4 子能力 | pi-ast 57 语言+56 语法包 | 5 语言+5 语法包 | read summary/ast_search 覆盖瘫痪级 | vendor pi-ast（纯 Rust）或分批补语法包+升 0.39 |
| H12' | iso 后端 | `pi-iso/lib.rs:280-291` 8 后端 | 恒 Rcopy | 隔离全量拷贝慢+耗盘 | reflink→btrfs/zfs→overlayfs→apfs/projfs |
| H13' | xdev 通用挂载 | `xdev.ts:83-86` | 3 硬编码 URI | 上下文瘦身机制整缺 | registry get(name) 通用化派发 |
| H14' | system prompt 模板+覆盖链 | `system-prompt.md` 251 行+组装器 1,040 行 | 10 行角色描述；覆盖链零命中 | 行为上限受制 | 移植最小四段+SYSTEM.md/--append-system-prompt |
| H15' | 认证链+OAuth 最小集 | `auth-storage.ts:5639-5693`、`registry/oauth/` 19 模块 | 2 层、零 OAuth | 企业/SaaS 凭证无法接入 | CredentialSource trait 分层+env 层；anthropic device-code 验证架构 |
| H16' | ToolExecutionUpdate 生产者 **新** | `tool-execution.ts:1122-1205` | 管道已通但全部工具 `update_tx: None`（main 复核） | 流式 diff 用户不可见（管道无水） | edit/write execute 内 send ToolUpdate |
| H17' | task 结构化半边（yield 信封+schemaMode strict） | `yield-assembly.ts`、`task/types.ts:114-154` | 无 | 子代理结构化输出前提 | yield 信封先行 |
| H18' | cargo-deny+NOTICES | `deny.toml`、`about.toml`、`ci.yml:152-160` | 无 | 供应链/分发合规零防护 | 照抄骨架，**跑全量 check**（比 omp 更严） |
| H19' | 发布矩阵+发布后验证 | `ci.yml:522-670,818-850` | 2 目标、tag 冒烟仅本机 | arm64/mac 无产物；坏 release 上线才知 | +aarch64-linux/darwin，签名 secret 缺省跳过，下载验证 job |
| H20' | edit replace 模式 | `edit/index.ts:44-48` 五模式 | 单 hashline | 最常用 fallback 缺 | 补 replace 模式即可覆盖大头 |

### 中优先（45 项，按组）

**MCP/配置组**：MCP OAuth 全链（M1）· prompts/resources 方法补齐（随 H7'）· server→client 请求应答 ping/roots · .mcp.json 兼容（根文件只读 provider 先行）· 模型角色 10 个（M12）· retry 策略配置化+usage-aware fallback（M29）· .env 分层加载+`${VAR}` 告警（M25，静默空串踩坑）· bash 超时对齐（300s/0=禁用/maxTimeout，基线 #3 已定论）· 压缩配置面（新）· server.auth_token 走 SecretString。
**LLM 组**：凭据刷新租约+限流块持久化（M14 余）· 多账号/兄弟直轮 · 目录+定价数据化（M13，cost_usd 落来源）· Usage/内容块/消息元数据对齐（M27）· partial-JSON 修复（M17）· 泄漏清洗中间件（M17）· prefix-binding（新）· OpenAI reasoning 方言族（新）· OpenAI 缓存 TTL/prompt_cache_key · provider 适配器 bedrock/vertex/gemini-cli 等按需 2-3 家。
**CLI/协议组**：子命令骨架（M2）· slash 62 缺失分批（M3）· SlashSpec 元数据化+协议内 slash（M4）· `--print` json（M5）· 8 个点名 launch flags · ACP list/resume/configOption/extMethod + update 类型补 5 类 + ToolCall 流式。
**会话/任务组**：async/job-manager+子代理复活/软预算/fallback 继承（M20）· 自动标题（M21）· todo eager+完成提醒 · plan propose 批准流（M19）· manage_skill（M22）· skills Claude/Codex provider（M23）· /compress 模式面 · IRC bus ACK/唤醒 · 后台预压缩+idle compaction（新）· provider 图像预算（新·正确性邻近）· date-cwd 提醒（新）。
**工具面组**：grep/glob 参数对齐+skip 分页+取消静默截断（M28，含新发现）· tokens 多编码（M28b）· 流式 grep 换芯 grep-searcher/-pcre2 · fuzzyFind（~100 行）· github 工具补 search_code/run_watch · lsp 工具补 6 动作 · parse_cache/node_chain_at。
**Web 组**：per-tool 工具卡移植 10 个（M8）· ask 线协议 options（M7 残余）· 消息队列+dequeue（M9，纯前端）· 图像回显（M10）。
**工程组**：CI 硬化（concurrency/paths/npm ci/profiles，M31）· musl 目标+冒烟 · justfile 最小四条 · 发布编排 xtask+安装器 ~80 行 · telemetry metrics+弱测试 crate 补齐（telemetry 0 测试）· docs 用户向 6-8 篇+逐工具 10 篇（M32）· 依赖升级一批（arboard wayland/image 四格式/tiktoken 0.11）· HTML→MD/正文抽取 · 上下文文件 GEMINI.md 等+@-import（M24）· HMAC 占位符出站脱敏（M26）。

### 低优先（30 项）

computer/tts/pi-voice 整域（L1 既定暂缓）· pi-vcs（L2）· metaharness+edit-benchmark（L3）· snapcompact 多形状+CJK '?' 修复（L4）· mnemopi 深语义 6 模块（L5）· ExtensionAPI/文件式插件/marketplace（L6 结构性边界，事件面逐步扩）· browser-relay MV3+cdp_url attach（L7）· legacy SSE-only MCP（L8）· Python omp-rpc（L10，被 H4' 阻塞）· sharpshooter/prewalk/cleanse（L11）· eval rb/jl（L12）· read PDF/office/rar/7z/iso 归档+SQL 参数化（L13）· pi-natives 其余长尾（L14：clipboard/fs-watch/diff 引擎/文本宽度）· write 归档/SQLite 行写（L15）· inspect_image/image_gen 多 provider（L16）· Web 键位重映射/emoji/ref 补全/LaTeX（L17）· collab 托管 relay/QR（L18）· Docker/Nix（L19）· session-stats/cleanup-scan 管线（L20）· daemon broker/profile 多配置（L21）· goals 状态机+aside commit/discard（L22）· 独立 icon/头图（L23 余）· i18n goal.* 7 键补译（一行修）· think/goal hidden 工具 · workspace 死条目 tree-sitter 清理 · ast-grep-core 0.39 升级 · SHA256SUMS.txt · actionlint（可选项）· Smithery · auth-gateway/broker · in-band 方言库 · vibe_* 5 件 · MCP requestIdFormat · dump 工具注册条件核实。

### 明确不迁移（维持）

TUI 差分渲染引擎（25k 行，与 rustyline+Web 路线冲突）；TS 动态插件 1:1（结构性不可）；mnemopi 全量深语义；Node SDK/robomp；bazel/自托管基建；.cargo/config.toml（无 cmake 系依赖，合理缺失）；omp nix/bazel CI 覆盖面。

---

## 四、不确定与需进一步确认（17 项）

**行为/口径类**
1. omp 远程压缩超时原值：基线称 180s，今日 grep `session/` 与 settings-schema 均未见 timeout 项——可能在 compaction-methods.ts 内部常量，需读 `compaction-v2-streaming.ts` 定位（不影响 Gyre 10s 硬编码判定）。
2. KeyRing「限流自动切 key」生效性：`agent/lib.rs:700-704` doc 声称自动切换 vs `engine.rs:486-489` 注释称 429 走同模型退避——两处口径不一，429 分支是否先退避后换 key 未读全。
3. xdev discoverable 计数：今日 grep `loadMode="discoverable"` 命中 18 处 vs 基线 16——差额疑似 checkpoint/rewind；需按 `isMountableUnderXdev`（扣除 KEEP_TOP_LEVEL）重算实际可挂载数。
4. omp 条件注册工具（tts/review/resolve/report_issue）注册分支仍未核实，需读 `index.ts` createTools 装配段。
5. omp settings 总键数 ~400 仍为估算（settings-schema.ts ~6k 行多行对象无法安全 grep 计数）。
6. omp docs 129 vs 基线 130 差 1 篇，需 `find docs -name '*.md' | wc -l` 定论（不影响 25× 缺口判定）。
7. omp SHAPE_VARIANTS 条目总数未数完（确认存在，基线口径 15）。
8. RPC id 类型策略：Gyre u64 vs omp 可选 string/requestIdFormat 可配——跨协议统一待定。
9. ACP `UsageUpdate` 是否 ACP 规范 update 类型（若私有扩展需 initialize capabilities 声明）。

**Gyre 侧待核**
10. ask options 落地时点：在途 80 文件无 git 归因手段，无法断定是否昨日后新增（不影响残余缺口判定；`git log -p crates/tools/src/ask.rs` 可定论）。
11. workspace.dependencies 是否还有其他死条目：本轮仅反查 tree-sitter（死）/ab_glyph（活）/ast-grep（注释）三个疑点，其余 50+ 条未逐一 grep 消费者。
12. fastembed（ort 预编译）在 musl/aarch64 的工具链兼容性——扩发布目标前必须先证。
13. server 侧 eval/debug/browser/pty 四可选组是否注册未逐项验证（hub/todo/ask/checkpoint/security_scan/memory/github 已证同构）。
14. `.agent/` 与 `.gyre/` 双项目目录边界（config/skills/AGENTS.md 用前者，rules/approval/socks5/collab 用后者）设计意图未文档化。
15. `before_tool_intercept`/`after_tool_override` 的装配层消费面未逐一追踪。
16. Gyre prompt 多语言化方向（include_str 编译期中文 → 运行时切换需改加载层）为产品决策非代码判定。

**omp 侧待核**
17. RustCrates：快照前 2 天 pi-natives/pi-ast 的提交级新增清单拿不到（reflog 无 commit subject），以能力面差集代替；Gyre pattern.rs/fs_cache.rs 落地时点同理不可断代。

---

## 五、路线图建议（顺序约束）

1. **立即**：H1' 提交在途工作（含 H1/H2/H3/H10 已修项）→ H2'/H3' 两个正确性修复（各 ≤半天，测试锁定）→ H18' cargo-deny 骨架 + `npm ci` 一行。
2. **第一梯队（顺序硬约束优先）**：H4' RPC 握手（先于命令扩容）→ H7' MCP 通知消费（今日在途 http.rs/stdio.rs 的自然延续）→ H16' update_tx 生产者（管道已通，补水即见效）→ H5' TurnRecovery → H6' web_search 链。
3. **第二梯队**：H8'/H9'（async 先于 hub 监督）→ H13' xdev 通用化 → H14' system prompt 模板 → H15' 认证链+OAuth 最小集 → H17' yield 信封。
4. **第三梯队**：CLI/协议组（M2-M5+flags）→ H11'/H12'（ast vendor/iso 后端重资产）→ Web 组 → 中优先均质件按需拉动。
5. **低优先**：按用户需求拉动；i18n 7 键与 workspace 死条目属顺手修。

**总量估算**：高优先 ≈ 30-40 人日（较昨日 +5，因新增 2 bug 与 update_tx 生产者）；中优先 ≈ 70-90 人日；低优先按需。无架构阻塞，全部为可排期增量。

---

## 附：本轮证据方法

- 8 路并行只读 scout（CoreLoop/Tools/CliProto/ConfigExt/LlmLayer/RustCrates/BuildCi/ResDocs），每路要求 file:line 证据与 FIXED/PERSISTS/NEW 三态判定；完整归档 `agent://CoreLoop` … `agent://ResDocs`。
- main 交叉验证：omp 快照 `git log`（18781d8295 未变）；Gyre `git status/diff`（80 文件在途确认）；六项承重结论直读复核（openai.rs 伪适配 `:30-34` vs `:55`、rpc.rs 无 ready/negotiate、fs.rs 无文本行选择器、client.rs:125 出价 2024-11-05、tools 全域 update_tx: None 17 处、engine.rs:1021 空回复直接完成）；H1/H2/H3/H10 修复落地直读确认；魔法关键词注入顺序与 reflect 语义双侧复核（Gyre 注释+测试 vs omp 源码）。
- 基线 `docs/oh-my-pi-gap-analysis-2026-09-04.md` 全部结论以今日源码重验；口径偏差逐条修正（79/19/41/58/77/129 等计数）。
