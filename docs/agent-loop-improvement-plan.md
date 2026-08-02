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

---

## 第四轮深度对比（2026-07 第四次）：跳出 agent-loop.ts，看 coding-agent 产品层 + snapcompact

> 前三轮聚焦 [`third/oh-my-pi/packages/agent`](../third/oh-my-pi/packages/agent/src) 的循环本体（`agent-loop.ts` / `append-only-context.ts` / `compaction/`）。
> 本轮把视角**抬高一层**——逐目录扫描 [`third/oh-my-pi/packages`](../third/oh-my-pi/packages)（`snapcompact` / `coding-agent`）与
> [`packages/agent/src`](../third/oh-my-pi/packages/agent/src) 中此前未逐行精读的 `proxy.ts` / `thinking.ts` / `replay-policy.ts`，
> 对照 Gyre 全部 23 个 crate，识别出 **9 项前三轮计划完全未涵盖**的产品级 / 成本级差距。
>
> 核心结论：Gyre 已把 oh-my-pi 的 **循环引擎**（执行 / 压缩 / 分支 / 中断 / 并发）基本吃透，但 oh-my-pi 真正的差异化能力
> 在 **引擎之上的智能层**：图像化压缩、自适应思考预算、双代理评审、目标预算、跨会话学习、计划审批、多工具配置互操作。
> 其中 **P0-L（snapcompact 图像压缩）** 直接把昂贵的 LLM summarize 替换为本地 PNG 渲染，长会话压缩成本可降一个数量级，ROI 最高。

### 🔴 P0-L：snapcompact 图像化压缩（本地 PNG 帧，替换昂贵 summarize）

**问题**：Gyre [`Compactor::summarize`](../crates/context/src/compaction.rs) 走**本地 LLM 请求**压缩——慢、贵、可能失败、丢精度。oh-my-pi
[`snapcompact`](../third/oh-my-pi/packages/snapcompact/src/snapcompact.ts) 把被丢弃的历史序列化为紧凑文本，渲染成 **bitmap-font PNG 帧**，
视觉模型直接读图回放——**完全本地、确定性、无 LLM 调用、零 API key**。栅格化与 PNG 编码在 native 代码（[`pi-natives`](../third/oh-my-pi/crates/pi-natives)）。

**差距清单**：

| 能力 | Gyre | oh-my-pi |
|---|---|---|
| 本地 PNG 帧渲染（无 LLM 调用） | ❌（LLM summarize） | ✅ `render` / `renderMany` |
| Provider 感知帧形状（按计费 eval 选） | ❌ | ✅ Anthropic `6x12-dim` / Google `doc-8on16-sent-dim` / OpenAI `8on16-bw` |
| 图像预算护栏（防网关丢帧） | ❌ | ✅ `providerImageBudget` / `MAX_FRAMES_DEFAULT` |
| `preserveData` 持久化帧归档 | ❌ | ✅ 压缩条目内挂载，rebuild 时重附 |
| 高分辨率边缘帧（HQ） | ❌ | ✅ `HQ_EDGE_FRAMES`（1932px Claude 高清行） |

**实施前置条件**（2026-07 评估，本轮未落地——需独立攻关会话）：

> 经核查，Gyre 工作区**当前无 PNG / 字体 / DEFLATE 依赖**（`Cargo.lock` 无 `image`/`png`/`ab_glyph`/`flate2`），
> 且 assistant [`ContentBlock`](../crates/core/src/message.rs) **无 `Image` 变体**（仅 Text/Thinking/ToolCall；`Image` 仅存在于 `UserContent`/`ToolImage`）。
> 因此 P0-L 不是「移植 13 行」级别，而是涉及：
>
> 1. **新增依赖**：PNG 编码（`image` 或 `png` crate）+ 字体（`ab_glyph` + 等宽 TTF，或内嵌 8x16 bitmap 字体 ~2KB）。
> 2. **assistant 图像块贯通**：`ContentBlock::Image` + 各 provider transform（Anthropic image block / OpenAI `image_url` / GLM），
>    否则压缩后摘要消息的图像块会触发 400。
> 3. **栅格化质量 + eval**：oh-my-pi 的帧形状经 SQuAD recall eval 调优（stopword dimming、多栏布局、provider 计费感知）；
>    朴素渲染的文本密度不足 → 视觉模型误读 → 比文本 summarize 更差。**必须用真实视觉模型验证召回率**才能上线。
>
> 前置 1-2 可在一个攻关会话完成；前置 3 是成败关键，需 eval 数据集。建议作为独立里程碑，不与其他改动混排。

**任务**：

- [ ] 在 [`crates/context`](../crates/context/src/compaction.rs) 引入 `ImageCompactor`（独立模块，不替换现有 `summarize`，作为可选 backend）：
  序列化丢弃历史 → 文本归一化（剥 ANSI / 折叠空白 / 换行折叠为全块字符保留行结构）→ 栅格化为 PNG 帧。
- [ ] 栅格化用 Rust 原生实现（`image` + 内嵌 6x12/8x16 bitmap 字体，或 `ab_glyph` + 等宽字体），无需 Node 依赖。
- [ ] 帧形状按 [`Model`](../crates/core/src/model.rs) provider 分流：Anthropic / OpenAI / Google / Unknown 四档（移植 [`resolveShape`](../third/oh-my-pi/packages/snapcompact/src/snapcompact.ts)）。
- [ ] [`ProviderMessage`](../crates/core/src/message.rs) 内容块支持 `ContentBlock::Image { data: Vec<u8>, media_type: "image/png", .. }`（若已有则复用）；
  压缩后摘要消息挂载多帧 image 块。
- [ ] [`Compactor`](../crates/context/src/compaction.rs) 加 `compaction_backend: Summarize | Snapcompact | Hybrid` 配置，默认 `Summarize`（向后兼容），
  视觉模型（Claude / GPT-4o / Gemini）opt-in `Snapcompact`。
- [ ] 帧数预算 + 网关丢帧护栏（移植 `providerImageBudget`）。

**验收**：含大型历史的会话经 snapcompact 压缩后，摘要消息含 image 块、token 估算显著低于 summarize、信息可由视觉模型回读；
非视觉模型回退 `Summarize` backend。

**参考实现**：[`snapcompact.ts`](../third/oh-my-pi/packages/snapcompact/src/snapcompact.ts) `compact` / `renderMany` / `resolveShape` / `serializeConversation`；
[`pi-natives`](../third/oh-my-pi/crates/pi-natives)（Rust 栅格化）。

---

### 🟡 P1-K：auto-thinking 自适应思考预算（按 prompt 难度调档）

**问题**：Gyre [`AgentBuilder::thinking`](../crates/agent/src/lib.rs) 是**静态**配置——整个 run 用同一 `ThinkingConfig`（budget_tokens）。
oh-my-pi [`auto-thinking/classifier.ts`](../third/oh-my-pi/packages/coding-agent/src/auto-thinking/classifier.ts) 在 `thinking: "auto"` 模式下，
**每个 prompt** 用 tiny/smol 模型分类难度（`trivial|moderate|hard` 或 `low|medium|high|xhigh`），映射为 `Effort` 并钳到当前模型支持范围
（不低于 `Effort.Low`）。简单问题少思考省 token / 降延迟，难题深度推理。

**差距清单**：

| 能力 | Gyre | oh-my-pi |
|---|---|---|
| 静态 thinking budget（per-run） | ✅ | ✅ |
| per-prompt 难度分类 | ❌ | ✅ `classifyDifficulty` |
| 在线（smol 模型）/ 本地（on-device）双后端 | ❌ | ✅ |
| Effort 钳到模型支持范围 | ❌ | ✅ `clampAutoThinkingEffort` |
| 分类失败回退（不阻断 turn） | ❌ | ✅ |

**任务**：

- [x] [`crates/core/src/llm.rs`](../crates/core/src/llm.rs) 新增 [`Effort`](../crates/core/src/llm.rs) 档位（Minimal/Low/Medium/High/XHigh，含 `default_budget` / `budget_clamped` / `parse`）+ [`ThinkingClassifier`](../crates/core/src/llm.rs) trait + [`ThinkingPolicy { Static | Auto }`](../crates/core/src/llm.rs)（`resolve()` 异步解析；Auto 调分类器、钳位、模型不支持→None、失败→fallback）。
- [x] [`crates/llm/src/thinking.rs`](../crates/llm/src/thinking.rs) 实现 [`LlmThinkingClassifier`](../crates/llm/src/thinking.rs)：复用 `LlmProvider` + tiny `Model`，单次 8-token 补全（`ANSWER_MAX_TOKENS=8`），从 `MessageEnd` 权威文本解析 `Effort`。
- [x] 输入截断保护：[`truncate_classifier_input`](../crates/core/src/llm.rs)（head 4000 + tail 2000，移植 `HEAD_CHARS/TAIL_CHARS`）。
- [x] 分类失败 / provider 错误 / 不可解析 → 返回 `None`（`ThinkingPolicy::Auto` 回退 fallback，不抛、不阻断 turn）。
- [x] [`AgentBuilder::thinking_policy`](../crates/agent/src/lib.rs) + run_loop 解析：在 user_msg move 进 context 前提取 prompt 文本，循环前 `policy.resolve(&prompt_text, &model).await` 一次/run（难度恒定，分类器成本 ≤ 一次 tiny 调用）。
- [x] CLI 装配（[`main.rs`](../crates/cli/src/main.rs)）：`[agent].auto_thinking` + `auto_thinking_model` 配置；启用时复用当前 provider/api + tiny id 构造分类器，未配 tiny 模型回退静态 + 告警。
- [ ] *后续*：server 装配（`assemble` 需传入 model 参数，待签名调整）；本地 on-device 分类器后端（移植 `classifyLocal` 3-class）。

**验收**（通过，`cargo test -p agent-core` 19/19、`-p agent-llm` 59/59、`-p agent` 19/19；全 workspace 全绿）：

- [x] 单元测试：[`effort_default_budget_monotonic`](../crates/core/src/llm.rs) / `effort_budget_clamped_respects_bounds` / `effort_parse_recognizes_aliases`（档位单调、钳位、别名解析）。
- [x] 单元测试：[`truncate_classifier_input_*`](../crates/core/src/llm.rs)（短文本原样、长文本 head+tail）。
- [x] 分类器测试（[`thinking.rs`](../crates/llm/src/thinking.rs)）：`classify_parses_effort_from_provider_text`（low/Medium/high/xhigh 容错解析）、`classify_returns_none_on_unparsable_output`、`classify_returns_none_on_provider_error`。
- [x] 集成测试（[`lib.rs`](../crates/agent/src/lib.rs)）：[`auto_thinking_resolves_budget_from_classifier`](../crates/agent/src/lib.rs)（High 分类 → 下发 32_000 预算）、[`auto_thinking_returns_none_when_model_unsupported`](../crates/agent/src/lib.rs)（supports_thinking=false → 本轮不思考）。

**参考实现**：[`classifier.ts`](../third/oh-my-pi/packages/coding-agent/src/auto-thinking/classifier.ts)；[`thinking.ts`](../third/oh-my-pi/packages/agent/src/thinking.ts) `ThinkingLevel`。

---

### 🟡 P1-L：advisor 双代理评审（只读观察者 + emission-guard）

**问题**：Gyre [`supervisor`](../crates/supervisor/src/lib.rs) 只观测 **TaskTool 派生的子 Agent**（spawn/finish/log），
**没有**一个独立的「评审 Agent」持续看主 Agent transcript 并提建议。oh-my-pi [`advisor`](../third/oh-my-pi/packages/coding-agent/src/advisor)
是一个**只读第二 Agent**：快照主 transcript（经 secret obfuscator 脱敏）→ 用独立 model（近窗口时自动 promote 到更大 sibling）→
经 `advise` 工具下发 `nit|concern|blocker` 级别建议到主 Agent 的 yield queue。配套 [`emission-guard`](../third/oh-my-pi/packages/coding-agent/src/advisor/emission-guard.ts)
在代码层 dedupe 重复建议（真实 advisor 模型会 spam "Stop." 114 次，issue #3520）、过滤无内容 filler。
[`watchdog.ts`](../third/oh-my-pi/packages/coding-agent/src/advisor/watchdog.ts) 发现 `WATCHDOG.md`（仓库/用户级评审准则）注入 advisor system prompt。

**差距清单**：

| 能力 | Gyre | oh-my-pi |
|---|---|---|
| 只读评审 Agent（独立 context/model） | ❌（仅子 Agent 执行） | ✅ `AdvisorAgent` |
| secret 脱敏后喂评审模型 | ❌ | ✅ `SecretObfuscator` |
| 近窗口自动 promote 到更大 model | ❌ | ✅ `maintainContext` |
| 三级 severity（nit/concern/blocker） | ❌ | ✅ |
| 代码层 dedupe / filler 过滤 | ❌ | ✅ `AdvisorEmissionGuard` |
| WATCHDOG.md 评审准则发现 | ❌ | ✅ |

**任务**：

- [ ] 新建 [`crates/advisor`](../crates) crate（依赖 `agent`/`llm`/`core`，不引入循环依赖）：`Advisor` 包装一个独立 `Agent`（只读工具集 + 独立 provider/model）。
- [ ] `AdvisorHost` trait：`snapshot_messages()` / `enqueue_advice(severity, note)` / `obfuscator` / `maintain_context(tokens) -> bool`。
- [ ] 主 [`run_loop`](../crates/agent/src/lib.rs) 每轮结束（或每 N 轮）触发 advisor 增量评审；advice 经现有 [`steer_rx`](../crates/agent/src/lib.rs)
  或新增 `advice_rx` 注入主上下文（与 steering 同语义但带 severity 标记）。
- [ ] `EmissionGuard`：normalize（NFKC + 折叠标点 + 小写）后 dedupe + filler 黑名单（移植 [`normalizeAdvisorNote`](../third/oh-my-pi/packages/coding-agent/src/advisor/emission-guard.ts)）。
- [ ] `WATCHDOG.md` 发现（cwd walkup + `~/.gyre/` + 项目 `.gyre/`）注入 advisor system prompt。

**验收**：advisor 在主 Agent 跑偏时下发 blocker 建议被注入；重复 "Stop." 仅入队一次；secret 不泄露到 advisor model。

**参考实现**：[`advisor/runtime.ts`](../third/oh-my-pi/packages/coding-agent/src/advisor/runtime.ts) / [`emission-guard.ts`](../third/oh-my-pi/packages/coding-agent/src/advisor/emission-guard.ts) / [`watchdog.ts`](../third/oh-my-pi/packages/coding-agent/src/advisor/watchdog.ts)。

---

### 🟡 P1-M：goals 目标 + token/时间预算追踪

**问题**：Gyre 有 [`deadline`](../crates/agent/src/lib.rs)（ wall-clock 上限）和 [`max_turns`](../crates/agent/src/lib.rs)，但**没有**「目标对象 + 预算追踪」概念——
用户无法说「花最多 50k token 解决这个问题」。oh-my-pi [`goals`](../third/oh-my-pi/packages/coding-agent/src/goals) 维护
[`Goal`](../third/oh-my-pi/packages/coding-agent/src/goals/state.ts)（id / objective / status / `tokenBudget` / `tokensUsed` / `timeUsedSeconds`），
状态机 `active → paused | budget-limited | complete | dropped`，超预算时经隐藏消息（`deliverAs: steer|followUp|nextTurn`）
注入 `goal-budget-limit.md` 提示让模型收尾，配套 continuation prompt。

**差距清单**：

| 能力 | Gyre | oh-my-pi |
|---|---|---|
| deadline（wall-clock） | ✅ | ✅ |
| max_turns | ✅ | ✅（loop-limit） |
| Goal 对象（objective + token/time budget） | ❌ | ✅ `GoalModeState` |
| 预算耗尽 → 收尾提示注入 | ❌ | ✅ `goal-budget-limit.md` |
| goal 状态机（pause/resume/complete/drop） | ❌ | ✅ |
| 用户 `/loop 10` / `/loop 10m` 命令 | ❌ | ✅ [`loop-limit.ts`](../third/oh-my-pi/packages/coding-agent/src/modes/loop-limit.ts) |

**任务**：

- [ ] [`crates/core`](../crates/core/src/context.rs) 加 `Goal` / `GoalModeState` 类型；[`AgentBuilder`](../crates/agent/src/lib.rs) 加 `.goal(Goal)`。
- [ ] [`run_loop`](../crates/agent/src/lib.rs) 每轮累加 `tokens_used` / `time_used`，超 `token_budget` 时注入 budget-limit 隐藏消息（经 steering 通道）。
- [ ] 新增 `goal` 工具（create/get/complete/resume/drop）让模型自管理目标（移植 [`goal-tool`](../third/oh-my-pi/packages/coding-agent/src/goals/tools)）。
- [ ] [`crates/cli`](../crates/cli/src/repl.rs) / [`crates/server`](../crates/server/src/lib.rs) 加 `/loop <count|duration>` 命令解析（移植 [`parseLoopLimitArgs`](../third/oh-my-pi/packages/coding-agent/src/modes/loop-limit.ts)）。

**验收**：设定 50k token 预算，耗尽时模型收到收尾提示并主动 complete；`/loop 10` 跑 10 轮自停。

**参考实现**：[`goals/runtime.ts`](../third/oh-my-pi/packages/coding-agent/src/goals/runtime.ts) / [`state.ts`](../third/oh-my-pi/packages/coding-agent/src/goals/state.ts)；[`loop-limit.ts`](../third/oh-my-pi/packages/coding-agent/src/modes/loop-limit.ts)。

---

### 🟢 P2-L：hindsight 跨会话学习库 + mental models

**问题**：Gyre [`memory`](../crates/memory/src/structured.rs) 是 **per-session BM25** 词项重叠，无跨会话积累、无「心智模型」层。
oh-my-pi [`hindsight`](../third/oh-my-pi/packages/coding-agent/src/hindsight) 是独立学习后端：
[`bank.ts`](../third/oh-my-pi/packages/coding-agent/src/hindsight/bank.ts) 三种作用域（`global` / `per-project` / `per-project-tagged`），
[`mental-models.ts`](../third/oh-my-pi/packages/coding-agent/src/hindsight/mental-models.ts) 在 bank 首次启动时 seed 一组策划模型（idempotent），
后台 reflect 填充、consolidation 时自动 refresh；mental models 作为 `<mental_models>` 块注入 developer 指令（**绕过每轮 recall HTTP 成本**），
带 anti-feedback wrapper 防模型把它当命令。

**差距清单**：

| 能力 | Gyre | oh-my-pi |
|---|---|---|
| per-session 记忆（BM25） | ✅ | ✅ |
| 跨会话 / 跨项目学习库 | ❌ | ✅ hindsight bank |
| 三种作用域（global / per-project / tagged） | ❌ | ✅ |
| mental models（命名摘要 + 自动 refresh） | ❌ | ✅ |
| seed 模型（首次启动 idempotent 植入） | ❌ | ✅ `seeds.json` |
| 注入 developer 指令（免每轮 recall） | ❌ | ✅ |

**任务**：与已计划 [`P2-1 向量记忆`](#p2-1记忆语义检索向量) 合并设计——向量 recall 解决「同义不同词」，
hindsight mental models 解决「跨会话沉淀」。优先级低于循环引擎项；需先决定存储后端（JSONL vs SQLite vs 独立服务）。

**参考实现**：[`hindsight/`](../third/oh-my-pi/packages/coding-agent/src/hindsight) 全模块；[`mnemopi`](../third/oh-my-pi/packages/mnemopi)（独立记忆服务 + MCP）。

---

### 🟢 P2-M：plan-mode 计划审批流（写保护 + handoff）

**问题**：Gyre 有 [`Mode`](../crates/core/src/lib.rs)（architect/code/ask/debug）切换，但**没有**「计划审批」语义——
architect 模式只改 system prompt，不阻止写、不强制先出计划。oh-my-pi [`plan-mode`](../third/oh-my-pi/packages/coding-agent/src/plan-mode)：
`PlanModeState`（enabled / planFilePath / workflow: parallel|iterative / reentry），[`plan-protection.ts`](../third/oh-my-pi/packages/coding-agent/src/plan-protection.ts)
经 compaction protection matcher 保证 plan 文件的 read 结果在 prune/shake 中存活，[`plan-handoff.ts`](../third/oh-my-pi/packages/coding-agent/src/plan-mode/plan-handoff.ts) 切换模式时把批准计划注入新模式。

**任务**：

- [ ] [`Mode`](../crates/core/src/lib.rs) 加 `Plan` 变体：启用时所有写工具（write_file / apply_hashline / run_command）经 [`ApprovalPolicy`](../crates/core/src/tool.rs) 强制 Deny + 提示「先出计划」。
- [ ] 计划落盘到 `local://PLAN.md`（或 `.gyre/plans/<slug>.md`），read 结果在 [`compact_active_path`](../crates/context/src/lib.rs) 经 protection matcher 保留。
- [ ] 用户审批后切到 `Code` 模式，计划路径经 handoff 注入 system prompt。
- [ ] [`crates/cli`](../crates/cli/src/repl.rs) `/plan` 命令 + Web UI 入口。

**参考实现**：[`plan-mode/`](../third/oh-my-pi/packages/coding-agent/src/plan-mode) 全模块。

---

### 🟢 P2-N：discovery 多工具配置互操作（agents.md / .claude / cursor / codex …）

**问题**：Gyre [`skills`](../crates/skills) 只发现原生 `.gyre/skills` + agentskills.io 规范，**不读**其他 Agent 工具的配置。
oh-my-pi [`discovery`](../third/oh-my-pi/packages/coding-agent/src/discovery) 经统一 capability registry 发现并转换 **12 种**外来配置源：
`agents.md` / `.claude` / `claude-plugins` / `cline` / `codex` / `cursor` / `gemini` / `opencode` / `github` / `mcp-json` / `vscode` / `windsurf`，
统一映射到 capability（context-file / extension / hook / instruction / mcp / prompt / rule / skill / slash-command / tool）。

**任务**：在 [`crates/skills`](../crates/skills) 旁新建 `crates/discovery`（或扩 skills），按 capability 分类转换主流外来配置（优先 `agents.md` / `.claude` / `cursor` / `codex`）。
让 Gyre 开箱即用继承用户既有 Agent 工具配置，降低迁移成本。

**参考实现**：[`discovery/index.ts`](../third/oh-my-pi/packages/coding-agent/src/discovery/index.ts) + 12 个 provider 文件。

---

### 🟢 P2-O：proxy 流式透传 + scrubPartialJson

**问题**：oh-my-pi [`proxy.ts`](../third/oh-my-pi/packages/agent/src/proxy.ts) 提供「代理流」模式：把上游 provider 事件透传下游，
对进行中的 tool_call 维护 `partialJson` side-channel（按 `contentIndex` 累积），在 `toolcall_end` 清除、`done`/`error` 时
[`scrubPartialJson`](../third/oh-my-pi/packages/agent/src/proxy.ts) 兜底清理未闭合块——使最终 `AssistantMessage` 不再读作「仍在流」。
Gyre 有自己的流式（[`AgentEvent`](../crates/core/src/message.rs)），但无此「透传代理」语义（用于 ACP / HTTP 中继 / 网关场景）。

**任务**（低优先，按需）：若 Gyre 要做 provider 网关 / ACP 中继，参考 `scrubPartialJson` 在流终点清理未闭合 tool_call 块。
Gyre 的 [`persist_interrupted`](../crates/agent/src/lib.rs) 已部分覆盖「中断兜底」，可与之合并设计。

**参考实现**：[`proxy.ts`](../third/oh-my-pi/packages/agent/src/proxy.ts) `scrubPartialJson` / `processProxyEvent`。

---

### 🟢 P2-P：replay-policy refusal 过滤（细化 P2-J）✅ 已完成

**问题**：oh-my-pi [`replay-policy.ts`](../third/oh-my-pi/packages/agent/src/replay-policy.ts)（仅 13 行）检测 API 级 refusal
（`stopReason=error` + `stopDetails.type=refusal|sensitive`），从 provider replay 过滤——避免 refusal 文本被反复重放。
Gyre 无 refusal 识别（与普通 error 混同）。此为已计划 [`P2-J`](#p2-jprovider-refusal-过滤--流中断保留已完成-tool_call) 的最小子集，
可独立速修：[`StopReason`](../crates/core/src/message.rs) 加 `stop_details` 字段 + [`build_provider_context`](../crates/context/src/lib.rs) 过滤 refusal assistant 消息。

**任务**（已完成）：

- [x] [`crates/core/src/message.rs`](../crates/core/src/message.rs) 新增 [`StopDetails { kind }`](../crates/core/src/message.rs)（serde 字段名 `type`，对齐 oh-my-pi wire）+ [`AssistantMessage.stop_details`](../crates/core/src/message.rs)（`#[serde(default)]` 向后兼容）+ [`is_provider_refusal`](../crates/core/src/message.rs)（Error + refusal-like 详情）。
- [x] Provider 端填充：[`openai.rs`](../crates/llm/src/openai.rs) / [`glm.rs`](../crates/llm/src/glm.rs) / [`deepseek.rs`](../crates/llm/src/deepseek.rs) 把 `content_filter` → `StopReason::Error` + `StopDetails::new("sensitive")`；[`anthropic.rs`](../crates/llm/src/anthropic.rs) `stop_details: None`（无直映射）。
- [x] Replay 过滤：[`convert_to_llm`](../crates/context/src/lib.rs) 对 `is_provider_refusal()` 的 assistant 消息返回 `None`；孤立 tool 消息由既有 [`sanitize_provider_messages`](../crates/context/src/lib.rs) 清理。

**验收**（通过，`cargo test -p agent-core` 19/19、`-p agent-context` 58/58、`-p agent-llm` 59/59；全 workspace 全绿）：

- [x] 单元测试（[`message.rs`](../crates/core/src/message.rs)）：`stop_details_refusal_like_detection` / `is_provider_refusal_requires_error_and_refusal_like_details` / `stop_details_serde_roundtrip_preserves_type_field` / `assistant_message_serde_back_compat_missing_stop_details`（旧持久化无字段→None）。
- [x] 集成测试（[`lib.rs`](../crates/context/src/lib.rs)）：[`build_filters_provider_refusal_assistant`](../crates/context/src/lib.rs)（refusal assistant 被过滤、普通 Error 保留、refusal 文本不重放）。

**参考实现**：[`replay-policy.ts`](../third/oh-my-pi/packages/agent/src/replay-policy.ts)（完整 13 行，已移植）。

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
| **第四轮** | **P0-L snapcompact 图像化压缩** | **大** | 🟡 **已设计，延后独立攻关**：工作区无 PNG/字体依赖 + assistant ContentBlock 无 Image 变体，需新增依赖 + 跨 provider 图像块贯通 + 真实视觉模型 eval（朴素渲染召回率不足会比 summarize 更差）。详见章节「实施前置条件」 |
| **第四轮** | **P1-K auto-thinking 自适应思考预算** | **中** | ✅ 已完成（`Effort` + `ThinkingPolicy` + `LlmThinkingClassifier` + run_loop 解析 + CLI 装配；core 19/llm 59/agent 19 测试全绿；server 装配待 model 参数签名调整） |
| **第四轮** | P1-L advisor 双代理评审 | 大 | 🟡 独立评审 Agent + emission-guard + WATCHDOG.md；新 crate，工作量大但差异化价值高 |
| **第四轮** | P1-M goals 目标 + token/时间预算 + `/loop` 命令 | 中 | 🟡 Goal 对象 + 预算耗尽收尾 + 用户命令；用户体验提升明显 |
| **第四轮** | P2-L hindsight 跨会话学习 + mental models | 大 | 🟢 与 P2-1 向量记忆合并设计；需定存储后端 |
| **第四轮** | P2-M plan-mode 计划审批流 | 中 | 🟢 Mode::Plan + 写保护 + handoff；架构师模式增强 |
| **第四轮** | P2-N discovery 多工具配置互操作 | 中 | 🟢 继承 agents.md / .claude / cursor / codex 配置；降低迁移成本 |
| **第四轮** | P2-O proxy 流式透传 + scrubPartialJson | 小 | 🟢 仅在做 provider 网关 / ACP 中继时需要；按需 |
| **第四轮** | **P2-P replay-policy refusal 过滤（P2-J 最小子集）** | **极小** | ✅ 已完成（`StopDetails` + `is_provider_refusal` + provider content_filter→sensitive + convert_to_llm 过滤；core 19/context 58/llm 59 测试全绿） |

**明确的高回报切入点**：

- **第一/二轮已验证**：P0-A（字节级稳定前缀）省去长会话每轮数万 token 重复 prefill；P0-B（length + 残缺 tool_call 占位）修正「执行参数被截断的工具调用」正确性 bug。
- **第三轮首选**：**P0-G（结构化 handoff 提示词）**——一处提示词改动即可显著提升 summarize 压缩与分支切换 handoff 的恢复质量（模型不再丢线索/重复工作），改动极小、零风险、立即可见。**P1-G（停止边界 steering 再检查）**——修复「用户在最后一轮期间发消息被搁置到下次 prompt」的 UX 缺陷，改动小。两者建议作为第三轮首批落地。
- **第四轮首选**：**P0-L（snapcompact 图像化压缩）**——把昂贵的本地 LLM summarize 替换为本地 PNG 渲染，长会话压缩成本与延迟降一个数量级，且信息以图像形式保留（视觉模型直接读图回放，比文本摘要丢信息少）。工作量大（Rust 栅格化 + bitmap 字体 + provider 感知帧形状）但 ROI 最高，建议作为本轮攻关首选。**P1-K（auto-thinking）** 与 **P2-P（refusal 过滤）** 改动小、立即可见，可作为本批「速修暖身」与 P0-L 并行。
- **第四轮差异化**：**P1-L（advisor 双代理评审）** 与 **P1-M（goals 目标预算）** 是 oh-my-pi 真正区别于普通 Agent 循环的「智能层」——Gyre 已把循环引擎吃透，下一步竞争力于此。建议 P0-L 落地后启动。

---

## 第五轮深度对比（2026-08）：产品层三件套（TTSR / conflict:// / magic keywords）

> 依据 [`oh-my-pi-feature-analysis.md`](oh-my-pi-feature-analysis.md) 的 P0 优先级落地。
> 循环引擎已吃透，本轮补**引擎之上的智能层与工具面**。全部完成，610 项 workspace 测试全绿。

### 🔴 P0-TTSR：时间旅行流规则（零上下文成本硬约束）✅ 已完成

**问题**：规则写进 system prompt 永远吃 token 且是 advisory（模型可无视）；无规则时又零约束。

**任务**（新 crate `crates/ttsr`，约 1400 行 + 集成）：

- [x] `rule.rs`：frontmatter 解析（自写极简解析器）+ `Rule` 模型（scope: text/thinking/tool/tool:NAME(GLOB)、
  globs 全局门、interruptMode: always|never、repeat: once|always、condition 正则 OR + astCondition）。
  glob 简写（`*.rs` → `tool:write_file`）；Rust `regex` crate 线性时间（消除 JS 版 ReDoS）。
- [x] `matcher.rs`：`TtsrManager` 每流缓冲（text/thinking 增量累积；工具载荷一次性快照），
  门控顺序：禁用过滤 → 注入抑制（once）→ 作用域 → 路径 glob → 条件；`digest_for` 按工具提取
  matcherDigest（write_file→content、apply_hashline→patch、run_command→command）；astCondition
  经 `agent_ast::search` 对内存文本做结构匹配（含 metavariable 同一性）。
- [x] `coordinator.rs`：线程安全协调器——`check_text_delta`/`check_thinking_delta`（可中断规则
  命中返回名字，流式中断）、`check_tool_calls`（Always → Abort；Never → 折叠
  `<system-reminder>` 进工具结果）、`render_injection`（`[ttsr-injection:…]` 标记 +
  `<system-interrupt>` 包装）、`restore_from_messages`（会话加载扫描标记恢复抑制）。
- [x] `run_loop` 五插入点：生成器头恢复抑制（snapshot_nodes 扫描）→ MessageStart 清缓冲 →
  流式 TextDelta/ThinkingDelta 命中即 break（drop event_stream = 关闭 HTTP 连接，违规增量不发射）→
  中断后丢弃部分输出（discard）、注入、合成 aborted TurnEnd、continue 重试 → MessageEnd 工具
  载荷快照（Abort 不 append 直接重试；Never 提醒暂存）→ 工具结果回填时 `take_reminder` 前置折叠。
- [x] 抑制状态持久化：注入消息带 `[ttsr-injection:name1,name2]` 标记（User 消息），压缩/分支切换/
  恢复后 `restore_from_messages` 重建抑制（repeat=once），不重复注入。
- [x] 装配：`[ttsr]` 配置段（enabled/disabled_rules）+ CLI/server 双端 `<cwd>/.gyre/rules/*.md`
  发现（`discover_rules` 解析失败跳过并告警）+ `config.example.toml` 文档段。
- [x] 测试：ttsr crate 32 项（frontmatter/作用域/glob 简写/缓冲累积/抑制/restore/digest/ast 匹配/
  abort-vs-remind）+ agent 集成 4 项（流式中断重试、恢复抑制、工具 Never 折叠、工具 Always 丢弃重试）。

**v1 边界**（文档化）：文本/思考作用域的 `interruptMode: never` 按 Always 处理（无流式折叠通道）；
`repeat: after-gap` 未实现（once|always）；注入消息在 UI 显示为带标记的用户消息（透明可见）。

### 🔴 P0-conflict：conflict:// 合并冲突解决 ✅ 已完成

**问题**：模型处理 Git 冲突需读 200 行手抄；读改写三方易错。

**任务**（`crates/tools/src/conflict.rs` ~550 行 + fs 集成）：

- [x] 扫描状态机：严格列 0 标记（`<<<<<<<`/`|||||||`/`=======`/`>>>>>>>`，CRLF 容忍），
  仅闭合块返回；`RawBlock` → `ConflictBlock`（侧文本 + 标记行原文）。
- [x] 会话级 `ConflictHistory`：path+start_line 复用 id；read 注册 / write 解决后移除。
- [x] read：`<file>:conflicts` selector（扫描+注册+逐块摘要）；`conflict://<N>`（完整块带原文
  行号）/ `conflict://<N>/ours|theirs|base`（单侧）；`conflict://*` 读取报错。
- [x] write：`conflict://<N>` 支持 `@ours`/`@theirs`/`@base`/`@both` token 与替换文本；
  `conflict://*` 批量（`N: @ours` 指令行或全文应用全部；按文件分组 + 自底向上保持锚点）。
- [x] splice 防漂移：标记行内容校验（首尾标记 + sep/base 按序出现，非固定索引——ours 行数不定）；
  区域以换行结尾时 replacement 补换行；CRLF 行尾跟随。
- [x] 注入：`Agent` 持 `Arc<Mutex<ConflictHistory>>`，run_loop ToolContext 注入；9 处构造点补 None。
- [x] 测试：conflict.rs 12 项 + fs 端到端 6 项（扫描→单侧读→@ours 解决→漂移拒绝→批量指令→
  未启用报错→通配读拒绝）。

### 🟢 P0-keywords：magic keywords（ultrathink / orchestrate / workflowz）✅ 已完成

**问题**：用户无法一句话切换行为契约（深度推理 / 多代理编排）。

**任务**（`crates/agent/src/keywords.rs` ~400 行 + run_loop 集成）：

- [x] 词边界匹配：Rust regex 无 look-around → `find_iter` + 前后字符检查实现 omp 边界语义
  （排除字母数字下划线/`./`/`-`/`\`/`::`，后不跟 `.`/`(`；仅小写全词）。
- [x] markdown 掩码：长度保持地抹掉围栏代码块（≥3 反引号/波浪号，行首闭围栏）、内联代码、
  HTML 注释、XML/HTML 标签（嵌套同名深度计数 + 自闭合）。
- [x] `ultrathink` → 跳过 auto-thinking 分类器、`Effort::XHigh` 预算拉满（模型不支持思考则 None）；
  `orchestrate` → 10 条编排契约隐藏通知；`workflowz` → 需 task 工具在场（Gyre 无 eval，注入
  简化版 task 工作流提示）。通知先于用户消息注入 context。
- [x] 测试：单元 9 项（边界排除/围栏/内联代码/XML 嵌套/注释/复数不命中/workflowz 门控）+ 集成 2 项
  （orchestrate 注入、orchestrates 复数不注入）。

**排期建议（接续）**：P0 三件套后，按 [`oh-my-pi-feature-analysis.md`](oh-my-pi-feature-analysis.md)
P1 顺序推进：web_search + 站点抽取、Advisor（P1-L）、LSP 写操作（WorkspaceEdit/rename_file）、
resolve 暂存（ast_rewrite 预览→应用）。

---

## 第五轮补充（2026-08）：P1 四件套（web_search / Advisor / LSP 写操作 / resolve 暂存）

> 接 P0 三件套之后按 [`oh-my-pi-feature-analysis.md`](oh-my-pi-feature-analysis.md) P1 顺序落地。
> 全部完成，workspace 645 项测试全绿。

### 🟡 P1-4：web_search + 站点感知抽取 ✅ 已完成

- [x] `WebSearchProvider` trait + 顺序回退链（首个非空结果胜出；失败汇总错误）：
  `DuckDuckGoHtml`（免 key，HTML 端点解析）为主，`Searxng`（`GYRE_SEARXNG_URL` env 懒加载）可选。
- [x] 站点抽取 `extract_site`：arxiv（citation 元标签 + abstract 段）、crates.io（API JSON：版本/下载量）、
  npm（registry latest）、github（raw README 首段）→ 结构 markdown（锚点保留）。
- [x] 复用 `fetch_http`（SSRF 每跳校验/15s/1MiB）；**fetch_client 补身份 UA**——crates.io API 对无 UA
  返回 403（实测修复）。DDG 在本环境不可达（网络策略），错误信息明确提示 `GYRE_SEARXNG_URL` 出路。
- [x] `web_search` 工具（`CapabilityTier::Network` 审批门槛）进 `core_tools`；6 单测（解析/回退链/截断/编码）。
- [ ] 后续：jina/perplexity 等 key provider、更多站点抽取器（mdn/stackoverflow/reddit）。

### 🟡 P1-5：Advisor 双代理评审（P1-L）✅ 已完成

- [x] 新 crate `crates/advisor`（仅依赖 agent-core，无循环依赖）：
  - `EmissionGuard`：normalize（NFKC+小写+折叠标点）→ `(severity, 文本)` 去重 + filler 黑名单
    （stop/ok/continue/lgtm…）——真实教训：无去重 advisor 会 spam（issue #3520）；
  - `SecretObfuscator`：sk-/ghp_/AKIA/Bearer/env 导出等 8 类内置 pattern + 可追加，替换 `<redacted>`；
  - `WATCHDOG.md` 发现：cwd walkup + `~/.gyre` + 项目 `.gyre`，注入评审 system prompt；
  - `Advisor::review`：快照渲染（角色 + 截断 + 工具结果折叠 + 脱敏，总上限 24k 字符）→
    独立 provider 一次流式调用 → `[severity] note` 解析 → guard 过滤。
- [x] run_loop 集成：每 N 轮（默认 4，builder 可调）在 aside drain 后评审一次，建议以
  `[advisor:<severity>]` 标记 User 消息注入 context（与 aside 同语义：不打断在途工具）+ Say 事件；
  评审失败仅 warn 不阻断主循环。
- [x] 装配：env `GYRE_ADVISOR=1` 启用（复用主 provider + watchdog 自动发现）；CLI/测试双验证。
- [x] 测试：advisor crate 15 项（guard 去重/filler、脱敏、watchdog 发现、解析/快照/截断）+ agent 集成 2 项
  （blocker 注入、filler 不注入）。
- [ ] 后续：近窗口 promote 到更大 model（MVP 固定主模型）、advice 经 yield queue 而非 context 直注。

### 🟡 P1-6：LSP 写操作 ✅ 已完成

- [x] `crates/lsp/src/edits.rs`：`apply_text_edits` WorkspaceEdit 应用器——行切分 + UTF-16 列→字节
  （代理对中间列严格报错）、重叠校验（排序后相邻检查）、自底向上降序拼接；6 单测（多行区间/
  重叠拒绝/插入/代理对/越界）。
- [x] client 扩展：`LspRenameEdit` 补 end 位置（原实现只存 start，编辑无法应用）、`LspCodeAction`
  补 `command`（executeCommand 路径）、新增 `execute_command`（manager 透传）。
- [x] 新工具 `lsp_apply`（Write 审批门槛）：`{edits: [...]}` 直传（lsp rename/code_actions 输出原样回传）
  或 `{code_action_index, uri, line, character}` 重取应用；多文件按 uri 分组逐文件读改写，
  old_text 非空时漂移校验；command 型 code action 执行后返回响应。
- [x] 装配：`lsp_tool()` 构造器共享 LspPool，`lsp_apply` 与 `lsp` 同池（同一套语言服务器）。
- [ ] 后续：rename_file（willRenameFiles 文件移动 + 引用重写）、诊断版本账本联动应用后延迟诊断。

### 🟡 P1-7：resolve 暂存（ast_rewrite 预览→应用）✅ 已完成

- [x] `ast_rewrite {preview: true}`：不落盘，返回 `(proposed) N replacements` + 首处差异预览，
  暂存到会话级队列（同文件重复 preview 只留最新）。
- [x] `write_file` 三路 `xd://` 设备：`xd://pending`（列出队列）、`xd://resolve`（应用：重读文件 +
  重放重写 + **计数与字节数双重复查**防漂移，要么全成要么全不，失败条目保留可重试）、
  `xd://reject`（丢弃指定）；content 支持 `path` / `path:pattern` 精确过滤。
- [x] 注入：Agent 持队列 Arc，run_loop ToolContext 注入；9 构造点补 None。
- [x] 测试 4 项：暂存→应用、漂移拒绝、丢弃、重预览替换 + 过滤应用。

**排期建议（接续）**：按 P2 顺序推进：eval 内核、DAP、pr:///issue:// GitHub 缓存、provider 路由、
Collab 补齐、snapcompact（P0-L）、规则导入（P2-N）。

---

## 第六轮（2026-08）：P2 首批三件套（pr:///issue:// / discovery 规则导入 / Provider fallback）

> 按 feature 分析 P2 排期，取成本可控、价值直接的三项；workspace 655 项测试全绿。

### 🟢 P2-10：pr:// / issue:// GitHub 读取 + 文件缓存 ✅ 已完成

- [x] `github.rs` 提取 `api_get_json`（pub(crate)）：GET `<owner>/<repo>/<path>` + **auth 指纹文件缓存**
  （`<workspace>/.gyre/cache/github/{指纹}__{owner}__{repo}__{path}.json`；列表 TTL 60s、单对象 300s；
  匿名/鉴权缓存隔离；缓存损坏回退直连）。
- [x] `render_gh_uri`：单对象 → `#N 标题 [state] — @user · 时间` + body + 合并状态/增删行 + 评论数；
  列表 → 逐条 `#N [state] 标题 — @user` + 详情提示。
- [x] read_file 路由：`pr://<owner>/<repo>[/<N>][?state=…]`、`issue://` 同构；缺省 `state=open&per_page=10`；
  裸 `pr://` 提示；非法路径报错。schema 描述已更新。
- [x] 真实冒烟：匿名读 rust-lang/rust 开放 PR 列表 10 条成功（无 GH_TOKEN）。
- [ ] 后续：缓存失效机制（bash 变异？）、评论/审查详情读取、`gh` CLI 优先。

### 🟢 P2-14：discovery 外来配置互操作（AGENTS.md/CLAUDE.md/Cursor/Cline）✅ 已完成

- [x] 新 crate `crates/discovery`（**零依赖**纯 std）：`discover(cwd)` → 按来源优先级排序的
  `DiscoveredSection`（source/path/name/globs/content），`source:path` 去重（walkup 不同层级全保留）。
- [x] 四来源：`AGENTS.md`（walkup + `~/.agent`）、`CLAUDE.md`/`.claude/CLAUDE.md`（walkup）、
  `.cursor/rules/*.mdc`（frontmatter: description/globs/alwaysApply + body）、
  `.clinerules/*.md` + `.clinerules.md`。
- [x] `render_section`：`外来配置[cursor] <path> name（适用于 globs）:\n内容` 注入段。
- [x] CLI 装配：`base_context_files` 追加外来段（与 AGENTS.md 同通道）；i18n 4 语言键
  （`context.foreign_loaded`）；REPL 冒烟验证「已加载 2 条外来 Agent 配置」。
- [x] 5 单测（发现/去重/MDC frontmatter/Cline 目录与单文件/渲染）。
- [ ] 后续：codex/gemini/opencode/windsurf/vscode/mcp-json 来源、rule→TTSR 规则转换。

### 🟢 P2-11：Provider 路由增强（fallback 链）✅ 已完成

- [x] `LlmError::is_fallbackable()`（core）：Transport/5xx/429/401/403 可换适配器重试；
  其余 4xx/StreamInterrupted/Decode/Unsupported 立即上抛。
- [x] `ProviderRegistry::route_all(api)` + `stream_fallback`：依序尝试全部支持该线协议族的适配器，
  首个成功建流者胜出；失败分类决定是否换下一家；全失败汇总「全部 N 个适配器失败（最后一个: …）」。
  多适配器注册（各带不同 key env 的适配器）即凭证轮换通道。
- [x] `impl LlmProvider for ProviderRegistry::stream` 默认走 fallback（单适配器与旧行为等价）。
- [x] 3 测试：Transport 跳下一家、5xx fallback 而 400 立即失败（不尝试第二家）、全失败汇总。

### 🟢 P2-12：Collab 协议补齐（帧扩展 / write token 权限 / 快照分块恢复）✅ 已完成

> 目标：从「实验性中继」到「可分享的 live session」。WS 端到端冒烟已验证。

- [x] **帧类型扩展**：`WireFrame` 新增 `Hello`（proto/name/write_token）、`Welcome`（proto/read_only/entry_count）、
  `SnapshotChunk`（seq/final_chunk/chunk）、`Bye`（reason）、`Error`（message）——serde tag=type 兼容扩展；
  Gyre 全密封形态（hello 亦密封，中继盲视更严格，与 omp 明文 hello 有差异，已注释说明）。
- [x] **write token 权限**：`generate_write_token()`（16B CSPRNG → 32 hex）；`Relay::set_write_token/clear_write_token` +
  `publish_with_token`——受管房间无令牌发布 → `WriteForbidden`；未注册房间保持开放（demo 兼容）。
  `CollabClient::with_write_token`（缺省只读 view）。WS 层：`/api/collab/room` 生成并注册 token、
  `/collab/{room}?wt=` 连接级校验。
- [x] **快照分块 + 恢复**：`chunk_snapshot`（UTF-8 字符边界切割，默认 48 KiB/块，适配 WS 64 KiB 帧上限）、
  `SnapshotAssembler`（seq 严格递增、final_chunk 收尾、乱序/重复拒绝）、`append_snapshot_log`/`read_snapshot_log`
  （`~/.gyre/collab/<room_id>.jsonl` JSONL 追加，取最后一行恢复）。
- [x] **WS 退避**：不适用——Gyre 无 WS 客户端重连路径（桥为服务端接收侧），断线恢复由快照日志 + `cleanup_empty` 覆盖；已标注。
- [x] 测试 +13（权限 6：开放房/受管房/清 token/只读 client 拒发/帧 roundtrip/token 格式；快照 7：单块/空/UTF-8/大回环/乱序/未完成/落盘）；
  clippy 新代码清零（collab 预存 4 条移植 doc 警告不动）。**WS 端到端冒烟**：裸 socket 验证——只读连接发布被拒
  （server warn + 零送达），带 wt 连接发布广播送达订阅者。
- [ ] 后续：浏览器 guest（collab-web 前端移植）、host 侧 read_only 裁决（Hello.write_token 已就绪）、transcript 分页拉取。

### 🟢 P2-13：Provider 路由增强（模型 fallback 链 + API key 轮换）✅ 已完成

> 目标：把「单模型单 key」升级为「主模型失败自动换备、多 key 限流自动轮换」。
> 配置冒烟已验证（合法链通过 / 环与重复引用报错）；agent 层 mock 集成测试验证切换语义。

- [x] **模型 fallback 链（配置维度）**：`ModelProfile.fallbacks`（alias/id 引用，跨线协议族亦可，
  如 Anthropic 主 → OpenAI 备）；`Config::resolve_chain` 依序展开 + **防环防重复**（配置错误
  启动即报错，validate 阶段全量检查）；`ModelProfile::to_model` 统一 profile→Model 映射
  （cli/server 装配复用，消除两处内联差异）。
- [x] **调用链语义**：agent loop 每轮 `[主模型] + fallbacks` 依序尝试，可重试错误
  （网络/5xx/429/401/403）换下一模型，**不可重试错误立即上抛不浪费备选**；全链失败
  **只算 1 个 mistake**（轮内换模型不重复计错）；命中备用模型打 `model fallback 成功` 日志。
- [x] **API key 轮换（配置维度）**：`ModelProfile.api_keys` 多 key 列表（`${ENV}` 展开）→
  `KeyRing`（AtomicUsize round-robin，并发安全）；每轮请求换下一个 key；空 = 单 key 不轮换。
  `RuntimeOverrides::api_key` 命中时**优先于轮换**（host 动态凭证语义不变）。
- [x] **与既有机制叠加**：外层模型链（agent loop）⊃ 内层适配器链（registry.stream_fallback
  同 api 多适配器）——两层 fallback 组合为完整语义；凭证轮换独立于 registry（ctx 级覆盖）。
- [x] 测试 +5（config 3：链展开顺序/缺引用+环报错/key 环+to_model；agent 5：主失败换备成功、
  全链失败单 mistake、硬错误中断链、key 轮换+override 优先、KeyRing 取模）；clippy 新增代码
  清零（config 回归 HEAD 基线 26 条预存警告，本轮新增 2 条已修）。
- [ ] 后续：`fallbacks` 支持内联 profile（含 key 差异的匿名备选）、失败计数/熔断（同模型连续
  失败 N 次后跳过）、per-model 并发上限（max_in_flight 配置化）。

### 🟢 P2-14：Collab 浏览器 guest（端到端加密 Web 视图）✅ 已完成

> 目标：把 `/collab/{room}` 分享链接从「WS 握手错误页」变成可用 UI——浏览器端
> WebCrypto 解密查看 + 聊天。浏览器端到端验证通过（可写 tab ↔ 只读 tab 跨连接消息闭环）。

- [x] **GET 分发**：`/collab/{room_id}` 普通浏览器 GET（无 WS Upgrade 头）→ 返回单文件
  guest 页（`crates/server/src/collab_guest.html`，内嵌 CSS/JS，零外部依赖）；
  WS 升级路径不变。`OptionalWsUpgrade` extractor（axum 未导出 WS rejection 类型，
  自行判定 Upgrade 头，握手不完整降级为 guest 页，无副作用）。
- [x] **端到端加密**：房间密钥取自链接 `#` 片段（base64url → 32B），WebCrypto AES-GCM
  与 Rust codec 对偶（`[12B IV][ciphertext+tag]`）；密钥永不发服务器。
- [x] **帧协议对偶**：WireFrame serde `tag=type` snake_case 全 9 变体前端渲染——
  chat 气泡（自己/他人分侧）、hello/presence 在线列表、bye/error 系统消息、
  **snapshot_chunk 聚合恢复**（seq 校验 + final 收尾，乱序丢弃重来）。
- [x] **权限模型**：`?wt=` 携带 → 可写（输入启用）；缺省 → 只读 view（输入禁用 +
  「无写令牌」提示）。中继层 write token 校验不变（受管房间只读连接发布被拒）。
- [x] **断线重连**：指数退避 1s→2s→4s→8s 封顶（P2-12 曾标注「无 WS 客户端重连路径」——
  guest 即新客户端，重连语义落地）。
- [x] 测试 +2：guest 页浏览器 GET 200 HTML（含 WebCrypto/分块/重连标记）、
  密封布局常量与 Rust codec 一致性；**浏览器端到端冒烟**：可写 tab seal 消息 → 中继 →
  只读 tab 解封渲染；在线列表/加入提示联动；只读 tab 输入禁用。
- [x] **排障记录**（关键 bug）：`crypto.subtle.encrypt` 返回 ArrayBuffer，
  `Uint8Array.set(ArrayBuffer)` 静默不拷贝 → 密封帧密文全零、解封恒 OperationError。
  修复：`new Uint8Array(await encrypt(...))`。evaluate 自测路径因显式包裹而幸免，
  页面路径踩坑——注释已写明。
- [ ] 后续：transcript 分页（host 侧以快照块下发会话历史）。

---

## 第七轮（2026-08）：Collab 中继历史重放（新加入者先见历史再收实时）✅ 已完成

> 接 P2-14 guest 页：新开 tab 只有空白与实时帧，看不到已有消息。
> 方案选型：**中继层内存 ring buffer**（vs host 代答 / 服务端持久化）——服务端纯转发无状态、
> host 常不在线、guest 页无需发 Sync 即可拿历史；ring buffer 只存**密封字节**（密钥盲视）。

- [x] **relay 历史环**（[`crates/collab/src/relay.rs`](../crates/collab/src/relay.rs)）：
  `Room { sender, history: VecDeque }`，`publish_with_token` 先 push 历史（截断至
  `DEFAULT_REPLAY_LIMIT = 200`，快照分块单帧 ≤48 KiB，最坏 ~9.6 MiB/房间，60s 清理周期回收）
  **再判 `receiver_count() == 0`**——无订阅者（全员掉线）的帧仍进历史，重连可找回；
  房间不存在时 publish 自动创建（发方从不 join 自身）。
- [x] **原子 `join_with_replay`**：历史克隆与 subscribe 在同一 `rooms` 锁内完成，
  杜绝「取历史后、订阅前」的丢帧缝隙；`join` 委托其丢弃历史部分（兼容旧调用）。
- [x] **空房间语义**：无订阅者房间不再于 join/publish 时移除——历史须跨掉线存活；
  统一由 server 60s 周期 `cleanup_empty` 回收（有订阅者保留）。
- [x] **WS 桥历史补发**（[`crates/server/src/lib.rs`](../crates/server/src/lib.rs) `collab_relay`）：
  连接建立后先逐条补发历史密封帧，再进广播循环；发送失败（客户端断开）即返回。
- [x] **guest 页收帧即已连接**：历史补发先于 welcome 到达、host 离线时无 welcome——
  `handle()` 对非 welcome 帧先置「已连接」再分发。
- [x] 测试 +4 / 改 1：`replay_history_to_new_subscriber`（历史+实时间无缝隙）、
  `replay_ring_is_bounded`（cap=5 发 8 取最近 5）、`offline_frames_survive_reconnect`（掉线帧重连找回）、
  `publish_to_empty_room_returns_zero_and_keeps_history`（空房间保留 + 订阅者退出后 cleanup 才回收）；
  collab 27/27 绿，clippy 新代码清零。
- [x] **浏览器端到端冒烟**：tab A 发消息（自收回显渲染）→ tab B 新开即见历史
  （chat 帧 + 对方 hello 加入提示），再发实时帧 tab B 顺序到达；控制台零错误
  （P2-14 遗留的自收自解闭环已随重建验证通过）。

---

## 第八轮（2026-08）：Collab host 侧 read_only 裁决（Welcome 单播握手）✅ 已完成

> 接第七轮：guest 页此前以链接 `?wt=` 乐观判定读写（`writable = !!wt`），错误/过期 wt
> 会 UI 可写但中继拒发（WriteForbidden 静默），体验断裂。本轮把裁决权交给 host。

- [x] **本服务即 host 进程**（对标 oh-my-pi 的 coding-agent 持密钥）：[`new_collab_room`]
  本地生成密钥后不再丢弃——登记到 `SessionManager.collab_hosts`（`HostSecret{key, write_token}`），
  仅存内存、从不外发；**中继 [`Relay`] 保持密钥盲视**（只存密封字节），host 只 seal 自己的
  裁决帧、永不解封他人帧。
- [x] **裁决与中继发布校验同源**（[`seal_host_welcome`]）：连接 `wt` 对照规范令牌
  （[`Relay::set_write_token`] 登记的同一值）→ 密封 `Welcome{read_only, entry_count: 0}`
  **单播直发**该连接（不走中继广播，不干扰其他 guest；历史重放前先发握手帧）。
  正确 wt → 可写；缺失（view 链接）/错误 wt → 只读。
- [x] **guest 页收编裁决**：`welcome` 帧优先于链接乐观判定——`read_only` → 禁用输入 +
  状态栏「已连接（proto N，只读）」+ badge 只读 + 说明性 placeholder；可写裁决幂等确认。
  `connected` 标志防止 hello 回显帧覆盖 welcome 的丰富状态。
- [x] 测试 +1：`host_welcome_adjudicates_read_only_from_connection_wt`（正确/缺失/错误 wt
  三态解封断言）；server 8/8、workspace 682 全绿；clippy 新代码清零。
- [x] **浏览器端到端冒烟**：同房间三链接形态——view 链接（只读，输入禁用）、全链接
  （可写，发送成功）、**错误 wt**（host 裁决只读，输入禁用 + 提示「链接 wt 无效」）；
  可写 tab 消息被两个只读 tab 实时渲染；控制台零错误。
---

## 第九轮（2026-08）：Collab transcript 下发（host 以快照块推送会话历史）✅ 已完成

> 接第八轮：guest 能看到实时帧与中继历史，但看不到**绑定 agent 会话**的完整 transcript。
> 本轮把会话历史以快照分块下发（分页传输：48 KiB/块，`Welcome.entry_count` 告知块数）。

- [x] **会话绑定**：`/api/collab/room?sid=<session_id>` —— `SessionParams` 新增 `sid`，
  `HostSecret` 增 `session_id` 字段；Web UI（c5-ui `newCollabRoom`）自动携带当前会话 id。
- [x] **transcript 渲染**（[`render_session_transcript`]）：与 `read_history` 同源解析
  （新旧持久化格式兼容），逐条渲染 `[user]/[thinking]/[assistant]/[tool]/[tool result]/
  [status]/[ask]` 前缀文本；`SoftRequirement` 等内部机制消息跳过；超限
  （`TRANSCRIPT_MAX_CHARS` = 4 MiB）截断**保留尾部最新**并加省略标记。
- [x] **WS 桥下发**：`collab_relay` 连接时先渲染绑定会话 → `chunk_snapshot` 分块 →
  `Welcome.entry_count` 填块数 → 逐块 seal **单播直发**（welcome → 快照块 → 中继历史 →
  实时广播的顺序；均不走中继，不干扰其他 guest）。
- [x] **guest 页加载态**：`welcome.entry_count > 0` → 「正在加载会话快照（共 N 块）…」，
  聚合收齐后 snapshotDone 渲染「已恢复快照（N 块 / M 字符）——以下为快照原文」。
- [x] 测试 +3：`render_session_transcript_renders_all_roles`（全 role 前缀 + 跳过内部消息）、
  `render_session_transcript_missing_or_empty_returns_none`、`host_welcome_carries_entry_count`；
  server 11/11、workspace 685 全绿；clippy 新代码清零。
- [x] **浏览器端到端冒烟**：720 KiB 冒烟会话（user/thinking/assistant 长文本/tool_result）→
  绑定房间 → guest 收到 welcome（entry_count=15）+ 15 块快照（每块 ~48 KiB）聚合渲染
  （240 K 字符全文，含省略逻辑未触发）；只读 view 链接权限裁决不受影响。



## 第十轮（2026-08-02）：P0 五项（eval 内核 / 输出最小化器 / goals 预算 / cache 可见性 / NDJSON RPC）✅ 全部完成

对标 [`oh-my-pi-architecture-benchmark.md`](oh-my-pi-architecture-benchmark.md) §五 P0 清单。全 workspace 测试通过（agent 101 / tools 78 / config 41 / cli 44 / eval 8 / llm 70 / server 11，零失败）。

### 🔴 P0-1：eval 内核 + 环回桥 ✅（新 crate `crates/eval`，~1200 行）

- [x] **Python NDJSON 持久内核**（`manager.rs`）：`python3 -u -c <内嵌 PYTHON_DRIVER>`，会话级内核池（session_key = `cwd|session`），共享命名空间；同一会话并发 execute 经 Mutex 串行化；三种回收路径（keep=false / idle 清扫 / abort kill）。
- [x] **环回桥**（`bridge.rs`）：127.0.0.1 随机端口 axum；per-run 轮换 bearer token（`RunGuard` drop 注销，无效 401）；run 的 `cancel.child_token()` 下构造 `ToolContext` 执行宿主工具 → `{"ok":true,"output":…}`。
- [x] **eval 工具**（`tool.rs`）：`language enum["py"]` / `code` 必填 / `keep` 默认 true；Execute 级 + Exclusive + interruptible；**懒加载桥**（`OnceCell`，首次 execute spawn，/mode 切换重建 Agent 自动换新审批）。
- [x] **装配**（cli/main.rs）：`[eval] enabled` 或 `GYRE_EVAL=1` 启用；桥注册表为「不含 eval」快照（防自我递归），`WithEval` 适配器附加到 Agent 侧；会话标签经 RwLock 共享（/resume 换会话隔离内核池）。
- [x] 驱动内嵌 `tool` 代理（属性访问 → `tool.<name>(**kwargs)` POST /call；401 中文提示；`tool.reset()`）。
- [x] 测试 8 项（含真实 python3 内核端到端：1+1 result、print stdout 帧、命名空间共享、traceback、桥 401）；修 1 个 Rust 原始串引号数 bug（`r#""""`）。

### 🔴 P0-2：bash 输出最小化器 ✅（`crates/tools/src/minimizer.rs`，~1100 行）

- [x] `OutputFilter` trait + `Minimizer`（首过滤器胜出；无命中且超 `max_lines` 走通用 head(100)+tail(100) 折叠，带 `… [已折叠 N 行] …` 标记）。
- [x] 5 个内置过滤器：GitStatus（节头+计数+未跟踪目录按父目录折叠）、GitDiff（保留 stat+hunk 头丢内容行）、GitLog（前 15 条提交）、Cargo（error/warning+上下文+Finished 汇总）、Python（traceback 尾块+FAILED 列表）；全部确定性纯函数，仅实际压缩时返回 Some。
- [x] `RunCommandTool` 挂接（`shell.rs`）：输出捕获后应用，命中 → summary + `> 输出已由 minimizer 压缩（{filter}）；如需完整输出请重跑该命令。`
- [x] 配置 `[agent.commands.minimizer] enabled/max_lines`（默认开）；cli/server 双装配点 + rpc 同步。
- [x] 测试 21 项（每过滤器命中/不命中/空输出 fixture + 确定性断言）。

### 🔴 P0-3：goals 目标预算 ✅（`crates/agent` + `crates/config` + cli）

- [x] `GoalBudget`/`GoalState`：记账口径 input+cache_write+output（cache_read 折扣价不计）；token + 墙钟双限；`note_usage` 首次超限一次性置位。
- [x] run_loop 集成：用量累计点（`summary.usage.add` 后）记账 → 停止边界第四类检查（steering→aside→followUp→goal）：软模式注入 `⚠ 目标预算已用尽（…）` 提醒后续跑一轮；硬模式不注入直接收尾；`[advisor:…]` 式 Say 事件。
- [x] `/goal` 命令：查看 / `set <tokens>` / `extend <tokens>`；扩展后未超限重置提醒标记；i18n 四语言。
- [x] 配置 `[goals] token_budget/time_budget_secs/hard_stop`（默认关）；共享 Arc 跨 Agent 重建保持。
- [x] 测试 6 项（4 单元 + 2 循环级集成：软模式注入续跑 4 次调用 / 硬模式 3 次调用不注入）。

### 🔴 P0-4：prompt-cache 可见性 ✅（`crates/llm` + cli）

- [x] openai 适配器补 `usage.prompt_tokens_details.cached_tokens` → cache_read；anthropic 补 `cache_read_input_tokens`/`cache_creation_input_tokens`（deepseek/glm 已有）。
- [x] `/status` 新增 cache 命中率行（`status.cache` 四语言）：`cache_read / (input+cache_read)`，为零时省略。

### 🔴 P0-5：NDJSON RPC 模式 ✅（`crates/cli/src/rpc.rs` ~1000 行 + `docs/rpc.md`）

- [x] `agent --rpc`：stdin 逐行 JSON 请求 / stdout 逐行 JSON 事件；协议 `prompt|cancel|ping` → `event|done|pong|error`（单行 JSON、\n 转义、usage 五字段）；进程内一个 Agent + 同一 Context 跨 prompt 复用。
- [x] 取消经 `Agent::run_with_cancel`（每轮独立 token）；SIGINT 优雅退出；审批在 RPC 模式自动拒绝（`--approval-mode yolo` 可全自动）。
- [x] 与 --serve/--acp 互斥；复用 main 装配（TTSR/advisor/goals/memory/fallback 全对齐）。
- [x] docs/rpc.md：协议规范 + Python 客户端示例（~40 行）。
- [x] 测试 11 项（协议编解码纯函数）+ 冒烟：ping → pong / cancel → no running turn / bogus → unknown type（单行 JSON 全部正确）。

### 已知取舍

- eval 桥回调工具的审批为「构造时当前 mode」的快照（/mode 切换后旧 Agent 弃用，新 Agent 懒加载新桥）；RPC 模式未注册 eval（v1 限定 CLI/REPL）。
- minimizer 压缩后的完整输出暂不落 artifact（模型可重跑同命令获取全文）；通用折叠默认关闭（max_lines=0）。
- goals 记账不含 cache_read（折扣价）；时间预算从首次用量起计。

## 第十一轮（2026-08-02）：P1 六项（DAP / review / 统计仪表盘 / /diff /fresh / plan-mode / 心智模型）✅ 全部完成

对标 [`oh-my-pi-architecture-benchmark.md`](oh-my-pi-architecture-benchmark.md) §五 P1 清单。全 workspace 57 套件零失败。

### 🟡 P1-1：DAP 调试器 ✅（新 crate `crates/dap`，~1900 行）

- [x] **帧协议**（`frame.rs`）：Content-Length 编解码纯函数（7 测试：往返/跨 chunk 分片/多帧合并/缺头/超限/垃圾流）。
- [x] **会话**（`session.rs`）：stdio 传输、initialize→initialized→launch 握手、pending map+oneshot 按 id 配对、事件广播（stopped/continued/output/thread/terminated/Other）、send_sync、close；反向请求（runInTerminal）v1 自动回错误防阻塞。
- [x] **适配器探测**（`probe.rs`）：lldb-dap / dlv dap / python -m debugpy.adapter 三适配器 PATH 探测（含 python 模块 import 预检，避免 30s 握手超时）。
- [x] **debug 工具**（`manager.rs`）：14 动作（launch/attach/continue/pause/next/step_in/step_out/threads/stack_trace/scopes/variables/evaluate/set_breakpoint/remove_breakpoint），断点注册表，launch args 自动补 request 字段（debugpy/dlv 要求）。
- [x] cli 接线：`[tools.enabled] debug` 可选组（main/rpc 装配 + OPTIONAL_TOOL_KEYS 白名单）。
- [x] 测试 18 项（含 duplex 会话模拟与 lldb 冒烟，本机无适配器时显式 skip）。

### 🟡 P1-2：/review 评审流 ✅（`crates/cli/src/review.rs` ~700 行 + `prompts/review.md`）

- [x] `/review [--staged] [N]`：git diff 收集（非 git 仓库提示；>256KiB 截断降级 stat+前 200 行）→ diff 权重（文件数×3+新增行/20）→ 自动 1/2/4 个 TaskTool 子代理并行评审（tasks 数组）→ P0-P3 分级 + 置信度排序聚合输出（不进模型上下文）。
- [x] 提示词 `prompts/review.md`（关注点 + 分级 + 机器可解析 `[P<n>] <conf> <loc> | <desc> | <sugg>` 行格式）。
- [x] 测试 12 项（diff 解析含引号路径 rename、权重、切分、发现提取、排序、渲染）；修 2 个实现期 bug（引号包裹路径解析、空报告测试的 i18n 语言依赖）。

### 🟡 P1-3：统计仪表盘 ✅（`crates/server` + `web/c5-ui`）

- [x] `/api/stats` 扩展：sessions/usage/tools/top_models/daily 聚合（受限扫描：最近 ≤50 会话文件、单文件 ≤4MiB；usage 从 assistant 消息累加；tools 计数含错误归因（tool_call_id→工具名映射按文件隔离）；daily 按会话文件 mtime 归天，自实现 civil_from_days 免 chrono）。
- [x] `/api/stats/trend?days=14`：窗口逐日补零（1..=90 钳位）。
- [x] c5-ui Statistics 页面（`#/stats` hash 路由 + 侧栏「统计」）：指标卡 + 14 天趋势 **SVG 自绘柱状图**（零新依赖）+ 工具 TOP 表 + 模型用量表，四语言。
- [x] 测试 3 项（聚合/趋势补零/日期换算，与 Python datetime 交叉验证）。

### 🟡 P1-4：/diff /fresh ✅（`crates/cli`）

- [x] `/diff [--staged] [ref]`：git diff 工作区/暂存区/指定 ref，>300 行截断；非 git 仓库明确提示。
- [x] `/fresh`：全新会话（新 id + 空上下文，保留模型/模式/历史记录），与 /resume 对称实现（含 session_label 同步）。
- [x] i18n 四语言（session.fresh / diff.title 等 4 键）。

### 🟡 P1-5：plan-mode ✅（`crates/core` + `config` + `prompt` + cli + server + skills）

- [x] `Mode::Plan`：与 architect 同构的写保护（仅 plans/*.md 可写，硬拒绝不可绕过含 yolo；执行类命令需确认）。
- [x] `prompts/system-plan.md`：规划工作流提示词（目标/步骤/验收/风险 + 产出后请求批准）。
- [x] `/plan` 命令 + `/mode plan` + Tab 补全；server/skills/rpc 全模式解析点同步。
- [x] 测试 4 项（非 plans 写拒绝 / plans 写放行 / 执行需确认 / yolo 下只读仍生效）。

### 🟡 P1-6：心智模型 ✅（`crates/memory` + cli 装配）

- [x] `mental_models.rs`：内置 6 条仓库级种子（先读 AGENTS.md/先跑测试/最小改动/证据优先/可维护性/小步验证）+ 自定义 seeds 路径（JSON/markdown）+ 项目 mental_models.md 积累；合并注入 `<mental_models>` 段（max_inject_chars 2000，尾部最新、行首对齐截断）。
- [x] `add_mental_model`（时间戳条目）+ `consolidate_mental_models`（LLM 去重/分组/提炼，独立提示词，复用 consolidate 流式模式）。
- [x] cli/rpc 装配：LocalMemoryStore 链式配置；ConsolidateHook 任务成功后追加心智模型合并。
- [x] 测试 15 项（种子加载/合并顺序/截断/注入/追加/clear/提示词形状）。

### 已知取舍

- DAP：单会话句柄（v1 无多会话 id 路由）；反向请求自动回错误；launch args 透传（target 由模型给）。
- review：评审结果仅宿主侧输出，不进模型上下文（避免污染）；引号/特殊路径 diff 按 git 格式双分支解析。
- 仪表盘：sessions.total = 实际扫描文件数（≤50 上限）；无会话内时间戳，daily 按文件 mtime 归天。
- plan-mode：approved-plan 自动化（write xd://propose → 批准解锁）留 P2；v1 为模式 + 人工批准。
- 心智模型：内置种子恒注入（无关闭开关）；截断无显式标记。

## 第十二轮（2026-08-02）：P2 第一波（ssh / browser / snapcompact / 向量记忆探测）✅

对标 `oh-my-pi-architecture-benchmark.md` §五 P2。全 workspace 61 套件零失败。

### 🟢 P2-1：ssh 工具 ✅（`crates/tools/src/ssh.rs` ~980 行 + 接线）

- [x] OpenSSH 子集解析器：Host/HostName/Port/User/IdentityFile/ProxyJump、大小写不敏感、`Host *`/`?` 通配 + `!` 否定、引号值、行内注释、`~` 展开、精确匹配优先于通配（刻意简化并文档注明）；找不到主机报错并列可用别名。
- [x] 动作：connect（解析+校验返回参数，不建连）/ exec（`ssh -T -o BatchMode=yes`，-p/-l/-i/-J/-o 全部显式钉死防 ssh 自身配置覆盖；120s 超时、双流各 200KiB 上限 + UTF-8 边界截断、exit code 标注）/ list / disconnect（v1 无长连接占位）。
- [x] 测试 19 项（纯函数，无真实连接）+ 1 个 #[ignore] 冒烟。
- [x] cli 接线：`[tools.enabled] ssh` 可选组 + `SSH_PROMPT_SECTION` 注入（改名避免与 github 的 PROMPT_SECTION 冲突——修子代理 3 个编译错误：const fn str match、split 生命周期、名字冲突）。

### 🟢 P2-2：browser 工具 ✅（新 crate `crates/browser` 5 文件）

- [x] launcher：PATH 探测 chromium/chrome（BROWSER_PATH 覆盖）、`--headless=new --remote-debugging-port=0`、临时 user-data-dir、stderr 解析 DevTools ws 端点；close 时 SIGTERM→SIGKILL 整进程组（nix signal + tempfile，二者已在既有 Cargo.lock）。
- [x] cdp：tokio-tungstenite WebSocket + JSON-RPC id 配对（HashMap<u64, oneshot>）、事件分流、10s 超时。
- [x] 7 动作：navigate（load 事件水位过滤）/ evaluate（returnByValue+awaitPromise）/ screenshot（PNG 落盘 .gyre/artifacts）/ click / text（el.value + input/change 事件，兼容 React）/ scroll / close。
- [x] cli 接线：`[tools.enabled] browser` + PROMPT_SECTION 注入。

### 🟢 P2-3：snapcompact 图像化压缩 ✅（新 crate `crates/snapcompact` + context/config/agent/cli 贯通）

- [x] **渲染 crate**（20 测试，clippy 0 警告）：`ab_glyph` 栅格化内嵌 Liberation Mono（SIL 兼容自由许可，320KB 复制进资产）+ `image` PNG 编码；8×16 单元格（scale 按 `h_scale_factor = scale/height_unscaled` 反推，advance 精确 8px）；1568² 帧、白底黑字、行盒 16px 贴内容高度；CJK/未知字形折叠 `?`；ANSI 剥离（CSI/OSC）、`\r\n` 折叠、tab→4 空格、控制字符剔除、超长行截断、空页过滤、确定性输出。
- [x] **预算常量**（移植 snapcompact.ts）：DEFAULT_MAX_FRAMES 80、FRAME_TOKEN_ESTIMATE 5024、FRAME_DATA_BYTES_BUDGET 3MB（→17 帧护栏）、provider 图像预算（anthropic 90 / openai 200 / google 200 / 未知 5）。
- [x] **Compactor::snapcompact**（context，5 集成测试）：切分逻辑与 summarize 同构（ToolResult 边界保护）；帧超预算保首尾丢中间（omitted 计数入说明文本）；摘要消息 = Text 说明 + `UserContent::Image` 块（user 角色——**关键发现**：`UserContent::Image` 已存在且全 provider transform 已序列化（anthropic base64 block / openai+glm image_url / deepseek 占位），无需动 ContentBlock/LLM 层）。像素级测试：解码帧 → 帧高 = 行数×16、每行行盒有墨迹。
- [x] **配置**：`[compaction] backend = "summarize"|"snapcompact"` + `vision_models`（`*`/`?` 通配清单，空 = 全启用）+ `max_frames`；`wildcard_match` + `parse_compaction_backend`（未知值回退 summarize + warn）放 config crate（4 测试）。
- [x] **agent 决策点**：AgentBuilder/Agent 加 `compaction_backend` + `compaction_max_frames`（默认 Summarize/80）；run_loop 压缩链第二级按 backend 选 `CompactionStrategy::Summarize | Snapcompact`；cli/rpc 装配按 `vision_models` 通配匹配当前 model.id 决定是否启用（每次 /model 重建重判，不匹配回退 summarize 并提示）。
- [x] 验收对齐：长会话压缩成本从「LLM summarize 调用」降为「本地确定性渲染」；图像信息保真度由帧内行结构测试保障；真视觉模型回读 eval 需 API key，作为后续 opt-in（`GYRE_VISION_EVAL=1` 预留，见取舍）。

### 🟢 P2-4：向量记忆 —— 探测完成，实施排第二波 ✅（`docs/vector-memory-eval.md`）

- [x] 精读 mnemopi：fastembed JS + onnxruntime-node、`memory_embeddings` 表（embedding_json + model 对齐）、混合打分 vecWeight 0.5 / fts 0.3 / importance 0.2、`<memories>` 注入格式（分数前缀 bullet）。
- [x] 4 方案对比：candle（纯 Rust 慢）/ ort 直驱（快但自建 tokenizer）/ BM25 增强（零依赖非语义）/ **fastembed-rs v5（推荐**：AllMiniLML6V2Q int8 ~24MB、与 JS 栈同族；注意其默认后端是 ort 2.0.0-rc）。
- [x] 推荐双层：`crates/memory` 内 `vec_memory.rs` + feature `vec-embed` 门控 fastembed；L1 确定性随机投影桩离线保底；融合侵入 `recall_in`，注入格式零改动；Phase0-5 实施计划 + 无网测试策略（固定向量桩）。

### 已知取舍

- ssh：无长连接/无 sshfs（v1）；精确匹配优先于 OpenSSH 文件顺序先得（文档注明）。
- browser：v1 单标签页、无网络拦截、无 stealth；SIGKILL 兜底可能留僵尸（进程组击杀）。
- snapcompact：CJK 折叠 `?`（中文会话可读性损失，内嵌中文字体排 P2 后续）；v1 单形状 8on16（无 OMP 的多变体/foveation/双栏/stopword dimming）；真实视觉模型回读 eval 需 key（像素级测试兜底）；Hybrid 后端（帧+LLM 摘要）未做（config 只收两值）。
- 向量记忆：探测结论 → 实施为 P2 第二波（feature 门控 + 投影桩离线保底）。

### 下一波候选

- P2 第二波：向量记忆实施（按 vector-memory-eval.md Phase0-5）、远程压缩（P2-E：OpenAI/Codex 原生 compaction 端点）、hybrid 压缩后端。
- P3：TTS/STT、tiny 本地模型、computer-use、marketplace、agent registry、launch broker、autoresearch。
- 内部重构（对标报告 §六）：agent/lib.rs 6400 行拆分、criterion 基准门、文档体系、配置 schema 驱动。

## 第十三轮（2026-08-02）：P2 第二波（向量记忆实施 + 远程压缩）✅

对标 `oh-my-pi-architecture-benchmark.md` §五 P2 剩余两项。全 workspace 61 套件零失败 + vec-embed feature 独立验证。

### 🟢 P2-5：向量记忆实施 ✅（`crates/memory/src/vec_memory.rs` 552 行 + structured 融合）

- [x] **双层嵌入**：`Embedder` trait（embed/dim）+ `StubEmbedder`（固定种子确定性）+ `ProjectionEmbedder`（char n-gram + 固定种子随机投影，L1 离线保底）+ `#[cfg(feature="vec-embed")]` `FastembedEmbedder`（fastembed-rs 5.17.4，AllMiniLML6V2Q int8，懒加载 + 失败自动降级 L1，永不 panic）。
- [x] **旁路存储** `vecs.jsonl`（`{id, model, dim, data(base64 f32 LE)}`）：upsert/delete/clear/replace_all/get_many/all；records.jsonl 为唯一事实源、向量仅加速件；模型变更（vecs.jsonl 的 model 与当前不符）清空重嵌；缺失向量跳过 dense 不报错。
- [x] **融合**：`RecallOptions.vec_weight: f64`（默认 0.5，mnemopi 对齐）；`score = vec_weight·max(0,cosine) + fts + importance + temporal`；`vec_weight=0`/无向量严格回退原公式（逐项相等测试）。`StructuredMemoryStore::with_embedder` 挂载，默认无 → 行为完全兼容。
- [x] 13 新测试（Stub/Projection 确定性：近义排前、dense 越词法、模型重嵌、幂等 upsert/delete、损坏行跳过、缺失向量不 panic）；真模型 `#[ignore]` + `GYRE_VEC_INTEGRATION=1`。
- [x] 依赖：root `fastembed = { version = "5", default-features = false, features = [ort-download-binaries-rustls-tls, hf-hub-rustls-tls] }`；memory feature `vec-embed`（默认关，主分支编译零影响）；`cargo check --features vec-embed` 实编译通过。
- [ ] CLI/server 接线（`cfg.memory.enabled → StructuredMemoryStore + LazyEmbedder`）：留 P2 后续——现状 structured 仍未接线，接线时一并决定与 LocalMemoryStore 的取舍。

### 🟢 P2-E：远程压缩 ✅（remoteEndpoint 模式，`LlmSummaryProvider` + config + 5 装配点）

- [x] `[compaction] remote_endpoint: Option<String>`（默认 None）：设置后摘要生成先 POST 远程，失败（非 2xx/超时/空摘要）回退本地 LLM。
- [x] 两种 wire 格式：路径以 `/chat/completions` 结尾 → OpenAI 兼容 `{model, messages, stream:false}` 读 `choices[0].message.content`（覆盖 llama.cpp/vLLM 自托管）；否则自定义 `{systemPrompt, prompt}` → `{summary}`。10s 超时。
- [x] 装配：cli/main.rs（3 处）+ rpc.rs + server/lib.rs 共 5 处 `LlmSummaryProvider::new` 传 `cfg.compaction.remote_endpoint.clone()`。
- [x] 测试：本地 mock HTTP（std TcpListener 零新依赖）：chat/completions 格式、自定义格式、500 回退、空摘要回退 + config TOML 反序列化，共 5 新测试。
- [x] **v1 排除**：provider 原生 `/responses/compact`（OpenAI/Codex）——Gyre 无 Responses API 支持；OMP 的 preserveData replacementHistory 语义留后续。
- [x] 主控修复：mock helper `body: &str` 借用逃逸进线程 → to_owned（1 处）；VectorStore 方法 dead_code → lib.rs 导出 VectorStore/VecEntry（2 处警告清零）。

### 已知取舍

- 向量记忆：L2 需构建期下载 ORT 预编译库（feature 门控缺省关）；MiniLM 英文为主、中文语义弱（备选 bge-small-zh 留配置化）；`vec_weight` 默认 0.5 可调。
- 远程压缩：自定义格式要求远端返回 `{summary}` 字段；无鉴权头配置（需要时走 base_url 前缀或代理，留后续）；本地回退依赖现有 LLM 路径，远程与本地结果可能风格不一致（OMP 同款取舍）。

### 下一波候选

- P3：TTS/STT、tiny 本地模型、computer-use、marketplace、agent registry、launch broker、autoresearch。
- P2 收尾：memory 接线决策（structured vs local 二选一或双轨）、hybrid 压缩后端（帧+LLM 摘要）、原生 /responses/compact、bge-small-zh 模型配置化。
- 内部重构（对标报告 §六）：agent/lib.rs 6400 行拆分、criterion 基准门、文档体系、配置 schema 驱动。

## 第十四轮（2026-08-02）：工程面（agent 单体拆分 + 基准门）✅

对标报告 §九 结语明确的两项内部重构。功能面 P0-P3 已全补齐，本轮纯工程。

### 🟢 agent/lib.rs 单体拆分 ✅（6448 → 4794 + 新 engine.rs 1662）

- [x] 按 §六.1 方案拆出 **`src/engine.rs`（执行循环 + 工具并发域）**：run_loop（970 行级生成器）、PendingTask/run_pending_task/schedule_and_run/run_batch/poll_and_run/record_run_end/persist_interrupted。lib.rs 保留类型层（Agent/AgentBuilder/GoalState/KeyRing/RuntimeOverrides）+ tests（3800 行安全网）+ 顶层装配。
- [x] 纯机械移动 + 模块边界修正：模块名 `engine`（避开与 `run_loop` 函数同名冲突）；`use super::*` 继承类型层；顶层 fn/struct/async fn 统一 `pub(crate)` + 显式 re-export（glob 不传递）；PendingTask 字段 `pub(crate)`（tests 构造字面量）。
- [x] 契约不变：**61 套件全绿**（agent 78 测试含拆分前后行为等价）；无新 clippy error（基线 ~170 条文档/风格警告维持原状，非本次引入）。
- [ ] 二次拆分候选（steering 三通道 / injections 注入点 / harmony 已有）：engine.rs 仍 1662 行，下一轮可继续按同一模式。

### 🟢 criterion 基准门 ✅（对标 §六.2，criterion 0.5 + html_reports）

| bench | 文件 | 实测基线（本机 Ryzen 3700X） |
|---|---|---|
| grep/glob/行匹配吞吐 | crates/tools/benches/search.rs | glob 200 文件 72.7µs；10k 行匹配 23.4ms |
| tiktoken 计数 | crates/core/benches/tokenize.rs | 8KB cl100k 1.42ms；gpt-4o 1.41ms |
| 压缩耗时 | crates/context/benches/compaction.rs | summarize 序列化 25.8µs；snapcompact 90 行帧 5.13ms |
| TTSR 匹配延迟 | crates/agent/benches/ttsr.rs | 80 块增量 289.8µs；工具调用 7.7µs |

- [x] 全部确定性夹具（tempfile 小仓库 / 内存行集 / 固定消息流），无外部依赖。
- [x] **踩坑**：criterion 0.5 默认 test harness 下 `running 0 tests`（criterion_main 的 main 被 harness 吞掉）→ 每个 bench 需 `[[bench]] harness = false`（4 处）。
- [x] 首次基线已立；CI 回归门（p99 >10% 报警）可基于 target/criterion 历史数据后续接。

### 已知取舍

- 拆分只做第一刀（engine.rs），steering/injections 二次拆分留下一轮（同模式、低风险）。
- 基线数字是首测，噪声大（单机无锁频）；回归门阈值应等 2-3 次运行稳定后定。

### 下一波候选

- 内部重构续：engine.rs 二次拆分、文档体系升级（每 crate 架构 doc + DEVELOPING.md）、配置 schema 驱动（schemars → /settings 面板 + config set 校验）。
- 产品面：P3 按需（TTS/STT、tiny 本地模型、computer-use、marketplace、agent registry、launch broker、autoresearch）；P2 收尾（memory 接线决策、hybrid 压缩、原生 /responses/compact）。

### 补记：P2-5 向量记忆 CLI/server 接线 ✅（memory backend 切换）

- [x] `[memory] backend = "local" | "structured"`（`MemoryBackend`，kebab-case，缺省 local 向后兼容；非法值反序列化报错）。config.example.toml 同步说明。
- [x] 三处装配（cli main / rpc / server）按 backend 分支：
  - **local**：既有 LocalMemoryStore + 心智模型 + ConsolidateHook（LLM 合并，仅 local 生效，structured 逐条积累无合并语义）。
  - **structured**：`StructuredMemoryStore::new(&cwd).with_mental_models_config(..).with_embedder(default_embedder())`——`default_embedder()` 工厂按 `vec-embed` feature 选 L2 fastembed（懒加载+降级 L1）或 L1 确定性投影（384 维），装配层零 feature 感知。
- [x] StructuredMemoryStore 补心智模型支持（P1-6 能力不丢）：`with_mental_models_config` + `mental_models()`（seeds 前置 + 项目 mental_models.md），`summary()` 合并格式对齐 LocalMemoryStore（`<mental_models>` 段）；未配置时行为与旧版逐字一致。
- [x] 顺带修既有 bug：`auto_consolidate` 的 `#[serde(default)]` 实际反序列化缺省 false 与 `MemoryConfig::default()` true 不一致 → 显式 `default_auto_consolidate()` = true（新测试暴露）。
- [x] 验证：config 51（+1）、memory 34（+1，vec-embed feature 下同）；**workspace 61 套件零失败**；clippy 四 crate 零 error；RPC 冒烟：structured 配置 + 项目 `.agent/config.toml` 下 ping 应答 pong。
- [ ] 已知取舍：server 装配为简化版（无 MentalModelsConfig，与既有 local 分支同）；main 路径正常任务装配经编译级等价验证（RPC 冒烟覆盖 rpc.rs 路径）。
