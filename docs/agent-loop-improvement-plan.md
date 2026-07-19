# Agent 循环改进计划

> 对标 `third/oh-my-pi` 的 `packages/agent`（`agent-loop.ts` + `compaction/` + `append-only-context.ts`），
> 识别 Gyre `crates/agent` 循环的差距并分阶段提升。
>
> 基准对照：
> - Gyre 循环本体：[`crates/agent/src/lib.rs`](../crates/agent/src/lib.rs) `run_loop`
> - Gyre 压缩实现：[`crates/context/src/compaction.rs`](../crates/context/src/compaction.rs)
> - oh-my-pi 循环本体：[`third/oh-my-pi/packages/agent/src/agent-loop.ts`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) `runLoop`

## 背景与现状

Gyre 已正确移植的核心能力（保持，不列入改进）：

- Ports & Adapters 解耦（Provider/Tools/Context/Prompt/Approval 以 trait 注入）
- 软工具需求（提醒 → 升级强制）：[`lib.rs`](../crates/agent/src/lib.rs) soft_requirement
- steering 中途打断（非阻塞消费注入消息）
- 流中断兜底持久化（`persist_interrupted`，避免丢失已生成回复）
- ISO 隔离（worktree / rcopy）
- LSP 写入效果（编辑后 format/diagnostics）
- length 截断续写自愈、`stop_reason == None` 自愈
- 多 Provider（anthropic / openai / glm / deepseek）

## 改进项（按优先级）

---

### 🔴 P0-1：重写上下文压缩 Shake（块检测 + 占位化 + 落盘）

**问题**：当前 [`Compactor::shake`](../crates/context/src/compaction.rs) 仅去连续重复 Status、删空助手消息，压缩能力极弱。长会话中大型 ToolResult（读文件、grep 输出）持续占用窗口。oh-my-pi 的 [`shake.ts`](../third/oh-my-pi/packages/agent/src/compaction/shake.ts) 用「外科手术式压缩」逐块替换。

**差距清单**：

| 能力 | Gyre | oh-my-pi |
|---|---|---|
| 围栏代码块 / 顶层 XML 元素检测替换 | ❌ | ✅ `scanTextForBlockRanges` |
| 重型 ToolResult → 占位符 | ❌（直接删） | ✅ `ToolResultShakeRegion` |
| 被替换内容落盘到 artifact（可回溯） | ❌（信息丢失） | ✅ offload/persistence |
| 保护最近 N token 不动 | ❌ | ✅ `protectTokens: 16_000` |
| 最小节省阈值门控 | ❌ | ✅ `minSavings: 4_000` |

**任务**：

- [x] 移植 `scanTextForBlockRanges`：扫描围栏代码块（```` ``` ```` / `~~~`）与顶层 XML 元素 span，返回 `[start, end)` 区间（移植 [`shake.ts`](../third/oh-my-pi/packages/agent/src/compaction/shake.ts) 第 131-183 行逻辑）。
- [x] 引入 `ShakeConfig { protect_tokens, min_savings, fence_min_tokens, tool_result_min_tokens }`，移植 DEFAULT_SHAKE_CONFIG 默认值。
- [x] 实现 ToolResult 整体占位化：超阈值 token 的 ToolResult 替换为占位符 `[已归档：工具结果…，详见 artifact://<id>]`，保留 `tool_call_id`（配对完整）。
- [x] 实现 artifact 落盘：被替换大块写入 `.gyre/artifacts/<id>`（内容哈希去重）；`read_file` 支持 `artifact://<id>` 回读（fs.rs `resolve_artifact`，id 仅允许十六进制防穿越）。
- [x] 保护窗口：从日志尾部累计 token，落在 `protect_tokens` 内的条目不参与归档。
- [x] 节省阈值门控：估算节省量，低于 `min_savings` 则整体跳过（避免无效 churn）。
- [x] cli/server 装配注入 `DirSink`（指向 `<cwd>/.gyre/artifacts`）。

**验收**（全部通过，`cargo test -p agent-context` 35/35）：

- [x] 单元测试：含大型代码块的日志 shake 后 token 数显著下降且占位符带 artifact 指针。
- [x] 回归：现有 `shake_drops_duplicate_status` / `shake_drops_empty_assistant` 测试保持通过。
- [x] 压缩前后信息不丢：`DirSink` 落盘 + `read_file artifact://...` 可还原原始内容。
- [x] 新增覆盖：块检测（围栏/XML/未闭合/围栏内 XML 抑制）、保护窗口、阈值门控、配对完整性、DirSink 去重与回读。

**参考实现**：[`third/oh-my-pi/packages/agent/src/compaction/shake.ts`](../third/oh-my-pi/packages/agent/src/compaction/shake.ts)、`tool-protection.ts`

---

### 🔴 P0-2：工具并发执行（shared / exclusive）

**问题**：Gyre 工具执行严格顺序：[`lib.rs`](../crates/agent/src/lib.rs) `for (id, name, args) in tool_calls` 串行。一轮里模型并发请求多个 `read_file` 时，I/O 被串行化。oh-my-pi 的 [`executeToolCalls`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) 并发执行，按工具声明的 `concurrency` 调度。

**任务**：

- [x] 定义 [`enum Concurrency { Shared, Exclusive }`](../crates/tools/src/lib.rs)（可后续扩展为 `Fn(args) -> Concurrency`）。
- [x] 给 [`Tool`](../crates/tools/src/lib.rs) trait 加 `fn concurrency(&self) -> Concurrency`；默认按 [`CapabilityTier`](../crates/core/src/tool.rs) 分级——ReadOnly → `Shared`，Write/Execute/Network → `Exclusive`（无需逐工具覆写即覆盖 write_file / apply_hashline / replace_block / run_command）。
- [x] 改造 [`lib.rs`](../crates/agent/src/lib.rs) 工具执行段：审批串行（Ask 逐个）→ 抽取 [`schedule_and_run`](../crates/agent/src/lib.rs) 调度器，Shared 用 `futures::future::join_all` 并发、Exclusive 作屏障串行（先排空前一批 Shared 再单独执行）。
- [x] 审批门禁：保持逐工具 Ask（安全优先），Allow 的进入执行桶；Deny/未知/用户拒绝直接回填错误、不执行。
- [x] 结果按原始调用顺序回填 + 发射事件（确定性顺序，便于观测/重放）。

**验收**（全部通过，`cargo test -p agent` 4/4）：

- [x] 单元测试：3 个只读探针（各 60ms）并发执行，最大在途数 ≥2、总耗时 < 150ms（远小于串行 180ms）。
- [x] 回归：3 个写探针 Exclusive 串行，最大在途数 == 1。
- [x] 覆盖：`Concurrency` 默认映射（ReadOnly→Shared，Write/Execute→Exclusive）。
- [x] 既有 `persist_interrupted` 测试保持通过；全 workspace 编译。

**参考实现**：[`third/oh-my-pi/packages/agent/src/agent-loop.ts`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) `executeToolCalls` 第 1603-2013 行，尤其 concurrency 调度 1973-1991。

---

### 🟡 P1-1：分级压缩触发（shake 优先，省昂贵 summarize）

**问题**：当前 [`lib.rs`](../crates/agent/src/lib.rs) near_limit 时无脑 `shake → summarize → prune` 三连。summarize 要发一次 LLM 请求（贵、慢、可能失败），应仅在 shake 不够时才上。

**任务**：

- [x] 改造 [`near_limit`](../crates/agent/src/lib.rs) 分支为分级压缩：先 shake → 重建评估；已脱离上限则停（不再 summarize）。
- [x] 仍超限时才 summarize；summarize 后仍超限才 prune 兜底。状态消息按实际触发的阶段动态拼接（`shake` / `shake + summarize` / `shake + summarize + prune`）。
- [x] 记录压缩到日志：shake 经 [`compact`](../crates/context/src/lib.rs) 记录 `saved/tool_results/blocks`；循环记录触发的阶段序列。
- [x] 新增 [`set_shake_config`](../crates/context/src/lib.rs)（`InMemoryContext`/`PersistentContext`）支持调参（保护窗口/阈值/块门槛）。

**验收**（通过，`cargo test -p agent` 5/5；context 35/35；tools 36/36）：

- [x] 测试 [`staged_compaction_skips_summarize_when_shake_suffices`](../crates/agent/src/lib.rs)：构造近上限且 shake 可救回的场景（大 ToolResult + 计数 summarize + 桩 Provider），验证 `summarize` 调用次数 == 0、状态仅报告 `shake`。

---

### 🟡 P1-2：运行控制（deadline + pause_turn + 覆盖率遥测）

**问题**：Gyre 缺三类停止场景处理与运行级可观测性。

| 场景 | Gyre | oh-my-pi |
|---|---|---|
| `pause_turn`（非终止停顿，Codex 进度更新） | ❌ 直接结束 | ✅ 最多续 8 次 |
| 整轮 deadline 超时 | ❌ | ✅ `isDeadlineExceeded` |
| GPT-5 Harmony 协议泄露检测 | ❌ | ✅ truncate/abort/escalate |
| 增量 thinking signature 失效 | ❌ | ✅ transformMessages 剥离签名 |
| OTEL span / AgentRunCoverage | ❌（仅 turns/tool_calls/usage） | ✅ 完整链路 + 覆盖率 + skipped tool |

**任务**：

- [x] [`AgentBuilder`](../crates/agent/src/lib.rs) 加 [`deadline(Duration)`](../crates/agent/src/lib.rs)；循环顶部检查 `Instant::now() >= deadline_at`，超时优雅停止（`Say` 警告 + `Stop` 钩子 + `Done(success=false)` + `Idle`）。
- [x] [`StopReason`](../crates/core/src/message.rs) 新增 `Pause` 变体；循环识别 `Pause` 且无工具调用时，在 [`MAX_PAUSED_CONTINUATIONS`](../crates/agent/src/lib.rs)（=8）上限内重新采样续写，超限按完成停止；有工具调用的轮次重置计数。移植 oh-my-pi `MAX_PAUSED_TURN_CONTINUATIONS`。
- [~] ~~扩展 `AgentRunSummary`（skipped_tools / 每轮 token / 单工具耗时）~~ — **跳过（属遥测范畴，按需不做）**。
- [~] ~~[`crates/telemetry`](../crates/telemetry/Cargo.toml) tool 执行 OTEL span~~ — **跳过（遥测，不做）**。

**验收**（通过，`cargo test -p agent` 7/7；context 35/35；tools 36/36；core 10/10）：

- [x] 测试 [`deadline_stops_run_gracefully`](../crates/agent/src/lib.rs)：循环每轮都调工具（永不自然结束），80ms 后被 deadline 停止，以 `Done(success=false)` 收尾并发出 deadline 警告。
- [x] 测试 [`pause_turn_resamples_then_caps`](../crates/agent/src/lib.rs)：`PausingProvider` 每轮返回 `Pause`，循环重采样至上限后停止，provider 调用次数恰为 `MAX_PAUSED_CONTINUATIONS + 1`。
- [~] *遥测相关项（AgentRunSummary 指标 / OTEL span）按用户要求跳过。*

---

### 🟡 P1-3：会话树 / 分支导航

**问题**：Gyre [`InMemoryContext`](../crates/context/src/lib.rs) 是 `log: Vec<AgentMessage>` 纯线性，无法「撤销某步重试」「探索两条方案」。oh-my-pi 是会话树（每条消息带 parentId，可 fork/切换）。这也限制了 [`SessionList.tsx`](../web/c5-ui/src/components/agent/SessionList.tsx) 无法做分支视图。

**任务**：

- [x] 给 [`SessionNode`](../crates/core/src/context.rs) 加 `id` + `parent_id`，`InMemoryContext.log` 升级为节点 forest（[`tree.rs`](../crates/context/src/tree.rs) 纯函数：路径回溯 / 最近公共祖先 / 叶子枚举 / 节点删除 / 迁移）。
- [x] [`build_provider_context`](../crates/context/src/lib.rs) 改为从当前活跃叶子向根回溯构建消息序列（`branch_path_nodes`）。
- [x] 分支切换时复用 [`summarize`](../crates/context/src/compaction.rs) 生成 handoff（移植 [`branch-summarization.ts`](../third/oh-my-pi/packages/agent/src/compaction/branch-summarization.ts) 的 `collectEntriesForBranchSummary` → [`collect_entries_for_branch_summary`](../crates/context/src/tree.rs)；经 [`switch_branch_with_handoff`](../crates/context/src/lib.rs) 注入）。
- [x] 持久化格式（[`PersistentContext`](../crates/context/src/persistence.rs)）升级为 `SessionNode` JSONL + 活跃叶子 sidecar（`.jsonl.leaf`），旧线性日志（裸 `AgentMessage` 行）无损迁移为单链树。
- [x] TUI/Web 侧：后端 [`GET /api/sessions/{id}/branches`](../crates/server/src/lib.rs) 分支树 + [`POST .../branches/switch`](../crates/server/src/lib.rs) 切换（含 handoff）；`read_history`/`read_first_user` 兼容新格式。前端 [`BranchTreeModal.tsx`](../web/c5-ui/src/components/agent/BranchTreeModal.tsx) 递归渲染分支树（高亮活跃路径 + 叶子切换 + handoff 选项），经 [`SessionList.tsx`](../web/c5-ui/src/components/agent/SessionList.tsx) 行菜单「分支」入口打开；`useAgentSession` 新增 `fetchBranches` / `switchBranch`（切换后自动重连重载 transcript）。

**关键设计（树形压缩保枝）**：[`compact_active_path`](../crates/context/src/lib.rs) 用「贪心匹配保留命中节点 id + 保留被 off-path 分支依赖为祖先的节点」替代粗暴重建——压缩只影响 LLM 看到的活跃路径，**分支数据零丢失、挂载点不上移**；无依赖的旧前缀节点才删除（常见无分支压缩不污染叶子列表）。

**验收**（全部通过，`cargo test --workspace` 全绿；agent-context 48/48、agent-core 10/10、agent-server 4/4）：

- [x] 测试 [`fork_and_switch_reflects_each_path`](../crates/context/src/lib.rs)：在节点 b fork 出 b1、c1，切换叶子时上下文正确反映各自路径（a,b,b1 ↔ a,b,c1）。
- [x] 测试 [`prune_preserves_other_branch`](../crates/context/src/lib.rs)：压缩主分支后，旁支（branch-x）路径仍完整可达（m0,m1,branch-x）。
- [x] 测试 [`switch_branch_with_handoff_injects_summary`](../crates/context/src/lib.rs)：切换时被离开分支的独有后缀经 `SummaryProvider` 折叠为 handoff 摘要注入新分支。
- [x] 迁移测试 [`legacy_linear_jsonl_migrates_to_single_chain_tree`](../crates/context/src/persistence.rs)：旧 JSONL 日志无损加载为单链树。
- [x] 持久化测试 [`fork_persists_across_reload`](../crates/context/src/persistence.rs)：fork 产生的多分支跨 reload 保留。

**说明**：工作量最大，建议在 P0/P1 前几项落地后再启动。

---

### 🟢 P2-1：记忆语义检索（向量）

**问题**：[`structured.rs`](../crates/memory/src/structured.rs) 用 BM25-ish 词项重叠，对「同义不同词」召回差。oh-my-pi 原版用 SQLite + 向量嵌入（独立 embed-worker）。

**任务**：

- [ ] `MemoryRecord` 加 `embedding: Option<Vec<f32>>` 字段。
- [ ] retain 时经 [`crates/llm`](../crates/llm/src/lib.rs) provider 抽象调嵌入模型生成向量。
- [ ] recall 增加 cosine 相似度通道，与现有 BM25/重要性/时间加权融合。
- [ ] 存储仍用 JSONL（避免引入 SQLite 依赖）；规模上来再评估迁移。

**验收**：召回测试中「同义不同词」query 能命中相关记忆。

---

### 🟢 P2-2：provider 方言 / in-band 工具

**问题**：GLM/DeepSeek 等模型 function-calling 不稳。oh-my-pi 用 13 种 dialect，owned dialect 把工具规格渲染进 prompt 文本、不发原生 `tools`，并解析 in-band tool 调用。Gyre [`crates/llm/src/transform.rs`](../crates/llm/src/transform.rs) 方言覆盖窄。

**任务**：

- [x] 实现 [`Dialect::Xml`](../crates/llm/src/dialect.rs)：把工具规格 + 调用格式渲染进 system prompt（不发原生 `tools`），并从模型文本解析 `<tool_call>{json}</tool_call>` 为 `ContentBlock::ToolCall`（模型无关、可移植；解析失败保留原文便于纠正）。
- [x] 实现 [`InbandProvider`](../crates/llm/src/inband.rs) 包装任意 `LlmProvider`：请求侧清空 `tools`/`tool_choice`、注入工具段；响应侧累积文本、在 `MessageEnd` 重建内容为「纯文本 + ToolCall 块」并修正 `stop_reason = ToolUse`，agent 循环据此执行（与原生路径一致）。
- [x] 可切换：[`wrap_inband_if`](../crates/llm/src/inband.rs) 经环境变量 `GYRE_INBAND_TOOLS=1` opt-in（cli/server 装配处），默认走原生 function-calling，零行为变化。
- [x] 增量流式抑制：新增 [`XmlToolStreamParser`](../crates/llm/src/dialect.rs)（跨 chunk 安全），InbandProvider 改为**边收边解析**——`<tool_call>`/`</tool_call>` 标记与内联 JSON 不再作为 TextDelta 下发（UI 不泄露标记），仅下发普通文本；工具调用在闭合后于 `MessageEnd` 重建。批量 `parse_tool_calls` 现复用同一解析器（单一真相源）。
- [ ] *后续可选：GLM 私有标记（`<|tool_call|>`）方言（需真实 GLM 验证其 in-band 协议）。*

**验收**（通过，`cargo test -p agent-llm` 56/56；全 workspace 编译）：

- [x] dialect 测试：渲染含工具名/格式/schema；解析单/多调用保序、周边文本保留、无调用原样返回、非法 JSON 保留可见、未闭合按文本、缺 arguments 默认空对象。
- [x] inband 测试：从带 `<tool_call>` 的流式文本提取出 `read_file` ToolCall，并把 `stop_reason` 修正为 `ToolUse`。

---

---

## 第二轮深度对比（2026-07）：新发现差距

> 在第一轮 P0/P1 大部分落地后，逐文件精读 [`third/oh-my-pi`](../third/oh-my-pi/packages/agent/src) 的 `agent-loop.ts`（`runLoopBody` / `executeToolCalls` / `streamAssistantResponse`）、`append-only-context.ts`、`replay-policy.ts`、`thinking.ts`、`compaction/openai.ts`，对照 Gyre 现状，识别出**计划文档此前未涵盖**的 6 项差距。其中 P0-A（字节级稳定前缀）与 P0-B（length + 残缺 tool_call）分别直接影响成本与正确性，建议优先。

### 🔴 P0-A：AppendOnlyLog 字节级稳定前缀（最大化 prompt cache 命中）

**问题**：Gyre [`build_provider_context`](../crates/context/src/lib.rs) 每轮 `convert_to_llm(active_path_messages())` **重建整个** `ProviderMessage` Vec，且 [`cache_key: None`](../crates/agent/src/lib.rs)。oh-my-pi 的 [`AppendOnlyContextManager.syncMessages`](../third/oh-my-pi/packages/agent/src/append-only-context.ts) 维护「每条消息 digest」，找最长字节稳定前缀，仅 truncate 到分歧点再 append 尾部——provider 的 KV 缓存保持到分歧点，模型只从变化消息重新 prefill。

**现状**：Gyre 已有 [`fingerprint_of`](../crates/context/src/lib.rs)（system + tools 冻结）与 Anthropic [`anthropic_apply_cache`](../crates/llm/src/transform.rs) 多点 `cache_control` breakpoint，但 breakpoint 只「标记可缓存点」，**实际命中取决于发送字节是否稳定**。一旦 `supersede_read_results` / shake 归档 / 图像剥离 / steering 重写改动任一中间消息，整段对话被重新序列化、前缀缓存全失效。

**差距清单**：

| 能力 | Gyre | oh-my-pi |
|---|---|---|
| 每条消息 digest 追踪 | ❌ | ✅ `#messageDigests` |
| 最长字节稳定前缀截断 | ❌（全量重建） | ✅ `#longestStablePrefix` |
| 分歧点之后增量 append | ❌ | ✅ `syncMessages` case 3 |
| 压缩/重写后保留稳定前缀 | ❌（全清重发） | ✅ 仅当数组短于上次才 clear |
| `cache_key` 传递 | ❌（恒 `None`） | ✅ sessionId 回退 |

**影响**：长会话每轮重复 prefill 数万 token（oh-my-pi 注释援引 #3406：单条消息重写触发 ~40k token 全量 re-prefill，本地 / llama.cpp 后端尤甚）。延迟与成本双升。

**任务**：

- [x] 在 [`InMemoryContext`](../crates/context/src/lib.rs) `Inner` 加 `prefix_digests: Vec<u64>`（每条 [`ProviderMessage`](../crates/core/src/message.rs) 的确定性 digest，由 [`digest_message`](../crates/context/src/lib.rs) 基于 `Debug` 格式化 + `DefaultHasher` 计算）。
- [x] [`build_provider_context`](../crates/context/src/lib.rs) 计算 [`longest_stable_prefix`](../crates/context/src/lib.rs)（新 digest 序列 vs 上次），填入新增的 [`ProviderContext.stable_prefix_len`](../crates/core/src/context.rs)。注：`convert_to_llm` 本身确定性，故前缀字节本就稳定；`stable_prefix_len` 把「稳定到第几条」显式暴露给 provider，供精确放置 `cache_control` breakpoint（移植 [`syncMessages`](../third/oh-my-pi/packages/agent/src/append-only-context.ts) 的「append 命中 / 压缩 clear / 原地重写保前缀」语义）。
- [x] [`CompletionRequest.cache_key`](../crates/core/src/llm.rs) 从 [`built.fingerprint`](../crates/agent/src/lib.rs) 回退填充（[`agent/lib.rs`](../crates/agent/src/lib.rs)，原恒 `None`）。
- [x] invalidate 时机：[`compact_active_path`](../crates/context/src/lib.rs)（统一压缩入口）/ [`set_active_leaf`](../crates/context/src/lib.rs) / [`switch_branch_with_handoff`](../crates/context/src/lib.rs) / [`set_system`](../crates/context/src/lib.rs) 四处 `inner.prefix_digests.clear()`（移植 [`invalidateForModelChange`](../third/oh-my-pi/packages/agent/src/append-only-context.ts)）。

**验收**（通过，`cargo test -p agent-context` 52/52；全 workspace 编译 + 测试全绿；clippy 对改动 crate 无新警告）：

- [x] 测试 [`build_tracks_stable_prefix_on_append`](../crates/context/src/lib.rs)：连续 append 时第二次 build 的 `stable_prefix_len` == 上次消息数（前缀全命中）。
- [x] 测试 [`stable_prefix_partial_hit_when_middle_deleted`](../crates/context/src/lib.rs)：删除中间消息后首条 digest 命中、分歧点之后重发（`stable_prefix_len == 1`）。
- [x] 测试 [`compact_clears_stable_prefix`](../crates/context/src/lib.rs)：压缩后 `compact_active_path` 清空 digest → `stable_prefix_len == 0`（全量重放）。
- [x] 测试 [`set_system_invalidates_stable_prefix`](../crates/context/src/lib.rs)：system 变更后稳定前缀归 0（即使 messages 未变）。
- [x] 回归：原有 48 个 context 测试零失败（含 fork / branch handoff / persistence 迁移）。

**后续可选增强**（非本批次）：provider 层消费 `stable_prefix_len`——如 Anthropic [`anthropic_apply_cache`](../crates/llm/src/transform.rs) 把 `cache_control` breakpoint 从固定「倒数第二条」改为落在稳定前缀末尾，进一步减少缓存段重建。

**参考实现**：[`append-only-context.ts`](../third/oh-my-pi/packages/agent/src/append-only-context.ts) `AppendOnlyContextManager` / `syncMessages` / `#messageDigest`。

---

### 🔴 P0-B：length 截断 + 残缺 tool_calls 占位补全（正确性 bug）

**问题**：Gyre [`run_loop`](../crates/agent/src/lib.rs) 仅在 `truncated && tool_calls.is_empty()` 时续写。若输出被 `max_tokens` 截断**且** assistant 已含 `ToolCall` 块，会落到下方正常执行路径，**执行参数可能被截断（JSON 残缺）的工具调用**——引发工具报错甚至误操作（如 `write_file` / `apply_hashline` 参数不全）。

**差距清单**：

| 场景 | Gyre | oh-my-pi |
|---|---|---|
| length 截断且无 tool_calls → 续写 | ✅ | ✅ |
| length 截断且有 tool_calls → 占位跳过 + 续写 | ❌（直接执行残缺调用） | ✅ `createAbortedToolResult("length")` + 续写 |
| error/aborted 轮含 tool_calls → 占位补全配对 | ❌ | ✅ 维持 tool_use/tool_result 配对 |

**任务**：

- [x] [`run_loop`](../crates/agent/src/lib.rs) 加 `else if truncated && !tool_calls.is_empty()` 分支：为每个 tool_call 回填 `ToolResult::Error { recoverable: true, ... }` 占位（保留 `tool_call_id` 配对），注入续写指令进入下一轮（受 `max_turns` 保护，移植 oh-my-pi `runLoopBody` length skip）。
- [x] `stop_reason == Error/Aborted` 且含 tool_calls：新增 `else if matches!(stop_reason, Error | Aborted) && !tool_calls.is_empty()` 分支，回填占位 result 后以 `success=false` 立即 `Done` 终止（不续写、不执行，满足严格校验配对的 provider：GLM / Z.ai 等）。

**验收**：

- [x] 测试 [`length_truncated_tool_call_gets_placeholder_not_executed`](../crates/agent/src/lib.rs)：length 截断 + 含 ToolCall → 工具未执行（`max_seen==0`）、回填占位 result（`tool_call_id="trunc1"` 命中）、续写（provider 调用 ≥2）。
- [x] 测试 [`error_stop_reason_with_tool_call_gets_placeholder_and_stops`](../crates/agent/src/lib.rs)：Error + tool_calls → 工具未执行、回填占位、立即终止（provider 仅调 1 次、`success=false`，未陷入 max_turns 循环）。
- [x] 回归：全 workspace 测试全绿（agent 9/9、context 52/52、其余 crate 零失败）。

**参考实现**：[`agent-loop.ts`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) `runLoopBody` 第 999-1018 行（length / deadline skip）、第 886-913 行（error / aborted 占位）。

---

### 🟡 P1-C：软工具升级护栏（升级上限 + 非合规 detour 跳过）

**问题**：Gyre [`soft_requirement`](../crates/agent/src/lib.rs) 只做「上一轮未调用 → 本轮强制」升级判定，**无升级次数上限**（模型持续不从可致无限强制循环），且非合规 detour 工具**会被实际执行**（产生副作用）。

**差距清单**：

| 能力 | Gyre | oh-my-pi |
|---|---|---|
| 升级次数上限（避免无限强制） | ❌ | ✅ `MAX_SOFT_TOOL_ESCALATIONS = 3` → abort |
| 非合规 detour 不执行 | ❌（执行） | ✅ 配 `skipped` 占位，不触发副作用 |
| 「仅调用所需工具」合规判定 | ❌ | ✅ `calledOnlyRequiredTool` |

**任务**：

- [x] [`run_loop`](../crates/agent/src/lib.rs) 加 [`MAX_SOFT_TOOL_ESCALATIONS = 3`](../crates/agent/src/lib.rs) 常量 + `soft_escalations` 计数；非合规轮 `saturating_add(1)`，超过上限则回填占位后 `Done(success=false)` 中止（移植 [`agent-loop.ts`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) 第 951-956 行）。
- [x] 软需求 pending 时，在工具执行段前加非合规检查：`compliant = !tool_calls.is_empty() && all(name == required)`（移植 `calledOnlyRequiredTool`）。非合规（含 detour 或空）→ 每个 tool_call 回填 `ToolResult::Error { recoverable: true }` 占位（提示「请先调用所需工具」），**不执行**、`escalate_soft = true` 强制下轮；合规后重置 `soft_escalations = 0`。

**验收**：

- [x] 测试 [`soft_requirement_skips_detour_and_aborts`](../crates/agent/src/lib.rs)：`AlwaysDetourProvider` 每轮返回 detour（`other`），`soft_requirement = required_tool` → detour `max_seen == 0`（未执行）、`calls < 10`（在 escalate 上限 abort，非跑满 `max_turns=100`）、`success == false`。
- [x] 回归：全 workspace 测试全绿（agent 10/10、context 52/52、其余 crate 零失败）。

**参考实现**：[`agent-loop.ts`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) `MAX_SOFT_TOOL_ESCALATIONS` + `softNonCompliant` 分支（第 938-981 行）。

---

### 🟡 P1-D：工具结果 coerce / malformed 归一化

**问题**：第三方工具（MCP / 扩展 / 用户自写 `AgentTool`）可能返回畸形结果（content 非数组、缺字段、执行抛异常），直接持久化会污染会话文件、令下一轮请求 400。oh-my-pi [`coerceToolResult`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) 统一归一化 + 标记 malformed。

**任务**：

- [x] 审查结论：Gyre 已基本规避 oh-my-pi `coerceToolResult` 场景——[`ToolResult`](../crates/core/src/message.rs) 是强类型 enum（无「content 非数组」畸形）；[`run_pending_task`](../crates/agent/src/lib.rs) 已把 `Err(ToolError)` 归一化为 `ToolResult::Error`；MCP [`tool.rs`](../crates/mcp/src/tool.rs) 把 `call_tool` 失败映射为 `ToolError::Execution`；空 content 经 [`convert_to_llm`](../crates/context/src/lib.rs) 填占位「(无输出)」。**剩余唯一风险**：`tool.execute()` panic（第三方工具 unwrap None / 越界）会无 catch 传播终止整个 agent run。
- [x] 补全：[`run_pending_task`](../crates/agent/src/lib.rs) 用 `std::panic::AssertUnwindSafe + futures::FutureExt::catch_unwind` 包裹 `tool.execute()`，panic 归一化为 `ToolResult::Error { recoverable: false, message: "工具执行 panic: …" }`（不传播、不污染会话）。
- [~] ~~记录 malformed 计数到 `AgentRunSummary`~~ — **跳过（遥测范畴，按需不做）**。

**验收**（通过，`cargo test --workspace` 全绿；agent 11/11、context 52/52、其余 crate 零失败）：

- [x] 测试 [`panicking_tool_is_caught_and_normalized`](../crates/agent/src/lib.rs)：`PanicTool` execute 内 `panic!("boom")` → `run_pending_task` 返回 `ToolResult::Error { recoverable: false }` + `mistake_inc == true` + 消息含 `panic`/`boom`（不传播终止测试）。
- [x] 回归：`Err(ToolError)` 路径与正常 `Ok` 路径行为不变（10 个原有 agent 测试零失败）。
- [x] 说明：content 非数组 / `{}` 场景由强类型 `ToolResult` + MCP 适配层 + 空 content 占位已规避，无需额外 coerce。

**参考实现**：[`agent-loop.ts`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) `coerceToolResult` / `hasSubstantiveToolResultContent`（第 222-296 行）。

---

### 🟢 P2-E：OpenAI Responses 远程压缩（服务端 compaction）

**问题**：oh-my-pi [`compaction/openai.ts`](../third/oh-my-pi/packages/agent/src/compaction/openai.ts) 对 OpenAI Responses / Azure / Codex API 调用服务端 `/compact` 端点，服务端原生压缩并保留 previous summary（`OPENAI_REMOTE_COMPACTION_PRESERVE_KEY`）。Gyre [`summarize`](../crates/context/src/compaction.rs) 走本地 LLM 请求。

**任务**：仅当接入 OpenAI Responses API 时启用；其余 provider 维持本地 summarize。优先级低（需特定 API + 真实端点验证）。

---

### 🟢 P2-F：精确 token 计数（tiktoken 级）

**问题**（修正）：Gyre **已集成** `tiktoken-rs`（非纯启发式），但 [`TokenCounter::openai`](../crates/context/src/token.rs) **硬编码 `cl100k_base`**——gpt-4o / o1 / o3 / o4-mini 系列应使用 **`o200k_base`**（对中文 / emoji 编码差异显著）。偏差直接影响压缩触发时机（[`near_limit`](../crates/core/src/context.rs)）与 shake 保护窗口（[`protect_tokens`](../crates/context/src/compaction.rs)）。

**任务**（已完成）：

- [x] [`TokenCounter`](../crates/context/src/token.rs) 升级为双编码器（cl100k + o200k），新增 [`is_o200k_model`](../crates/context/src/token.rs)（gpt-4o 家族 + o1/o3/o4 → `o200k_base`）+ `count_text_for` / `count_context_for(model_id)`。
- [x] [`build_provider_context`](../crates/context/src/lib.rs) 改用 `count_context_for(&model.id)`；`Inner` 缓存 `last_model_id` 供同步 [`token_usage`](../crates/context/src/lib.rs) 选对 BPE。
- [x] `openai()` 同时加载两套词表；`o200k_base` 加载失败时回退 `None`（gpt-4o 退用 cl100k，精度略降但不阻断）。
- [~] ~~动态校准 `context_window_guard`~~ — **跳过**（按 model 族选 BPE 已消除主要偏差；动态校准需真实 usage 反馈通道，收益边际）。

**验收**（通过，`cargo test -p agent-context` 55/55；全 workspace 全绿）：

- [x] 测试 [`o200k_model_detection`](../crates/context/src/token.rs)：gpt-4o / o1 / o3 / o4-mini → o200k；gpt-4 / gpt-3.5 / claude / glm / deepseek → cl100k。
- [x] 测试 [`count_text_for_picks_o200k_for_gpt4o`](../crates/context/src/token.rs)：gpt-4o 与 gpt-4 对中文 + emoji 文本编码不同；纯 ASCII 两编码器一致（对照确认差异来自选择而非数据）。
- [x] 回归：原 52 个 context 测试零失败（含 build / 压缩 / 持久化）。
- [x] 说明：非 OpenAI provider（claude / glm / deepseek）无公开 BPE 词表，统一按 cl100k 近似（与 oh-my-pi accurate 模式同为 tiktoken，覆盖面一致）。

---

## 第三轮深度对比（2026-07 第三次）：循环控制流 / 工具接口 / 提示词 / 健壮性

> 前两轮已落地 shake 重写、工具并发、运行控制（deadline/pause）、会话树、稳定前缀、length 占位、
> 软工具护栏、coerce/panic 兜底、精确 token 计数。本轮逐行精读 [`agent-loop.ts`](../third/oh-my-pi/packages/agent/src/agent-loop.ts)
> 的 `runLoopBody` 双层循环结构、`executeToolCalls` 调度细节、`replay-policy.ts`、`run-collector.ts`、
> `types.ts`（工具/钩子接口），对照 [`crates/agent/src/lib.rs`](../crates/agent/src/lib.rs) 与
> [`crates/tools/src/lib.rs`](../crates/tools/src/lib.rs)，识别出 **9 项计划此前未涵盖**的差距。
>
> 核心结论：Gyre 的循环是**单层 `loop` + 单 steering 通道**，oh-my-pi 是**双层循环（外层停-续）+ 三通道消息注入**；
> 工具接口缺 partial-result 流式回调与若干元数据；summarize 提示词过于简陋。其中 **P0-G（结构化 handoff 提示词）**
> 直接决定压缩/分支切换的恢复质量，改动极小、收益最高，建议本轮首选。

### 🔴 P0-G：结构化 handoff 摘要提示词

**问题**：Gyre [`LlmSummaryProvider::summarize`](../crates/context/src/compaction.rs) 的提示词仅一句
「将以下已发生的对话历史压缩为简洁要点摘要，保留关键决策、文件改动与未决事项」。oh-my-pi
[`compaction-summary.md`](../third/oh-my-pi/packages/agent/src/compaction/prompts/compaction-summary.md)
用强约束结构化模板，直接决定 summarize 压缩与 [`switch_branch_with_handoff`](../crates/context/src/lib.rs)
注入的 handoff 质量——长会话压缩后模型常常「丢线索」「重复已做的工作」根因即在此。

**差距清单**：

| 能力 | Gyre | oh-my-pi |
|---|---|---|
| 结构化段落（目标 / 约束 / 进展[已完成·进行中·受阻] / 关键决策 / 下一步 / 关键上下文） | ✅ [`compaction-summary.md`](../prompts/compaction-summary.md) | ✅ 固定模板 |
| 强制保留未答问题/待用户响应 | ✅ | ✅ IMPORTANT 段 |
| 强制保留精确文件路径/函数名/错误信息/仓库状态 | ✅ | ✅ MUST preserve |
| 「仅输出摘要、勿加额外文本」约束 | ✅ | ✅ |
| 多任务会话区分多个目标 | ✅ | ✅ |

**任务**：

- [x] [`compaction.rs`](../crates/context/src/compaction.rs) 的 [`LlmSummaryProvider`](../crates/context/src/compaction.rs) 提示词改为结构化模板（中文版，移植 oh-my-pi [`compaction-summary.md`](../third/oh-my-pi/packages/agent/src/compaction/prompts/compaction-summary.md) 结构），system prompt 改为「严格按给定 Markdown 结构输出交接摘要，不要输出任何额外文本」。
- [x] 模板外置为 [`prompts/compaction-summary.md`](../prompts/compaction-summary.md)（与 [`prompts/system-*.md`](../prompts) 一致），经 [`include_str!`](../crates/context/src/compaction.rs) 编译期内嵌——与既有 `agent-prompt` crate 同款模式（确定性、无运行期 IO 失败模式）。抽出纯函数 [`summary_user_prompt`](../crates/context/src/compaction.rs) 便于测试。
- [x] 同一模板天然复用于 [`switch_branch_with_handoff`](../crates/context/src/lib.rs)：分支切换摘要经同一 [`LlmSummaryProvider`](../crates/context/src/compaction.rs) 生成（提示词同源，无需分叉）。

**验收**（通过，`cargo test -p agent-context` 56/56；clippy 对改动零新增警告）：

- [x] 测试 [`summary_prompt_is_structured_and_embeds_history`](../crates/context/src/compaction.rs)：构造对话历史，断言提示词含 `## 目标` / `## 进展` / `### 已完成` / `### 受阻` / `## 下一步` / `## 关键上下文` 全部结构段、含「精确的文件路径」与「尚未回答的问题」强约束、且原样内嵌对话历史。
- [x] 回归：原 55 个 context 测试零失败（含 [`summarize_replaces_old_with_handoff`](../crates/context/src/compaction.rs) ——「上下文摘要」前缀来自 [`Compactor::summarize`](../crates/context/src/compaction.rs) 包装层，与 provider 提示词解耦，改动不破坏）。
- [x] 实际效果由真实 LLM 调用产出（结构化段 + 路径保留），单测验证提示词侧正确性。

**参考实现**：[`compaction-summary.md`](../third/oh-my-pi/packages/agent/src/compaction/prompts/compaction-summary.md)、`compaction-update-summary.md`、`handoff-document.md`。

---

### 🟡 P1-G：停止边界的 steering/follow-up 再检查（外层停-续循环）

**问题**：Gyre [`run_loop`](../crates/agent/src/lib.rs) 是**单层 `loop`**，steering 仅在每轮**顶部** drain（[`lib.rs`](../crates/agent/src/lib.rs) 第 482-494 行）。当模型自然完成（`tool_calls.is_empty()`，第 814 行）时直接 `Done + return`，**不再检查 steering**——若用户在最后一轮模型调用/工具执行期间发了消息，该消息被搁置到下次手动 prompt。oh-my-pi `runLoopBody` 是**双层 `while`**：内层处理工具调用，外层在「agent 本该停止」处 [`onBeforeYield`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) + 重新 poll steering/aside/followUp，有新消息则 `continue` 续跑（第 1064-1091 行）。

**差距清单**：

| 能力 | Gyre | oh-my-pi |
|---|---|---|
| 停止边界重新 poll steering | ✅ [`run_loop`](../crates/agent/src/lib.rs) 停止块 re-check | ✅ 外层 drain |
| follow-up（停止边界跟进消息）通道 | ❌（留 P1-H） | ✅ `getFollowUpMessages` |
| `onBeforeYield` 钩子（让出前回调） | ❌ | ✅ |
| 外部 abort 时不 drain（避免搁浅） | ✅ `!cancel && !deadline` 守卫 | ✅ 显式跳过 drain |

**任务**：

- [x] [`run_loop`](../crates/agent/src/lib.rs) 「无工具调用 → 任务完成」分支改为：收尾前 re-check steering（`try_recv` + drain 全部）；若非 cancel/deadline 且有新消息，append + 发「已注入停止边界 steering」提示后 `continue` 续跑，而非 `return`（移植 oh-my-pi `runLoopBody` 外层停-续语义）。
- [x] 保留 cancel / deadline 路径不 drain（守卫 `!cancel.is_cancelled() && !deadline_exceeded`，移植 oh-my-pi「stranding hazard」注释——abort 时消息落地历史却永不响应）。

**验收**（通过，`cargo test -p agent` 13/13；下游 cli/server/acp 编译通过；改动行 clippy 零新增警告）：

- [x] 测试 [`stop_boundary_steering_continues_run`](../crates/agent/src/lib.rs)：provider 第一轮返回无工具文本前注入 steering → 停止边界检测到 → 续跑（provider 调用 2 次，而非搁置后仅 1 次），并发出「停止边界 steering」提示。
- [x] 测试 [`cancelled_run_does_not_drain_steering`](../crates/agent/src/lib.rs)：第一轮注入 steering 并 cancel → run 经流式中断路径以 Error 收尾（非 Done）、停止边界未触达、steering 留队列不进上下文（防搁浅）、provider 仅调用 1 次。
- [x] 回归：原 11 个 agent 测试零失败（含 pause / deadline / length 占位 / soft 护栏 / panic 兜底）。
- [x] 说明：cancel/deadline 的主防护在「循环顶部 cancel 检查」与「流式 `select!` 的 `cancel.cancelled()` 分支」；停止边界的 `!cancel` 守卫为该窗口的防御性兜底，正确且廉价。

**参考实现**：[`agent-loop.ts`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) 第 1045-1091 行（外层 drain + abort 跳过）。

---

### 🟡 P1-H：三通道消息注入（steering / aside / followUp）+ 非消费 peek

**问题**：Gyre 单 [`steer_rx`](../crates/agent/src/lib.rs) 通道，每轮 `try_recv` drain 全部，无「非消费探测」。oh-my-pi 区分三类消息且 aside 支持「惰性求值 + 丢弃过时」语义（[`resolveAsides`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) 第 690-698 行）：aside 是 thunk，注入瞬间求值，可返回 null 丢弃（如已被更新编辑取代的过期诊断）。

**差距清单**：

| 通道 | 语义 | Gyre | oh-my-pi |
|---|---|---|---|
| steering | 实时用户输入，立即注入 | ✅ | ✅ |
| aside | 非打断式补充（过期可丢弃） | ❌ | ✅ 惰性 thunk |
| followUp | 停止边界跟进 | ❌ | ✅ |
| `hasSteeringMessages` | 非消费 peek（工具执行期探测） | ❌ | ✅ |

**任务**：

- [ ] [`Agent`](../crates/agent/src/lib.rs) 增加 aside / follow-up 接收端（或单一 `SteeringKind` enum 通道），在停止边界（配合 P1-G）与工作轮边界分别处理：工作轮中 aside 与 steering 合并注入；停止边界 aside 等 followUp 一起批量注入（移植第 1052-1061 行「stop boundary 只 steering 触发新轮、aside 留外层」语义）。
- [ ] aside 支持 `Box<dyn FnOnce() -> Option<AgentMessage>>` 惰性求值形式，注入时调用、None 则丢弃。
- [ ] 工具执行段（配合 P1-I）增加非消费 `has_steering()` 探测（peek），不提前消费消息。

**验收**：测试「过期 aside（编辑已取代）注入时被 drop」「停止边界 aside 不触发额外模型轮、与 followUp 批量注入」。

**参考实现**：[`agent-loop.ts`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) `resolveAsides`（690-698）、停止边界 drain（1069-1087）、`hasSteeringMessages` peek（1660-1673）。

---

### 🟡 P1-I：interruptMode（immediate/wait）+ interruptible 工具 + 执行期轮询

**问题**：Gyre 工具执行期，steering 无法中断**正在运行**的工具——只能 `cancel` 整个 run（[`run_loop`](../crates/agent/src/lib.rs) 顶部 cancel 检查）。oh-my-pi [`executeToolCalls`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) 在 `interruptMode: "immediate"` 模式下，对标记 `interruptible: true` 的工具（如 `job` 轮询后台任务）按 250ms 轮询 steering 队列（[`STEERING_INTERRUPT_POLL_MS`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) 第 114 行），触发 `steeringAbortController` 提前结束等待，边界 dequeue 再注入（第 1998-2015 行）。`"wait"` 模式则等工具完成。

**差距清单**：

| 能力 | Gyre | oh-my-pi |
|---|---|---|
| `interruptMode`（immediate / wait） | ✅ [`InterruptMode`](../crates/agent/src/lib.rs) | ✅ |
| `interruptible` 工具标记 | ✅ [`Tool::interruptible`](../crates/tools/src/lib.rs)（[`RunCommandTool`](../crates/tools/src/shell.rs)=true） | ✅ |
| 执行期 steering 轮询中断长工具 | ✅ [`poll_and_run`](../crates/agent/src/lib.rs) 250ms `UnboundedReceiver::len` peek | ✅ 250ms poll |
| 中断后保留已完成工具的真实结果 | ✅（`join_all` 等全部；中断工具返回 cancel 错误，已完成者保留真值） | ✅ |

**任务**：

- [x] [`Tool`](../crates/tools/src/lib.rs) trait 加 `fn interruptible(&self) -> bool { false }`；[`RunCommandTool`](../crates/tools/src/shell.rs) 覆写为 `true`（其 [`execute`](../crates/tools/src/shell.rs) 已用 `select! { cancel.cancelled() => .. }` 响应 `ctx.cancel`，无需改 cancel 逻辑）。
- [x] [`Agent`](../crates/agent/src/lib.rs) 加 [`InterruptMode { Wait, Immediate }`](../crates/agent/src/lib.rs) + builder `.interrupt_mode()`。**默认 `Immediate`**（非 Wait）：因当前仅 `run_command` 标记 interruptible，行为变化精确收敛到「steering 中途打断在途 shell 命令」这一期望 UX，其余工具（read/write/grep 等默认 false）零影响。
- [x] [`run_loop`](../crates/agent/src/lib.rs) 工具执行段：批级 token = `cancel.child_token()`（run-cancel 向下传播；steering 单独触发不影响 run 级语义）；[`poll_and_run`](../crates/agent/src/lib.rs) 在 Immediate + batch 含 interruptible 工具时 `select!` race 工具执行 vs 每 250ms 用 [`UnboundedReceiver::len`](https://docs.rs/tokio)（**非消费 peek**）探测 steering，命中即 `batch_token.cancel()`。
- [x] 中断语义：Gyre 工具 `execute` 必返回 `Result`（无「未出结果」悬空），中断工具返回 `ToolError::Execution("命令被取消")`（recoverable）→ [`run_pending_task`](../crates/agent/src/lib.rs) 归一化为 `ToolResult::Error`；同批已完成的工具保留真实 result。无需独立 tail sweep（Gyre 强类型模型与 oh-my-pi 不同）。

**验收**（通过，`cargo test --workspace` 全绿；agent 14/14、tools 36/36；改动文件 clippy 零新增警告）：

- [x] 测试 [`interruptible_tool_is_aborted_by_steering`](../crates/agent/src/lib.rs)：interruptible 阻塞工具（30s）执行期间后台注入 steering → 批级 token 在 ~250ms 轮询周期内触发取消、工具标记 interrupted 并让出（8s 超时内完成而非跑满 30s）、provider 续跑到下一轮（calls≥2）。
- [x] 回归：workspace 全部 crate 零失败（含 deadline / pause / length 占位 / soft 护栏 / panic 兜底 / 工具并发）。
- [x] 说明：非 interruptible 工具（默认 false）所在的 batch `need_steering_poll=false`，不轮询、不被打断（Wait 等价行为）；可通过 `.interrupt_mode(InterruptMode::Wait)` 全局关闭。

**参考实现**：[`agent-loop.ts`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) `checkSteering`（1653-1679）、`watchSteeringWhileRunning`（2004-2015）、tail sweep（2020-2030）。

---

### 🟢 P2-G：工具 partial-result 流式回调

**问题**：Gyre [`Tool::execute`](../crates/tools/src/lib.rs) 签名 `execute(input, ctx) -> Result<ToolResult, ToolError>` 一次性返回。长时工具（`run_command` 流式输出、`job` 轮询）执行期间 UI 无增量。oh-my-pi [`tool.execute`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) 第 1864-1878 行多一个 `partialResult` 回调，工具执行中多次下发部分结果，循环转发为 `tool_execution_update` 事件（[`types.ts`](../third/oh-my-pi/packages/agent/src/types.ts) 第 708 行）。

**任务**：

- [ ] [`Tool::execute`](../crates/tools/src/lib.rs) 增加重载或新增 `execute_streaming(input, ctx, on_partial) -> Result<ToolResult, ToolError>`，默认实现转调 `execute`（零行为变化）。
- [ ] [`run_pending_task`](../crates/agent/src/lib.rs) / [`run_batch`](../crates/agent/src/lib.rs) 调用 streaming 版，`on_partial` 转发为新 `AgentEvent::ToolUpdate { tool_call_id, partial }`。
- [ ] `RunCommandTool` 改用 streaming 版，逐行下发 stdout 增量（前端 [`Transcript.tsx`](../web/c5-ui/src/components/agent/Transcript.tsx) 实时显示）。
- [ ] partial result 同样经 coerce/归一化（防畸形）。

**验收**：`run_command` 长输出时前端逐行增量；非流式工具走默认实现、行为不变。

**参考实现**：[`agent-loop.ts`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) 第 1864-1878 行；`AgentToolUpdateCallback`（[`types.ts`](../third/oh-my-pi/packages/agent/src/types.ts) 543）。

---

### 🟢 P2-H：工具元数据（intent / customWireName / lenientArgValidation / concurrency(args)）

**问题**：Gyre [`Tool`](../crates/tools/src/lib.rs) 仅 name/description/schema/capability/concurrency(enum)。oh-my-pi [`AgentTool`](../third/oh-my-pi/packages/agent/src/types.ts) 第 606-625 行更丰富：

| 元数据 | 作用 | Gyre | oh-my-pi |
|---|---|---|---|
| `intent` | 注入 schema 的 `i` 字段，模型声明调用意图（审计/可观测） | ❌ | ✅ require/optional/omit/fn |
| `customWireName` | OpenAI 自定义工具线名匹配（GPT-5 `apply_patch`） | ❌ | ✅ |
| `lenientArgValidation` | 校验失败仍透传原始 args（容错） | ❌ | ✅ |
| `concurrency(args)` | 按参数决定并发（如 read 全文件 vs 读首行） | ❌（仅 enum） | ✅ fn |

**任务**：

- [ ] [`Tool`](../crates/tools/src/lib.rs) trait 加 `fn intent(&self) -> IntentMode { Omit }`、`fn custom_wire_name(&self) -> Option<&str> { None }`、`fn lenient_args(&self) -> bool { false }`；[`Concurrency`](../crates/tools/src/lib.rs) 扩展或新增 `fn concurrency_for(&self, args) -> Concurrency`（默认调 `concurrency()`）。
- [ ] [`run_batch`](../crates/agent/src/lib.rs) 工具查找支持 `custom_wire_name` 回退匹配；调度器用 `concurrency_for(args)`。
- [ ] intent（若启用 `intent_tracing`）注入 schema 首字段、执行时剥离、透传到 `tool_execution_start` 事件。

**验收**：intent 注入/剥离往返；customWireName 匹配；lenient 模式坏 args 仍进 execute；`concurrency_for(args)` 按参数分流。

**参考实现**：[`types.ts`](../third/oh-my-pi/packages/agent/src/types.ts) 第 605-625 行；[`normalizeTools`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) `injectIntentIntoSchema`（539-576）。

---

### 🟢 P2-I：扩展钩子能力（beforeToolCall block / afterToolCall override / transformAssistantMessage）

**问题**：Gyre [`Hook`](../crates/core/src/lib.rs) trait 的 `on_event(&HookEvent)` 仅观察（不可变、无返回），无法 block 工具或改写结果。oh-my-pi 在审批之外另有：[`beforeToolCall`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) 第 1832-1845 行（可 `block: true` + reason → 抛 `ToolCallBlockedError`）、[`afterToolCall`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) 第 1892-1927 行（可改写 result/content，re-coerce 防污染）、[`transformAssistantMessage`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) 第 1380-1382 行（流结束、入 context/UI/dispatch 前展开内联宏，单一真相源）。

**差距清单**：

| 钩子 | 能力 | Gyre | oh-my-pi |
|---|---|---|---|
| beforeToolCall | block + reason | ✅ [`Hook::before_tool_intercept`](../crates/core/src/hook.rs) | ✅ |
| afterToolCall | 改写 result | ✅ [`Hook::after_tool_override`](../crates/core/src/hook.rs) | ✅（re-coerce） |
| transformAssistantMessage | 后处理 assistant 消息（宏展开） | ❌（需宏系统，后续） | ✅ |

**任务**：

- [x] [`Hook`](../crates/core/src/hook.rs) trait 扩展两个**带默认实现**的方法（向后兼容，现有实现无需改动）：[`before_tool_intercept`](../crates/core/src/hook.rs) `(tool, args) -> Option<String>`（`Some(reason)` 阻止）、[`after_tool_override`](../crates/core/src/hook.rs) `(tool, result) -> Option<ToolResult>`（`Some` 替换）。直接挂在既有 `Hook` 上（默认 `None` = 不干预），未新增独立 trait。
- [x] [`run_pending_task`](../crates/agent/src/lib.rs) 接入：审批后、execute 前逐钩子调 `before_tool_intercept`，命中即回填可恢复 `ToolResult::Error`（不执行 execute）、仍发 after 观察事件；execute 后、after 观察前逐钩子调 `after_tool_override`，替换 result（故观察事件与回填给模型的都是最终结果）。
- [ ] [`Agent`](../crates/agent/src/lib.rs) 加 `transform_assistant` 钩子（assistant 消息后处理/宏展开）：**后续**——Gyre 无内联宏系统，待有具体宏需求再上。

**验收**（通过，`cargo test --workspace` 全绿；agent 16/16、core 10/10；改动文件 clippy 零新增警告）：

- [x] 测试 [`before_tool_intercept_blocks_execution`](../crates/agent/src/lib.rs)：钩子拦截 `echo` → 不调用 execute、回填可恢复 Error（消息含「钩子拦截」）、不计 mistake、before+after 观察事件均触发 1 次。
- [x] 测试 [`after_tool_override_rewrites_result`](../crates/agent/src/lib.rs)：钩子把 `EchoTool` 的 `real-output` 改写为 `rewritten`、after 观察事件看到改写后的最终结果。
- [x] 回归：workspace 全部 crate 零失败（Hook trait 仅新增带默认实现的方法，既有实现零影响）。

**参考实现**：[`agent-loop.ts`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) beforeToolCall（1832-1845）、afterToolCall（1892-1927）。

---

### 🟢 P2-J：provider refusal 过滤 + 流中断保留已完成 tool_call

**问题**：两类健壮性细节。①oh-my-pi [`replay-policy.ts`](../third/oh-my-pi/packages/agent/src/replay-policy.ts) 检测 API 级 refusal（`stopReason=error` + `stopDetails.type=refusal/sensitive`），从 provider replay 过滤（保留其余消息）——Gyre 无 refusal 识别，与普通 error 混同，refusal 文本会被反复重放。②oh-my-pi [`retainCompletedToolCalls`](../third/oh-my-pi/packages/agent/src/agent-loop.ts) 第 1496-1521 行：流中断时保留已到达 `toolcall_end` 的工具调用，仅丢弃未完成的（参数不安全）。Gyre [`persist_interrupted`](../crates/agent/src/lib.rs) 保留已生成回复，但未细粒度区分已完成/未完成 tool_call。

**任务**：

- [ ] [`StopReason`](../crates/core/src/message.rs) / assistant 消息增加 refusal 标记（或 `stop_details`），[`build_provider_context`](../crates/context/src/lib.rs) 过滤 refusal assistant 消息不重放。
- [ ] 流中断兜底（[`persist_interrupted`](../crates/agent/src/lib.rs)）区分已完成/未完成 tool_call：保留完成的、丢弃未完成的并标注 `stream_interrupted_after_content`。

**验收**：refusal 消息不被重放；中断时已完成 tool_call 保留、未完成的丢弃且标注。

**参考实现**：[`replay-policy.ts`](../third/oh-my-pi/packages/agent/src/replay-policy.ts)；[`retainCompletedToolCalls`](../third/oh-my-pi/packages/agent/src/agent-loop.ts)（1496-1521）。

---

### 🟢 P2-K：run-collector 可观测性（再评估）

**问题**：oh-my-pi [`run-collector.ts`](../third/oh-my-pi/packages/agent/src/run-collector.ts) 提供 [`AgentRunSummary`](../third/oh-my-pi/packages/agent/src/run-collector.ts)（chats 按 stop_reason 分桶 + 延迟；tools 按 ok/error/skipped/blocked/timeout/aborted 分桶 + 每工具名计数 + 延迟；usage 含 reasoning/cache 读写；cost；errors 按类型分桶）与 [`AgentRunCoverage`](../third/oh-my-pi/packages/agent/src/run-collector.ts)（toolsAvailable/Invoked/Unused、modelsUsed、providersUsed）。原计划 P1-2 标记「遥测按需跳过」。Gyre [`AgentRunSummary`](../crates/core/src/lib.rs) 仅 turns/tool_calls/usage 三项。

**任务**（最小版已落地；OTEL span / chat 延迟分桶仍跳过）：

- [x] [`AgentRunSummary`](../crates/core/src/message.rs) 新增 [`ToolCounters`](../crates/core/src/message.rs)（total/ok/error）+ `tools_by_name: BTreeMap<String, ToolCounters>` + `tools_available` / `tools_invoked`（coverage）；新字段均 `#[serde(default)]` 向后兼容。
- [x] [`AgentRunSummary::record_tool`](../crates/core/src/message.rs) 在 [`run_loop`](../crates/agent/src/lib.rs) 工具结果回填时调用；[`unused_tools`](../crates/core/src/message.rs) 返回「注册但从未调用」。`tools_available` 在 run 启动时从 `specs` 填充。

**验收**（通过，`cargo test --workspace` 全绿；agent 17/17、core 10/10；改动文件 clippy 零新增警告）：

- [x] 测试 [`run_summary_records_tool_counters_and_coverage`](../crates/agent/src/lib.rs)：记录 a(ok)+a(error)+b(ok) → `tools_by_name["a"]=(total=2,ok=1,error=1)`、invoked 含 a/b 不含 c、`unused_tools()==[c]`。
- [x] 回归：workspace 全部 crate 零失败（`AgentRunSummary` 仅新增带 `#[serde(default)]` 字段，`Default` 派生与既有 `::default()` 构造零影响）。
- [ ] *后续可选*：skipped/blocked 计数（需在软需求 detour / hook 拦截回填路径也调 record_tool）、chat 按 stop_reason 分桶 + 延迟、OTEL span。

---

## 排期建议

| 阶段 | 项 | 预估 | 状态 / 说明 |
|---|---|---|---|
| 阶段一 | P0-1 shake 重写 + P1-1 分级触发 | 中 | ✅ 已完成 |
| 阶段一 | P0-2 工具并发 | 小 | ✅ 已完成 |
| 阶段二 | P1-2 运行控制（deadline / pause_turn） | 中 | ✅ 已完成（遥测按需跳过） |
| 阶段三 | P1-3 会话树 / 分支导航 | 大 | ✅ 已完成 |
| **新一批** | **P0-A 字节级稳定前缀** | **中** | 🔴 成本/延迟高回报，建议首批 |
| **新一批** | **P0-B length + 残缺 tool_call 占位** | **小** | 🔴 正确性 bug，低风险速修 |
| **新一批** | **P1-C 软工具升级护栏** | **小** | 🟡 健壮性，防无限强制循环 |
| 阶段四 | P1-D coerce 归一化（catch_unwind 防 panic） | 小 | ✅ 已完成 |
| 阶段四 | P2-F 精确 token 计数（按 model 族选 BPE） | 小 | ✅ 已完成 |
| 阶段四 | P2-1 向量记忆、P2-2 方言（GLM in-band）、P2-E 远程压缩 | 中-大 | 🟢 待评估（需新依赖 / 真实 API） |
| **第三轮** | **P0-G 结构化 handoff 摘要提示词** | **极小** | ✅ 已完成（`prompts/compaction-summary.md` + `include_str!` + 纯函数测试，56/56） |
| **第三轮** | **P1-G 停止边界 steering 再检查（外层停-续循环）** | **小** | ✅ 已完成（run_loop 停止块 re-check + cancel/deadline 守卫 + 2 测试，agent 13/13） |
| **第三轮** | P1-H 三通道消息注入（aside/followUp + peek） | 中 | 🟡 peek 已随 P1-I 落地（`UnboundedReceiver::len`）；aside/followUp 通道待具体消费者 |
| **第三轮** | P1-I interruptMode + interruptible 工具 + 执行期轮询 | 中 | ✅ 已完成（`Tool::interruptible` + `InterruptMode`(默认 Immediate) + `poll_and_run` 250ms peek + run_command 可中断，workspace 全绿） |
| **第三轮** | **P2-I 扩展钩子（before 拦截 / after 改写）** | **小** | ✅ 已完成（`Hook::before_tool_intercept` + `after_tool_override`（带默认实现）+ run_pending_task 接入 + 2 测试，agent 16/16） |
| **第三轮** | **P2-K run-collector（最小版）** | **小** | ✅ 已完成（`AgentRunSummary` + `ToolCounters` 工具 ok/error 分桶 + available/invoked coverage + `unused_tools`，workspace 全绿；OTEL/chats 分桶仍跳过） |
| **第三轮** | P2-G partial-result 流式回调、P2-H 工具元数据、P2-J refusal 过滤/中断保留 | 中 | 🟢 工具接口/健壮性增强，按需排期 |

**明确的高回报切入点**：

- **第一/二轮已验证**：P0-A（字节级稳定前缀）省去长会话每轮数万 token 重复 prefill；P0-B（length + 残缺 tool_call 占位）修正「执行参数被截断的工具调用」正确性 bug。
- **第三轮首选**：**P0-G（结构化 handoff 提示词）**——一处提示词改动即可显著提升 summarize 压缩与分支切换 handoff 的恢复质量（模型不再丢线索/重复工作），改动极小、零风险、立即可见。**P1-G（停止边界 steering 再检查）**——修复「用户在最后一轮期间发消息被搁置到下次 prompt」的 UX 缺陷，改动小。两者建议作为第三轮首批落地。
