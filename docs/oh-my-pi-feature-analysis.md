# oh-my-pi 核心特色功能调研与 Gyre 借鉴优先级

> 调研对象：`third/oh-my-pi`（badlogic/pi-mono 的 fork，MIT）
> 调研日期：2026-08-01
> 方法：8 个并行 scout 精读 `docs/*.md` + 源码逐文件核对（`packages/coding-agent` / `packages/agent` / `packages/tui` / `crates/*`），
> 对照 Gyre 全部 24 个 crate 与 [`agent-loop-improvement-plan.md`](agent-loop-improvement-plan.md)（5 轮对比已覆盖循环引擎层）。
> 文中所有 omp 侧文件路径均相对 `third/oh-my-pi/`。

---

## 〇、项目速览与现状基线

**oh-my-pi**：TypeScript 引擎（`packages/coding-agent` 约 1MB 源码）+ Rust N-API 原生层（`pi-natives`/`pi-shell`/`pi-ast`/`pi-iso` 等约 55k 行 Rust）+ 25k 行 TUI。核心卖点：32 个内置工具、40+ provider、14 LSP ops、28 DAP ops、"最完整的开箱即用 agent 表面"。

**Gyre 基线**（24 crates / 约 47k 行 Rust）：**循环引擎层已大量移植**（`agent-loop-improvement-plan.md` 记录：shake 重写、工具并发、会话树、字节级稳定前缀、length 占位、软工具护栏、三通道注入、interruptMode、结构化 handoff、auto-thinking、refusal 过滤等均已落地）。**产品层空白**：

| 能力 | Gyre | oh-my-pi |
|---|---|---|
| web_search（多 provider 链 + 站点抽取） | ❌ 无 | ✅ 23 provider + 74 抽取器 |
| DAP 调试器 | ❌ 无（debug 仅提示词差异） | ✅ 28 ops / 14 适配器 |
| eval 内核（Python/JS + 环回桥） | ❌ 无 | ✅ 双内核 + 工具回调桥 |
| TTSR 时间旅行流规则 | ❌ 无 | ✅ 流式 abort+注入+重试 |
| magic keywords | ❌ 无 | ✅ ultrathink/orchestrate/workflowz |
| conflict:// 冲突解决 | ❌ 无（read 无 `:conflicts`） | ✅ @ours/@theirs/@base |
| pr:// / issue:// / agent:// / history:// | ❌ 无（已有 skill/memory/mcp/local/artifact/http） | ✅ 15 个内部 scheme |
| Advisor 双代理评审 | ❌ 无（supervisor 仅观测子代理） | ✅ 独立评审 Agent（已计划 P1-L） |
| goals 目标预算 / plan-mode / 规则导入 | ❌ 无 | ✅（已计划 P1-M / P2-M / P2-N） |
| snapcompact 图像化压缩 | ❌ 无（LLM summarize） | ✅ 本地 PNG 帧（已设计 P0-L） |
| Collab 完整协议 | ⚠️ 仅 relay+codec（密封布局逐字节一致） | ✅ 14 帧协议 + 权限 + 快照恢复 |
| LSP 写操作 | ⚠️ 11 ops 只读 | ✅ WorkspaceEdit 应用 + willRenameFiles |
| resolve 暂存 | ⚠️ ast_rewrite 直接应用 | ✅ (proposed) → 应用/拒绝 |
| TUI 引擎 / 扩展插件系统 | ⛔ 走 rustyline+Web / inventory 路线 | ✅（不建议移植，见 §三） |

---

## 一、核心特色功能逐项分析

### 1. TTSR 时间旅行流规则（Time-Traveling Stream Rules）⭐ 最独特

**功能亮点**：规则不写进 system prompt（零上下文成本），而是对**流式输出**实时匹配——regex 匹配文本/思考增量、ast-grep 匹配 write/edit 载荷。命中即**中途 abort 流**，注入规则为隐藏 system 消息，从同一轮重试。违规输出被"时间旅行"抹掉重来。另有 `once`/`after-gap` 重复策略与 `interruptMode: never` 的非打断变体（把提醒折进工具结果）。

**技术实现**（`export/ttsr.ts` 591 行 + `session/ttsr-coordinator.ts` 497 行）：
- 每流缓冲（text/thinking/toolcall）；同步 `checkDelta` + 原生 ast-grep `checkAstSnapshot`（匹配 `matcherDigest`——write 的完整 content、hashline 的 added-lines，非整文件）。
- 命中 → 同步 `agent.abort()`（客户端 AbortController → fetch signal → SSE 断连，**无需任何 provider 特殊 API**）→ 50ms 延迟任务校验 retry token → `replaceMessages` 抹掉部分输出（`contextMode: discard`）→ append `ttsr-injection` 隐藏消息 → `continue()` 重跑。
- 抑制状态存**分支日志元数据**（`ttsr_injection` entry，`compaction/entries.ts`），压缩/恢复后依然抑制，不重复注入。

**设计思路**：把"规则"从提示词（advisory，模型可无视、永远吃 token）变成**硬约束**（violation → 强制重试）。成本经济学：规则只在触发时才进上下文；abort 的请求不产生 usage 计费（SSE 部分输出丢弃），重试前缀字节不变 → 命中 provider prompt-cache 折扣价。

**适用场景**：仓库级强制规范（禁 `Box::leak`、禁危险命令）、安全红线（检测到密钥写入即拦截）、团队规则落地。

**Rust 移植评估**：核心引擎 easy（约 600 行纯逻辑，`regex` crate 线性时间，**天然消除 JS 版 ReDoS 风险**）；coordinator medium（需要 abort/continue/replaceMessages 原语——Gyre 已有 cancel token + `persist_interrupted`，基础良好）；astCondition 需要 tree-sitter 查询等价物（Gyre 已有 `crates/ast`）。**Gyre 改进计划文档完全未覆盖此项。**

### 2. magic keywords（ultrathink / orchestrate / workflowz）

**功能亮点**：三个独立小写词让单轮进入专属行为契约：`ultrathink` → 跳过 auto-thinking 分类器、强制最高 Effort；`orchestrate` → 注入 10 条多代理编排契约；`workflowz` → 确定性多子代理评估工作流（需 task+eval 双工具在场）。

**技术实现**：词边界正则（排除代码/路径/标识符/函数调用）+ **markdown 结构掩码**（`modes/markdown-prose.ts` 长度保持地抹掉代码围栏/内联代码/HTML/XML 后再匹配）；命中后以 `display:false` 隐藏 custom 消息排队在用户消息**之前**。单测覆盖 `orchestrate,` 命中、`orchestrate()` 排除。

**设计思路**：不引入新 UI 或 system prompt 变更，一句话切换行为契约；与 TTSR 的"流规则"互补（TTSR 是事后纠正，keywords 是事前契约）。

**移植成本**：约 250 行掩码扫描器 + 3 个 prompt 模板。Gyre 已落地 auto-thinking（P1-K），`ultrathink` 直接复用 `Effort::Max` 钳位逻辑。

### 3. 内部 URL 协议族（15 个 scheme）+ conflict:// 冲突解决

**功能亮点**：一条路径字符串 + 一套 selector 语法统一文件/归档/SQLite/内部协议/网页：`pr://1428` 读 PR、`agent://<id>/findings.0.path` 按路径提取子代理输出 JSON 字段、`conflict://3` 写 `@theirs` 一键解决合并冲突、`conflict://*` 批量、`ssh://` 读远程文件。GitHub 是"另一个文件系统"——**一个 read 工具教一次就会**。

**技术实现**：
- 进程级 `InternalUrlRouter` 单例（`internal-urls/router.ts`，`Map<scheme, ProtocolHandler>`，handler 无状态，会话状态经 `ResolveContext` 注入）；read 工具分发阶梯：file:// 展开 → web → router → 归档 → SQLite → 本地 FS。**未注册 scheme 回退到 MCP handler**（开放命名空间）。
- conflict:// 不走 router（read.ts/write.ts 直接 `parseConflictUri`）：read 侧纯状态机扫描 `<<<<<<<` 块注册进会话 `ConflictHistory`（id 稳定、splice 按内容重定位抗行漂移），write 侧按内容 splice + 边界回声修剪（防模型粘贴过度）；`@base` 对 2-way 冲突显式报错；批量 `conflict://*` 按文件分组、自底向上应用。
- pr:// 背后是 `gh` CLI + SQLite 缓存（`tools/github-cache.ts`：auth 指纹化、软 TTL 300s / 硬 TTL 7d、stale-while-refresh、bash 变异命令自动失效）。

**设计思路**：模型不需要 12 个 GitHub 工具各学一套参数——**工具面收敛**是模型顺从度的核心杠杆（README 数据：edit 格式对了，Grok 通过率 6.7%→68.3%）。

**移植评估**：conflict:// 子系统独立且纯逻辑（约 600 行，easy-medium），**收益立即可见**；pr:///issue:// medium（gh 封装 + 缓存语义）；agent:///history:// medium（依赖子代理产物生态）；ssh/vault/mcp 方案 hard（外部依赖重，按需）。Gyre 已有 skill:// memory:// mcp:// local:// artifact:// http(s):// 且带 SSRF 防护——**路由器模式半成品，缺 pr/issue/agent/history/rule/conflict 六个**。

### 4. Advisor 双代理评审（第二模型旁观）⭐ 智能层

**功能亮点**：独立"评审 Agent"（**自己的 Agent、自己的 ToolSession、自己的 append-only context、独立 model**，默认 slow 推理链且永不继承主模型）读主 Agent 每轮增量，以 `nit|concern|blocker` 三级注记注入主循环。配套 `WATCHDOG.md` 仓库级评审准则。

**技术实现**（`coding-agent/src/advisor/`）：
- delta 游标（`#lastCount`/`#deliveredPrefix`）只喂增量；secret 脱敏（`SecretObfuscator`）；近窗口自动 promote 到更大 sibling 模型。
- **emission-guard 在代码层去重**（真实 advisor 模型会 spam "Stop." 114 次，issue #3520——NFKC 归一化 + 4096 FIFO 去重 + 每轮一条门控 + filler 过滤）；注记为 XML 转义 `<advisory severity=...>`，经 aside/steer/preserve 三通道路由。
- WATCHDOG.md/.yml 发现（用户目录 + cwd→仓库根逐级，全部加载不就近停），注入 advisor system prompt。

**设计思路**：主模型自审是盲区（自己看不出自己的错）；第二个模型在**独立 context** 上看同一份 transcript，专注力不被打断。Gyre 的 supervisor 只观测 TaskTool 子代理，没有评审视角。

**移植成本**：medium-large（新 crate，依赖 agent/llm/core）。Gyre 计划文档已列为 **P1-L 待办**——emission-guard 与 WATCHDOG.md 是成败关键，非可选装饰。

### 5. snapcompact 图像化压缩（本地 PNG 帧）⭐ 成本级

**功能亮点**：把将被丢弃的历史序列化为紧凑文本、渲染成 **bitmap-font PNG 帧**，视觉模型直接读图回放——**完全本地、确定性、零 LLM 调用、零 API key**。长会话压缩成本降一个数量级。

**技术实现**（`packages/snapcompact/src/snapcompact.ts`）：
- `serializeConversation`（`¶user:`/`¶ai:`/`¶think:`/`¶call:` 分节、工具结果折进调用块加 DIM 灰墨标记、无用小工具对丢弃）→ normalize（剥 ANSI、空白折叠为全块字符 U+2588、NFKD 去组合符、CJK 半角→全角）→ 按 **provider 计费模型选帧形状**（`resolveShape`：Anthropic 28px patch 计费、Google 固定 1120 tok/图、OpenAI 32px patch）→ 栅格化 PNG（`crates/pi-natives/src/snapcompact.rs` 本就是 Rust）。
- 预算护栏：`MAX_FRAMES_DEFAULT=80`、3MB/请求、`HQ_EDGE_FRAMES=3` 高分辨率边缘帧。
- **SQuAD v1.1 eval 数据**：opus-4.8 工具可读性 f1 0.806（11on16-bw）、gemini-3.5-flash 0.934——帧形状是调优出来的，不是拍脑袋（差的形状 f1 0.287，比文本摘要还差）。

**风险（Gyre 计划文档已分析）**：Gyre 无 PNG/字体依赖 + assistant `ContentBlock` 无 Image 变体 + 需真实视觉模型 eval 验证召回率——朴素渲染比文本 summarize 更差。前置 1-2 一个攻关会话可完成，前置 3 是成败关键。

### 6. eval 双内核 + 环回桥（Python + Bun worker）

**功能亮点**：持久 Python 子进程（NDJSON 协议）+ Bun worker JS VM，**任一内核可回调 agent 自身工具**（`tool.read` 从 Python 里读 CSV，`tool.bash` 从 JS 里执行）——"agent 加载 CSV、JS 画图、不出 cell"。

**技术实现**（`coding-agent/src/eval/`）：
- Python 侧 127.0.0.1 HTTP 环回桥（`tool-bridge.ts`：bearer token + 每次运行注册 + abort 屏蔽），JS 侧 in-process postMessage 往返。
- 保留桥 `__agent__`（schema 化子代理）/`__completion__`/`__budget__`/`__concurrency__`；会话级内核保留（按 sessionId+cwd+interpreter 键控）、IdleTimeout 看门狗、SIGINT 取消、死内核重试。

**移植评估**：medium。Rust 侧 spawn Python 子进程 + NDJSON 很自然；桥的安全模型（token + 注册 + abort 屏蔽）必须照搬。**Gyre 完全没有 eval 能力**——这是数据分析类任务（pandas describe、图表）的硬缺口。

### 7. DAP 调试器（28 ops / 14 内置适配器）

**功能亮点**：真调试器：lldb-dap 步进 C 二进制、dlv 走 Go goroutine、debugpy 暂停 Python。C 段错误 → attach → 步到坏指针 → 读帧。

**技术实现**（`coding-agent/src/dap/`）：完整 DAP 客户端（Content-Length 帧、stdio/unix-socket/TCP 传输、`${port}` 替换、socket 就绪门）、单根会话树管理（launch/attach 握手、反向请求 runInTerminal/startDebugging、断点跨树同步、空闲清理）、适配器自动选择（extension→rootMarker→原生偏好）。

**移植评估**：hard（全新子系统约 4000 行等效）。**Gyre 的 debug 模式只有提示词差异**——这是 Gyre "debug" 模式与 omp 的实质差距。

### 8. LSP 深度（14 ops）+ 写操作应用

**功能亮点**：Gyre 的 11 个 LSP ops 只读；omp 的 rename 走 `workspace/willRenameFiles`（重导出/桶文件/别名导入先更新再移动文件）、code actions 完整 resolve+executeCommand、WorkspaceEdit 应用（`lsp/edits.ts`：重叠校验、自底向上文本编辑、documentChanges 操作排序）、诊断版本账本去重 + 编辑后延迟诊断注入。

**移植评估**：medium。`edits.ts`（WorkspaceEdit 应用）可独立移植；rename_file 需要 Gyre 无的文件移动+引用重写。

### 9. task 子代理（schema 校验 + PAL 写时复制隔离 + IRC）

**功能亮点**：子代理 yield 出 **schema 校验过的类型化结果**（ArkType，调用方 > agent > 会话优先级），父代理直接读对象不解析散文；PAL 写时复制隔离（APFS/Btrfs/ZFS/reflink/overlayfs/projfs/rcopy 多后端）——基线捕获 → 运行 → 合成树 diff → 分支提交或 .patch 合并回；子代理间 in-process IRC 总线（回执/唤醒/复活）；并发护栏（spawn 策略、深度截止、**per-provider 流信号量**防死锁 #3749）。

**Gyre 现状**：TaskTool 已有（含并发护栏与独立 token 预算），iso crate 已有 rcopy/worktree——**PAL 多后端与 schema 化输出是差距**。隔离是 hard（平台后端矩阵），schema 化输出 medium。

### 10. web_search（23 provider 链 + 74 个站点感知抽取器）

**功能亮点**：`auto` 按序走 provider 链（perplexity/gemini/anthropic/codex/xai/exa/jina/kagi/tavily/brave/duckduckgo/searxng…，免 key 的有 6 个）；命中 URL 直接交 read 抓取；**站点感知抽取**把 arxiv/npm/crates.io/github/reddit/mdn/stackoverflow 等转成结构 markdown（锚点保留）。

**技术实现**（`coding-agent/src/web/search/`）：懒加载 provider 注册表 + 顺序回退（首个可渲染响应胜出）；query 指令解析器 851 行（`site:`/`inurl:`/`after:` 等，宽松后过滤）；`formatForLLM` 输出 answer+Sources/Citations/Related（240 字符片段上限）。**注意：`docs/tools/web_search.md` 已过期（写 25 provider，源码 23——bing/yahoo 已删），移植以源码 `SEARCH_PROVIDER_OPTIONS` 为准。**

**移植评估**：medium。**Gyre 完全空白**；read 已有 http 抓取 + SSRF 防护，补 provider 链 + 10-20 个高频站点抽取器即可。风险：provider API 易变，保持 trait 抽象 + 懒加载。

### 11. Provider 路由（roles / fallback chains / 凭证轮换 / 模型目录）

**功能亮点**：角色路由（default/smol/slow/plan 各挂模型链，`priority.json`）；retry fallback chains（429/配额 → 下一模型接管本轮，冷却后恢复；链键优先级 exact model > provider/* > role > default）；**round-robin 凭证轮换**（`AuthStorage` 8k 行：session 亲和 + 每凭证退避 + usage 排序选择，防单 key 烧配额）；path 作用域模型；10.5 万行捆绑模型目录（`catalog/src/models.json`）。

**移植评估**：medium。Gyre 已有 ProviderRegistry + 多 profile 切换，缺**运行时自动降级**（fallback chains）与**凭证轮换**。收益：稳定性/成本，属"基础设施红利"。

### 12. Collab 完整协议

**功能亮点**：Gyre 已有加密中继内核（`codec.rs` 直接移植自 omp 的 codec.ts，密封布局逐字节一致）但**只有 relay+room 广播**。omp 侧另有：14 帧协议（welcome/snapshot-chunk/entry/event/state/bus/agents/ui-request/transcript/bye/error）、**读写权限模型**（16B write token，只读 view 链接）、快照分块（512KB）+ 断线恢复（guest 端复刻 session 到 `~/.omp/collab/<roomId>.jsonl`）、WS 退避 1s→30s、QR 链接、浏览器端可加入。

**移植评估**：medium（协议帧 + 权限 + 快照同步，约 1500 行等效）。收益：从"实验性中继"到"可分享的 live session"。

### 13. 其余（低优先或已覆盖）

| 功能 | 要点 | Gyre 状态 |
|---|---|---|
| 多格式规则导入 | 8 种 context-file + 7 种 rule 格式原生读取（Cursor MDC/Cline/Codex AGENTS.md/Copilot），名字级去重，优先级遮蔽 | 计划 P2-N |
| resolve 暂存 | ast_edit 先 (proposed) 预览，`write xd://resolve` 应用（含计数一致性复查）；非工具而是**纯文本写入设备** | Gyre ast_rewrite 直接应用 |
| omp commit | LLM 拆分原子提交：hunk 级 `git apply --cached`、Kahn 拓扑排序依赖、环拒绝、文件优先级打分（源码 100 > shell 80 > … > 测试 10） | 无 |
| shell completions | 从活 CLI 元数据生成 bash/zsh/fish 脚本 + `__complete` 动态回调 | 无 |
| 会话操作 | `/fresh`（重置 provider 流状态不动本地记录）、fork（继承 prompt-cache key）、加密分享 | fork/export 已有，/fresh 无 |
| TUI 引擎 | 25k 行 TS 差分渲染器；**核心思想是 append-only 原生回滚契约**（回滚==已提交帧前缀，可证明），非虚拟 DOM | Gyre 走 rustyline+Web 路线 |
| 扩展/插件系统 | TS 模块钩子（工具/命令/热键/TUI），marketplace | Rust 无动态加载，不推荐全移植 |
| 原生工具 | in-process grep/rg/brush-shell/summary/fs_cache——**Gyre 已是纯 Rust 进程**，此优势天然等价；fs_cache 实为 (root, WalkOptions) 键 + 1s TTL + 路径失效（非 mtime 键），可借鉴给 Gyre 的 glob/grep | 部分 |

---

## 二、对 Gyre 的借鉴价值矩阵

| # | 功能 | 借鉴方式 | 落地步骤（简） | 预期收益 | 潜在风险 |
|---|---|---|---|---|---|
| 1 | **TTSR 流规则** | 新 crate `crates/ttsr`（或并入 skills） | ①`Rule` 形状 + regex 编译 + 作用域门控（移植 `capability/rule.ts`，用 `regex` crate）②缓冲 + 匹配引擎（移植 `export/ttsr.ts`）③coordinator 接 `run_loop`：abort→50ms 延迟→replaceMessages→注入→continue（Gyre 已有 cancel/`persist_interrupted` 基座）④`ttsr_injection` 分支日志元数据（复用 `SessionNode` 树）⑤非打断变体：afterToolCall 折进工具结果 | 规则零上下文成本、硬约束生效；仓库级红线落地；**差异化卖点** | abort 竞态（omp 用 retry token + generation 守卫，照搬）；`contextMode: keep` 的 assistant 尾续写合法性 |
| 2 | **conflict:// 解决** | 扩展 `crates/tools/fs.rs` | ①状态机扫描器 + `ConflictHistory`（会话级注册表）②read 侧 `:conflicts` selector ③write 侧 splice + @tokens + 批量 `conflict://*` ④边界回声修剪 | 合并冲突一键解决；模型不再手抄 200 行文件 | 内容重定位匹配的边界 case（CRLF、2-way @base 报错）；10MiB 扫描上限说明 |
| 3 | **magic keywords** | `crates/agent` + prompts | ①掩码扫描器（markdown 结构排除）②词边界匹配 ③隐藏 custom 消息排队在用户消息前 ④`ultrathink` 挂 `Effort::Max` 钳位 | 单词切换行为契约；复用已落地的 auto-thinking | 关键词误触发（掩码必须长度保持）；合成轮禁用 |
| 4 | **web_search + 抽取器** | `crates/tools` 新工具 | ①`WebSearchProvider` trait + 3-6 个适配器（duckduckgo/searxng 免 key 先行，perplexity/jina 有 key 可选）②顺序回退链 ③10-20 个站点抽取器（arxiv/npm/crates/github/mdn 优先）④结果格式化（answer+sources，240 字符截断） | agent 走出沙箱；arXiv/GitHub 直接可读 | provider API 漂移（trait 抽象 + 懒加载）；抽取器正则脆弱（单测锁定） |
| 5 | **Advisor**（P1-L） | 新 crate `crates/advisor` | 计划文档已列：独立 Agent + 只读工具集 + `AdvisorHost` trait；**emission-guard 必须照搬**（去重/filler/每轮一条）；WATCHDOG.md 发现；secret 脱敏 | 独立评审视角，抓主模型盲区 | advisor 拖慢主循环（增量快照 + 异步）；近窗口 promote 复杂 |
| 6 | **LSP 写操作** | 扩展 `crates/lsp` | ①`edits.rs` WorkspaceEdit 应用（重叠校验 + 自底向上）②rename 走 willRenameFiles ③code actions resolve + executeCommand ④诊断版本账本 | rename 不再漏 callsite（README 承诺兑现）；代码操作闭环 | 编辑应用顺序/重叠正确性需单测矩阵 |
| 7 | **resolve 暂存** | `crates/tools` ast_rewrite | ①ast_rewrite 先 dry-run 出预览 ②会话级 pending 队列 ③`write_file` 路径 `xd://resolve` 分发 ④应用时计数一致性复查 | codemod 安全；模型先预览后落盘，减少错误写入 | 预览/应用漂移（stale 检测）；与审批门禁重叠 |
| 8 | **eval 内核 + 桥** | 新 `crates/eval` | ①Python 子进程 NDJSON 内核（保留/重置/IdleTimeout）②HTTP 环回桥（token + 按 run 注册）③`__agent__` 等保留桥 ④工具面暴露 | 数据分析任务闭环（pandas/图表） | 桥安全（token 生命周期、abort 屏蔽）；Python 依赖环境 |
| 9 | **DAP 调试** | 新 `crates/dap` | ①DAP 客户端（帧/传输/握手）②会话树管理 ③适配器解析（先 lldb-dap/dlv/debugpy 三个）④28 ops 工具面 | debug 模式名实相符 | 工作量大；适配器行为差异大 |
| 10 | **pr:///issue:// + GitHub 缓存** | 扩展 fs.rs + github.rs | ①router 注册 pr/issue handler ②`gh` 封装（已有 github tool 可复用）③SQLite 缓存（auth 指纹 + TTL）④bash 变异失效 | GitHub 工具面收敛为一个 read | gh CLI 依赖；缓存一致性 |
| 11 | **provider 路由增强** | `crates/config`/`crates/llm` | ①roles 映射（default/smol/slow）②fallback chains（429/配额→换模型）③凭证轮换（session 亲和） | 稳定性 + 成本控制 | 冷却/恢复语义复杂；与现有 profile 体系整合 |
| 12 | **Collab 协议补齐** | `crates/collab` | ①帧类型扩展 ②write token 权限 ③快照分块 + 恢复 ④WS 退避 | 从 demo 到可用 | 协议演进成本（Gyre 已锁定密封布局，可兼容扩展） |
| 13 | **snapcompact**（P0-L） | `crates/context` | 计划文档已列：PNG 依赖 + 字体 + `ContentBlock::Image` 贯通 + **视觉模型 eval 验证召回率** | 压缩成本降一个数量级 | 前置三件套；召回率不达标会劣于 summarize |
| 14 | 规则导入（P2-N）/ mental models（P2-L）/ plan-mode（P2-M）/ goals（P1-M） | 各自 | 计划文档已列 | 迁移成本↓ / 跨会话沉淀 / 流程约束 / 预算控制 | 各有独立前置 |
| 15 | TUI 引擎 | ⛔ 不移植 | 只借鉴**回滚契约思想**（append-only 承诺）改进 Web 端 diff 推送 | — | 25k 行 TS 引擎移植成本与 Gyre 的 rustyline+Web 路线冲突 |
| 16 | 扩展/插件系统 | ⛔ 不移植 | 用配置驱动扩展（Gyre 的 `inventory` 编译期注册已是替代） | — | Rust 无 TS 模块动态加载 |
| 17 | omp commit | 可选独立 CLI | hunk 级暂存 + 拓扑排序（`git2` crate） | 提交质量 | 与 Gyre 核心无关，独立项目 |

---

## 三、优先级排序建议（结合 Gyre 实际需求）

排序原则：①填补 Gyre 真实空白且与现有架构契合 ②移植成本/收益比 ③与既有改进计划（P0-L/P1-L/P1-M/P2-N 等）衔接不重复。

### 🔴 P0 — 立即启动（小成本、大差异，计划文档未覆盖）

> **落地状态（2026-08-01）**：本组三项已全部落地——
> TTSR（`crates/ttsr` 新 crate + `run_loop` 五插入点集成 + `.gyre/rules` 发现 + `[ttsr]` 配置）、
> conflict://（`crates/tools/src/conflict.rs` + read `:conflicts` / write `conflict://N`）、
> magic keywords（`crates/agent/src/keywords.rs`：`ultrathink` 拉满思考预算 + `orchestrate` 契约注入，
> `workflowz` 需 task 工具；env `GYRE_MAGIC_KEYWORDS=0` 可关）。详见 [`agent-loop-improvement-plan.md`](agent-loop-improvement-plan.md) 第五轮。

**1. TTSR 流规则系统**
- 理由：oh-my-pi 最有辨识度的智能层，Gyre 计划文档**完全空白**；核心约 600 行纯逻辑 + coordinator 500 行；Gyre 的 cancel/中断/分支树基座全部就绪；`regex` crate 线性时间反超 JS 原版（消除 ReDoS）。规则零上下文成本与硬约束强制是"规则体系"的两难解。
- 示例：`.gyre/rules/box-leak.md` 配 `condition: ["(?i)Box::leak"]` + `scope: tool:write` —— 模型写 `Box::leak` 的瞬间流被 abort，注入"生产代码路径禁用 Box::leak，用 Arc\<str\>"，从同轮重试。（对应 omp 演示动画 `ttsr-poster.webp` 场景。）

**2. conflict:// 合并冲突解决**
- 理由：独立子系统约 600 行，纯状态机 + 内容重定位，无外部依赖；模型处理冲突从"读 200 行手抄"变"写 `@theirs` 一行"。收益立即可见、风险可控。
- 示例：`read_file :conflicts` 列出 `⚠ 1 conflict` → `write_file {path: "conflict://1", content: "@theirs"}` → 干净解决；`conflict://*` 批量收尾。

**3. magic keywords（ultrathink 先行）**
- 理由：约 250 行 + 3 个模板；Gyre 已落地 auto-thinking，`ultrathink` 直接接 `Effort::Max` 钳位，**半天工作量**；是用户可感知的"智能感"提升。
- 示例：用户输入 `ultrathink 分析这个死锁` → 分类器被跳过、思考预算拉到模型上限；`orchestrate` → 隐藏注入 10 条编排契约，子代理并行度提升。

### 🟡 P1 — 短中期（与已计划项衔接）

> **落地状态（2026-08-01）**：本组四项已全部落地——
> **web_search**（`crates/tools/src/web_search.rs`：DDG 免 key + 可选 searxng（`GYRE_SEARXNG_URL`）顺序回退链、
> arxiv/crates.io/npm/github 站点抽取、fetch_http 复用 + UA 头修复 crates.io 403）；
> **Advisor**（新 crate `crates/advisor`：EmissionGuard 去重/filler 过滤 + SecretObfuscator 脱敏 + WATCHDOG.md 发现，
> run_loop 每 4 轮独立评审注入 `[advisor:…]`；env `GYRE_ADVISOR=1` 启用）；
> **LSP 写操作**（`crates/lsp/src/edits.rs` WorkspaceEdit 应用器（重叠校验+自底向上+UTF-16）+ `lsp_apply` 工具
> （edits 直传 / code_action_index 两模式）+ code action command 执行）；
> **resolve 暂存**（`ast_rewrite preview:true` → `xd://pending`/`xd://resolve`/`xd://reject`，计数复查防漂移）。
> 详见 [`agent-loop-improvement-plan.md`](agent-loop-improvement-plan.md) 第五轮。

**4. web_search + 站点感知抽取**
- 理由：Gyre 唯一"无网"缺口；read 已具备 SSRF 防护的 HTTP 抓取，补 provider 链 + 高频站点抽取器（arxiv/crates/github/mdn 先行 10 个）。23 个 provider 不必全做，先 3-6 个。
- 示例：模型查"crates.io 上 xx crate 最新版本" → duckduckgo 无 key 搜索 → 命中 crates.io → 站点抽取器转结构 markdown → 直接引用版本号。

**5. Advisor 双代理评审（执行 P1-L）**
- 理由：计划已列但未动；调研确认关键细节：**emission-guard 是必备非装饰**（无去重会 spam，真实 issue #3520），WATCHDOG.md 是差异化载体。Gyre supervisor 只有观测，没有评审。
- 示例：主 agent 把 ENOENT 错误吞掉时，advisor 以 `concern` 注记提醒"修复不再匹配用户的验收标准"——主 agent 收到并纠正。

**6. LSP 写操作（WorkspaceEdit 应用 + rename_file + code actions）**
- 理由：Gyre README 自述"LSP 集成：重命名符号"，但 11 ops 只读、rename 不应用 edits——**承诺与实现有 gap**；`edits.ts` 移植 medium，正确性可用单测矩阵锁定。
- 示例：模型 `lsp rename` 把 `formatBytes` 改名 → 走 `workspace/willRenameFiles` → barrel 文件的 re-export 与 3 个引用文件同时更新 → `search` 确认 0 匹配。

**7. resolve 暂存（ast_rewrite 预览→应用）**
- 理由：Gyre ast_rewrite 直接落盘，codemod 出错即污染；暂存语义（proposed → 计数复查 → 原子应用）是低风险高安全收益。
- 示例：`ast_rewrite "console.log($$$)"` 返回 `(proposed) 3 replacements` → 模型检查后 `write_file {path: "xd://resolve", content: "替换 console.log"}` → 原子应用，要么全成要么全不。

### 🟢 P2 — 中期大工程（按资源排期）

> **落地状态（2026-08-01）**：本组六项已落地——
> **pr:///issue://**（`api_get_json` + auth 指纹文件缓存 `.gyre/cache/github/` + `render_gh_uri`，
> read_file 路由 `pr://owner/repo[/N]`；实测匿名读 rust-lang/rust PR 列表）、
> **规则导入 P2-N**（新 crate `crates/discovery` 零依赖：AGENTS.md/CLAUDE.md/Cursor .mdc/Cline
> 四来源转换注入 CLI context）、
> **Provider 路由**（`is_fallbackable` 错误分类 + `stream_fallback` 链 + registry 默认走 fallback）、
> **Collab 协议补齐**（帧扩展 Hello/Welcome/SnapshotChunk/Bye/Error + 16B write token 权限 +
> 48KiB 快照分块 + `~/.gyre/collab/` JSONL 恢复日志；WS 端到端冒烟通过）、
> **Provider 路由增强**（`ModelProfile.fallbacks` 模型级跨 api fallback 链 + `api_keys` 多 key
> 轮换环；防环校验、全链失败单 mistake、override 优先）、
> **Collab 浏览器 guest**（`/collab/{room}` GET 分发单文件 HTML：WebCrypto AES-GCM 对偶
> codec、全帧渲染、快照分块聚合、`?wt=` 可写/只读、断线指数退避；浏览器端到端冒烟通过）、
> **Collab host 裁决**（服务端即 host 进程：内存保留房间密钥 + 规范令牌，WS 桥按连接 wt
> 单播密封 Welcome 裁决帧；guest 页按裁决收编输入——错误/过期 wt 判只读；三链接形态冒烟通过）、
> **Collab transcript 下发**（`/api/collab/room?sid=` 绑定 agent 会话，WS 桥把会话历史渲染为
> 快照块分页推送——`Welcome.entry_count` 告知块数、guest 聚合渲染全史；Web UI 自动携带 sid）。
> 详见 [`agent-loop-improvement-plan.md`](agent-loop-improvement-plan.md) 第六至九轮。

**8. eval 双内核 + 环回桥** — 数据分析硬缺口；Python 子进程 + NDJSON + token 桥，桥安全模型照搬。
**9. DAP 调试器** — debug 模式名实相符；先 lldb-dap/dlv/debugpy 三适配器，28 ops 不必一次做完。
**10. pr:///issue:// + GitHub SQLite 缓存** — 工具面收敛；gh 封装已有基础。
**11. Provider 路由增强** — fallback chains + 凭证轮换，稳定性/成本红利；与 config/llm 现有结构整合。
**12. Collab 协议补齐** — 帧协议 + 权限 + 快照恢复；密封布局已兼容。
**13. snapcompact（P0-L）** — ROI 最高但前置三件套（PNG 依赖 + Image 块贯通 + 视觉模型 eval），按计划文档作为独立攻关会话。
**14. 规则导入（P2-N）** — 迁移成本杠杆；8 种格式解析 breadth 型工作。

### ⚪ P3 — 远期/按需

**15. mental models（P2-L）** 与 P2-1 向量记忆合并设计（存储后端先定）；**16. plan-mode（P2-M）**、**goals（P1-M）**（用户体验提升，非核心差异）；**17. /fresh、fork 增强**（小）；**18. shell completions**（小，价值低）；**19. omp commit**（独立 CLI，与核心解耦）。

### ⛔ 不建议

- **TUI 引擎移植**：25k 行 TS 差分渲染器 vs Gyre 的 rustyline+Web 双前端路线冲突；仅借鉴"append-only 回滚契约"思想到 Web 端 diff 推送。
- **扩展/插件系统**：Rust 无 TS 模块动态加载；Gyre 的 `inventory` 编译期注册已是等价替代。

---

## 四、验证方法与注意事项

1. **TTSR 的正确性验证**：单测覆盖 abort 竞态（retry token + generation 守卫）、`contextMode: keep` 下 assistant 尾续写合法性、压缩后抑制状态保留（`ttsr_injection` 元数据走分支日志而非 LLM 上下文——这是 omp 踩坑后的关键设计，勿改）。
2. **conflict:// 的边界**：CRLF 往返、2-way @base 显式报错、内容重定位对行漂移的容忍、边界回声修剪上限（MAX_ECHO_LINES=12）。
3. **web_search 的坑**：omp 自己的文档已过期（25 vs 23 provider）——移植时以**源码 `SEARCH_PROVIDER_OPTIONS` 为准**；抽取器是易碎品，每个配 fixture 单测。
4. **advisor 的坑**：无 emission-guard 的 advisor 会 spam（真实 issue #3520）；近窗口 promote 是复杂度大头，MVP 可先固定模型档位。
5. **snapcompact 的坑**：帧形状必须 eval（SQuAD 数据：差的形状 f1 0.287，比文本摘要还差）；Gyre 计划文档的前置三件套评估准确，勿跳过。

---

## 五、结论

Gyre 已把 omp 的循环引擎吃透（五轮对比落地，见 [`agent-loop-improvement-plan.md`](agent-loop-improvement-plan.md)），真正的差异化空白在**智能层**：

- **P0（即做）**：TTSR（流规则）、conflict://、magic keywords
- **P1（短中期）**：web_search、Advisor（P1-L）、LSP 写操作、resolve 暂存
- **P2（中期）**：eval 内核、DAP、pr:// 体系、provider 路由、Collab 补齐、snapcompact（P0-L）、规则导入

建议以 **TTSR 为下一里程碑**：它同时是成本优化（规则零上下文）、正确性强化（硬约束）与差异化卖点，且 Gyre 现有基座（中断、分支树、ast）恰好完备。
