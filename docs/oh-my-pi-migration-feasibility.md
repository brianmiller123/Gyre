# oh-my-pi 历史功能模块迁移可行性分析报告

> 调研对象：`third/oh-my-pi`（badlogic/pi-mono fork，MIT，v17.2.3，Bun+TS 引擎 + Rust N-API 原生层）
> 调研日期：2026-08-02
> 方法：2 个并行 scout 精读 `packages/*`（15 个 TS 包，~350–370k LOC）与 `crates/*`（9 个 Rust crate + vendor，~185k LOC）、`python/*`（~39k LOC）、Bazel/Cargo 双构建轨；对照 Gyre 全部 31 个 crate 源码与本仓库既有调研文档
> （[`oh-my-pi-feature-analysis.md`](oh-my-pi-feature-analysis.md)、[`oh-my-pi-architecture-benchmark.md`](oh-my-pi-architecture-benchmark.md)、[`agent-loop-improvement-plan.md`](agent-loop-improvement-plan.md) 第 1–14 轮、[`vector-memory-eval.md`](vector-memory-eval.md)）。
> 文中 omp 侧路径均相对 `third/oh-my-pi/`。

---

## 〇、结论速览

1. **现状**：Gyre 与 oh-my-pi 是"同源对照移植"关系，14 轮改进已把**循环引擎层与大部分产品层**落地（TTSR/conflict:///magic keywords/web_search 骨架/Advisor/LSP 写操作/resolve 暂存/pr:// 体系/规则导入/Provider fallback+轮换/Collab 全协议+guest+host+transcript/snapcompact/ssh/browser/eval 内核/minimizer/DAP//review//stats//fresh/plan-mode/心智模型/向量记忆/远程压缩/NDJSON RPC/ACP）。**本轮报告聚焦"剩余未迁移模块"的迁移可行性**。
2. **迁移可行性总体结论：高**。omp 的 Rust 层刻意设计为 napi-free（除 pi-natives 一个 cdylib 外，pi-iso/pi-voice/pi-walker 均明文声明"不携带 N-API 依赖"），意味着**逻辑代码可近乎逐字搬运**；但三个大块**明确不建议迁移**：TUI 差分渲染引擎（24k TS，与 Gyre rustyline+Web 双前端路线冲突）、TS 动态插件/市场（Rust 无模块动态加载，inventory 编译期注册已是等价替代）、内嵌 bash（brush + 47 个 uutils ≈ 88k vendor LOC，是"Windows 无 bash"的税，Gyre 走 OS shell 天然等价）。
3. **推荐迁移的剩余清单（按性价比排序）**：
   - 🔴 快赢（各 ≤3 人日）：checkpoint/rewind 会话回卷、todo 工具、`.ipynb` 读写往返、web_search 抽取器扩充、omp-rpc 类型化 Python 客户端落地
   - 🟡 中程（各 3–8 人日）：pi-walker 并行遍历缓存整合、web_search provider 链扩充、模型目录数据化、memory 工具面（learn/reflect/retain/recall）、hooks 事件总线 MVP、TTS 云端后端
   - 🟢 大件（各 10–20 人日）：security_scan v1、computer-use（Linux X11 先行）、pi-voice 音频采集/播放、pi-ast 60 grammar 扩展、pi-iso PAL 后端矩阵
4. **最大前置冲突**：tree-sitter 版本分裂（Gyre `crates/ast` 锁 0.24 对齐 ast-grep-core facade；omp pi-ast 用 0.25 + 60 grammars）。迁移 grammar 集前必须统一版本线。
5. **工作量总量**：推荐迁移项合计约 **90–120 人日**（不含明确排除项）；全部按"新 crate/既有 crate 新模块 + 装配点一行注册 + 单测锁定契约"的既有模式落地。

---

## 一、核心功能逻辑剖析（剩余未迁移模块）

按 omp 侧源码结构逐模块剖析：做什么、怎么做（代码级）、Gyre 侧对应状态。

### 1.1 pi-walker：并行目录遍历 + 扫描缓存（crates/pi-walker，6,182 行 + 586 测试）

- **逻辑**：`ignore::WalkBuilder` 并行遍历（rayon）之上叠加**全量扫描条目缓存**（`cache.rs`：DashMap，键 = (root 规范化, include_hidden, use_gitignore, skip_node_modules, detail 元数据档位)；TTL 默认 1000ms；**空结果快速重检**（cache_age ≥ 200ms 时 force_rescan 重试一次，防陈旧负结果）；`FS_SCAN_CACHE_MAX_ENTRIES=16` 最旧优先淘汰）。`FileType/WalkDetail/WalkOrder` 枚举 + glob 辅助 + 心跳取消支持；被 glob/grep/AST/shell builtins 消费。
- **关键设计**：缓存存**遍历条目**而非工具结果；工具侧过滤在取回后做；**写路径必须显式失效**（omp 在 write/hashline/patch/replace 四处调 `invalidateFsScanAfterWrite/Delete/Rename`）；`.git` 恒剪除；node_modules 仅在显式请求时纳入。
- **Gyre 状态**：`crates/search/src/fs_cache.rs`（234 行）已移植同构子集（TTL 1000ms + invalidate + 空结果重检），但**无并行遍历、无 mtime/size 全量档位、无 fuzzyFind、无 AST 文件发现消费**；写工具尚未接失效钩子（现靠短 TTL 兜底）。
- **迁移判定**：**值得补**。把 Gyre 的 fs_cache 扩为 pi-walker 语义（并行 + 双档位 + 写路径失效），grep/glob/ast 共享，收益是重复扫描消除 + 一致性。约 3–5 人日。

### 1.2 pi-shell：内嵌 bash（brush）+ 47 个 uutils + 输出最小化器（41,523 行 + 88k vendor）

- **逻辑**：brush-core（reubeno/brush，26,352 行 bash 解释器：expansion/completion/jobs/prompt/history）为引擎，47 个 uutils/coreutils 0.8.0 经 `pi-uutils-ctx`（446 行 thread-local stdio/cwd/env/cancel shim）修补后作为**内置命令**进程内执行（`coreutils.rs::run_uutil`），另含 jaq（jq 克隆）、pi-uu-grep（ripgrep 系）、pi-uu-diff（similar 系）、moreutils（sponge/ts/ifuse/isutf8/combine）。**输出最小化器**：~30KB 引擎 + 24 类过滤器（~65 个 TOML 工具定义：git/gh/glab/cargo/jvm(121.8KB)/ruby/python/go/js_tools/docker/dotnet/bun/cpp/cloud/system/lint/generic），按命令类型压缩 stdout/stderr 后再交给上层。
- **设计动机**：Windows 无 bash、管道进程内零 fork 开销、输出可编程裁剪。代价：88k vendor 行 + patch 维护面。
- **Gyre 状态**：`run_command` 走 `/bin/sh -c` / `cmd /C` 子进程（`crates/tools/src/shell.rs`），**天然等价**；minimizer（`crates/tools/src/minimizer.rs`，~1100 行）已有 5 个过滤器（git status/diff/log、cargo、python）且挂在 run_command 输出路径。
- **迁移判定**：**不迁移整壳**（brush+uutils 是环境税不是功能）。**可借鉴**：minimizer 按 pi-shell 的 TOML 定义扩充过滤器（机械活，每个配 fixture 单测）；pi-uutils-ctx 的 catch_unwind 纪律（边界恢复）对 Gyre 工具隔离有参考价值。minimizer 扩充约 4–6 人日。

### 1.3 pi-ast：60 语言 grammar 的 AST 工具包（3,402 行）

- **逻辑**：tree-sitter 0.25 + ~60 个 grammar crate（含 `-next`/`-updated`/`-sg` 包名变体）+ ast-grep-core 0.39：ast-grep 模式检索/重写（`ops.rs`）、结构摘要（`summary.rs`）、句法块范围解析（`block.rs`，供 `replace block N:`）。
- **Gyre 状态**：`crates/ast`（1,100 行）已移植同构能力（tree-sitter 0.24 + 6 语言 grammar：rust/python/js/ts/go + ast-grep-core facade 0.24），`replace_block/ast_search/ast_rewrite` 工具已接线。
- **迁移判定**：**扩充 grammar 集值得**（模型在非 Rust 语言上做 AST 操作是常规需求），但**前置是统一 tree-sitter 版本线**（见 §三.1 风险）。60 grammar 全量移植 ≈ 4–6 人日（主要是 Cargo 依赖 + 数据适配 + 测试矩阵），可按语言热度先 10–15 个。

### 1.4 pi-iso：文件系统隔离 PAL（4,047 行）

- **逻辑**：`IsolationBackend` async trait + 8 后端：APFS clonefile / btrfs snapshot / ZFS clone / Linux FICLONE reflink / overlayfs（+fuse-overlayfs 兜底）/ Windows block-clone + ProjFS / `Rcopy`（git worktree / 递归复制）；配套 git-diff 兼容树 diff（`diff.rs`）。供 task 子代理 PAL 写时复制隔离。
- **Gyre 状态**：`crates/iso`（1,273 行）已有 rcopy/worktree 分支隔离（TaskTool 用）；overlayfs/reflink 等高性能后端未做。
- **迁移判定**：**按需补**。Linux 场景 reflink（FICLONE）+ overlayfs 收益最大（大仓库子代理隔离成本显著下降）；Windows ProjFS 复杂度高可缓。约 10–15 人日（平台矩阵是主要成本）。

### 1.5 pi-voice：语音引擎（1,197 行，napi-free by design）

- **逻辑**：miniaudio 采集/播放流（`audio.rs`，maudio crate；darwin 走 bindgen 重生成绑定）+ WebRTC 0.17 + Opus 实时通话对端（`live.rs`，create_offer/answer 跑在调用方 tokio runtime）。pi-natives 的 `audio.rs/live.rs` 只是薄 `#[napi]` 适配。
- **Gyre 状态**：无任何语音能力（P3 列表项）。
- **迁移判定**：**音频采集/播放先行**（6–10 人日，maudio + flume 直接可用）；**实时 WebRTC 通话后置**（10–15 人日，webrtc 0.17 编译重、生态停滞，价值取决于产品定位——CLI agent 的语音会话是差异化但非刚需）。TTS 见 1.12。

### 1.6 web_search：23 provider 链 + 74 站点抽取器

- **逻辑**：懒加载 provider 注册表 + 顺序回退（首个可渲染响应胜出；免 key 6 个：duckduckgo/searxng 等）；query 指令解析器 851 行（`site:`/`inurl:`/`after:` 等，宽松后过滤）；**站点感知抽取器**把 arxiv/npm/crates.io/github/reddit/mdn/stackoverflow 等转结构 markdown（锚点保留）；`formatForLLM` 输出 answer+Sources/Citations/Related（240 字符片段上限）。⚠ 文档 `docs/tools/web_search.md` 已过期（25 vs 源码 23 provider），移植以源码 `SEARCH_PROVIDER_OPTIONS` 为准。
- **Gyre 状态**：`crates/tools/src/web_search.rs` 已有 **DDG 免 key + 可选 searxng** 顺序回退 + arxiv/crates.io/npm/github 抽取。
- **迁移判定**：**继续补 provider 与抽取器**。provider 每个 0.5–1 人日（trait 抽象 + 懒加载，omp 已证明 API 易变）；抽取器每个 0.25–0.5 人日（易碎品，**必须配 fixture 单测**）。先 3–6 个 provider + 10–20 个抽取器 ≈ 7–11 人日。

### 1.7 catalog：模型目录（2.1MB models.json + 14k TS）

- **逻辑**：`models.json` 全量模型数据库（~10.5 万行）：provider 家族分类/identity 判定/兼容别名/思考标记/variant 折叠/宿主发现（codex/gemini/cursor/devin 等）。`model-manager`/`model-cache`/`model-thinking` 消费。
- **Gyre 状态**：无目录；模型元数据散在 config + tiktoken 映射，vision/thinking 能力靠手工标记（snapcompact 的 `vision_models` 通配清单即手工维护）。
- **迁移判定**：**数据化值得**（vision/thinking/token 上限自动判定，消除 snapcompact/压缩决策的手工配置漂移），但**不必照搬 2.1MB**——提炼 Gyre 实际 provider（openai/anthropic/deepseek/glm）的高频模型字段即可。约 4–6 人日。

### 1.8 checkpoint / rewind：会话回卷（深度分析，2026-08-02 代码级侦察）

> ⚠ 注意：omp 的 `docs/tools/checkpoint.md`/`rewind.md` 已过期（描述的是 `safeCount`/`replaceMessages(prefix)`/`#advisorRuntime.reset()` 的旧实现），以下以**源码**为准（`packages/coding-agent/src/tools/checkpoint.ts` 全文件、`session/agent-session.ts`、`session/session-manager.ts`、`session/checkpoint-entries.ts`、`session/session-context.ts`）。

- **功能本质**：探索式工作流开 `checkpoint` → 探索 N 轮 → `rewind(report)` 在 turn_end 把会话叶移回检查点、追加分支摘要 + 隐藏报告、用**从分支重建的上下文**整体替换模型上下文、重置 advisor 运行时；yield 守卫强制模型 yield 前完成回卷。单活跃检查点、无堆栈、不碰文件系统/git。
- **工具层无副作用**：`CheckpointTool` 仅校验（已激活 → `ToolError`）并记录三字段 `{checkpointMessageCount, checkpointEntryId, startedAt}`（`message_end` 在 toolResult 条目持久化**之后**捕获，entryId = 该 toolResult 条目自身）；`RewindTool` 仅校验（无检查点/重复回卷/空报告）并返回 `{report, rewound:true}`，**副作用全部延迟到 turn_end**。
- **turn_end 应用序**（`#applyRewind`，agent-session.ts:6563-6609）：①`branchWithSummary(checkpointEntryId, report)`（条目缺失 → 捕获异常回退 root：`branchWithSummary(null, …)`）——先 `#setLeaf` 移叶再追加 `branch_summary` 条目；②追加隐藏 `custom_message(rewind-report)`（`display:false`，模板含 report）；③`replaceMessages`：从分支重建上下文整体替换（LLM 侧 `branchSummary→user`、`rewind-report→developer`）；④`advisors.resetSessionState({preserveCost:true})`；⑤`#todo.syncFromBranch()`；⑥仅 `openai-codex-responses` 模型关闭 provider 会话；⑦清状态。
- **yield 守卫**（`#enforceRewindBeforeYield`，6524-6541）：检查点活跃且无 pending rewind 且文本停止 → 追加 developer `<system-warning>` + 调度 `agent.continue()` + `agent_end(willContinue:true)`；错误/中止/压缩接管路径跳过。
- **可恢复性**：全部状态可从会话树重建（rehydrate 扫描分支：成功 checkpoint toolResult 条目 / rewind-report 条目），resume/切换/分支安全；压缩对检查点**无感知但安全**——条目永不删除，被切弃的压缩条目孤儿化即可。
- **边界矩阵（21 条，代码验证）**：单活跃；重复回卷拒绝；中止（abort）跳过本次 turn_end 但 pending 保留至下次；xdev `write xd://rewind` 与原生工具双源归一；会话切换快照/回滚；子代理按 `taskDepth===0 || 显式白名单` 门控；回卷 toolResult 防双写（`#rewoundToolResultIds`）；进程死亡 → resume 后 rehydrate 恢复 pending。
- **Gyre 基座（侦察确认，`crates/context` + `crates/agent`）——比预期完备得多**：
  - 会话树同构：`ContextManager` + `SessionNode{id,parent_id}` 森林 + `active_leaf` 写指针（lib.rs:36-99）；回卷 = `set_active_leaf(checkpoint_id)`（lib.rs:346-356，下一条 append 自动 fork，被弃尾保留为分支）——**正是 omp 的 branch/resetLeaf 语义**；`switch_branch_with_handoff`（lib.rs:391-463）就是 omp `branchWithSummary` 的移植版（`collect_entries_for_branch_summary` tree.rs:147-191）。
  - 上下文重建：`build_provider_context` + StablePrefix digest（lib.rs:362-391, 828-833）回卷后自动重算稳定前缀——**等价 replaceMessages 前缀语义，无需显式 replace 原语**。
  - 隐藏消息：`context.append(user_text)` 即 steering/advisor 注入同款通道。
  - advisor 无状态（快照喂入，advisor/src/lib.rs:120-174），reset 只需清 `EmissionGuard` 去重集（guard.rs:9-12，需加 `reset()`）；goals 按 token 记账不引用消息索引（无悬垂指针）。
  - 压缩就地重写先例：`compact_active_path`（lib.rs:110-180）+ `persist_full`（persistence.rs:83-97）证明会话中途改史是被制裁的操作。
- **Gyre 需新增（5 点）**：①检查点状态捕获：工具结果 append 后记录 `active_leaf()` NodeId（engine.rs:1455-1458 位点）+ 可选语义标记消息（`[checkpoint:…]`，TTSR `[ttsr-injection:…]` 恢复先例）；②pending-rewind 状态挂在**长生命周期 Agent**（server 会话常驻；CLI `/session` 重建靠 rehydrate 兜底）；③turn_end 应用 + 守卫：接缝是 **stop-boundary 块（engine.rs:943-1076）与 Done 前路径**，而非 `fire_on_turn_end`（13 个 TurnEnd 位点只有 3 个走它，truncated-continue/TTSR abort/deadline/max_turns 均绕过）；④`EmissionGuard::reset()`；⑤子代理门控：sub_tools 注册表不注册 checkpoint/rewind（天然隔离；`TaskTool.depth: Arc<AtomicUsize>` 已有，task_tool.rs:66）。
- **Gyre 特有风险（omp 没有）**：**压缩会删节点**——omp 条目永不删除，检查点条目必然存在；Gyre `compact_active_path` 会重写/删除节点，检查点与 rewind 之间发生压缩可能使检查点 NodeId 失效。处置：①compactor 增加 pinned 节点集参数（保护检查点节点）；②回卷时按 `checkpoint` 语义回退扫描（找最近成功 checkpoint toolResult，omp rehydrate 同款），最后回退 root。双保险。
- **无迁移负担项**：Gyre 4 个 provider 全为无状态 chat 接口（anthropic messages / openai·deepseek·glm chat/completions，crates/llm 源码确认）——omp 的 codex provider-session 关闭逻辑**无需移植**；omp 的 bash withBranchTransition 深克隆（in-flight bash 挂旧分支）在 Gyre 无 in-flight 工具回写会话树的对应物，也无需移植。
- **迁移判定**：**高性价比，可移植**。4–6 人日（工具对 1 + 状态捕获与 turn_end 应用 1.5 + 守卫 0.5 + 压缩 pinning/语义回退 0.5–1 + rehydrate 0.5–1 + 9 个集成测试 1，以 omp 的 `agent-session-checkpoint-rewind-branch.test.ts` 为行为契约移植）。前置重构（engine.rs 二次拆分）**不是硬依赖**——守卫可先落在 stop-boundary 现有块内。

### 1.9 todo 工具（`coding-agent/src/tools/todo.ts`）

- **逻辑**：单 op 判别器（init/start/done/drop/rm/append/view）；**单活跃任务不变量**（多 in_progress 只留第一个，无活跃时自动提升首个 pending）；错误原子性（任何引用错误**丢弃整个变更**，防止重试撞"已存在"）；`storage: "session"|"memory"`；失败注入隐藏下轮提醒；子代理不继承（prewalk 例外）。
- **Gyre 状态**：无 todo 工具（当前无任务列表状态机）。
- **迁移判定**：**直接照搬**，约 2 人日（状态机 + 工具 + 会话持久化 + REPL `/todo`）。与 harness 侧的 todo 语义一致可降低模型迁移成本。

### 1.10 notebook：`.ipynb` 文件读写往返（`coding-agent/src/edit/notebook.ts`）

- **逻辑**：**只做文件转换不做执行**——read 把 `.ipynb` 渲染为可编辑文本（`# %% [code] cell:N` / `[markdown]` / `[raw]` 标记），编辑/write 经 `serializeEditedNotebookText` 往返回 nbformat 4.5 JSON；`cell:N` 引用既有单元则保留原 metadata 克隆，否则新建；空文件起点空 notebook；源码数组按换行切分（尾换行单条、无尾换行单条、空源空数组）。内核执行走 eval 工具，两者互不耦合。
- **Gyre 状态**：eval 内核已移植（`crates/eval`，NDJSON + 环回桥），**但 read/write 无 ipynb 视图**。
- **迁移判定**：**值得**（数据科学场景 read 一个 ipynb 就能编辑执行闭环）。2–3 人日（纯 serde_json + 标记渲染/往返 + 单测矩阵：metadata 保留/新单元/非法输入错误面）。

### 1.11 hooks 事件总线（`src/extensibility/hooks/*`，现已被 extension runner 吸收）

- **逻辑**：事件面三组——会话（session_start/before_switch/before_branch/before_compact/compact/tree/shutdown）、循环（context/before_agent_start/turn_start/end/auto_compaction/ttsr_triggered/todo_reminder）、**工具前后**（`tool_call` 可返回 `{block, reason, input}` 替换执行参数；`tool_result` 可改写 content/isError）。模块 default-export 工厂 `pi.on(...)`。当前实现已并入 extension runner（`ExtensionToolWrapper`）。
- **Gyre 状态**：`crates/tools/src/intercept.rs` 只有 run_command 命令拦截（cat/grep/find 重定向）；approval 网关是配置级；**无通用事件总线**。
- **迁移判定**：**事件总线值得做，TS 模块加载不值得**。Rust 侧等价物：`inventory` 注册的 `Hook` trait（before_tool_call/after_tool_call/on_session_event），装配点注入 run_loop——与既有三通道注入点天然同址。5–8 人日。**前置**：engine.rs 二次拆分（steering/injections 独立模块，见 §五.0），否则 hook 挂接点继续膨胀 lib.rs/engine.rs。

### 1.12 tts 工具（`coding-agent/src/tools/tts.ts`）

- **逻辑**：双后端——本地 Kokoro-82M ONNX（共享 tiny-model worker，q8，输出 PCM16 WAV；`.mp3` 目标自动改 `.wav` 并明说）与 xAI Grok Voice 云端（`<baseURL>/tts`，MP3/WAV，60s 超时）；`providers.tts` 路由 local/xai/auto；文本 1..15000 字符。
- **Gyre 状态**：无。
- **迁移判定**：**云端后端先行**（2–3 人日：HTTP 调用 + 文件落盘 + 配置路由）；**本地 Kokoro 后置**（8–12 人日：ort 推理链 + 模型下载/缓存——Gyre 已有 fastembed 可选 feature 可复用"构建期下载 ORT + 懒加载 + 降级"模式）。P3 定位不变。

### 1.13 security_scan（`coding-agent/src/security/*`）

- **逻辑**：四动作（preflight 解析 git 目标 + OAuth 凭证 + 知识库 + **不可变计划指纹**；start 后台 job 执行存储计划；status/cancel）+ 仓库外项目键控状态存储 + `security://scans` 只读 URI 命名空间。禁用默认。
- **Gyre 状态**：无（只有 prompts/review.md 子代理评审流）。
- **迁移判定**：**v1 缩小版值得**（preflight + 本地扫描 job + status/cancel，不接 OAuth/知识库），10–15 人日；完整版涉及凭证面与后台作业基础设施，与 robomp 同属产品级决策。

### 1.14 computer-use（`pi-natives/src/desktop*.rs` + `keys.rs` + `power.rs` + 文档 computer-use.md）

- **逻辑**：X11（desktop_x11.rs）与 xcap/enigo（mac/win）屏幕捕获 + 输入注入 + 键位解析（keys.rs）；sixel/kitty 图形输出（TUI 用）。文档 306 行定义了动作面。
- **Gyre 状态**：无。
- **迁移判定**：**Linux X11 v1 先行**（10–15 人日：x11rb 捕获 + XTEST 注入 + 工具面）；跨平台全矩阵 25–40 人日且需 mac/win 真机 CI——按需排期。P3 定位不变。

### 1.15 mnemopi 高级检索（packages/mnemopi，~18k）

> **落地状态（2026-08）**：P0 闭环（工具面 recall/retain/reflect + auto-retain + 注入格式 + sleep）与 P1 算法层（同义词/意图权重/MMR/时间解析）已在 `crates/memory` 落地；`recall_in` 支持 `mmr_lambda/use_intent/use_synonyms` 开关，tokenize 升级为 CJK 逐字切分。**MMR 中文调优（2026-08）**：近重复折叠（containment ≥ 0.9 压尾）+ 保序分数缩放 + 中文停用字过滤。reflect 经 LLM 提炼写入 mental_models.md（注入可见）。**Recall 补齐（2026-08）**：queryAsksCurrent（now/latest/recent 类查询抬 temporal 权重到 0.45）、diversifyByCoverage（MMR 前查询词覆盖贪心）、中文时间表达（三天前/上周一/去年等，Gyre 增强，omp 无）。剩余：情景图/三元组/复调（明确不做）、learn 工具（retain 已覆盖写面）。

- **逻辑**：bun:sqlite 内存引擎 + beam 检索（store 38KB/recall 40KB/consolidate 36KB）+ 情景图/标注/模式/shmr/复调检索/真值整合/时间解析/三元组/类型化记忆/二进制向量（vectorIndexTopK/cosineSimilarityPairs/mmr 走 natives）+ MCP server + CLI。
- **Gyre 状态**：`crates/memory` 已有 LocalMemoryStore/StructuredMemoryStore + 心智模型 + vec_memory 双层嵌入（Stub/Projection/Fastembed，vec_weight 0.5 对齐 mnemopi 融合公式）——**beam 的召回融合已移植**；缺 learn/reflect/retain/recall 工具面与情景图/三元组等深度语义。
- **迁移判定**：**工具面补齐 3–5 人日**（在 StructuredMemoryStore 上挂 4 个工具，复用既有 recall/consolidate）；深度语义（情景图/三元组/复调）**不做**——复杂度与收益不成比例，向量记忆已覆盖主路径。

### 1.16 omp-rpc / robomp（python/omp-rpc 6,844 行；python/robomp 32,453 行）

- **逻辑**：omp-rpc 是**零依赖**类型化 stdio JSONL 客户端（client.py 81KB/protocol.py 55KB + URI 与工具宿主接口）。robomp 是自托管 GitHub triage/fix bot（FastAPI + 队列/slot_pool/sandbox + SQLite + GitHub 事件后端 + proxy 反代 + 仪表盘），驱动 `omp --mode rpc`。
- **Gyre 状态**：`agent --rpc`（crates/cli/src/rpc.rs ~1000 行）已实现，docs/rpc.md 附 ~40 行示例客户端；**无类型化 Python 客户端、无 bot**。
- **迁移判定**：omp-rpc **值得落地**（3–5 人日：按 docs/rpc.md 协议写 py.typed 客户端 + 单测，打通 `agent --rpc` 外部生态）；robomp **独立产品不迁移**（20–30 人日，与核心路线无关）。

### 1.17 metaharness / stats / 其余

- **metaharness**（8.5k TS + 25KB py + React web）：统一基准运行器 + "Harbor" 运行存储 + REST/SSE + web 仪表盘 + edit 基准（对 coding-agent/hashline 跑真实编辑任务）。Gyre 已有 criterion 基准门（4 组 bench）；**场景级 eval（真实任务回放）是差距**，10–15 人日。
- **stats**（6.3k TS React + Chart.js）：用量/成本/增益聚合 + 本地 web 仪表盘。Gyre 已有 `/api/stats` + SVG 自绘趋势页（零新依赖）；**差额是深度聚合**（gain/窗口/用户指标），4–6 人日。
- **auth-broker/gateway + auth-storage（326KB）**：OAuth 凭证生命周期/轮换/网关。Gyre 已有 api_keys 轮换环 + secrecy；完整 OAuth 设备流 5–8 人日，**按需**。
- **mupdf WASM**：omp 二进制内嵌 mupdf 供 read 读 PDF。Gyre read 无文档解析；PDF 支持 4–6 人日（mupdf-rs 或 poppler），**可选**。
- **pi-uu-grep / pi-uu-diff**（3.9k/0.6k）：已被 Gyre search crate（ignore+globset+regex）与 similar 等价覆盖，**无需迁移**。
- **wire / natives 加载器 / Bazel 矩阵**：wire 已被 collab codec 逐字节移植；natives 加载器与 Bazel 是 FFI 分发税，纯 Rust 单二进制**无对应物**。

---

## 二、技术栈及依赖项兼容性评估

### 2.1 架构映射：omp 三层 vs Gyre 单层

| omp 层 | 构成 | 迁移到 Gyre 的形态 | 兼容性 |
|---|---|---|---|
| TS 引擎（packages/coding-agent + agent + tui，~180k TS） | agent 循环/会话/工具/TUI | 已由 crates/agent+context+tools+cli 等价替代（14 轮） | ✅ 已迁移 |
| Rust 原生层（crates/pi-*，84.4k 一阶行） | **唯一 N-API 耦合点是 pi-natives cdylib**（23.2k）；其余 crate 明文 napi-free | 纯 Rust crate 直接移植，pi-natives 的 `#[napi]`/ThreadsafeFunction/task::blocking 换成 tokio 原生等价 | ✅ **高**（作者已为纯 Rust 消费设计好） |
| vendor 层（100.6k） | brush + 47 uutils + jaq（88k 是 shell 相关） | 仅当需要内嵌 shell 才迁移 | ⛔ 不迁移（见 §一.1.2） |
| Python 层（39k） | omp-rpc（零依赖）+ robomp（FastAPI bot） | omp-rpc → Gyre `--rpc` 客户端；robomp 独立 | 🟡 客户端迁移 / bot 独立 |
| 构建 | Bun workspace + Bazel（crate_universe + zig/musl/msvc toolchains）+ Cargo 双轨 | 纯 Cargo workspace（Gyre 现状）；Bazel 矩阵对单二进制无意义 | ✅ 无冲突 |

**关键证据**：pi-iso 的 `projfs.rs` 注释"napi-derived types replaced with platform-neutral IsoError"、pi-voice 与 pi-walker 的 lib.rs 均声明"napi-free by design"——**omp 的原生层本来就是按"可脱离 JS 消费"设计的**，这是迁移可行性最高的根因。

### 2.2 依赖逐项兼容性矩阵

**已在 Gyre workspace（可直接复用，版本基本同族）**：

| 依赖 | omp 版本 | Gyre 版本 | 结论 |
|---|---|---|---|
| ignore / globset / regex | 0.4 / 0.4 / 1 | 0.4 / 0.4 / 1（workspace） | ✅ 同族（pi-walker/pi-uu-grep 依赖直接成立） |
| similar | 3.1 | 3.1 | ✅ 完全一致（pi-uu-diff 可直接搬） |
| portable-pty | 0.8 | 0.8 | ✅ 完全一致（Gyre pty crate 已用） |
| tiktoken-rs | 0.6 | 0.6 | ✅ |
| syntect | 5 | 5 | ✅ |
| image / png | 0.25 | 0.25（png feature） | ✅ |
| tokio / flume / parking_lot | 1 / 0.3 / 0.12 | 1 / — / — | ✅ flume/parking_lot 未入 Gyre 但无冲突 |
| serde / serde_json / toml | 1 / 1 / 0.8 | 1 / 1 / 0.8 | ✅ |
| sha2 / base64 / rand | 同族 | 已有 | ✅ |
| fastembed | JS 2.1（onnxruntime-node） | **Rust 5**（ort-download-binaries-rustls） | ✅ 语义等价、栈不同（vec_memory 已落地） |

**需新增依赖（按模块）**：

| 模块 | 新依赖 | 风险 | 评估 |
|---|---|---|---|
| pi-walker 整合 | dashmap、rayon | 低（纯内存结构） | 直接采纳 |
| pi-ast grammar 集 | tree-sitter 0.25 + ~60 grammar crates | **中高：与 Gyre 现有 0.24 线冲突**（见 §三.1） | 前置统一版本线 |
| pi-iso 扩展 | 无新 crate（libc/windows-sys 已有） | 中（平台后端行为矩阵） | 可行 |
| pi-voice 音频 | maudio（darwin 需 bindgen 重生成）、audiopus_sys（vendored opus，无 pkg-config） | 中（构建期 bindgen/静态编译） | 可行 |
| pi-voice 实时 | webrtc 0.17 | **高**（编译重、生态停滞、异步模型与 tokio 集成有坑） | 后置 |
| tts 本地 | ort（经 fastembed feature 已带）、Kokoro 权重下载 | 中（构建期联网 + 权重缓存） | 复用 vec-embed 模式 |
| computer-use | x11rb/xkeysym（Linux）、xcap/enigo（mac/win） | 中高（平台差异） | Linux 先行 |
| security_scan | git2 或 git CLI + tokio 后台任务 | 低中 | 可行 |
| hooks/todo/checkpoint/notebook | 无 | 低 | 零依赖 |

**Python 侧**：omp-rpc 零依赖（stdlib only）→ Gyre `docs/rpc.md` 已定义协议，客户端可独立落地；robomp 的 fastapi/uvicorn/pydantic 栈与 Gyre 无交集。

### 2.3 构建与运行时兼容

- **Rust 版本**：omp workspace edition 2024、rust-version ≥1.85，Gyre 同（rust-toolchain.toml 1.85）。omp 唯一的 nightly feature（`alloc_error_hook`）在 pi-natives 内，移植时丢弃即可。
- **panic 纪律**：omp 刻意 `panic=unwind` 以便在 napi/uutils 边界恢复；Gyre 无 FFI 边界，unwind 语义天然成立，pi-uutils-ctx 的"边界 catch_unwind"纪律可借鉴到工具隔离层。
- **license**：omp MIT、uutils MIT、brush MIT、Kokoro Apache-2.0、Liberation Mono SIL（Gyre 已内嵌）——**无许可冲突**。
- **二进制形态**：omp 发 `.node` 多平台矩阵（8 个 addon 变体 + AVX2 modern/baseline），Gyre 单二进制无此分发税——**Bazel/zig/xwin 全部不需要**。

---

## 三、潜在的代码冲突与风险点

### 3.1 🔴 tree-sitter 版本分裂（最大前置冲突）

- **现状**：Gyre workspace 声明 `tree-sitter = "0.25"`，但 `crates/ast` 实际锁 **0.24**（注释明言"与 ast-grep-core 的 facade 0.24 对齐，避免 links 冲突"）；omp pi-ast 用 **0.25** + ast-grep-core 0.39 + 60 grammars。
- **后果**：直接搬 grammar 集 → 版本分裂（0.24 的 6 个 grammar 与 0.25 的 60 个 grammar 无法共存于同一依赖图）。
- **处置**：①升级 ast-grep-core 到 0.39 系并统一 0.25 线（先跑 `crates/ast` 1100 行测试确认 facade API 兼容）；②或维持 0.24 线只补该线有 grammar 的语言。**迁移 grammar 集前必须二选一并锁死**。

### 3.2 🟡 重复实现冲突（命名/语义竞争）

| omp 模块 | Gyre 已有 | 冲突点 | 处置 |
|---|---|---|---|
| pi-walker 缓存 | `crates/search/src/fs_cache.rs` | 两套缓存语义（TTL 键/空检/失效）并存会双缓存不一致 | **扩展现有 fs_cache** 到 pi-walker 语义，不另立 crate |
| pi-shell minimizer | `crates/tools/src/minimizer.rs` | 过滤器注册表形态不同（Rust 函数 vs TOML 定义） | 只搬过滤器**语义**，保持现有 trait+fixture 模式 |
| pi-iso | `crates/iso`（rcopy/worktree） | 后端枚举扩展 | 在现有 `IsolationBackend` 上增后端，不重构 |
| mnemopi 召回 | `crates/memory/src/vec_memory.rs` | 融合公式已对齐（vec_weight 0.5） | 只补工具面，不重写存储 |
| web_search | `crates/tools/src/web_search.rs` | provider/extractor 注册表已存在 | 扩充，勿照搬 TS 文件结构 |
| stats | `crates/server` /api/stats | 聚合口径差异 | 增量扩展端点，不动 c5-ui 自绘路线 |

### 3.3 🟡 与既有循环注入点的时序冲突

- checkpoint/rewind、hooks（tool_call/tool_result）、advisor reset、TTSR abort、steering 三通道都挂在 run_loop 同批插入点。omp 的教训：rewind 必须先 reset advisor、TTSR 用 retry token + generation 守卫、advisor 有 emission-guard——**移植时每个新钩子必须明确其在五类停止边界检查中的优先级**（steering→aside→followUp→goal→…，见 goals 集成模式）。
- 风险源：`crates/agent/src/lib.rs`（4,794）+ `engine.rs`（1,662）仍是大单体；hooks 总线若直接往里塞会加速失控。**前置重构**：engine.rs 二次拆分（steering.rs / injections.rs），把 TTSR/advisor/followUp 注入点抽成显式模块再挂 hooks。

### 3.4 🟡 平台与构建风险

- **webrtc 0.17**：编译时间长、依赖树陈旧（可能拖慢 CI 数分钟级）；与 tokio 的集成需注意其自带 runtime。**先音频后实时**即是为控制此风险。
- **maudio darwin bindgen**：需 darwin 环境重生成绑定，Gyre 无 mac CI → Linux 先行，darwin 留 feature 门控。
- **audiopus_sys**：vendored opus 静态编译（无 pkg-config），需确认交叉编译（musl）路径——omp 已在 Bazel 注解里踩过，Cargo 侧需复刻 patch。
- **grammar 大集**：60 个 tree-sitter grammar 的编译时间与内存（每个都是 C 生成器），CI 增量编译可能显著变慢；建议按语言热度分批。

### 3.5 🟡 安全面

- **security_scan**：设计上自带 OAuth 凭证 + 后台 job + 仓库外状态——凭证处理必须走 Gyre 的 secrecy 惯例；v1 建议**去掉 OAuth 面**，只做本地扫描 + 计划指纹，把攻击面留到产品化时。
- **hooks 总线**：tool_call 的 `{block, input}` 替换能力 = 工具参数改写权限。若未来支持外部脚本钩子，这是**代码执行面**；Rust 侧 v1 只允许编译期注册的 Hook（inventory），不开放运行时加载。
- **computer-use**：XTEST 输入注入 + 屏幕捕获是敏感能力，必须进 `[tools]` 可选组 + 审批门禁（与 browser/debug 同模式）。

### 3.6 🟢 低风险确认

- 命名冲突：omp `pi-*` vs Gyre `agent-*` 前缀无碰撞；内部 URL scheme 已独立实现（read 工具 9 个 scheme），新增 agent:///history:///rule:///security:// 需与既有 router 阶梯（file→web→scheme→archive→sqlite→fs）对齐。
- 测试契约：Gyre 61 套件安全网（agent 78 / tools 78 / config 51 / cli 44 / eval 8 / llm 70 / server 11 等）——新模块按既有"外部契约测试、禁源码 grep、full-suite safe"纪律。

---

## 四、迁移工作量估算

> 单位：人日（pd），单人全栈；"移植"指**语义移植**（参考 omp 实现，不拷贝 TS/JS 代码），符合本仓库既定原则。

| # | 模块 | 估时 | 前置依赖 | 优先级 |
|---|---|---|---|---|
| 1 | checkpoint/rewind | 4–6 | 无（engine 拆分非硬依赖） | 🔴 P0 |
| 2 | todo 工具 | 2 | 无 | 🔴 P0 |
| 3 | `.ipynb` 读写往返 | 2–3 | 无 | 🔴 P0 |
| 4 | web_search 抽取器扩充（+10–20 站点） | 3–5 | 无 | 🔴 P0 |
| 5 | omp-rpc 类型化 Python 客户端 | 3–5 | docs/rpc.md 已定稿 | 🔴 P0 |
| 6 | pi-walker 缓存整合（并行 + 双档位 + 写失效钩子） | 3–5 | 无 | 🟡 P1 |
| 7 | web_search provider 扩充（+3–6 个） | 4–6 | trait 已就绪 | 🟡 P1 |
| 8 | 模型目录数据化（提炼版） | 4–6 | 无 | 🟡 P1 |
| 9 | memory 工具面（learn/reflect/retain/recall） | 3–5 | structured store 已接线 | 🟡 P1 |
> **已落地（2026-08）**：recall/retain/reflect（reflect = LLM 提炼 → mental_models.md 注入）+ auto-retain + sleep + P1 算法层（同义词/意图/MMR/时间解析）+ MMR 中文调优。learn 未做（retain 已覆盖写面）。
| 10 | hooks 事件总线 MVP（tool_call/tool_result/会话事件） | 5–8 | **engine.rs 二次拆分** | 🟡 P1 |
| 11 | minimizer 过滤器扩充（TOML 定义驱动） | 4–6 | 无 | 🟡 P1 |
| 12 | TTS 云端后端（xAI 兼容） | 2–3 | 无 | 🟡 P1 |
| 13 | pi-ast grammar 扩展（先 10–15 语言） | 4–6 | **tree-sitter 版本统一** | 🟢 P2 |
| 14 | security_scan v1（preflight+本地扫描+status/cancel） | 10–15 | 后台作业基础设施 | 🟢 P2 |
| 15 | computer-use Linux X11 v1 | 10–15 | 无 | 🟢 P2 |
| 16 | pi-voice 音频采集/播放 | 6–10 | 无 | 🟢 P2 |
| 17 | pi-iso PAL 后端扩展（reflink/overlayfs 先行） | 10–15 | 无 | 🟢 P2 |
| 18 | metaharness 场景级 eval 体系 | 10–15 | criterion 已立基线 | 🟢 P2 |
| 19 | stats 深度聚合 | 4–6 | /api/stats 已有 | 🟢 P2 |
| 20 | TTS 本地 Kokoro（ORT） | 8–12 | fastembed 模式复用 | 🟢 P2 |
| 21 | auth OAuth 设备流/网关 | 5–8 | secrecy 已有 | ⚪ P3 |
| 22 | PDF 文档解析（mupdf-rs 等） | 4–6 | 无 | ⚪ P3 |
| 23 | pi-voice 实时 WebRTC 通话 | 10–15 | 音频先行 | ⚪ P3 |
| 24 | robomp GitHub bot | 20–30 | --rpc 已就绪 | ⚪ 独立产品 |
| ⛔ | TUI 引擎（24k TS） | 60+ | — | 不迁移 |
| ⛔ | 扩展/插件市场 | 30+ | — | 不迁移 |
| ⛔ | 内嵌 bash（brush+uutils 88k） | 30–40 | — | 不迁移 |
| ⛔ | Bazel 构建矩阵 | — | — | 无对应物 |

**合计（推荐项 1–23）**：约 **115–165 人日**；若按"P0+P1 先做"收敛，前 12 项约 **35–55 人日**即可拿到 80% 的用户可见收益。

---

## 五、重构建议与分步实施路线图

### 0. 两个前置重构（任何大件之前先做）

1. **tree-sitter 版本统一**（0.5–1 pd 决策 + 2–3 pd 迁移）：评估 ast-grep-core 0.39 线，将 `crates/ast` 升到 0.25 统一线；`crates/ast` 1100 行测试全绿为验收。**这是 pi-ast grammar 集、snapcompact 无关、但影响后续一切 AST 扩展的闸门**。
2. **engine.rs 二次拆分**（2–3 pd）：按 benchmark §六.1 续拆 `steering.rs`（三通道队列与 drain）与 `injections.rs`（TTSR/advisor/followUp/未来 hooks 的注入点枚举）。**hooks 总线与 checkpoint/rewind 的挂接点必须落在 injections.rs**，否则重蹈 agent/lib.rs 单体的覆辙。

### Phase A — 快赢（第 1–2 周，约 12–18 pd）

| 步骤 | 落点 | 验收 |
|---|---|---|
| A1 checkpoint/rewind | `crates/agent` 新模块 `checkpoint.rs`（CheckpointState/PendingRewind/CompletedRewind + 守卫）+ `crates/tools` 两工具（不入 sub_tools）+ 压缩 pinned 节点集 + JSONL 语义标记 | 探索 30 轮后 rewind → 上下文塌缩为报告（branchSummary→user、rewind-report→developer）；会话树分支可回读；压缩后检查点仍可回卷（pinned + 语义回退）；resume/会话切换后 rehydrate 恢复；yield 守卫循环续跑；omp 9 个集成场景全移植（agent 套件） |
| A2 todo 工具 | `crates/agent` 状态机 + 工具 + REPL `/todo` | 单活跃不变量、错误原子性、失败提醒注入（6–8 单测） |
| A3 `.ipynb` 往返 | `crates/tools/src/fs.rs`（read 视图 + write 回写） | metadata 保留、cell:N 克隆、非法输入错误面（fixture 单测） |
| A4 web_search 抽取器 | `crates/tools/src/web_search.rs` 注册表扩充 | 每个抽取器配 fixture（npm/crates.io/github/mdn/stackoverflow 先行） |
| A5 omp-rpc 客户端 | `python/omp-rpc/`（新目录）+ docs/rpc.md 引用 | 客户端过 `agent --rpc` 冒烟：prompt→事件→done；单测协议编解码 |

### Phase B — 性能与智能层（第 3–6 周，约 25–35 pd）

| 步骤 | 落点 | 验收 |
|---|---|---|
| B1 pi-walker 缓存整合 | `crates/search/src/fs_cache.rs` 扩展（rayon 并行 + Minimal/Full 档位 + fuzzyFind） | 与 grep/glob/ast 共享条目；写工具（write/hashline/replace_block/ast_rewrite）接失效钩子；缓存命中一致性测试 |
| B2 provider 链扩充 | `web_search.rs` 懒加载注册表 + 3–6 provider（perplexity/jina/kagi/tavily/brave 按 key 可选） | 顺序回退、429 冷却、无 key 时优雅降级（mock 单测） |
| B3 模型目录 | `crates/catalog`（新数据 crate）或 config 内嵌表：vision/thinking/context/token 上限 | snapcompact 的 `vision_models` 通配改为目录驱动；config 测试不回归 |
| B4 memory 工具面 | `crates/memory` 4 工具 + cli 装配 | learn→recall 闭环跨会话；consolidate 流程复用 |
> **已落地（2026-08）**：recall/retain 两工具（cli/server/rpc）+ `AutoRetainHook` + `StructuredSleepHook` + 注入格式对齐；P1 算法层（synonyms/intent/mmr/temporal）并入 `crates/memory/src` 新模块。
| B5 hooks 总线 MVP | 新 `crates/hooks`（或 agent 内模块）：`Hook` trait + inventory 注册 + before/after_tool_call + 会话事件 | 与 approval 网关、intercept 规则的优先级语义文档化 + 单测（block/参数改写/结果改写） |
| B6 minimizer 扩充 | TOML 定义驱动过滤器表（git/gh/cargo/jvm/python 高频先） | 每过滤器 fixture：命中/不命中/空输出；原始输出可经 artifact 回读 |
| B7 TTS 云端 | `crates/ttsr` 旁新 `crates/tts` 或 tools 新工具：HTTP 合成 + 配置路由 | 冒烟：文本→文件落盘；错误面（无凭证/超时） |

### Phase C — 大子系统（第 7–12 周，约 45–60 pd，按资源弹性）

| 步骤 | 落点 | 验收 |
|---|---|---|
| C1 pi-ast grammar 扩展 | `crates/ast` 按热度 +10–15 语言（js/ts/go 已有，补 py 细化、c/cpp/java/kotlin/swift 等） | 每语言 replace_block/ast_search fixture 单测；编译时间预算记录 |
| C2 security_scan v1 | 新 `crates/security`：preflight（git 目标 + 计划指纹）+ 本地扫描 job + status/cancel + 结果落盘 | 无凭证面；指纹不变性；扫描结果幂等可读（单测） |
| C3 computer-use Linux v1 | `crates/tools/src/desktop.rs`（x11rb 捕获 + XTEST 注入 + 键位映射） | `[tools.enabled] computer` 可选组 + 审批；冒烟截图/点击；无 X 优雅报错 |
| C4 pi-voice 音频 | 新 `crates/voice`：maudio 采集/播放 + 工具面（record/play） | Linux 冒烟录音回放；darwin feature 门控 |
| C5 pi-iso PAL 扩展 | `crates/iso` 增 reflink（FICLONE）/overlayfs 后端 | 大仓库隔离冒烟：子代理写操作不污染基树；diff 正确 |
| C6 metaharness 场景 eval | `crates/eval` 或新 `crates/harness`：场景回放 + 通过率报告 | 3 个种子场景（编辑/搜索/会话）；与 criterion 基线并存 |
| C7 stats 深度聚合 | `crates/server` /api/stats 扩展（会话内时间戳/gain 窗口） | 与既有面板数据一致（3 会话验证） |
| C8 TTS 本地 Kokoro | `crates/tts` 加 ort 后端 + 权重缓存 | `GYRE_TTS_LOCAL=1` 冒烟 WAV 产出；降级路径 |

### Phase D — 按需（不阻塞主路线）

- auth OAuth 设备流、PDF 解析、实时 WebRTC、robomp bot、ProjFS 后端、fuse-overlayfs 兜底。
- **明确不再讨论**：TUI 引擎、扩展/插件市场、内嵌 bash、Bazel 矩阵（理由见 §〇）。

---

## 六、验证方法与注意事项

1. **checkpoint/rewind**：rewind 必须在 turn_end 应用（非工具执行内）；advisor reset 先于 replaceMessages；branchWithSummary 的 entryId 缺失走 root 兜底——三路径都要单测。
2. **todo 原子性**：任何引用错误丢弃整个变更（防重试撞"已存在"）；单活跃任务归一化在 op 后强制。
3. **ipynb**：nbformat 4.5 的 metadata 保真、`execution_count/outputs` 的 code/markdown 差异、源码数组换行切分规则——每项 fixture 锁定。
4. **fs_cache 失效**：create/delete/rename 三形态（删除路径不存在时 fallback 父目录规范化再挂文件名，omp 踩过）；漏失效 = 陈旧结果 bug。
5. **hooks 优先级**：tool_call 改写与 TTSR astCondition、approval 网关的执行顺序必须文档化并测试；v1 只开放编译期注册。
6. **web_search**：以源码 `SEARCH_PROVIDER_OPTIONS` 为准（文档已过期）；provider API 漂移靠 trait + 懒加载 + fixture。
7. **grammar 集**：版本统一先行；每批 grammar 记录编译增量，CI 时长超预算即降速。
8. **安全面**：computer/security/voice 全部进可选工具组 + 审批门禁；security_scan v1 不带 OAuth。
9. **纪律**：所有新子系统保持"外部契约测试、禁源码 grep、full-suite safe"；61 套件是回归安全网。

---

## 七、结语

oh-my-pi 的历史功能模块对本项目而言是**高纯度可迁移资产**：原生层刻意 napi-free、依赖族与 Gyre 高度同源（ignore/similar/portable-pty/tiktoken-rs/syntect 直接复用）、许可全兼容。剩余迁移面约 **35–55 人日（P0+P1）** 即可补齐用户可感知的差距（会话回卷、todo、notebook、搜索覆盖、内存工具面、事件钩子、TTS）；P2 大件（security_scan/computer-use/voice/AST 全语言）按资源排期；TUI、插件市场、内嵌 shell 三项维持明确不迁移结论。开工顺序上，**tree-sitter 版本统一与 engine.rs 二次拆分是两个前置闸门**，其余均可按 Phase A→B→C 顺序独立推进。
