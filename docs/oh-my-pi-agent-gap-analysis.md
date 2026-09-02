# oh-my-pi Agent 功能对比分析与 Gyre 对齐路线图

> 调研对象：`third/oh-my-pi`（v18.0.0，badlogic/pi-mono fork，MIT）vs 本仓库 Gyre（Rust workspace，31 crates，~77k 行）
> 调研日期：2026-08-22
> 方法：6 个并行只读 scout 精读双方源码（omp 侧 packages/{agent,ai,coding-agent,catalog} + docs + crates/*；Gyre 侧全部 31 crates + web + config），以**源码为唯一事实源**（仓库内既有 docs/oh-my-pi-*.md 仅作主张，逐项代码验证）；缺工具/缺能力点均经 grep 反向核验。
> 证据格式：omp 路径相对 `third/oh-my-pi/`，Gyre 路径相对仓库根。

---

## 〇、结论速览

1. **定位关系**：Gyre 是 omp 的"同源移植件"，此前 14 轮改进已把**循环引擎层与大部分智能/产品层**落地。本轮七维核查结论：**引擎层全对齐，产品层约 80% 对齐，差距集中在工具面 9 项、read 能力面、provider 生态与少量子系统**。
2. **可对齐性：高**。剩余差距中约 70–90%（按工作量计）可落地，且 omp 的 Rust 原生层（pi-shell/pi-walker/pi-builtins 已 vendor 进本仓库）刻意 napi-free，逻辑可近乎逐字搬运。**不可对齐项仅三类，且均有既定替代方案**：TS 动态插件/市场（→ inventory 编译期注册）、TUI 差分渲染引擎（→ rustyline + Web 双前端）、Node SDK/robomp（→ 已有 `--rpc` NDJSON 线协议）。
3. **工作量**：P0 快赢 14–17 人日；P1 中程 50–77 人日；P2 大件 43–65 人日。**P0+P1（约 65–95 人日）可收敛 80% 用户可见差距**。
4. **Gyre 已有超越 omp 的项**（对齐时勿回退）：审批链模式写入硬约束（ask 只读 / architect·plan 仅 `plans/*.md`，yolo 下仍生效）、含 shell 元字符命令永不自动放行、结构化记忆中文时间表达 + MMR 中文调优、4 语 i18n、SOCKS5 动态出站代理、DeepSeek V4 / GLM preserveReasoning 协议细节。

---

## 一、oh-my-pi Agent 功能清单与核心架构

### 1.1 总体架构：三层 + 原生支撑

```
┌─ 产品层  packages/coding-agent（~1MB TS，AgentSession 9.7k 行）
│   会话/审批/工具工厂/子代理/记忆/技能/plan-mode/checkpoint/TurnRecovery/slash 命令/扩展
├─ 引擎层  packages/agent（@oh-my-pi/pi-agent-core）
│   Agent 门面 + agent-loop 双循环 + 事件流 + 上下文管线 + 压缩策略 + steering/pause
├─ 提供层  packages/ai（@oh-my-pi/pi-ai）
│   streamSimple 统一入口 → 15 种 KnownApi 方言 + auth a/b/c 凭证轮换 + transient 重试 + in-flight 限流
└─ 支撑    pi-catalog（models.json 4409 模型/64 provider/9.6MB）· pi-natives（Rust N-API，22 模块）
           pi-tui（25k 行差分渲染）· pi-wire · snapcompact · omptype（schema 编译校验）· mnemopi
```

- **agent 引擎**（packages/agent/src/agent.ts:354 + agent-loop.ts:987 `runLoopBody`）：双循环——内循环跑工具批 + steering 中断轮询（250ms），外循环在 followUp/asides 到达时续跑；事件顺序契约 `agent_start → (turn_start → message_start → message_update* → message_end → tool_execution_* → toolResult) → turn_end* → agent_end`（agent-loop.ts:545-600）。
- **provider 层职责收敛**：pi-ai 只做单模型流式 + 凭证轮换 + 瞬时重试 + 限流；**模型 fallback 链、角色路由、TurnRecovery 全部在 coding-agent 层**（session/retry-fallback-chains.ts、model-resolver.ts）。
- **会话载体**：AgentSession（coding-agent）包 pi-agent-core Agent；SessionManager 用 JSONL v3 条目树持久化（message/compaction/branchSummary/custom/label/modelChange…，id/parentId + leaf 指针，blob 外置 sha256）。

### 1.2 七维功能清单

**① 任务规划与多步执行**
- plan-mode：`local://<slug>-plan.md` → `xd://propose` → PlanApprovalDetails 弹窗 → 批准后压缩接管 → 执行（plan-mode/state.ts、tools/resolve.ts）；`/plan`、`/plan-review`；prewalk（编辑前切廉价模型）。
- todo 工具：pending/in_progress/completed/abandoned/blocked，**单活跃不变量**（置新自动闭环旧，todo.ts:146）、phase 化、Markdown 往返、模糊匹配、持久化进 session entries 可分支回放。
- goals：单目标 objective + token/墙钟预算，turn 级核算含 cacheWrite，active/paused/budget-limited/complete/dropped 状态机，goal 工具 5 op（goals/runtime.ts）。
- magic keywords：ultrathink / orchestrate / workflowz，仅散文触发（markdown-prose.ts 长度保持掩码）。
- checkpoint/rewind：单活跃检查点（仅记 messageCount + entryId），rewind 副作用全部延迟到 turn_end（替换上下文 + 分支摘要 + 隐藏 rewind-report + yield 守卫强制回卷完成）；可 rehydrate 恢复。
- 子代理：task 工具四变体（flat/batch × isolation × effort），**ArkType schema 校验的 typed 输出**（permissive/strict），PAL 写时复制隔离（8 后端），IRC 进程级邮箱总线（回执/唤醒/复活），Agent Hub TUI（Alt+A 舵手/杀/复活/看 transcript），vibe 模式（director + fast/good 常驻 worker），/tan 全后台代理。
- auto-thinking：分类器按 prompt 难度定 Effort，默认 ceiling xhigh。

**② 工具与插件调用机制**
- 工具接口 `AgentTool`（types.ts:762-840）：omptype 编译式 schema、intent 字段注入（`i` 参数，可关）、onUpdate 流式进度、concurrency shared/exclusive/fn、interruptible、matcherDigest（TTSR 流式匹配）、approval 声明、renderCall/renderResult。
- 内置工具 **31 个**（builtin-names.ts）+ xdev 挂载 15 个 discoverable（browser/gh/lsp/checkpoint/rewind/security_scan/ask/ast_*/memory_*…，xdev.ts：`write xd://<tool>` 传 JSON args，todo/ask/grep/web_search 强制保顶）+ 隐藏 yield/goal/think + 别名 search→grep。
- 扩展机制：**TS 模块动态 import（不沙箱，同进程）**，ExtensionFactory + HookAPI 事件面（session_start/before_switch/before_agent_start/tool_call 可 block 可改写/tool_result 可改写…，hooks/types.ts:390-598）；marketplace 兼容 Claude 插件注册表（git/目录/直链，user/project 两级 scope）；slash 命令 7 源优先级（native>omp-plugins>claude>claude-plugins>agents>codex>opencode）。
- MCP 客户端：stdio / Streamable HTTP / legacy SSE 三传输 + OAuth discovery/flow + 资源订阅；`mcp__<server>_<tool>` 命名去重。
- read 单工具统一面：文件/目录/归档/SQLite/PDF/notebook(.ipynb)/URL/ssh:// + 16 个内部 scheme（pr:// issue:// agent:// history:// skill://…）+ selector 语法。

**③ 上下文与记忆管理**
- 管线 `transformContext → convertToLlm → normalizeMessagesForProvider → transformProviderContext`（agent-loop.ts:1514-1565）；append-only 模式（AppendOnlyLog，唯一突变 replaceTail 留给压缩，StablePrefix 字节指纹冻结 system+tools）。
- 压缩策略全套（compaction/ 8 文件）：触发=threshold/percent + reserve=max(15%窗口,16384)；切点永不切 toolResult；方法序 **remote → snapcompact → handoff → shake → soft**（session-maintenance.ts），失败 willRetry + 降级 + rescue shake；OpenAI Responses 服务端压缩。
- 记忆后端四选一互斥：off / local（learned.md）/ hindsight（情景 bank + mental models）/ mnemopi（向量检索 + MCP server）；工具面 retain/recall/reflect/memory_edit/learn/manage_skill + autolearn（实质回合后自动捕获）+ 压缩前 preCompactionContext 召回。
- skills：文件发现/过滤/系统提示块/mid-prompt 调用/skill:// 协议/manage_skill CRUD；上下文文件 8 格式导入（Cursor MDC/Cline/Codex AGENTS.md/Copilot applyTo/.codex/.gemini/.windsurf/.opencode）。
- 原生 tokenizer（pi-natives）：claude-v3/v47/v5/v5-sonnet/qwen3/deepseek-v3/kimi-k2/glm5 编码 + 字节上界免算 + thinkingSignature 计入。

**④ 多轮对话与状态维护**
- 会话树：JSONL v3 条目树 + branch()/resetLeaf()/branchWithSummary()；/fork /tree（交互导航器：过滤/搜索/标签/总结切换）/resume /new /fresh /clear /drop；自动标题（title-generator 22KB）；OAuth pin（/session pin）；从 Claude/Codex 会话导入。
- 事件流：AgentSessionEvent（auto_compaction_*/auto_retry_*/session_switch…）+ AgentEvent 细粒度（message_update 含 AssistantMessageEvent：text/thinking/toolcall start/delta/end）。
- steering/followUp/asides 三队列 + interruptMode immediate/wait + yieldQueue 空闲续跑；collab 会话镜像到 `~/.omp/collab/<room>.jsonl` 复用 resume 机制。

**⑤ 错误恢复与重试策略**
- ai 层：auth a/b/c 凭证轮换（AUTH_RETRY_MAX_ATTEMPTS=64，403/usage-limit/OAuth 失效直轮）+ transient oneshot 重试（3 次指数退避，Retry-After 上限 30s）+ **跨进程 in-flight 限流**（文件锁）+ replay 安全缓冲。
- 会话层 TurnRecovery（turn-recovery.ts 96KB）：AIError 分类（transient/usage-limit/empty-response/abort）；empty-stop 重试 + developer 提示 + **丢回合持久化 reparent**；unexpected-stop 分类器；usage-limit → 凭证轮换 + retry-after/backoff；模型 fallback 角色链 + cooldown 恢复；**上下文溢出不重试 → 压缩接管**；失败回合 retryRecovery 注解持久化。
- 引擎层：合成 error/aborted 消息 + `__synthetic` 标记；replay-policy 过滤 refusal；Harmony 泄漏自愈（truncate-resume 上限 2 / abort-retry 上限 2 / escalated）；/fresh 重置 provider 流状态；崩溃 postmortem 分类器。

**⑥ 权限控制与安全边界**
- 审批：工具声明 tier read|write|exec + 用户 `tools.approval.<name>` allow/deny/prompt + 模式比较（always-ask ≤ read / write ≤ write / yolo ≤ exec），yolo 下用户 deny 仍生效；MCP origin/reason 详情。
- ACP 远程审批门：仅 bash/edit/delete/move 需客户端许可（allow_once/allow_always/reject_once/reject_always），破坏性意图检测（acp-permission-gate.ts）。
- secrets：secrets.yml（项目+全局）+ env KEY/SECRET/TOKEN 模式 + 内置凭证 regex；**HMAC 可逆占位符**（密钥字节永不达 provider，参数执行前 deobfuscateToolArguments）+ redact-only 兜底。
- 隔离：task worktree PAL 8 后端（apfs/btrfs/zfs/reflink/overlayfs/projfs/rcopy）+ spawn-policy / read-only-policy / restrictToolNames；security_scan（preflight 计划 + SHA-256 树指纹 + 陈旧拒绝 + SARIF）；computer exposure 门控。

**⑦ 对外接口与运行环境依赖**
- 四入口：TUI（默认）/ `-p` 无头 / `omp --mode rpc`（NDJSON，v1/v2，~50 命令，`--mode rpc-ui` 扩展帧）/ `omp acp`（ACP JSON-RPC，工具路由 bash→terminal、read→fs、write→fs、edit/bash→request_permission）+ Node SDK 内嵌 + Python omp-rpc 类型化客户端 + robomp bot。
- provider 面：**68 条 provider descriptor / 4409 模型 / 9.6MB models.json**（catalog/src/provider-models/descriptors.ts:70-543）；认证 5 形态（env key / ~10 个 OAuth 流 / coding-plan 订阅 / keyless 本地 / 自定义 models.yml 9 方言）；凭证解析 7 级优先级。
- 运行时：Bun ≥ 1.3.14（Bun 专用 API）+ N-API pi_natives addon（5 平台 triple）+ 内嵌 brush bash + 67–68 个进程内 uutils + sherpa-onnx（TTS/STT）+ puppeteer-core + onnxruntime-node；部署 npm / bun --compile 单二进制 / 多阶段 Docker / Nix flake；shell completions 自生成。

---

## 二、Gyre 现状核查与功能差距矩阵

### 2.1 Gyre 架构一句话

纯 Rust 单二进制：`agent-core` 零依赖契约层（Tool/MemoryStore/ApprovalPolicy/Hook/ContextManager/LlmProvider 端口）→ `agent`（async_stream run_loop 五态机 + steering 三通道 + 审批门禁 + shared/exclusive 工具批调度）→ `context`（会话树森林 + active_leaf + StablePrefix 指纹 + 三级压缩 + JSONL 持久化）→ `llm`（ProviderRegistry + inventory 插件 + fallback 链 + KeyRing）→ `config`（分层 TOML + RulesEngine 7 步审批链）；宿主四入口 CLI / Web(axum REST+WS) / ACP(stdio+HTTP+SSE) / `--rpc`(NDJSON)。工具/Provider 全部 trait + inventory 编译期注册，无动态加载。

### 2.2 差距矩阵（✅ 已实现 · 🟡 部分实现 · ❌ 缺失 · ⚠ 行为不一致）

| # | 能力（omp） | Gyre 现状 | 判定 | 证据 |
|---|---|---|---|---|
| **① 任务规划与多步执行** | | | | |
| 1 | agent loop：turn/steering/followUp/asides/pause/deadline/interruptMode(250ms 轮询) | run_loop 五态机 + 三通道 drain + PauseGate + deadline + 250ms 批级 cancel | ✅ | crates/agent/src/engine.rs:78-1356 |
| 2 | queueMode/steeringMode | 三通道已按**新版**命名实现（omp 旧名已废弃，无对齐问题） | ✅ | agent/src/lib.rs:327-343 |
| 3 | plan-mode（local://plan → xd://propose → 批准弹窗 → 压缩接管执行） | `/plan` + architect/plan 写约束仅 `plans/*.md` + handoff；**无 propose 批准弹窗流** | 🟡 | crates/config/src/rules.rs:88-111 |
| 4 | todo 工具（单活跃不变量/phase/Markdown 往返/分支回放） | **无**（grep 核验零命中） | ❌ | — |
| 5 | goals（goal 工具 5 op + 预算状态机） | GoalBudget token/time/hard_stop + `/goal set\|extend`（非工具形态，语义近似） | ✅ | agent/src/lib.rs:45-66 |
| 6 | magic keywords（ultrathink/orchestrate/workflowz + prose 掩码） | 已实现（keywords.rs），仅散文/词边界 | ✅ | crates/agent/src/keywords.rs:1-123 |
| 7 | checkpoint/rewind（turn_end 应用序 + yield 守卫 + rehydrate） | **无**（会话树 fork/switch 仅部分补偿） | ❌ | — |
| 8 | task 子代理：typed schema 输出 + PAL 8 后端 + IRC + 复活 + Agent Hub | task 工具（深度护栏 16 / 并发 4 / token 预算 / iso rcopy / supervisor 观测 / swarm DAG）；**缺 typed 输出、IRC、hub、复活、Hub UI** | 🟡 | agent/src/task_tool.rs:324；crates/iso/src/lib.rs:60-230 |
| 9 | vibe 模式（director + fast/good 常驻 worker）、/tan、/btw | **无** | ❌ | — |
| 10 | auto-thinking 分类器 | LlmThinkingClassifier（Auto policy，失败回退） | ✅ | llm/src/thinking.rs |
| **② 工具与插件** | | | | |
| 11 | AgentTool 接口（omptype schema/intent/onUpdate/concurrency/interruptible/matcherDigest） | Tool trait（name/schema/capability/concurrency/interruptible/describe/partial update）；**无 intent 注入、无 matcherDigest**（TTSR 匹配在 ttsr crate 侧做） | 🟡 | tools/src/lib.rs:104-150 |
| 12 | 核心工具 read/write/edit/hashline/ast_grep/ast_edit/grep/glob/bash/lsp/debug/eval/browser/web_search/github/inspect_image | 全部在场（read_file/write_file/apply_hashline/ast_search/ast_rewrite/replace_block/run_command/run_pty_command/grep/glob/lsp/lsp_apply/debug/eval/browser/web_search/github/read_image/image_gen/ssh） | ✅ | tools/src/* |
| 13 | ask 工具（模型侧结构化提问，canPromptUser 门控） | **无**；仅宿主侧审批 AskMessage（模型无法主动提问） | ❌ | — |
| 14 | hub 工具（IRC + 后台任务 + 进程监督） | **无**（swarm 静态 YAML + supervisor 被动观测） | ❌ | — |
| 15 | checkpoint/rewind / security_scan / computer / memory_edit / learn / manage_skill | **全部缺失**（store 层有 forget/search/banks 但未暴露工具） | ❌ | — |
| 16 | fetch | read_file http:// 部分补偿（无专用工具） | 🟡 | tools/src/fs.rs |
| 17 | xd:// 设备机制（15 discoverable 工具挂载，保顶集合） | 仅 xd://pending/resolve/reject（ast_rewrite 暂存） | 🟡 | tools/src/fs.rs:921-930 |
| 18 | MCP 客户端三传输 + OAuth discovery + 资源订阅 | stdio 单传输 + tools/list/call + resources/read；无 OAuth | 🟡 | mcp/src/tool.rs |
| 19 | 扩展/插件：TS 动态 import + marketplace（Claude 兼容） | inventory 编译期注册 + Hook trait（BeforeTool/AfterTool/Stop/on_turn_end）；**无市场** | 🟡（替代方案） | core/src/hook.rs |
| 20 | hooks 事件面（tool_call block/改写、tool_result 改写、session 事件） | before_tool_intercept / after_tool_override 程序化钩子 + 事件面较窄 | 🟡 | core/src/hook.rs:50-90 |
| 21 | slash 命令 7 源优先级 + ~38 CLI 子命令 | 28 REPL 命令 + `.agent/commands/*.md` 自定义命令 | 🟡 | cli/src/repl.rs:145-382 |
| 22 | read 统一面：归档/SQLite/PDF/ipynb/selector 语法 | 文件/目录/URL/ssh/9 内部 scheme + `:conflicts`；**缺归档/SQLite/PDF/ipynb/selector**（grep 核验零命中） | 🟡 | tools/src/fs.rs:127-198 |
| **③ 上下文与记忆** | | | | |
| 23 | 上下文管线 transformContext→convertToLlm→transformProviderContext | build_provider_context + sanitize_provider_messages + StablePrefix digest | ✅ | context/src/lib.rs:362-391,750-830 |
| 24 | append-only + StablePrefix 字节指纹 + replaceTail 唯一变异 | 逐字节移植（AppendOnlyLog 语义 + 分支保全） | ✅ | context/src/lib.rs:180-260 |
| 25 | 压缩方法序 remote→snapcompact→handoff→shake→soft + 降级 | shake→summarize/snapcompact→prune + remote 回退本地 | ✅ | agent/src/engine.rs:340-400 |
| 26 | 切点永不切 toolResult + tool-protection（skill/artifact 保护） | supersede_read_results + 配对强制器 + skill:// 保护 | ✅ | context/src/tool_protection.rs |
| 27 | 原生 tokenizer（9 族编码 + 字节上界） | tiktoken-rs cl100k/o200k + chars/4 回退（非 OpenAI 模型精度低） | 🟡 | context/src/token.rs:8-138 |
| 28 | 记忆四后端 + learn/autolearn + memory_edit + mental models | local/structured 双后端 + recall/retain/reflect + 向量（L1 投影/L2 fastembed）+ 心智模型 + auto-retain + consolidate；**缺 learn/memory_edit 工具面**；中文时间表达/MMR 调优为**超集** | 🟡 | memory/src/structured.rs:460-560 |
| 29 | skills 发现/mid-prompt 调用/manage_skill/learn 提升 | file-backed + skill:// + 注入 + /skills；**缺 manage_skill/learn 提升** | 🟡 | skills/src/registry.rs |
| 30 | 上下文文件 8 格式导入 | discovery 4 来源（AGENTS.md/CLAUDE.md/Cursor .mdc/Cline） | 🟡 | crates/discovery |
| **④ 多轮与状态** | | | | |
| 31 | 会话树 JSONL v3 条目 + blob 外置 + branch/resetLeaf/branchWithSummary | SessionNode 森林 + JSONL + .leaf sidecar + set_active_leaf + switch_branch_with_handoff | ✅ | context/src/persistence.rs |
| 32 | /tree 交互导航（过滤/搜索/标签/总结切换） | REST /branches 列表 + switch；**无交互导航器 UI** | 🟡 | server/src/lib.rs:2702 |
| 33 | 自动标题 / 会话导入（--from-claude/--from-codex）/ /new /move | `_titles.json` 标题表；**无自动生成标题、无外部会话导入** | 🟡 | context/src/persistence.rs:175-390 |
| 34 | 事件流细粒度契约（AssistantMessageEvent text/thinking/toolcall） | AgentEvent + 三层生命周期 + TextDelta/ThinkingDelta | ✅ | core/src/message.rs:509-590 |
| 35 | collab：relay + full/view token + QR + 快照恢复 + guest 页 + transcript | 本地 host 中继 + 16B write token + 48KiB 分块 + guest 页 + transcript；**缺托管 relay/QR** | 🟡 | server/src/lib.rs:3189-3312 |
| **⑤ 错误恢复与重试** | | | | |
| 36 | auth a/b/c 凭证轮换（64 上限）+ OAuth 刷新 | KeyRing round-robin（多 key）；**无 OAuth** | 🟡 | agent/src/lib.rs:265-269 |
| 37 | transient 重试（3 次退避 + Retry-After 30s）+ 跨进程限流 | 瞬时错误白名单恢复 + **无退避重试、无 in-flight 限流** | 🟡 | engine.rs:770-800 |
| 38 | TurnRecovery（empty-stop 重试 + reparent、unexpected-stop 分类、cooldown、注解持久化） | 模型级/Provider 级双 fallback 链 + mistakes 上限 + 无 cooldown/注解持久化 | 🟡 | llm/src/registry.rs:63-113 |
| 39 | Harmony 泄漏自愈（truncate-resume/abort-retry/escalated） | 双计数器 + temperature 扰动 | ✅ | agent/src/harmony.rs |
| 40 | 合成 error/aborted + __synthetic 标记 / refusal 过滤 | persist_interrupted + 占位回填 + refusal-like 不重放 | ✅ | engine.rs:1660-1687 |
| 41 | 崩溃恢复（postmortem 分类器 / /fresh） | /fresh 重置流状态；**无 postmortem 分类器** | 🟡 | cli/src/repl.rs |
| **⑥ 权限与安全** | | | | |
| 42 | 审批 tier + 用户 allow/deny/prompt + 模式比较 | 7 步判定链（yolo 短路→deny→模式硬约束→工具覆盖→命令 glob→能力分级→模式档）+ 运行时 sidecar 切换 | ✅（超集） | config/src/rules.rs:117-231 |
| 43 | ACP 远程审批门（bash/edit/delete/move + allow_once/always） | **无 request_permission**（ACP 写走本地审批链） | ❌ | acp/src/rpc.rs:38-57 |
| 44 | secrets：HMAC 可逆占位符 + deobfuscateToolArguments | SecretString + ${ENV} 展开 + advisor SecretObfuscator；**无工具参数级占位符** | 🟡 | config/src/config.rs:236-243 |
| 45 | 隔离：PAL 8 后端 + spawn/read-only policy | iso 8 变体**仅 rcopy 实现**（其余 7 为诚实 Stub + 回退）+ task 深度护栏 | 🟡 | iso/src/lib.rs:60-230 |
| 46 | 危险命令模式（bash.patterns 强制 prompt） | 命令 deny 黑名单 + 元字符硬拒（更严） | ✅ | config/src/rules.rs:398-410 |
| **⑦ 对外接口与运行环境** | | | | |
| 47 | 四入口 TUI/print/RPC/ACP + Node SDK + Python 客户端 + robomp | REPL/one-shot/--rpc/--acp/--serve；**无 SDK、无类型化 Python 客户端、无 bot** | 🟡 | cli/src/main.rs:237-256 |
| 48 | RPC v1/v2 ~50 命令 + rpc-ui 扩展帧 | --rpc 仅 prompt/cancel/ping 3 命令 | 🟡 | cli/src/rpc.rs:38-53 |
| 49 | ACP 工具路由（bash→terminal/read→fs/write→fs） | 方法面 9 个（new/prompt/cancel/load/close/set_mode…）；**无工具路由** | 🟡 | acp/src/rpc.rs |
| 50 | provider 生态 68/4409 + 10 OAuth + 编码计划 + models.yml 9 方言 | 4 适配器（openai 兼容/anthropic/deepseek/glm）+ gemini 仅枚举 + inventory 可扩；**无目录数据化** | ❌ | llm/src/plugin.rs:38-49 |
| 51 | 模型目录数据化（vision/thinking 自动判定） | 手工 config + snapcompact vision 通配清单 | ❌ | config.example.toml |
| 52 | 内嵌 bash（brush + 67 uutils 零 fork） | vendor/pi-shell+brush-core 迁移中（InProcShell + 50+ 命令，扣留 rm/mv/ln；31m 前仍在改） | 🟡（进行中） | crates/shell/src/lib.rs |
| 53 | TUI 差分渲染引擎 | rustyline + Web 双前端（**既定路线**） | ⚠ 不可对齐 | — |
| 54 | 插件市场 / TS 模块扩展 | inventory 编译期（**既定替代**） | ⚠ 不可对齐 | — |
| 55 | i18n | 4 语言（**超集**，omp 无） | ✅ | crates/i18n |
| 56 | 可观测：OTel GenAI semconv + run-collector 覆盖率 | OTLP span + usage/cost 记账；无覆盖率指标 | 🟡 | crates/telemetry |

**汇总**：✅ 22 · 🟡 23 · ❌ 9 · ⚠ 2（不可对齐）。缺失项全部集中在**工具面（9 项：ask/hub/todo/checkpoint/rewind/security_scan/computer/memory_edit/learn/manage_skill）**与 **provider 生态数据**。

### 2.3 行为不一致清单（⚠）

1. **审批语义更严**：Gyre 模式写入硬约束（ask 全只读、architect/plan 仅 `plans/*.md`）在 yolo 下仍生效，omp 无此语义——同一 yolo 会话行为不同（有意加固，勿回退）。
2. **snapcompact v1 中文失真**：CJK 码点折叠为 `?`（omp 有全角/组合符处理 + SQuAD eval 调优的帧形状）——中文会话压缩质量劣于 omp。
3. **eval 单后端**：仅 Python（omp py/js/rb/jl 四内核 + 环回桥）。
4. **task 无 typed 输出**：父代理解析散文（omp 直接读 schema 校验对象）。
5. **记忆 reflect 语义**：Gyre reflect=LLM 提炼心智模型写入（注入可见）；omp reflect=对 bank 综合答案——同名不同义。
6. **queueMode 对齐**：omp 已废弃改名，Gyre 实现与新语义一致（无实际不一致）。
7. **provider 协议细节**：Gyre DeepSeek V4/GLM preserveReasoning 为自有实现（omp 无对应方言），行为以各自协议为准。

---

## 三、全面对齐可行性评估

### 3.1 架构差异（核心结论：可移植性已被证明）

| 维度 | omp | Gyre | 对齐难度 |
|---|---|---|---|
| 语言/运行时 | TS on Bun（Bun 专用 API）+ N-API addon | 纯 Rust 单二进制 | 已证明（14 轮移植；omp 原生层明文 napi-free） |
| 分层 | 引擎(agent)/提供(ai)/产品(coding-agent) 三层 | 契约层/实现层/宿主层 六边形 | 一一对应，trait 注入等价依赖注入 |
| 扩展 | TS 动态 import + marketplace | inventory 编译期 + Hook trait | **不可 1:1**，替代方案已定 |
| 会话 | JSONL v3 条目树 | SessionNode JSONL 树 + leaf sidecar | 语义等价（条目类型面窄） |
| 工具面 | 31 + 15 xdev + 3 隐藏 | 20 个 + 可选组装配 | 缺口 9 项纯增量 |
| provider | 68/4409（models.json 数据化） | 4 适配器（枚举 7） | 生态差是工作量不是架构问题 |
| 前端 | TUI 差分引擎 25k TS | rustyline + React Web | 不可 1:1（既定路线） |

### 3.2 关键阻塞点（按严重度）

1. 🔴 **动态插件/市场**（不可绕过）：Rust 无 TS 模块运行时加载。已有替代：inventory 编译期注册 + Hook trait（BeforeTool/AfterTool/Stop/on_turn_end）+ `.agent/commands/*.md` 文件级命令。**市场/第三方生态对齐不可行，需明示**。
2. 🟡 **TUI 引擎**（不可绕过）：与双前端路线冲突，明确不迁移；可借鉴"append-only 原生回滚契约"思想于 Web 端 diff 推送（低成本）。
3. 🟡 **provider 生态数据化**：68 provider/4409 模型目录是生成式维护的资产。Gyre 只需提炼实际 4 家 + 高频模型的**数据化目录**（vision/thinking/context 上限自动判定），不必照搬 9.6MB。
4. 🟡 **OAuth/编码计划订阅**：10 个 OAuth 流 + coding-plan 订阅是账号基础设施（回调服务器 + 凭证保险库）。Gyre 现为 env key + KeyRing；OAuth 设备流 v1 可按需补。
5. 🟢 **托管 collab relay**：omp 的 relay 是 Go 服务（未随仓库分发）；Gyre 本地 host 中继已可用，托管化是部署决策非代码差距。
6. 🟢 **tree-sitter 版本**：已统一 0.25（workspace Cargo.toml 证实），grammar 集 5 vs 50+ 是纯增量（编译时间/CI 是主要成本）。
7. 🟢 **平台矩阵**：computer/voice 需 mac/win 真机 CI；Linux X11 先行。

### 3.3 工作量估算（人日，单人全栈，语义移植）

| 档 | 条目 | 估时 | 前置 | 优先级 |
|---|---|---|---|---|
| P0 | todo 工具（状态机+工具+持久化+/todo） | 2 | 无 | 🔴 |
| P0 | ask 工具（模型侧结构化提问，接 AskMessage 通道） | 2 | 无 | 🔴 |
| P0 | memory_edit + learn 工具面（store 层已就绪） | 2 | 无 | 🔴 |
| P0 | ACP 远程审批门（request_permission + allow_once/always） | 3 | 无 | 🔴 |
| P0 | eval JS 内核（Bun/Deno 或 node worker + 环回桥复用） | 2–3 | eval crate 已就绪 | 🔴 |
| P0 | Gemini 适配器（google-generative-ai 已枚举） | 3–5 | llm trait 已就绪 | 🔴 |
| P1 | checkpoint/rewind（turn_end 应用序 + yield 守卫 + 压缩 pinning） | 4–6 | engine 已拆分 | 🟡 |
| P1 | hub 工具 + IRC 总线（进程内邮箱 + 后台任务） | 5–8 | task_tool 已就绪 | 🟡 |
| P1 | task typed 输出（outputSchema 校验 + 增量标签） | 3–5 | task 已有 | 🟡 |
| P1 | read 扩充（sqlite/归档/ipynb 往返；PDF 可选） | 6–8 | 无 | 🟡 |
| P1 | web_search provider 链 + 抽取器（3–6 provider + 10–20 站点） | 7–11 | trait 已就绪 | 🟡 |
| P1 | 模型目录数据化（提炼版，vision/thinking 自动判定） | 4–6 | 无 | 🟡 |
| P1 | Python 类型化 RPC 客户端（按 docs/rpc.md） | 3–5 | --rpc 已就绪 | 🟡 |
| P1 | OAuth 设备流 v1（凭证轮换接入 KeyRing） | 5–8 | secrecy 已有 | 🟡 |
| P1 | iso 高性能后端（reflink FICLONE + overlayfs） | 10–15 | iso trait 已就绪 | 🟡 |
| P1 | secrets 工具参数脱敏（HMAC 可逆占位符） | 3–5 | 无 | 🟡 |
| P2 | security_scan v1（preflight + 本地扫描 + status/cancel） | 10–15 | 无 | 🟢 |
| P2 | computer-use Linux X11 v1（截图 + XTEST 注入） | 10–15 | 无 | 🟢 |
| P2 | tree-sitter grammar 扩充（先 10–15 语言） | 4–6 | 版本已统一 | 🟢 |
| P2 | stats 深度聚合（gain/窗口/用户指标） | 4–6 | /api/stats 已有 | 🟢 |
| P2 | metaharness 场景级 eval（真实任务回放） | 10–15 | criterion 已立 | 🟢 |
| P2 | 托管 collab relay + QR | 5–8 | collab 已就绪 | 🟢 |
| ⛔ | TUI 引擎 / 插件市场 / robomp / Node SDK / voice-WebRTC / mnemopi 深度语义 | 不迁移 | 替代方案见 §四 | ⛔ |

**合计**：P0 14–17 · P1 50–77 · P2 43–65 → **可对齐项约 110–160 人日；P0+P1 ≈ 65–95 人日收敛 80% 收益**。

### 3.4 风险与依赖

- **provider API 漂移**（web_search/OAuth）：trait 抽象 + 懒加载 + fixture 单测锁定（omp 自证 API 易变，docs 已过期 2 处）。
- **checkpoint/rewind 与压缩互斥**：Gyre 压缩会删节点（omp 条目永不删）——必须 compactor pinned 节点集 + 语义回退扫描，双保险。
- **hub/IRC 与 swarm/supervisor 命名竞争**：扩展现有 crates，不另立重复实现。
- **ACL/安全面**：security_scan 的 OAuth/知识库面去掉（v1 本地化）；hooks 若开放外部脚本即代码执行面（v1 仅编译期注册）。
- **platform 构建**：overlayfs/reflink 需 Linux 真机矩阵测试；grammar 大集拖慢 CI 增量编译（分批）。
- **许可**：omp MIT、uutils MIT、brush MIT——无冲突（vendor 已入仓）。
- **既有 61 套测试安全网**：新模块按"外部契约测试、禁源码 grep、full-suite safe"纪律。

---

## 四、分阶段实施路线图

### Phase 0 · 前置（0.5–1 周，可并行）
- 无硬前置（tree-sitter 已 0.25；engine.rs 已二次拆分；shell 迁移进行中可继续收尾）。
- 建议先固化"差距矩阵"为验收基线（本报告 §二），每项完成即回填状态。

### Phase A · P0 快赢（2–3 周，14–17 人日）—— 补全工具面 6 项
| 项 | 验收 |
|---|---|
| todo 工具 | 单活跃不变量 + 会话持久化 + REPL `/todo`；以 omp todo.ts 行为契约移植测试 |
| ask 工具 | 模型可调用 ask → 走既有 AskMessage 通道（CLI stdin / Web WS / ACP 三端自动生效） |
| memory_edit + learn | 暴露 store 层 forget/search/banks + 教训写入；autolearn 可后置 |
| ACP 权限门 | `session/request_permission` + allow_once/allow_always/reject；bash/edit/delete/move 四类路由 |
| eval JS 内核 | 与 Python 内核同构（NDJSON + 环回桥 token 模型照搬） |
| Gemini 适配器 | `google-generative-ai` 枚举转真实适配器（SSE + thinking 映射），入 fallback 链测试 |

### Phase B · P1 中程（6–10 周，50–77 人日）—— 会话智能与生态
| 项 | 优先级建议 |
|---|---|
| checkpoint/rewind | 最高：探索式工作流刚需；先 tools 对 + turn_end 应用，后 yield 守卫与压缩 pinning |
| hub + IRC | 次高：多代理编排从"静态 DAG"升级"活会话"；supervisor 升级为观测+消息双面 |
| task typed 输出 | 与 hub 同批（task 工具面增强） |
| read 扩充（sqlite/归档/ipynb） | 高性价比；PDF 放最后（依赖选型 mupdf-rs/poppler） |
| web_search 扩充 | provider 每个 0.5–1pd、抽取器每个 0.25–0.5pd，全部 fixture 单测 |
| 模型目录数据化 | 提炼版 models.json（4 家高频字段），消除 snapcompact/压缩的手工清单漂移 |
| Python RPC 客户端 | 按 docs/rpc.md 写 py.typed + 单测 |
| OAuth 设备流 v1 | 先 1–2 个 provider（如 GitHub/OpenAI）验证管线，再扩展 |
| iso reflink/overlayfs | 大仓库子代理隔离成本直接下降 |
| secrets 工具参数脱敏 | 防密钥字节达 provider（与 advisor SecretObfuscator 复用） |

### Phase C · P2 大件（按资源排期，43–65 人日）
security_scan v1（去 OAuth 面）→ computer-use Linux X11 → grammar 扩充（10–15 语言）→ stats 深度聚合 → metaharness 场景 eval → 托管 relay/QR。逐项独立，无相互依赖。

### 明确不迁移项与替代方案（对齐边界）
| omp 能力 | 原因 | Gyre 替代 |
|---|---|---|
| TUI 差分引擎（25k TS） | 与 rustyline + Web 双前端路线冲突 | Web 端借鉴 append-only 回滚契约推送 diff |
| 插件市场 / TS 扩展 | Rust 无模块动态加载 | inventory 编译期注册 + Hook trait + 文件级命令；第三方生态对齐不可行（明示边界） |
| Node SDK 内嵌 | 语言栈不符 | `--rpc` NDJSON（已有）+ Python 客户端（Phase B） |
| robomp bot | 独立产品（20–30pd） | 用 `--rpc` 自建（协议已定稿） |
| 内嵌 bash 全量（brush+uutils 88k） | 环境税 | **已部分迁移**（vendor + crates/shell 进行中），按现状收尾即可，不追 1:1 |
| mnemopi 深度语义（情景图/三元组/复调） | 复杂度/收益不成比例 | 向量记忆 + MMR 中文调优已覆盖主路径 |
| voice/WebRTC 实时通话 | webrtc 0.17 生态停滞、编译重 | 如需语音：云端 TTS/STT 先行 |

---

---

## 六、实施回填（2026-08-22 当日完成）

报告发布当日即按 Phase A + 部分 Phase B/C 落地，全部改动**零回归**（全 workspace 测试通过）。
矩阵状态更新如下（证据为改动后的源码位置）：

| 矩阵条目 | 原状态 | 现状态 | 落地内容与证据 |
|---|---|---|---|
| ④ todo 工具 | ❌ | ✅ | `crates/tools/src/todo.rs`：五阶段机 + 单活跃不变量（start 自动闭环旧）+ `.gyre/todo.json` 原子持久化 + REPL `/todo`；13 项测试 |
| ③ ask 工具 | ❌ | ✅ | `crates/tools/src/ask.rs`：经 `ApprovalPolicy::prompt` 通道（CLI stdin / Web WS / ACP request_permission 同路），options 渲染 + 拒绝→可恢复错误 + steering 可中断；7 项测试 |
| ③ memory_edit + learn | ❌ | ✅ | `crates/tools/src/memory_tool.rs`：MemoryEditTool（search/forget/banks/clear+confirm）+ MemoryLearnTool（fact/lesson/mental_model）；`MemoryStore` trait 增默认方法 `forget`/`banks`，StructuredMemoryStore 真实删除（`crates/memory/src/structured.rs`）；12 项测试 |
| ⑥ ACP 远程审批门 | ❌ | ✅ | `crates/acp/src/{rpc,stdio,http}.rs`：`session/request_permission`（allow_once/always、reject_once/always、text 选项按 AskKind），超时 fail-closed 10min；经 `session.inbound` 回执（与 Web 同通道）；33 项测试 |
| ② eval JS 内核 | 🟡 | ✅ | `crates/eval/src/`：JS_DRIVER（node --input-type=module，NDJSON 同帧协议 + console 捕获 + tool.call 桥回调），`EvalSettings.node` 探测，EvalTool `language` 参数；13 项测试 |
| ⑦ Gemini 适配器 | ❌ | ✅ | `crates/llm/src/gemini.rs`（~730 行）：`Api::GoogleGenerativeAi` 全链路（SSE 状态机 + thinkingConfig 仅 2.5/3 系 + functionCall 合成 id + finishReason/错误映射与 glm 分类学一致）；85 项测试（含 15 新） |
| ① checkpoint/rewind | ❌ | ✅ | `crates/tools/src/checkpoint.rs` + `ToolContext.context` 字段：单活跃检查点（leaf+节点数），rewind 经 `switch_branch_with_handoff` 折叠分支（confirm 门禁 + 失败恢复检查点）；4 项测试 |
| ① task typed 输出 | 🟡 | ✅ | `crates/agent/src/{schema_check,task_tool}.rs`：JSON Schema 子集校验器（type/properties/required/items/enum/界限/组合子）+ `output_schema` 契约（`<output_contract>` 注入 + 校验失败同上下文纠错重试 ×2 + 紧凑 JSON 返回）；12 项测试 |
| ② read 二进制格式面 | 🟡 | ✅ | `crates/tools/src/fs.rs`：zip 族（zip/jar/war/ear/apk/whl）+ tar/tar.gz 成员浏览与读取、SQLite 表浏览 + `::SELECT` 只读查询（写/多语句拒绝）、ipynb 单元格渲染；新依赖 zip/tar/flate2/rusqlite(bundled)；4 项测试 |
| ② hub + 总线 | ❌ | ✅（v1 进程内） | `crates/core/src/hub.rs`（注册表 + 定向邮箱 + 错误语义）+ `crates/tools/src/hub_tool.rs`（send/recv/list，以 "main" 入册）；跨进程 IPC/回执/复活明确留待下轮；5 项测试 |
| ② security_scan | ❌ | ✅（v1 本地） | `crates/tools/src/security_scan.rs`：9 类密钥正则 + PEM 头 + 危险文件权限 + 上限防护（2000 文件/1MiB）+ 脱敏；3 项测试 |

**未完成（明确标注，非静默缩减）**：
- 模型目录数据化（models.json 提炼版）：config crate 数据工程，下轮
- computer-use X11：依赖选型（x11rb/xcap/enigo）+ 真机验证，下轮
- hub 跨进程 IRC/回执/复活、security_scan 远程知识库/SARIF、checkpoint turn_end 延迟应用：v2 边界

**装配**：CLI（`crates/cli/src/main.rs` build_agent）与 Web/ACP（`crates/server/src/lib.rs` assemble_agent）
均已注册全部新工具；todo/ask/checkpoint/rewind/security_scan/hub 恒开，memory_edit/learn 随 `[memory]` 启用。
新增工具未进子 Agent 工具集（ask 会绕过父会话、todo/checkpoint/hub 与父共享状态互踩、security_scan 并发抢 IO——有意为之，文档已注明）。

**测试**：`cargo test --workspace` 全绿（31 crate 零失败）；`cargo build --workspace` 干净。

## 五、结论

- **引擎层（七维中的循环/状态/压缩/恢复/审批内核）：Gyre 与 omp 已对齐，且 3 处超集**（模式硬约束、元字符命令硬拒、记忆中文语义）。
- **差距的 80% 集中在 9 个缺失工具与 read/provider 数据面**——全部是纯增量工作，无架构阻塞。
- **全面对齐"可行"但需定义边界**：TUI/插件市场/Node SDK/robomp 四类不可 1:1 对齐，替代方案已存在或已落地；其余按 P0（14–17pd）→ P1（50–77pd）→ P2（43–65pd）推进。
- **建议开工顺序**：Phase A 六件套先行（工具面补全，两周量级即可让模型侧行为对齐 omp）；Phase B 以 checkpoint/rewind 与 hub+IRC 为两个里程碑；每次落地以本报告矩阵回填为验收。
