# Gyre × oh-my-pi 差距分析报告（2026-09-06 · 第三轮逐文件核实）

> 调研对象：`third/oh-my-pi`（badlogic/pi-mono fork 快照 v18.1.2）vs 本仓库 Gyre（Rust workspace 33 crates，HEAD `1bdc14d`）。
> 方法：10 路并行只读 scout 分域逐文件核实（Cmds / Tools / Config / Ext / Prompts / Proto / RuntimeCore / FeatureMods / Build / TestsWeb），以 `docs/oh-my-pi-gap-analysis-2026-09-05-v2.md`（六轮修复声明为基线先验）逐项对当前工作树代码取证，每条结论要求双侧 file:line 证据；main 另直读复核 6 项承重结论（vendor 依赖边、会话 header、read 选择器、ManageCmd 子命令、保留字防误注入、RPC 命令面）。完整结构化产出存档 `agent://Cmds`、`agent://Tools`、`agent://Config`、`agent://Ext`、`agent://Prompts`、`agent://Proto`、`agent://RuntimeCore`、`agent://FeatureMods`、`agent://Build`、`agent://TestsWeb`。
> **工作树状态警告**：`vendor/`（pi-shell 173 文件、pi-builtins 130、brush-core 106、pi-walker 6）已从磁盘整体删除且**未提交**；根 Cargo.toml 仍声明 5 处 vendor 引用（`Cargo.toml:174-177` path deps + `:179-180` `[patch.crates-io] brush-core`），`cargo metadata` exit 101——**当前工作树不可构建，全部 CI 门（fmt/clippy/test/deny）与发布链阻断**。本报告全部结论为静态读码，未运行任何构建/测试。处置方案见 §〇。

> **修复落地（报告完成当日稍后）**：§三 #1（switch_branch 显式报错）、#2（`type = "sse"`
> 配置面激活 legacy SSE 传输）、#4（supports_thinking 统一透传 `enable_thinking`）与 §五
> H10 前半（ACP authenticate/logout 空 Ok 改显式错误）已修复，并通过全仓 fmt /
> clippy（`-D warnings`）/ test 门禁；新增 3 个回归测试。vendor/ 已按 §〇 路线 A 恢复。
> 备注：vendor/pi-shell 信号测试为并发时序 flake（单跑稳定），与本轮改动无关。

> **H5 流式守卫落地（同日）**：§五 H5 完成——生成文件拦截（文件名 10 模式流中早拦 +
> 头部 marker 执行前兜底，`agent_tools::generated_guard`）、apply_hashline 流式解析
> 预检（增量 JSON 字符串解码 + `parse_hashline` 必败判定）、跨轮同参工具循环守卫
> （omp ToolCallLoopGuard 全语义：canonicalize/intent 剔除/exempt/exactly-once）、
> Gemini 推理标题连跑中断（阈值 24 + 闩锁 + 通道切换重置）。`[agent.stream_guards]`
> 配置面 + 25 个新单测。有意偏差：omp 触发后整轮 abort → Gyre 沿用 TTSR 管线注入
> 修正自我修复（上限 3 轮后守卫静默）；omp removed-lines 内容预检不移植（与 hashline
> stale 恢复冲突）。

> **H3 异步后台作业落地（同日）**：§五 H3 完成——`agent-core::jobs::AsyncJobManager`
> 本体（omp 语句级对照：注册/上限/id 解析/owner 取消/投递退避重试/死信/watch·
> acknowledge·consume 抑制面/resume 恰好一次/保留期逐出/轮询阶梯，13 单测）、
> `run_command async:true` 参数面（作业自持取消令牌，立即返回 id）、hub 工具
> jobs/cancel/wait ids 作业面（watch 抑制投递 + 阶梯等待）、cli/rpc/server 三装配
> 点 + owner sink 投递回会话 + `[agent] async_enabled/async_max_jobs` 配置面
>（默认 true/15）。有意偏差：非零退出按 Gyre 惯例计 completed（结果带 `[exit N]`
> 前缀），omp 记 failed；idle 会话的投递落在历史中、由下一轮消费（无 idle 唤醒），
> omp 经 yield 队列空闲 flush 主动拉起 follow-up turn。

> **H15 MCP 收尾落地（同日）**：见上表。端到端验证（真实 REPL + 真实 stdio MCP server）：
> 8s 握手的 server 下 REPL 1.4s 就绪（预算 250ms 生效，日志 `启动预算内未完成握手，先用缓存
> 快照注册 Deferred 工具 tools=2`）→ `/mcp` 显示 `mcp__slow_echo［延迟连接］` → 后台连接完成后
> 同一清单转为 live 并列出 `MCP 提示词（斜杠命令）：/slow:greet` → `/slow:greet who=世界 tone=正式`
> 经 `prompts/get` 注入下一轮（模型可见 `SLOWPROMPT who=世界 tone=正式`）→ 模型触发的
> `mcp__slow_ping` 调用到达审批门禁（证明动态源 get/specs 在运行中 Agent 生效）。
>
> **H6 会话级结构化事件落地（同日）**：§高优 H6 完成——见上表。端到端验证：
> 脚本化 WS 桩服务（真实前端 + 真实 WS，仅帧序列受控）下顶栏徽标按
> `auto_retry_start(1/2)` → `auto_retry_end` 清除 → `auto_compaction_start` → 清除
> 依次翻转，展示行经 `Say` 双通道同步渲染。
>
> **H14 grep 参数面落地（同日）**：§高优 H14 完成——`agent-search::grep_opts`
> 全参数化（case/gitignore/hidden/单文件帽/总量帽精确截断/4MB 前缀窗口/30s 遍历
> 截止/跨行模式/绝对路径命中）+ `grep` 工具 omp 合同（`path` 字符串|数组、`;`
> 多目标、glob 条目展开、`case`/`gitignore`/`skip` 翻页、单文件 200 vs 多目标
> 20×20 窗口、`已显示文件 X-Y / 共 Z` 翻页提示、缺失/超大/溢出警示注记）+ 分组
> 锚点输出（`[路径#指纹]` 段头入快照库，apply_hashline 可直接锚定，与 read 同
> 约定）。**顺带修复两个既有正确性 bug**：(1) `ignore` crate `hidden(true)` 语义
> 反向——旧实现把隐藏文件**排除**（omp 语义是搜索隐藏文件）；(2) 旧 4MB 文件整
> 体跳过（pi-natives 是前缀窗口部分覆盖）。有意偏差：`:N-M` 行选择器与内部 URL
> 搜索面未做（H14 范围外）；`grep.contextBefore/After` 设置项无对应基础设施
>（常量 0）；总量帽输出侧精确截断（omp native 软帽语义）。

---

## 〇、P0：vendor 断链（需决策）

- 唯一直接消费者：`crates/shell`（`Cargo.toml:8-10` 依赖 pi-shell/pi-builtins/brush-core）——即 run_command 的 brush 主引擎（commit `932399f`）。但 workspace 级引用使**整个** workspace 元数据解析失败。
- 路线 A（推荐，最小动作）：`git restore vendor/`。HEAD 已含四目录（Cargo.lock 四条目佐证），零代码变更，Cargo.lock 不动，恢复后 workspace 即可构建。
- 路线 B（完成移除）：删根 manifest 5 处引用 → 重写 `crates/shell`（弃用 brush/pi-shell 进程内引擎，降级为系统 shell 单引擎；`crates/pty` 已有 portable-pty 原生实现可参考）→ 重生成 Cargo.lock → deny/about 许可面复核（`about.toml:9-14` 四 fork 注记失效）。工作量大，损失 50+ 进程内 coreutils 零 fork 特性。
- 恢复/定案前**禁止打 tag**（release 工作流必挂）。
- 基线不确定项 #1（vendor/pi-builtins 内容漂移方向）就此关闭：不可 diff，改判为「vendored 源缺失、构建断裂」。

## 一、结构与模块映射（现状刷新）

| | oh-my-pi | Gyre |
|---|---|---|
| 规模 | TS 18 packages（coding-agent 为主体）+ Rust 8 pi-crates + vendor | 33 crates（vendor 4 现处删除态） |
| CLI 入口 | `cli.ts` + `cli-commands.ts`：41 主子命令 + 3 别名（img/wt/q）+ RESERVED 词表 | `crates/cli`（bin `agent`）：15 flags + trailing task；**管理子命令面已落地**（手写路由非 clap subcommand）：`agent models list`、`auth list\|save\|remove\|login\|logout`、`mcp login\|logout`、`config path\|check`（`manage.rs:52-156`）；23 词保留字防误注入已实装（`main.rs:1865-1927`） |
| REPL | `modes/interactive-mode.ts`（自绘 TUI + keybindings.yml） | rustyline REPL，34 条内置斜杠命令 + `/skill:` 前缀 + .md 自定义命令 |
| RPC | `modes/rpc`：RpcCommand **42 变体** + ready 协商 + 宿主反向通道 | `crates/cli/src/rpc.rs`：**12 命令**（prompt/cancel/ping + get_state/set_model/set_thinking/get_messages/compact/get_usage/get_tree/switch_branch/list_models）+ ready v2 握手/分片 + `--rpc-forward-ask` 反向通道 |
| ACP | `modes/acp`：全量方法面 | `crates/acp`：stdio+HTTP/SSE 双传输，方法子集（见 §四） |
| Server | —（collab-web 静态站） | `crates/server`：axum 27+ REST + WS + rust-embed 前端（Gyre 超集面） |
| 测试 | 行为级测试文件 ≈700±100（tools 154 / ai ≥205 / mcp 36 / collab 14 / acp 13 / slash 29 / task ~37） | 内联 `#[test]` 1213 + 集成 25（mcp 9/shell 10/proxy 6）；telemetry 0、web 前端 0 |
| 提示词资源 | prompts 全树 ≈200 个 .md（tools 44 / system ~65 / agents 8 / goals 6 …） | prompts/ 7 个 .md + `crates/core/src/prompt_sections.rs` 四段系统章节 + 内联通知字符串 |
| 文档 | docs/ 100+ 文件（settings.md 100KB、environment-variables.md 102KB、per-tool 30 篇…） | docs/ 15 文件（14 为分析报告，唯一直接用户文档 `docs/rpc.md`） |

## 二、基线（09-05 v2）以来已核实落地的修复

以下各条均经当前代码直读取证（非转抄修复轮声明）：

**工具域**：① hashline 闭环三环节贯通（read 段头 tag `fs.rs:427-436` / 共享快照库 `hashline/tool.rs:29-45` / edit stale 校验+恢复 `:130-183`，有测试）；② read 行选择器族 `:N/:N-M/:N+K/:N-/多区间/:raw`（`fs.rs:172-303`；`:img` 有意分流 read_image）；③ update_tx 生产者（`write.rs:84-95` 统一写路径 + apply_hashline 逐区段 partial，覆盖面大于声明）；④ web_search 链 Tavily→Brave→DDG→SearXNG + recency 原生映射 + 401/429 fallback（`web_search.rs:108-173`）；⑤ hub 13 op 全量含进程托管 6 op（`hub_tool.rs:172-176,519-525`，真实 /bin/sh 回路测试）。

**命令域**：⑥ 23 词保留字防误注入（`main.rs:1865-1927`，含 omp #4845 原例回归测试）；⑦ 管理子命令面（models/auth/mcp/config，见 §一）；⑧ /tree /branch REPL+RPC+Web 三端（`repl.rs:315-331`、`rpc.rs:877-925`、`server/lib.rs:2802-2821`）。

**扩展域**：⑨ 技能跨工具 provider 四实现 + `[skills.providers]` 开关 + 同名去重（`skills/providers.rs:26-100`）；⑩ MCP OAuth 全链（RFC 9728/8414 发现 + DCR + PKCE + RFC 8707 + 刷新 + 401 单飞重试，`mcp/oauth.rs:405-1105`，axum 仿真 e2e）；⑪ MCP 重连退避+爆发熔断+半开探活（常量逐一对齐 `reconnect.rs:29-33`）；⑫ MCP 工具清单磁盘缓存+stale 回填（`cache.rs:60-150`，无 hash/TTL 为半边）；⑬ server instructions 注入 system prompt（`main.rs:798-802`）。

**协议域**：⑭ RPC ready v2 握手+出站分片+入站重组（`rpc.rs:325-540`）；⑮ RPC 命令面 12 条；⑯ `--rpc-forward-ask` 审批反向通道（`rpc.rs:1243-1360`）；⑰ ACP tool_call 真实 id+kind+并行区分测试（`adapter.rs:37-85`）；⑱ ACP available_commands 15 条注入（`main.rs:229-252`）；⑲ ACP 权限四选项机 allow/reject×once/always（`rpc.rs:389-421`）；⑳ OAuth 三流（anthropic/openai-codex/zai-coding-plan）+ 刷新引擎 + oauth.toml(0600)/auth.toml/env 四段解析链 + CLI（`llm/oauth/`、`config/auth.rs`）。

**运行时域**：㉑ 会话文件 header+版本化 v1 骨架+legacy 兼容+版本过新报错（`persistence.rs:21,42-108`）；㉒ 空停有界重试 3 次+瞬时错误白名单恢复+截断/错误中断按工具形态分流占位（`engine.rs:816-1055`，行为测试 20 条）；㉓ 429 同模型退避 ≤3 次/30s（`engine.rs:540-568`）；㉔ 悬空 tool_call 出口净化（`context/lib.rs:738-782`——基线记缺失，实为已落地）；㉕ engine 行为测试族（`agent/lib.rs:1037-2468`）。

**提示词/安全域**：㉖ 系统提示词四段（Tool Policy/Delegation/Workflow 六阶段/Delivery+Critical）装配顺序正确（`prompt_sections.rs:31-153`、`main.rs:784-796`）；㉗ secrets 管线级双向脱敏（HMAC 占位 + provider 边界覆盖 User/Tool/Assistant + `GYRE_SECRETS=off`，`core/secrets.rs`，round-trip 测试）。

**构建域**：㉘ 发布矩阵 5 腿（含 musl×2 experimental + darwin-arm64 + win32）；㉙ SHA256SUMS+发布后下载验证（完整性门严于 omp）；㉚ cargo-deny 三硬门 + about.toml；㉛ CI 硬化（concurrency/paths/dispatch/npm ci/最小权限）；㉜ max_turns 文档一致性；㉝ MCP http/stdio 传输层测试补齐。

## 三、修复声明与代码不符（本轮新发现，共 13 项）

| # | 项 | 声明 | 代码事实 | 影响 | 级 |
|---|---|---|---|---|---|
| 1 | switch_branch handoff 静默降级 | 第三轮：「summarizer 未注入→明确报错，不做静默降级」 | `context/lib.rs:365-371` `_ => None` 静默无摘要切换 | /branch 用户以为离支进展已交接，实际新分支无衔接 | 中 |
| 2 | ~~legacy SSE 传输不可达~~ **✅ 已修复（同日）**：`[mcp.servers.<name>]` 新增 `type = "sse"`（`transport` 别名等价），连接/重连分派接线（`client.rs:178-180`、`reconnect.rs:107-114`），附真实 axum legacy SSE 端点集成测试 | 第三轮：「legacy HTTP+SSE 传输回退」落地 | 见左 | ~~传输体成 dead code~~ 已可接入 | 中 |
| 3 | ~~MCP 工具变更不达运行中 Agent~~ **✅ 已修复（同日）**：`agent_tools::ToolSource` 动态源 + `McpToolSource`，`ToolRegistry::get` 改回 `Arc<dyn Tool>`，engine 每轮 `specs()`/每次 `get()` 实时求值——tools/list_changed、后台连接完成、重连恢复都在下一轮生效（父/子 Agent 注册表统一走动态源） | 第一轮：「tools/list_changed 刷新+回调」 | 见左（`on_tools_changed` 仍保留给宿主做展示/记账） | ~~运行期增删对模型不可见~~ 已可见 | 中 |
| 4 | supports_thinking 路径分叉 | —（新发现） | CLI/RPC 恒 false（`main.rs:2176`），Web 路径随 `[agent].enable_thinking` | 同一 profile 不同入口思考能力不同；ultrathink 被 CLI 短路 | 中 |
| 5 | ACP 命令目录手抄漂移 | —（新发现） | `acp_command_catalog` 15 条手工表 vs REPL 实有 30+（`main.rs:229-252`） | /paste /review /swarm 等未对编辑器暴露 | 中 |
| 6 | AssistantEvent::MessageUpdate 死变体 | —（新发现） | 枚举在（`core/llm.rs:267`）但五适配器只 yield MessageStart，零生产者 | 名义 partial 快照能力实际不存在 | 低 |
| 7 | 上下文注入双通道双格式 | —（新发现） | native .agent/AGENTS.md 裸文本 vs discovery 外来配置异构前缀并存；walkup 双系统各跑一遍；无 auto-loaded 条款（`config.rs:1030-1064`、`discovery/lib.rs:299-311`） | monorepo 父链 AGENTS.md 全量重复注入；模型可再扫描 | 中 |
| 8 | advisor 独立简化脱敏器 | 第二轮：secrets 管线级 | `advisor/obfuscator.rs` 仍为小 pattern 集 regex，未接 SecretsObfuscator（omp advisor 与全链共用同一 obfuscator） | GYRE_ADVISOR=1 时第二模型调用漏掩 | 低 |
| 9 | 保留词提示文案滞后 | —（新发现） | `main.rs:1921-1925` 提示未列本轮新增的 auth login/logout、mcp login/logout | 可发现性 | 低 |
| 10 | workspace 死条目三连 | 第一轮：已清 tree-sitter | `Cargo.toml:52,165,176` ab_glyph/schemars/pi-walker 零消费者（ast crate 本地声明自有版本） | 清单腐化；pi-walker 是 vendor 断链引用面之一 | 低 |
| 11 | RPC 无入站协商 | —（新发现） | 仅 ready 单向宣告，无 negotiate_protocol（`rpc.rs:325-332`） | v1-only 客户端遇 >1MiB 出站帧不可读 | 低 |
| 12 | get_tree 全量序列化 | —（新发现） | 无分页/过滤参数（`rpc.rs:877-885`） | 长会话（数千节点）开销 | 低 |
| 13 | expand_env 无默认值语法 | —（新发现） | `${VAR}` 未设静默空串，无 `:-default`/告警（`config/env.rs:7-31`） | 配置错误被掩盖 | 低 |

## 四、分域仍存缺口（择要；完整逐条见 agent:// 产出）

- **交互（高）**：可定制键位系统、双击 Esc 回退、**Esc 分级中断整缺——运行中唯一输入是 steering，交互路径无 SIGINT 处理，Ctrl-C 直接杀进程**（`main.rs:2684-2753`）；消息队列键位。前置架构决策：raw 模式按键层（crossterm）。
- **工具（高）**：bash async 参数+后台簿记（依赖 async job manager 本体，仍缺）；grep 参数面（case/gitignore/skip/分页/20×20/2000 帽/超时——50 硬截断、hidden 写死）；task 参数面（name/agent/effort/schemaMode/isolated）；隐藏工具 yield/goal/think；manage_skill/computer/tts/inspect_image。read 富格式缺 PDF/SVG/reader-mode（elision footer 已落地）。
- **工具（偏差，可保留）**：edit 单 hashline（无 replace/patch 兼容入口）、todo 动词集、ask 单问、checkpoint/rewind 语义、github 操作集互缺、lsp 缺 7 动作、browser 单实例、eval 缺 rb/jl、glob 单参、memory retain 单条+edit 无 update/invalidate、工具 discoverable 档+统一超时表。
- **配置（高）**：Settings 运行时单例/热更新/内省/config get-set（仅 path/check 两只读动作）；自定义 provider 配置面（headers/compat/auth none/keyless/modelOverrides——仅 extra_body）。**（中）**：模型静态目录、运行时发现广度（3 线协议 vs 6 类）、模型选择语法（:thinking 后缀/模糊别名）、采样面、!cmd、.env 加载、`--config` overlay、fallback 冷却/回切/留痕、SQLite 凭据多账号/粘性/记账、retry 参数可配置、disabledProviders、MCP 行为开关。
- **扩展（高）**：进程内扩展系统（结构性不可 1:1，替代形态未推进）；hook 事件面（缺 session_*/branch/compact*/input 改写/stop continuation；已落 deny+result 改写+turn_end）；文件式 hook 为 shell 命令形态（TS 模块形态不适用）。**（中）**：agent-plugins 标准包/市场链/插件运行时/自定义工具目录/managed skills/热重载；MCP prompts/list→斜杠、Deferred 延迟连接（启动仍被慢 server 串行阻塞）、.mcp.json 多源+disabled 名单+/mcp add 写回；技能 frontmatter 严格校验。
- **协议（高）**：RPC 命令面 12 vs 42（30 个仍缺，含 steer/new_session/set_todos/bash/export_html/login 等）；会话级结构化事件通道整缺（自动压缩/重试/model_changed 仅自由文本 status 行）；openai-responses 真 wire；线协议长尾族（bedrock/vertex/cursor/devin…按需）；collab proto=1 vs 3（产品决策）；ACP authenticate 空实现（危险：客户端误以为就绪）+session list/resume/fork+文本模式斜杠拦截（编辑器发 `/compact` 会整句发给 LLM）。**（中）**：RPC negotiate+事件映射宽度（tool_call 无 id、tool_result 仅 ok 布尔、无子代理帧）+宿主反向通道完整面；ACP fs 桥/plan/current_mode update 变体/always 持久化；流式块边界；Usage 字段扩展（reasoningTokens/缓存 TTL 分解/credits）；redactedThinking 回放丢失（严格 provider 400 风险）；AgentStart；SDK 一站式装配（engine 级 AgentBuilder 已有，会话级三处装配漂移——RPC auth 链曾因此漏接）。
- **运行时（高）**：流式守卫（StreamingEditGuard/ToolCallLoopGuard）完全缺位；子代理定义化（[[agents]] 发现/spawn-policy/只读白名单/软预算/grace）；worktree 隔离接线（iso 原语零生产调用方，engine 收尾分支不可达）；async job manager 本体。**（中）**：fork 语义（纯文件复制）；树导航剩余语义（ask 重答/filter/label/交互选择器）；resume 完备性（--continue/breadcrumb/外部导入）；fallback selector 冷却/回切/留痕；todo eager prelude/完成提醒/mid-run 对账；自动标题；持久复活；失败轮耐久丢弃；统一 OutputSink；task_tool 零测试。
- **外围（已定论）**：语音栈=纯绿地（基线不确定项 #6 关闭：crates/tts、crates/stt 从不存在，ttsr 为同名异义的流规则引擎）；**（中）**：advisor 专用模型（现伪独立=主模型 clone）/指纹去重/增量转录；goals objective+goal 工具（现仅预算记账）；plan-mode 闭环（现仅写限制+手动 /mode code）；SSH ControlMaster/传输（现冷连）；export/share（全缺）；web provider 生态（链已 4 家 vs omp 13+/查询规划/scrapers）；internal-urls 路由层+7 scheme；memories 后台 rollout（同步 hook 内联 vs lease/heartbeat）；commit 模块；markit；telemetry metrics/logs（仅 trace）；snapcompact provider 感知形状+CJK '?'。
- **构建（高）**：一键发布编排（无 xtask/scripts，手改版本打 tag）；NOTICES 从未生成且不随 release 发布（`rust.yml:354-355` 自注）；矩阵缺 linux-arm64-gnu 与 darwin-x64；RUSTSEC 5 项 advisories 软门待 triage。**（中）**：curl|sh 安装器、Docker、justfile、bench 执行门、macOS 签名/brew/nix、actionlint（双方皆无）。
- **测试/前端/文档（中）**：最薄弱五面=web 前端（0 测试 0 lint）/RPC v2 行为测试/ACP conformance/collab 会话语义/CLI 斜杠分发；task_tool 子代理委派零测试；前端 P2 审计 12 项实况=3 修/2 半修/5 未动/2 未查；M7 ask options web 半边、M8 per-tool 工具卡、M9 消息队列、M10 助手/工具结果图像回显仍缺；README 三语对 OAuth/hooks/会话树/模型角色零覆盖；~90 篇用户/开发者文档无对应物（最实在：tools per-tool 30 篇、mcp-* 4 篇、settings/env/skills）。

## 五、分级缺失清单（汇总）

### 高优先（正确性风险 / 平台承重 / 生态入口）

| # | 项 | omp 锚点 | Gyre 现状 | 建议 |
|---|---|---|---|---|
| H1 | **vendor 断链修复（P0）** | — | 工作树删除未提交，cargo metadata 失败 | 路线 A `git restore vendor/` 或完成路线 B（§〇） |
| H2 | Esc 分级中断 + 双击 Esc 回退 + 可定制键位 | `input-controller.ts:257-258,446-450`、`config/keybindings.ts` | 全缺；Ctrl-C 杀进程 | 先定 raw 模式按键层架构（crossterm），短期可做 RPC cancel 的 REPL 关键字/信号桥 |
| H3 | bash async + async job manager 本体 | `bash.ts:824-1102`、`async/job-manager.ts` | 参数与簿记全缺 | `agent/jobs.rs` + supervisor 挂载 + bash async 参数 |
| H4 | 子代理定义化（发现/spawn-policy/只读白名单/软预算/task 参数面/隔离接线） | `task/discovery.ts`、`read-only-policy.ts:10-29`、`executor.ts:107-125`、`worktree.ts` | 单一匿名 task，全缺 | [[agents]] + 白名单工具注册表 + iso 接 task isolation |
| H5 | 流式守卫（编辑预检/生成文件拦截/同参循环检测） | `stream-guards.ts:41,367-404` | 完全缺位 | engine 流式段接入；TTSR 不覆盖此语义 |
| H6 | ~~会话级结构化事件通道~~ **✅ 已落地（同日）**：`agent-core::SessionEvent`（压缩 start/end、重试 start/end、fallback applied/succeeded；omp wire 同形）+ `AgentEvent::Session` 与 `Say` 双通道（展示文本由结构派生，不漂移）+ engine 三处挂点（压缩失败不再被 `let _ =` 吞）+ `ServerFrame::Session` → Web 前端顶栏维护徽标（重试中 n/m、压缩中）/ RPC `kind=session` / ACP 跳过；剩余：`model_changed`（Gyre `set_model` 为宿主侧重建，已由 RPC 响应字段承载，非引擎事件）、`todo_reminder`/`thinking_level_changed`/`goal_updated` 等无对应运行时故不设空壳变体 | `agent-session-events.ts:18-61` | 双通道已落地 | ~~扩 SessionEvent~~ 完成 |
| H7 | openai-responses 真 wire（+按需 bedrock/vertex 等） | `providers/openai-responses.ts` | 诚实未实现 | 枚举位已预留，复用 SSE 助手 |
| H8 | Settings 单例/热更新/内省 + config get/set | `settings.ts`（3255 行） | 启动一次加载；仅 path/check | 分层 Settings 结构 + mtime 检测起步 |
| H9 | 自定义 provider 配置面（headers/compat/auth none/keyless/modelOverrides） | `models-config-schema-bundle.ts:298-329` | 仅 extra_body | ModelProfile 扩 headers+compat 位表 |
| H10 | ACP authenticate 实装 + session list/resume/fork + 斜杠拦截 | `acp-agent.ts:676-772` | authenticate 空 Ok；斜杠整句发 LLM | 先删危险空实现（返回 method not supported），再补 login 路由 |
| H11 | RPC 命令面扩容（30 个仍缺）+ negotiate_protocol + 事件映射宽度 | `rpc-types.ts:28-119` | 12 命令；tool_call 无 id | 沿 RpcEnvelope 分批；先 steer/new_session/set_todos/get_messages_page |
| H12 | CLI 子命令层广度 + shell 补全 + 提示文案同步 | `cli-commands.ts:22-233` | 12 管理动词 | usage/token/stats/gc/completions 先行 |
| H13 | 斜杠命令族：生命周期（/clear /handoff /shake /rename /pin /move /drop）、REPL /login /logout、/memory、/mcp 管理、/todo append、/resume 无参选择器 | `builtin-lifecycle.ts` 等 | 34 条中上述全缺/只读 | 沿已有会话/凭据 API 补 REPL 分发 |
| H14 | ~~grep 参数面~~ **✅ 已落地（同日）**：case/gitignore/skip 翻页、20×20 窗口+2000 精确帽、30s 遍历截止、4MB 前缀窗口+partial 提示、跨行模式、`.git` 跳过、hidden 语义修复、锚点分组输出、glob/分号多目标；剩余：`:N-M` 行选择器与内部 URL 搜索面、`grep.contextBefore/After` 设置项（omp 有；Gyre 无对应设置基础设施，常量 0） | `grep.ts:87-92` | 参数面全量 + 窗口化已落地 | ~~search crate 参数化~~ 完成 |
| H15 | ~~MCP 收尾~~ **✅ 已落地（同日）**：（1）Deferred 延迟连接——并发连接 + 250ms 启动预算（`McpLoadOptions`），预算外 server 转后台且用**适用**缓存快照先注册 Deferred 工具（元信息可见、执行时等连接就绪），后台完成后无缝换 live 并 fire 监听器（omp #2100 语义）；（2）缓存 hash+TTL——快照带 `version`/`config_hash`（只覆盖语义字段）+ 30 天 TTL，旧格式失效，注册表自理落盘（`store_tools`/`hydrate_from_cache` 已删）；（3）prompts/list+get → 斜杠命令 `/<server>:<prompt> k=v`（对齐 `buildMCPPromptCommands`），`prompts/list_changed` 接入刷新，补全表每轮重建；（4）tools 变更达运行中 Agent——`ToolSource` 动态源 + `McpToolSource`（`get` 改回 `Arc<dyn Tool>`），父/子 Agent 注册表统一动态，server 端增删下一轮即生效；（5）修复 Web/ACP 会话主 Agent 完全拿不到 MCP 工具；（6）`.mcp.json` 多源（user + `.agent/mcp.json` + `mcp.json`/`.mcp.json`）、`disabledServers` 拒绝名单、`agent mcp add|remove|enable|disable|list|status` 写回（§三 #2 SSE 判别已在同日修复）；SSE 判别激活 | `manager.ts:680-721,1235-1238`、`client.ts:81-82` | 全部落地（Deferred/缓存/prompts/动态工具/多源） | ~~缓存加 hash+TTL+Deferred；配置加 Sse 变体~~ 完成 |
| H16 | 发布链收尾：一键编排 + NOTICES 生成发布 + 矩阵补 arm64-gnu/darwin-x64 + RUSTSEC triage | `release.ts`、`ci.yml:813` | 手工发布；NOTICES 缺 | xtask release + about generate + deny advisories triage |
| H17 | SYSTEM.md 覆盖链 + --append-system-prompt | `system-prompt.ts:487-505,862-868` | 全缺 | config 键 + 装配双路径 |
| H18 | 进程内扩展替代形态（Rust hook 扩展/子进程扩展协议） | `extensibility/extensions/types.ts`（1786 行） | 仅 Hook trait | 架构决策后立子进程协议 v0 |
| H19 | hook 事件面广度（session_*/branch/compact*/input 改写/stop continuation） | `shared-events.ts:31-160` | 3 事件+turn_end | HookEvent 枚举扩容，engine 挂点补齐 |
| H20 | 回归测试网：task_tool、RPC v2、ACP conformance、web 前端基建 | test/ 700 文件 | 四面零/近零 | 每面最小行为测试先行 |
| H21 | collab 互操作 proto=3（产品决策） | `wire/src/index.ts:397` | proto=1 自有 | 决策后重写 frame.rs 或文档声明独立协议 |

### 中优先（能力纵深 / 体验差距，~60 项按组）

- **配置组**：模型静态目录/发现广度/窗口探测；模型选择语法（provider/id、:thinking 后缀、模糊别名）；角色内置集+元数据+config 示例段；采样 top_p/top_k；!cmd 凭据；.env 加载+${VAR} 告警+:-default；--config overlay；fallback 冷却/回切/留痕+usage-aware+全局 chains；SQLite 凭据多账号/粘性/记账；retry 参数可配置；per-model thinking 元数据（supports_thinking 分叉修复）；security/secrets 配置段；workspace.additionalDirectories；MCP 行为开关；disabledProviders。
- **协议组**：RPC 事件映射宽度（tool id/结果文本/子代理帧）；宿主反向通道完整面（host_tool_call/uri_request/extension_ui）；ACP fs 桥/plan/current_mode/locations/always 持久化；流式块边界（text/thinking start/end）；AgentStart+MessageUpdate 生产者；Usage 字段扩展；redactedThinking 回放；缓存 TTL/prompt_cache_key；compat 层扩位；SDK 一站式会话装配（消除三处装配漂移）；模型身份集中分类表。
- **运行时组**：fork 语义；树导航剩余语义（ask 重答/filter/label/交互选择器；handoff 静默降级改显式报错）；resume 完备性（--continue/breadcrumb/@claude 导入）；中断 cancel 路径续跑选项；todo eager+完成提醒+对账；自动标题；持久复活；失败轮耐久丢弃；统一 OutputSink/artifact。
- **工具组**：edit replace/patch 兼容入口；ask questions[]/multi/recommended；github search_*/run_watch/checkout/push；lsp 7 动作；browser tab/observe/aria；eval rb/jl；glob 开关/多路径/排序；memory items[]/update/invalidate；manage_skill；隐藏 yield/think；discoverable 档+统一超时表；read PDF/SVG/reader-mode/环断提示；inspect_image question 参数；xd:// 通用设备挂载框架。
- **扩展组**：agent-plugins 标准包；市场安装链；插件运行时加载；自定义工具目录（声明式先行）；自定义命令 TS/bundled/覆盖优先级；managed skills；frontmatter 严格校验；热重载；MCP disabled 名单+/mcp add 写回。
- **提示词组**：rules 双通道语义（alwaysApply/globs 按路径生效+auto-loaded 条款+跨通道去重——§三 #7 一并修）；人格预设+PERSONALITY.md；workstation 细节（GPU/终端/内核）；工作区树/多根；上下文 <file> 包装+@import；运行期通知模板文件化；7 子代理角色模板；goals/compaction 变体；TUI 主题 token 化；提示词行为测试补 SYSTEM.md 链/去重。
- **外围组**：advisor 专用模型+指纹去重+增量转录+统一脱敏器；goals objective+goal 工具；plan-mode 闭环（propose→批准→handoff→模型切换→计划保护）；SSH ControlMaster/传输/ssh:// 协议；export/share 加密密封；web provider 扩容+查询规划+scrapers；internal-urls 路由层+7 scheme；memories 后台 rollout（lease/heartbeat）；autolearn 技能侧控制器；commit 模块；markit；telemetry metrics/logs；snapcompact provider 形状+CJK。
- **构建组**：安装器+install-tests；Docker；justfile；bench 执行门+eval-bench 跑批；session-stats；changelog 自动化；macOS 签名；Homebrew；Nix；actionlint。
- **测试/前端/文档组**：collab 会话语义测试；server WS 帧协议 e2e；斜杠分发测试；web 前端 eslint+vitest 基建；P2 剩余 5 项+2 未查项；M7 ask options WS 线协议；M8 per-tool 工具卡注册表；M9 消息队列；M10 图像回显；README 三语补新功能；settings/skills/mcp/per-tool 用户文档起步。

### 低优先（长尾 / 按需拉动）

语音栈全栈（tts/stt/live，纯绿地已定论）· computer 桌面操控 · pi-vcs · metaharness · typescript-edit-benchmark · browser-relay · blob-broker · autoresearch/cleanse/compress/if-bench · stats · ServiceTier · /fresh 语义改名 · /git TUI · StopReason wire 拼写 · 技能暴露方式（/skill: 前缀 vs 每技能一命令）· /goal /status /agents 语义冲突定名 · 粘贴通道（OSC 5522/bracketed paste）· 死条目清理（pi-walker/ab_glyph/schemars）· 保留词提示文案同步 · get_tree 过滤/分页 · MessageUpdate 死变体处置 · assets 视觉资产 · 嵌套仓库上下文/active-repo-context · review 变体模板 · sharpshooter 精度模板 · 提示词重写管线 · session-color/qrcode · prompts 全树文件化

### 明确不迁移（维持）

TUI 差分渲染引擎（25k 行）· TS 进程内插件 1:1 · npm 分发面/robomp/Python omp-rpc 客户端 · bazel/自托管 runner · collab-web React guest（单文件形态维持，已迁 `crates/server/src/collab_guest.html`）· legacy pi 兼容 shim · omp nix/bazel CI 覆盖面 · read `:img`（分流 read_image，缺 question 视觉问答参数记入中优）

## 六、不确定与需进一步确认

1. **vendor/ 删除意图**（P0）：无法从工作树判定是有意瘦身还是误删/同步中断；处置前需用户确认（§〇 两路线）。
2. pi-walker 零消费原因：预留占位还是引入即未接线；路线 B 移除前建议与作者确认。
3. switch_branch 静默降级（§三 #1）：后续回归还是声明时即未按此实现，无逐行历史可考。
4. omp 计数口径：斜杠命令实测 73 主名（基线 72）；CLI 子命令 41 主名（基线 42）；RPC 42 变体（基线 ~50）；prompts/tools 44（基线 52）、prompts/system ~65（基线 ~95）——差异疑为 glob 截断/别名计入口径，不影响各项判定。
5. 空停重试第 3 次失败后的边界行为：omp 生成 terminal 分析帧 vs Gyre 按完成收尾，语义不等价未逐行为对比。
6. README.md:52 声称会话支持 "export"：FM 域判定 export 全缺，但 `server/src/lib.rs:3578-3664` 有 render_session_transcript 渲染函数——export 的落地边界需澄清（或为部分落地）。
7. omp .env「三级加载」的第三层级未独立复核（已证 profile 级与 CLI 入口级）。
8. ACP allow_always 是否需要持久缓存（产品需求未决）；ACP fs 桥依赖 Zed 客户端能力协商。
9. Gyre advisor 复用主模型为有意成本取舍还是待接 [advisor] 配置段的占位，代码未注明。
10. musl 双腿 experimental：rusqlite bundled 交叉编译链未实跑验证，首次真实 tag 前风险无法消除（release_sums 完整性硬门会点名缺资产）。
11. KeyRing「限流自动切 key」与 engine 429 退避的触发顺序口径（沿袭基线，合并核查）。
12. omp「80 站点 scrapers」未能按字面复核：当前快照 web/scrapers/ 仅 4 站点+types/utils。
13. 前端 P2-8（断点粒度）/P2-9（字体子集化）未抽查；P2-3 判定可能偏宽（发送侧已改句柄，预览侧原案未做）。
14. omp 内容块 ServerToolContent/Fallback 变体定义未逐行核对（仅核实 redactedThinking/TextSignature）。
15. rustyline 是否编译启用内置 bracketed paste 未查证（影响粘贴通道缺口优先级）。

## 七、路线图建议（顺序约束）

1. **立即**：P0 vendor 处置（§〇）→ §三 声明不符 13 项中的 #1/#2/#4（switch_branch 显式报错、SSE type 判别一行配置变体、supports_thinking 统一）→ ACP authenticate 空实现去除（H10 前半）。
2. **第一梯队**：H5 流式守卫 → H3 bash async+job manager → H14 grep 参数面 → H6 会话级事件 → H15 MCP 收尾 → H20 测试网四面。
3. **第二梯队**：H4 子代理定义化 → H8/H9 配置面 → H17 SYSTEM.md 链 → H19 hook 事件面 → H21+键位架构决策（H2）。
4. **第三梯队**：H11 RPC 扩容 → H10 ACP 完整面 → H12/H13 CLI/斜杠族 → H16 发布链收尾 → 中优各组按需。
5. **持续**：§三 声明-代码偏差作为回归类目长期跟踪（本轮 13 项中 5 项源自修复轮声明宽于实现）。

**总量估算**：高优 21 项 ≈ 30-45 人日（vendor P0 除外）；中优 ~60 项 ≈ 70-95 人日；低优按需。两处架构决策点：raw 模式按键层、collab 互操作。
