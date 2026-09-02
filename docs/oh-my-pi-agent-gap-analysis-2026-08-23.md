# Gyre × oh-my-pi Agent 能力系统审查与对齐路线图（2026-08-23）

> 调研对象：`third/oh-my-pi`（v18.0.0，badlogic/pi-mono fork，MIT）vs 本仓库 Gyre（Rust workspace，31 crates）
> 调研日期：2026-08-23；审查基线为**工作区当前状态**（含未提交改动；最后提交 08-06 `64242e1` + 此后 ~6600 行新增）
> 方法：6 个并行只读 scout 分域精读双方源码（omp 侧 packages/{agent,ai,coding-agent,catalog,mnemopi,snapcompact,natives} + docs；Gyre 侧全部 31 crates + web + config），另对承重结论（CLI/server 装配差异、security_scan 装配、iso 接线、omp 工具/钩子计数）做直接 grep/read 复核。**源码为唯一事实源**；仓库内既有 `docs/oh-my-pi-*.md` 仅作对照，本轮已发现其回填声明与今日代码不符之处（见 §2.3）。
> 证据格式：omp 路径相对 `third/oh-my-pi/`，Gyre 路径相对仓库根。

---

## 〇、结论速览

1. **引擎层已对齐且局部超集**：循环/steering/压缩/恢复/审批内核与 omp 语义等价，3 处有意加固（模式写入硬约束 yolo 下仍生效、含元字符命令永不自动放行、记忆中文语义调优）。
2. **产品层约 85% 对齐，且 08-22 文档 Phase A 的"六件套"确已落地**（todo/ask/checkpoint/rewind/memory_edit/learn/ACP 权限门/eval JS 内核/Gemini 适配器均在源码）。**但发现 4 处"已实现却未接线/不一致"的运行时缺口**：CLI 未装配 todo/ask/checkpoint/rewind（仅 Web/ACP 侧有）、security_scan 两端均未装配、429 Retry-After 未兑现、iso 隔离 API 零调用方。
3. **剩余真实差距收敛为五类**：①运行时接线修复（极小工作量）；②产品流半成品（plan propose 批准流、checkpoint turn_end 延迟应用、hub/IRC v2）；③接口面纵深（RPC 3→~40 命令、MCP 单传输、无 SDK/Python 客户端）；④provider 生态数据与账号设施（5 适配器 vs 68 provider/4409 模型、无 OAuth/coding-plan）；⑤外围能力（computer/tts/自动标题/会话导入/postmortem/定价表/metrics）。
4. **可对齐性：高**。除四类既定不可 1:1 项（TUI 差分引擎、TS 动态插件/市场、Node SDK/robomp、mnemopi 深度图语义）外，全部差距是纯增量或接线工作，无架构阻塞。估算：P0 修复 2–4 人日，P1 功能补全 35–55 人日，P2 外围 30–50 人日。

---

## 一、oh-my-pi Agent 功能清单与核心架构

### 1.1 总体架构：三层 + 原生支撑

```
┌ 产品层 packages/coding-agent（AgentSession 9685 行 + SessionManager 2948 行）
│   会话树/审批/工具工厂/plan-mode/todo/goals/checkpoint/子代理/记忆/技能/扩展/RPC/ACP
├ 引擎层 packages/agent（agent.ts 门面 + agent-loop.ts 双循环 + 纯函数压缩 + AppendOnlyContext）
├ 提供层 packages/ai（streamSimple 单点 → 14 种 KnownApi 方言 + a/b/c 凭证轮换 + transient 重试 + 跨进程限流）
└ 支撑   catalog（68 provider 描述符 / models.json 64 供应商 4409 模型 / 9.6MB）
         natives（NAPI addon：12 类 + ~50 函数）· mnemopi · snapcompact · omptype · tui · wire · hashline
```

分层纪律明确：`agent` 包只做纯引擎（双循环/上下文管线/压缩纯函数）；触发与降级链、事件扩展、方法偏好序全部在 `coding-agent`（`session/session-maintenance.ts:1725-1950` 触发、`:3527-3537` 降级重试；`session/compaction-methods.ts:10-40` 方法序）。

### 1.2 七维功能清单

**① 任务规划与多步执行**
- 双循环：内循环工具批 + steering 轮询 **250ms**（`agent-loop.ts:159` `STEERING_INTERRUPT_POLL_MS`；`runLoopBody` :987-1490，工具批 :2230-2760）；外循环 followUp/asides 到达续跑（`agent.ts:994-1005,1230-1262`）；pause 门（park 点 `agent-loop.ts:1067/:2444`）。
- plan-mode 全链：`local://<slug>-plan.md` → **`xd://propose` 写设备** → PlanProposalHandler → PlanApprovalDetails → 交互式四选一弹窗 → 合成 plan-approved 提示词 + 可选压缩接管执行（`plan-mode/approved-plan.ts`）。
- todo 工具（`tools/todo.ts` 1273 行）：单活跃不变量 `normalizeInProgressTask`（**:146**，于 :600/:614/:716 三处应用）、phase/status、Markdown 往返、条目持久化可分支回放。
- goals：隐藏工具 5 op，active/paused/budget-limited/complete/dropped 状态机，token/墙钟预算。
- checkpoint/rewind：单活跃检查点只记 `(messageCount, entryId)`；**rewind 副作用延迟到 turn_end 应用**（`session/agent-session.ts:1241-1245` 调 `#extractRewindReport/#applyRewind`，yield 守卫 :7303）；可 rehydrate。
- 子代理：task 工具（schema permissive/strict typed 输出、PAL 隔离、递归深度护栏，`task/structured-subagent.ts`）；IRC 邮箱总线（send/wait/inbox、park 唤醒、`MAILBOX_CAP=100`，`irc/bus.ts`）；Agent Hub 工具（messaging/jobs/launch 三 op 族）；vibe 模式（director + fast/good 常驻 worker）。
- magic keywords（ultrathink/orchestrate/workflowz，散文掩码）；auto-thinking 分类器（难度→Effort）。

**② 工具与插件调用机制**
- 工具面：**内置 30 个**（`tools/builtin-names.ts:1-31`：read/bash/edit/ast_grep/ast_edit/ask/debug/eval/github/glob/grep/lsp/inspect_image/browser/computer/checkpoint/rewind/security_scan/task/hub/todo/web_search/write/memory_edit/retain/recall/reflect/learn/manage_skill）+ 隐藏 3（yield/goal/think，:35）+ 旧别名 2（search→grep、find→glob，:39-42）+ `mcp__<server>_<tool>` 动态前缀。
- AgentTool 接口：omptype 编译式 schema（解释器先行，**第 3 次调用 JIT 编译**，`omptype/src/type.ts:590`）、intent 注入、onUpdate 流式进度、concurrency、interruptible、matcherDigest（TTSR 流式匹配）、approval 声明、renderCall/renderResult。
- xdev 设备挂载：15 个 discoverable 工具挂到 `xd://`（read 列文档/write 执行），保顶集合 `XDEV_KEEP_TOP_LEVEL`。
- 扩展三层：TS 模块动态 import（同进程不沙箱）+ **HookAPI 25 个事件**（`extensibility/hooks/types.ts:483-512`：11 session + 14 context/agent，含 tool_call/tool_result 可 block/改写）+ **ExtensionAPI 45 个事件**（`extensibility/extensions/types.ts:1222-1278`）+ marketplace（Claude 插件注册表兼容）+ slash 命令 7 源优先级。
- MCP 客户端：stdio / Streamable HTTP / legacy SSE 三传输 + OAuth discovery + 资源订阅。
- read 统一面：文件/目录/归档/SQLite/PDF/notebook/URL/ssh:// + 16 内部 scheme + selector 语法。

**③ 上下文与记忆管理**
- 管线 `transformContext→convertToLlm→normalizeMessagesForProvider→transformProviderContext`（`agent-loop.ts:1514-1565`）；AppendOnlyLog + StablePrefix 字节指纹（`append-only-context.ts:40-90,105-140`，唯一突变留给压缩）。
- 压缩纯函数引擎（`compaction/compaction.ts`：shouldCompact :337、findCutPoint :499 永不切 toolResult、compact 降级链 :1507-1690）+ 会话层触发（threshold/overflow/incomplete/idle）+ 方法序 **remote→snapcompact→handoff→shake→soft** + `COMPACTION_RECOVERY_BAND=0.8`；OpenAI Responses 服务端压缩（`compaction/openai.ts:777`，超时 180s，V2 流式）。
- snapcompact：**像素字体 PNG 帧**由视觉模型读回（本地确定性、零 LLM）；15 形状变体 + 按供应商计价公式（Anthropic 28px patch ≤4784 视觉 token / OpenAI 32px×1.2 / Google 固定 1120 token/帧）。
- 记忆四后端互斥：off / local（learned.md）/ hindsight（情景 bank + mental models）/ mnemopi（bun:sqlite + 精确余弦向量索引 + BAAI/bge-small-en-v1.5 384 维 + 情景图/三元组/Weibull 遗忘/睡眠整合 + **独立 MCP server 23 工具**，`mnemopi/src/mcp-tools.ts:284-394`）；工具面 retain/recall/reflect/memory_edit/learn/manage_skill + autolearn + **压缩前 preCompactionContext 召回**。
- skills：发现/过滤/系统提示块/skill:// 协议/manage_skill CRUD；上下文文件 8 格式导入。
- 原生 tokenizer（countTokens）：claude/qwen/deepseek/kimi/glm 等 9 族 + 字节上界免算。

**④ 多轮对话与状态维护**
- JSONL **v3 条目树**（15 种条目 + session 头 + 固定 256B 标题槽）；branch/resetLeaf/branchWithSummary/discardEntryDurably；/fork /tree /resume /new；自动标题生成；从 Claude/Codex 会话导入。
- 事件流：AgentEvent 细粒度（**无独立 toolResult 事件——工具结果以 role:'toolResult' 的 message_start/message_end 对发出**，此前文档主张已过期）+ AgentSessionEvent 扩展（auto_compaction_*/auto_retry_*/irc_message 等，`session/agent-session-events.ts:12-56`）。
- steering/followUp/asides 三队列 + interruptMode immediate/wait + yield 空闲续跑；collab 会话镜像复用 resume。

**⑤ 错误恢复与重试策略**
- ai 层：auth **a/b/c 凭证轮换**（`auth-retry.ts:81` `AUTH_RETRY_MAX_ATTEMPTS=64`；403/usage-limit/account-policy **直接轮换 sibling** :83-100；attemptedKeys 去重防循环 :116-122）；transient oneshot 重试（3 次、500ms 翻倍、退避上限 8s、单次等待上限 30s、Retry-After>30s 放弃，`oneshot-retry.ts:127-139,227-235`）；**跨进程 in-flight 限流**（文件租约：锁 10s/过期 30s/心跳 5s/signal 回退 250ms，`stream.ts:177-264,646`）。
- 会话层 TurnRecovery（**2371 行**，`session/turn-recovery.ts`）：AIError 分类、empty-stop 重试 + developer 提示 + 丢回合持久化 reparent、usage-limit→凭证轮换、模型 fallback 角色链 + cooldown、上下文溢出不重试→压缩接管、retryRecovery 注解持久化。
- 引擎层：合成 error/aborted + `__synthetic`、replay 过滤 refusal、Harmony 泄漏自愈、崩溃 postmortem 分类器。

**⑥ 权限控制与安全边界**
- 审批：tier `read|write|exec` × 用户 `allow|deny|prompt` × 模式 `always-ask|write|yolo` 比较序（`tools/approval.ts` policyKey 覆盖序）；yolo 下用户 deny 仍生效。
- secrets：secrets.yml + env 模式 + 内置 regex + **HMAC 可逆占位符**（密钥字节永不达 provider，执行前 `deobfuscateToolArguments`，`secrets/obfuscator.ts`）。
- 隔离：PAL **8 后端**（apfs/btrfs/zfs/reflink/overlayfs/projfs/rcopy/block-clone，`natives/native/index.d.ts:1293-1302`）+ spawn/read-only policy + security_scan（preflight + 树指纹 + SARIF）。
- ACP 远程审批门 + 破坏性意图检测。

**⑦ 对外接口与运行环境依赖**
- 四入口：TUI / `-p` 无头 / `--mode text|json|rpc|acp|rpc-ui`（`cli/args.ts:23`）；**RPC = NDJSON 协议 v2、42 条命令**（`modes/rpc/rpc-types.ts:28-77`，negotiate_protocol 握手、bash 后台派发、host_tools/host_uris 桥、extension_ui_request）；**ACP = JSON-RPC 2.0，agent 12 方法 + client 12 方法**（含 fs/read_text_file、terminal/*、elicitation/*，`utils/src/acp/connection.ts:243-312`，实现 2786 行）；Node SDK 数十子路径导出；Python omp-rpc（1807 行 TypedDict 镜像，`python/omp-rpc/src/omp_rpc/protocol.py`）；robomp GitHub bot。
- provider 面：**68 provider 描述符**（`catalog/src/provider-models/descriptors.ts:70-546`）/ **models.json 64 供应商 4409 模型 9.6MB**（脚本实测计数）；认证 5 形态（env key / OAuth 17 模块 / coding-plan 5 项 / keyless 12 项 / models.yml 自定义）；KnownApi **14 方言**（`catalog/src/types.ts:8-22`）。
- 运行时：Bun ≥1.3.14（Docker 固定 1.4.0）+ NAPI addon（5 平台矩阵）+ vendored brush-core 0.5.0 + pi-builtins **~117 命令模块**（uutils 移植，进程内零 fork）+ sherpa-onnx 1.13.2 + puppeteer-core 25.3.0(patched) + onnxruntime-node 1.26.0；部署 npm / `bun --compile` 单二进制 / 4 阶段 Docker（tini）/ Nix flake。

---

## 二、Gyre 现状核查与功能差距矩阵

### 2.1 Gyre 架构一句话

纯 Rust 单二进制（31 crates）：`agent-core` 零依赖契约层（Tool/MemoryStore/ApprovalPolicy/Hook/ContextManager/LlmProvider/Hub 端口）→ `agent`（run_loop 五态机 + 三通道 steering + 审批门禁 + shared/exclusive 批调度 + 250ms 批级 cancel `engine.rs:1614`）→ `context`（节点森林 + active_leaf + StablePrefix `lib.rs:450-485` + 2092 行压缩器 + JSONL 持久化）→ `llm`（ProviderRegistry + inventory 插件 + fallback 链 + KeyRing）→ `config`（7 步审批链）；宿主四入口 CLI / Web(axum, 27 REST + 2 WS) / ACP(stdio+HTTP+SSE) / `--rpc`(NDJSON)。工具/Provider 全 trait + 编译期装配，无动态加载。

### 2.2 差距矩阵（✅ 已实现 · 🟡 部分实现 · ❌ 缺失 · ⚠ 行为不一致/未接线）

| # | 能力（omp） | Gyre 现状 | 判定 | 证据 |
|---|---|---|---|---|
| **① 任务规划与多步执行** | | | | |
| 1 | 双循环 + 三通道 + pause + deadline + 250ms | run_loop 五态机 + steering/aside/followUp 三通道 + PauseGate + deadline + 250ms 批级 cancel | ✅ | agent/src/engine.rs:1614 等 |
| 2 | interruptMode immediate/wait | InterruptMode 已实现（Immediate 轮询触发批级 cancel） | ✅ | agent/src/lib.rs |
| 3 | plan-mode propose→批准弹窗→压缩接管 | `/plan` 切模式 + architect/plan 写约束（仅 `plans/*.md`）+ handoff；**无 `xd://propose` 批准流**（grep 零命中） | 🟡 | config/src/rules.rs:30-40 |
| 4 | todo 工具（单活跃不变量/持久化/回放） | TodoTool 五阶段机 + `.gyre/todo.json` + 单活跃不变量；**CLI 未装配，仅 server** | 🟡⚠ | tools/src/todo.rs:255；server:1134 vs cli/main.rs 零命中 |
| 5 | goals（工具 5 op + 状态机） | GoalBudget token/time/hard_stop + `/goal set\|extend`（宿主命令非工具） | 🟡 | agent/src/lib.rs:45-66 |
| 6 | magic keywords | keywords.rs，仅散文/词边界 | ✅ | agent/src/keywords.rs |
| 7 | checkpoint/rewind（turn_end 延迟 + rehydrate） | 工具对 + rewind=switch_branch_with_handoff（confirm 门禁）；**无 turn_end 延迟应用/yield 守卫**；CLI 未装配 | 🟡 | tools/src/checkpoint.rs:99,185 |
| 8 | task typed schema 输出 | JSON Schema 子集校验器 + 失败同上下文纠错重试 ×2 | ✅ | agent/src/schema_check.rs, task_tool.rs |
| 9 | task PAL 隔离（8 后端） | `Workspace::start_isolation` API 就绪（core/workspace.rs:73）+ iso 仅 rcopy；**`start_isolation` 全仓零调用方** | ❌⚠ | grep 零命中；iso/src/lib.rs:117-190 |
| 10 | IRC/hub（ACK/唤醒/复活/jobs/launch/Hub UI） | 进程内 Hub v1（注册表+定向邮箱，core/hub.rs）；无 ACK/唤醒/复活/后台任务句柄/Hub UI | 🟡 | core/src/hub.rs:5-9 |
| 11 | vibe 模式、/tan、/btw | 无 | ❌ | grep 零命中 |
| 12 | auto-thinking 分类器 | LlmThinkingClassifier（8-token 四档，失败回退） | ✅ | llm/src/thinking.rs:52-118 |
| **② 工具与插件** | | | | |
| 13 | AgentTool（intent/onUpdate/matcherDigest） | Tool trait 有 concurrency/interruptible/partial（update_tx）；**无 intent 注入、无 matcherDigest** | 🟡 | tools/src/lib.rs |
| 14 | 内置 30+3+别名 工具面 | 31 个（核心 7 + 可选 12 + 会话/记忆/子代理）；**缺 computer/tts/manage_skill/yield/think** | 🟡 | tools/src/lib.rs:232-240 |
| 15 | ask 工具 | AskUserTool 经 AskMessage 通道；**CLI 未装配**（Web/ACP 有） | 🟡⚠ | tools/src/ask.rs:38；cli/main.rs 零命中 |
| 16 | xdev discoverable 挂载（15 工具） | 仅 `xd://pending/resolve/reject`（ast_rewrite 暂存两阶段，含漂移复查） | 🟡 | tools/src/fs.rs:440-475 |
| 17 | MCP 三传输 + OAuth + 订阅 | **仅 stdio**（2024-11-05，tools+resources，mcp:// 路由）；无 OAuth/SSE/HTTP/订阅 | 🟡 | mcp/src/client.rs:223-290 |
| 18 | TS 动态插件 + marketplace | inventory 编译期（仅 provider）+ Hook trait + `.agent/commands/*.md`；**市场不可对齐** | ⚠ 替代 | llm/src/plugin.rs:13-42 |
| 19 | HookAPI 25 事件 / ExtensionAPI 45 | Hook trait **3 事件变体 + 3 方法**（on_turn_end/before_tool_intercept/after_tool_override） | 🟡 | core/src/hook.rs |
| 20 | slash 命令 7 源优先级 | 30 命令 + 自定义命令；无多源优先级合并；**命令表 26 条 vs 分发 30 条不一致**（/goal /session /resume /? 缺表） | 🟡⚠ | cli/src/repl.rs:147-174 vs 256-391 |
| 21 | read 统一面（归档/SQLite/PDF/ipynb/selector） | zip 族+tar/tar.gz 成员、SQLite `::SELECT` 只读、ipynb、ssh://、9 内部 scheme、`:conflicts`；**缺 PDF、selector 语法** | 🟡 | tools/src/fs.rs（+621 行新改动） |
| 22 | eval 四内核 + 环回桥 | Python + JS 双内核 + `tool.<name>()` 环回桥（token/鉴权防递归） | ✅(2/4 内核) | eval/src/lib.rs:110-300 |
| 23 | web_search 多 provider | DDG（免 key）+ Searxng 链式 | 🟡 | tools/src/web_search.rs:153-175 |
| 24 | inspect_image（视觉模型委派） | read_image **内联多模态**（同名不同机制，功能可达） | 🟡 | tools/src/image.rs:38 |
| 25 | security_scan | **工具已实现（9 类密钥正则+PEM+权限+脱敏）但 CLI 与 server 均未装配——运行时不可达** | ❌⚠ | security_scan.rs:209；装配层 grep 零命中 |
| **③ 上下文与记忆** | | | | |
| 26 | 上下文管线 + AppendOnly + StablePrefix | build_provider_context + digest 最长稳定前缀 + sanitize + 分支保全 | ✅ | context/src/lib.rs:450-485,750 |
| 27 | 压缩方法序 + 降级 + 切点保护 | shake→summarize(remote 回退)/snapcompact→prune + supersede_read_results + skill:// 保护；**方法序与 omp 不同但语义覆盖**（无 soft 独立档） | ✅ | context/src/compaction.rs; engine.rs:344-410 |
| 28 | 压缩前记忆召回 preCompactionContext | **新落地**（未提交）：RECALL_LIMIT=8/4000 字符/3 行查询，对齐 mnemopi 默认 | ✅ | context/src/compaction.rs（diff 核验） |
| 29 | 原生 tokenizer 9 族 | tiktoken cl100k/o200k + chars/4 回退（非 OpenAI 模型精度低） | 🟡 | context/src/token.rs |
| 30 | 记忆四后端 | local + structured（JSONL+banks）+ 向量（L1 投影/L2 fastembed）+ mental models；无 hindsight/mnemopi 独立形态 | 🟡 | memory/src/lib.rs |
| 31 | 记忆工具面 + autolearn + sleep | recall/retain/reflect/memory_edit/learn 全在 + AutoRetainHook（每 4 轮）+ SleepHook + MMR 中文调优 + queryAsksCurrent + 中文时间表达（**超集**）；reflect 语义与 omp 同名不同义（Gyre=LLM 提炼心智模型） | ✅⚠ | memory_tool.rs; memory/src/structured.rs:342-549 |
| 32 | skills + manage_skill | 文件发现（user+项目 walkup）+ skill:// + 注入 + /skills；**无 manage_skill**（grep 零命中） | 🟡 | skills/src/registry.rs:27-98 |
| 33 | 上下文文件 8 格式导入 | 4 来源（AGENTS.md/CLAUDE.md/Cursor .mdc/Cline） | 🟡 | discovery/src/lib.rs:23-34 |
| **④ 多轮与状态** | | | | |
| 34 | 会话树 + branch/branchWithSummary | SessionNode 森林 + JSONL + leaf sidecar + switch_branch_with_handoff | ✅ | context/src/persistence.rs |
| 35 | /tree 交互导航 | Web BranchTreeModal + REST /branches；CLI 无 | 🟡 | server/src/lib.rs; web/c5-ui |
| 36 | 自动标题生成 / 会话导入 | 自定义标题表（`_titles.json`）；**无 LLM 自动生成、无导入** | ❌ | context/src/persistence.rs |
| 37 | 事件流细粒度 | AgentEvent 三层生命周期 + TextDelta/ThinkingDelta/ToolExecUpdate | ✅ | core/src/message.rs |
| 38 | collab（relay/QR/托管） | 本地 host 中继 + AES-256-GCM + 16B 写令牌 + 历史重放 + guest 页；无托管 relay/QR | 🟡 | collab/src/relay.rs; server:3207-3349 |
| **⑤ 错误恢复与重试** | | | | |
| 39 | auth a/b/c 轮换（64 上限/403 直轮/OAuth 刷新） | KeyRing round-robin 多 key + RuntimeOverrides.api_key；无 OAuth、无 403/usage-limit 直轮分类 | 🟡 | agent/src/lib.rs:269-279; engine.rs:472-479 |
| 40 | transient 重试（3 次退避 + Retry-After 30s） | 瞬时错误白名单改写 stop_reason 续跑已完成工具；**无指数退避 sleep；429 retry_after_ms 存而未兑现，直接走 fallback** | 🟡⚠ | engine.rs:739-770; llm 各适配器 |
| 41 | 跨进程 in-flight 限流（文件租约） | 无 | ❌ | grep 零命中 |
| 42 | TurnRecovery（reparent/cooldown/注解持久化） | 模型级+Provider 级双 fallback 链 + mistakes 上限；无 empty-stop reparent/cooldown/注解 | 🟡 | engine.rs:464-526 |
| 43 | Harmony 泄漏自愈 | 双计数器（truncate-resume ×2 / abort-retry ×2）+ temperature 扰动 | ✅ | agent/src/harmony.rs |
| 44 | 合成错误 + refusal 过滤 | persist_interrupted + 占位回填 + refusal-like 不重放 | ✅ | engine.rs |
| 45 | 崩溃 postmortem 分类器 | 无（/fresh 仅重置流状态） | ❌ | — |
| **⑥ 权限与安全** | | | | |
| 46 | 审批 tier + 模式比较 | 7 步判定链（yolo 短路→deny→模式硬约束→工具覆盖→命令 glob→能力分级→模式档）+ 元字符命令永不白名单 + sidecar 运行时切换（**超集**） | ✅ | config/src/rules.rs:154-275 |
| 47 | ACP 权限门 + 工具路由 | `session/request_permission` 4 档 + fail-closed 600s 已实现；**无 bash→terminal、fs/*、elicitation/* client 方法路由** | 🟡 | acp/src/rpc.rs:363-393; adapter.rs |
| 48 | secrets HMAC 占位符 + 参数反混淆 | SecretString + ${ENV} 展开 + advisor SecretObfuscator 脱敏；**无工具参数级占位符**（密钥若进对话仍达 provider） | 🟡 | config/src/config.rs; advisor |
| 49 | PAL 隔离启用 | 未接线（见 #9） | ❌⚠ | — |
| **⑦ 对外接口与运行环境** | | | | |
| 50 | 四入口 + SDK + Python 客户端 + robomp | REPL/one-shot/--rpc/--acp/--serve 五形态；无 SDK/类型化 Python 客户端/bot（docs/rpc.md 契约已定稿） | 🟡 | cli/src/main.rs:344-397 |
| 51 | RPC 42 命令 v2 | **3 命令**（prompt/cancel/ping）+ 6 事件 | 🟡 | cli/src/rpc.rs:47-58 |
| 52 | ACP 12+12 方法 | 10 方法 + request_permission | 🟡 | acp/src/rpc.rs:37-80 |
| 53 | provider 68/4409 + 5 认证形态 | 5 适配器（openai/anthropic/deepseek/glm/gemini）+ InBand 方言 + inventory 可扩；无目录数据化/OAuth/coding-plan/keyless | ❌ | llm/src/plugin.rs:33-42 |
| 54 | 内嵌 bash（brush + ~117 uutils） | vendor/pi-shell+brush-core+pi-builtins 迁移进行中（InProcShell，扣留 rm/mv/ln） | 🟡 | crates/shell；vendor/ |
| 55 | cost 记账 | **透传上游：无本地定价表，OpenAI 兼容 API 下 cost_usd 恒 0** | ❌⚠ | grep `cost_usd=` 仅测试命中 |
| 56 | OTel metrics + run-collector | OTLP span + usage 聚合；**无 metrics 导出** | 🟡 | telemetry/src/lib.rs（76 行） |
| 57 | i18n | 4 语种编译期内嵌（**omp 无，超集**） | ✅ | crates/i18n |
| 58 | 部署 npm/compile/Docker/Nix | cargo 单二进制 + tag 触发 release CI；无 Docker/Nix | 🟡 | .github/workflows |

**汇总**：✅ 15 · 🟡 27 · ❌ 9 · ⚠ 叠加标记 8（部分与 🟡/❌ 重叠）。其中 **⚠ 未接线/不一致类 4 项（#4/#15 CLI 装配、#25 security_scan、#9 PAL、#40 Retry-After）是"代码已写、运行时不可达或行为不符"的修复型缺口**，成本极低。

### 2.3 与 08-22 文档的差异（本轮增量发现）

| 项 | 08-22 文档声明 | 本轮源码核验 |
|---|---|---|
| 装配 | "CLI 与 Web/ACP 均已注册全部新工具" | **不符**：CLI 仅注册 hub/memory/task，todo/ask/checkpoint/rewind 仅 server（cli/src/main.rs grep 零命中） |
| security_scan | "v1 本地已落地" | 工具在（security_scan.rs:209，3 项测试），**两端装配层均无注册** |
| 新落地 | — | 压缩前记忆召回（mnemopi preCompactionContext 对齐）、recall 的 queryAsksCurrent/diversifyByCoverage/中文时间表达（已提交 64242e1） |
| 未列项 | — | cost 恒 0（无定价表）、REPL 命令表 26≠30、README 宣称 str_replace/apply_diff 与 4 模式均过时（实际 5 模式）、main.rs 提示文案提及"LSP 服务端"但无该模式 |

---

## 三、全面对齐可行性评估

### 3.1 架构差异（结论：可移植性已被 15 轮实践证明）

| 维度 | omp | Gyre | 对齐判定 |
|---|---|---|---|
| 语言/运行时 | TS on Bun + NAPI addon | 纯 Rust 单二进制 | 已证明；omp 原生层明文 napi-free，vendor 已入仓 |
| 分层 | 引擎/提供/产品三层 | 契约/实现/宿主六边形，trait 注入 | 一一对应 |
| 会话 | JSONL v3 条目树（条目永不删，checkpoint/plan/todo 均以条目为锚） | 节点森林 + leaf 指针，**压缩会收缩/删除节点** | 语义等价，但 **checkpoint 延迟应用 + 压缩互斥需 pinning 语义**（关键风险，见 3.2-⑤） |
| 工具面 | 30+3+别名 + xdev 挂载 | 31 个编译期装配 | 缺口为纯增量 + 接线 |
| 扩展 | TS 动态 import + 25/45 事件钩子 + 市场 | 编译期 + 3 事件钩子 | **不可 1:1**；事件面可扩，动态加载不可 |
| provider | 68/4409 数据资产 + 5 认证形态 | 5 适配器 + inventory | 生态差是数据工程 + 账号设施，非架构 |
| 前端 | TUI 差分引擎（25k TS） | rustyline + React Web | 不可 1:1（既定路线） |

### 3.2 关键阻塞点（按严重度）

1. 🔴 **动态插件/市场**：Rust 无 TS 模块运行时加载，第三方生态对齐不可行——替代已定（inventory + Hook + 文件级命令），需作为明示边界。
2. 🟡 **TUI 差分引擎**：与双前端路线冲突，不迁移；可借鉴其 append-only 回滚契约用于 Web diff 推送。
3. 🟡 **provider 生态数据化 + OAuth/coding-plan**：68/4409 是生成式维护的数据资产，照搬无意义；提炼实际使用的 5–8 家 + 高频模型字段（vision/thinking/context 上限自动判定）即可消除手工清单漂移。OAuth 是账号基础设施（回调服务器 + 凭证保险库 + 17 流），按需 1–2 个 provider 起步。
4. 🟡 **RPC/ACP 接口纵深**：42 命令是 omp 生态（robomp/扩展 UI/host 桥）的载体；Gyre 需先扩协议再谈客户端，negotiate_protocol 版本握手应先行以免事后破坏兼容。
5. 🟡 **checkpoint×压缩互斥**：omp 条目树永不删条目故 rewind 安全；Gyre 压缩会删节点——turn_end 延迟应用必须配合 compactor pinned 节点集 + 语义回退扫描，双保险，否则 rewind 可能回卷到被压缩吞噬的位置。
6. 🟢 其余：tree-sitter grammar 集（版本已统一 0.25，纯增量）；computer/voice 需真机矩阵；PAL rcopy 在大仓库成本高（reflink/overlayfs 后端补齐前，task 隔离默认关闭是合理现状）。

### 3.3 工作量估算（人日，单人全栈，语义移植）

| 档 | 内容 | 估时 |
|---|---|---|
| **P0 修复接线** | CLI 装配补 todo/ask/checkpoint/rewind（复用 server 装配路径）；security_scan 两端装配；REPL 命令表+README 修正；429 Retry-After 兑现 + 指数退避（3 次/500ms 翻倍/30s 上限，对齐 oneshot-retry 语义） | **2–4** |
| **P1 功能补全** | plan propose 批准流 4–6；RPC 协议 v2 扩充（先 15–20 命令：negotiate/steer/set_model/compact/get_state/branch/set_todos…）+ Python 类型化客户端 8–12；hub v2（ACK/唤醒/jobs/launch/复活）5–8；MCP Streamable HTTP+SSE+OAuth discovery 5–8；模型目录数据化（提炼版）4–6；transient oneshot 重试完善 + 跨进程 in-flight 限流 3–5；checkpoint turn_end 延迟应用 + 压缩 pinning + rehydrate 3–5；secrets 工具参数 HMAC 占位符 3–5；PAL 接线（task isolation→Workspace，rcopy 先行）2–4；manage_skill 1–2；自动标题 1–2；read PDF 2–3；cost 定价表 2–3；telemetry metrics 2–3 | **35–55** |
| **P2 外围大件** | OAuth 设备流 v1（1–2 provider）5–8；iso reflink/overlayfs 后端 8–12；computer-use Linux X11 10–15；web_search provider 链 5–8；vibe 模式 5–8；会话导入 3–5；postmortem 分类器 3–5；grammar 扩充 4–6；Docker/Nix 打包 3–5；image_gen 多 provider 3–5 | **30–50** |
| ⛔ 不迁移 | TUI 引擎 / TS 插件市场 / Node SDK / robomp / mnemopi 情景图·三元组深度语义 / voice-WebRTC / browser-relay 扩展 | 替代方案见 §四 |

**合计可对齐项约 67–114 人日；P0+P1（37–59 人日）收敛剩余用户可见差距的 ~80%。**

### 3.4 风险与依赖

- **provider API 漂移**（web_search/OAuth/目录字段）：trait 抽象 + fixture 单测锁定；omp 自证 API 易变。
- **checkpoint×压缩**（3.2-⑤）：先做 pinning 再开 turn_end 延迟应用，顺序不可倒。
- **hub v2 与 swarm/supervisor 命名竞争**：扩展现有 core/hub + supervisor，勿另立。
- **RPC 协议膨胀**：先 negotiate_protocol 版本握手；命令按 omp 42 条清单裁剪，不盲抄。
- **PAL rcopy 性能**：接线时默认 opt-in（配置开关），reflink 后端落地后再默认。
- **secrets 占位符**：HMAC 密钥的本地保管与 deobfuscate 时机（执行前、日志后）需对齐 omp 语义，防密钥进日志。
- **既有测试安全网**：全 workspace 61+ 套测试；新模块按外部契约测试纪律，P0 修复项每项带行为测试。

---

## 四、分阶段实施路线图

### Phase 0 · 立即修复（本周，2–4 人日）—— 消除"已实现但不可达"


> **状态（2026-08-23）：已全部落地**。四项修复 + 两项顺带一致性修正（`/tools` 面板核心工具串 ×4 语言、CLI stdin 提示去 "LSP 服务端" 误称）+ 429 退避回归测试 ×2；验证：`cargo check --workspace --all-targets` 干净、全 workspace 3127 测试通过、REPL 冒烟（`/tools` 面板与提示文案实测更新；`builtin_commands` 26→30）。实现要点：429 在 fallback 链内**同模型**退避重试（≤3 次尝试 / 单次等待 ≤30s / 重试重走 key 轮换 / 等待可被取消打断），openai+anthropic 适配器补 429→`RateLimit` 映射（解析 `Retry-After`，缺省 5s）；security_scan 定 Write 能力档（密钥命中片段入模型上下文属敏感暴露面）。
| 项 | 验收 |
|---|---|
| CLI 装配对齐 | CLI 会话内模型可调 todo/ask/checkpoint/rewind（与 server 同一装配函数抽取复用） |
| security_scan 接线 | 两端注册 + 提示词段；审批 tier 定 Write |
| Retry-After/退避 | 429 时按 retry_after（上限 30s）等待重试 ≥1 次再进 fallback 链 |
| 文档一致性 | REPL 命令表补 /goal /session /resume /?；README 工具清单/模式数修正 |

### Phase A · P1 中程（6–10 周）—— 产品流闭环与接口纵深

优先序（依赖关系驱动）：
1. **RPC 协议 v2 扩充 + Python 客户端**（接口面是后续一切生态的前置；先 negotiate_protocol）。
2. **plan propose 批准流**（复用 xd:// 设备机制已有骨架；Web 弹窗 + CLI 四选一）。
3. **checkpoint turn_end 延迟应用 + 压缩 pinning**（顺序：pinning 先行）。
4. **hub v2**（ACK/唤醒/jobs/launch，supervisor 升级为观测+消息双面）。
5. **MCP HTTP/SSE/OAuth** + **模型目录数据化**（生态双件）。
6. secrets 占位符、PAL 接线、manage_skill、自动标题、PDF、定价表、metrics、in-flight 限流（互相独立，按资源插空）。

### Phase B · P2 外围（按资源排期）

OAuth v1 → iso 高性能后端 → computer-use X11 → web_search 链 → vibe → 会话导入/postmortem → grammar/打包/image_gen。

### 明确不迁移项与替代（对齐边界）

| omp 能力 | 原因 | Gyre 替代 |
|---|---|---|
| TUI 差分引擎 | 与 rustyline+Web 路线冲突 | Web 端借鉴 append-only 回滚契约 |
| TS 动态插件/市场 | Rust 无运行时加载 | inventory + Hook（事件面可从 3 扩到 ~10）+ 文件级命令 |
| Node SDK / robomp | 语言栈/独立产品 | `--rpc` NDJSON（P1 扩协议）+ Python 客户端 |
| mnemopi 深度语义（情景图/三元组/Weibull） | 复杂度/收益比 | structured + 向量 + MMR 中文调优已覆盖主路径（且为中文超集） |
| voice/WebRTC | 生态停滞、编译重 | 如需语音走云端 TTS/STT |

---

## 五、结论

- **引擎层对齐完毕**；产品层 08-22 的 Phase A 六件套已在源码落地，本轮新发现的真实缺口是 **4 处接线型不一致**（CLI 工具面、security_scan、Retry-After、PAL）+ 五类纵深差距。
- **全面对齐可行但需定义边界**：四类不可 1:1 项已有既定替代；其余按 P0（2–4pd，修复）→ P1（35–55pd，闭环）→ P2（30–50pd，外围）推进；P0+P1 即可收敛 ~80% 用户可见差距。
- **两个必须遵守的实施顺序**：压缩 pinning 先于 checkpoint 延迟应用；RPC 版本握手先于命令扩充。
- **建议立即动作**：Phase 0 四件修复（合计 ≤4 人日）先行——它们不成比例地损害当前能力声明的可信度（工具已写、测试已过、运行时不可达）。

---

## 附：本轮审查证据方法说明

- omp 侧计数均来自源码：内置工具 30（builtin-names.ts:1-31 逐项清点）、HookAPI 25 事件（hooks/types.ts:483-512 逐行计数）、ExtensionAPI 45（extensions/types.ts:1222-1278）、KnownApi 14（catalog/types.ts:8-22）、RPC 42 命令（rpc-types.ts:28-77 union 逐条）、models.json 64 供应商/4409 模型（脚本只读计数）、PAL 8 后端（natives index.d.ts:1293-1302）。
- Gyre 侧"缺失"结论均经 grep 反向核验：`AskUserTool|TodoTool|CheckpointTool|RewindTool`（cli/src/main.rs 零命中）、`security_scan|SecurityScan`（cli+server 装配层零命中）、`start_isolation`（crates/ 零调用方）、`xd://propose`（零命中）、`manage_skill`（零命中）。
- 工作区未提交改动经 `git diff --stat` 核对（49 文件，+6602/-558），其中 tools/fs.rs +621、memory_tool.rs +525、compaction.rs +239、structured.rs +292、temporal.rs +262、synonyms.rs +189、eval/manager.rs +133、shell.rs +257。
