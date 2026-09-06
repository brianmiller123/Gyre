# Changelog

本项目的所有显著变更将记录在此文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)，
版本号遵循 [Semantic Versioning](https://semver.org/spec/v2.0.0.html)。

## [Unreleased]

### Added
- Remote MCP OAuth 授权（对齐 oh-my-pi `mcp/oauth-*`）：`agent mcp login|logout <server>`
  子命令；RFC 9728 受保护资源 → RFC 8414/OIDC 授权服务器元数据自动发现
  （网关子路径 path-inserted / parent-relative 候选序、issuer §3.3 校验）、
  RFC 7591 动态客户端注册（失败带端点+状态详情）、授权码 + PKCE 回环流
  （复用回调服务器：state 防 CSRF + 手动粘贴赛跑）、RFC 8707 资源指示
  （公告值权威保留 / server URL 兜底同源剥离）；凭据（含 token_url/client_id/
  resource 刷新物）落 oauth.toml `mcp_oauth:<url>` 行（0600）。HTTP 传输连接时
  自动装载 Bearer：过期先刷后用，401 单飞刷新 + 一次性重试，确定性失败
  （invalid_grant）清凭据并透出登录指引。`[mcp.servers.<name>.oauth]` 子表
  手工指定 client_id/secret/scope/回调参数（不开放 DCR 的服务商）。

- OAuth 登录链（对齐 oh-my-pi `registry/oauth/`）：`agent auth login|logout` 子命令 +
  oauth.toml 凭据存储（0600 原子写）；PKCE S256 / 回环回调服务器（state 防 CSRF、
  手动粘贴赛跑）/ RFC 8628 设备码轮询引擎；内置 anthropic（回环 PKCE + rotation 刷新 +
  30 天授权寿命提醒）、openai-codex（固定 1455 回环 PKCE + JWT 身份）、zai-coding-plan
  （9999 精确回调 + 铸 `gyre` 长效 key）三流；运行时解析链升级为
  config → oauth.toml（将过期先刷后用，60s 提前量 + 10s 刷新超时）→ auth.toml → env，
  交互 / RPC / Web 三装配点统一；anthropic 适配器对 `sk-ant-oat` 令牌自动切换
  Bearer + oauth beta 头。

- CLI 管理子命令面（对齐 oh-my-pi cli-commands.ts 显式注册表语义）：`agent models list`、
  `agent auth list|save|remove`（密钥仅经 `--stdin` 读入，不回显、不落日志/不进 shell 历史；
  auth list 只判来源不显值，拼写漂移的孤儿条目会明确提示）、`agent config path|check`。
  裸保留词 #4845 防误注入提示更新为指向可用子命令。
- hub 工具进程托管 op：`start` / `ps` / `logs`（游标翻页 + grep + follow）/ `stop`（SIGTERM→
  宽限→SIGKILL，unix 杀整组）/ `restart`（复用启动规格）/ `describe`，既有 `send`/`wait`
  按 `name` 分流进程语义。就绪判定（日志正则 + TCP 端口探测）、崩溃自动重启（no/on-failure/
  always，退避 500ms→8s 共 5 试）。进程内实现：无 broker、无持久化、无 PTY、退出不主动
  推消息（用 `wait`/`logs` 轮询），与 omp broker 架构的偏差已在工具描述注明。
- 进程内 shell（`agent-shell` + vendored `pi-shell` / `pi-builtins` / `pi-walker` / `brush-core`）：
  brush 解析器 + 50+ 进程内 coreutils 内建，命令执行零 fork。
- Gemini provider 适配器；todo / ask / checkpoint / rewind 工具接入 CLI RPC 与 ACP；`security_scan` 工具装配。
- 长期记忆升级：结构化抽取、时间语义、中文同义词调优、记忆固化（consolidation）。
- eval harness：Python / JavaScript 双语言持久内核（NDJSON 协议）+ 127.0.0.1 环回桥 + `eval` 工具。
- **CI 执法链**：rustfmt / clippy（`-D warnings`）/ nextest 三门；vendor 四个 fork crate 随仓库分发
  （此前 `vendor/` 被 gitignore，fresh clone 无法编译，GitHub CI 与 tag 发布从未真正可用）。
- `.config/nextest.toml`：`retries = 0`（flaky 是 bug，不是遮羞布）、fail-fast 关闭、慢测试 60s 预警。
- `run_command` 工具参数：`cwd`（相对工作区根解析、须为已存在目录）、`env`（键名合法性校验）、
  `timeout`（1–3600s，`0` = 不限时，默认 120s）。
- MCP Streamable HTTP 传输：`[mcp.servers.<name>]` 支持 `url` + `headers`（值经 `${ENV}` 展开）
  双形态配置（含 `command` 即 stdio 向后兼容 / 含 `url` 即 http）；JSON 与 SSE 两种响应、
  `Mcp-Session-Id` 会话保持、`MCP-Protocol-Version` 握手后携带、`close` 发 DELETE 终止会话；
  `mcpCapabilities.http` 能力声明改为 `true`，README 三语与 `config.example.toml` 同步。
- 危险命令硬拦截清单：移植上游 `CRITICAL_BASH_PATTERNS` 21 条（`rm -rf /`、`--no-preserve-root`、
  fork 炸弹、dd/mkfs/shred、`> /etc/passwd`、`curl | sh` 及进程替换/eval 变体、关机、`nc -e` 等），
  命中即拒并附原因提示。
- `run_command` 非交互环境基线（移植上游 `NON_INTERACTIVE_ENV`）：pagers→cat、`TERM=dumb`、
  `NO_COLOR`、编辑器→`true`、`GIT_TERMINAL_PROMPT=0`、包管理器非交互；`GYRE_BASH_NO_CI` 可退出
  `CI=true` 注入。
- hashline 锚定闭环：`read_file` 文本读取输出 `[路径#标签]` 段头并把全文快照写入会话级
  版本库（与 `apply_hashline` 共享同一 store），stale-hash 恢复可凭读取版本重放（移植上游
  read↔snapshots 闭环）。
- `read_file` 行选择器族：`:N` / `:N-M` / `:N+K` / `:N-` / 逗号多区间 / `:raw` /
  `:raw:N-M` 复合（1 基绝对行号；字面路径优先；协议与归档路径不受影响）。
- 工具流式 partial 生产者：全部写工具（统一写路径）与 `apply_hashline` 逐区段进度经
  `ToolContext::update_tx` 推送 `ToolExecutionUpdate`；`ToolContext` 新增 `tool_call_id`
  逐调用贯通，partial 与工具调用一一配对。
- `apply_hashline` 支持共享快照存储注入（`HashlineTool::with_snapshots`）；CLI 装配
  read / 编辑两侧同库。
- ACP：`ToolExecutionStart/Update/End` 映射为真实 `toolCallId` 的 `tool_call` /
  `tool_call_update` 序列（并行同名工具不再不可区分）；折叠事件不再重复出卡；工具
  kind / 标题推断移植上游 `mapToolKind` / `buildToolTitle`。
- `--rpc` 协议 v2：连接先发 `ready` 帧（protocolVersion / maxFrameBytes），超 1 MiB 帧
  双向 `rpc_chunk` 分片重组（乱序容忍、重复忽略、>64 MiB 回退 `rpc_frame_error`）；
  `docs/rpc.md` 补协议章节。
- MCP：initialize 捕获 server `instructions` 并按 server 分节注入 system prompt；
  server→client 通知消费（`tools/list_changed` 触发工具清单刷新 + 监听器回调，
  resources/prompts 变更记日志）。
- CLI 保留顶层词防误注入（移植上游 #4845 语义）：`agent models` / `agent uninstall
  foo@bar` 等管理语法不再作为 prompt 计费；task 位置参数改为收集全部剩余词。
- `web_search` 新增 `recency` 参数（day/week/month/year；DDG `df` 与 SearXNG
  `time_range` 映射，week 在 SearXNG 降级为 month，对齐上游）。
- 空/意外停止有界重试：普通停止但回复为空时注入续跑提醒并重试（上限 3 次，
  对齐上游 `dropAssistantTurn` 语义，空消息不持久化）。
- `--rpc` 反向通道（`--rpc-forward-ask`）：审批 / 追问以 `request` 帧外发、宿主以
  `response` 帧回答（`answer: yes|no|text`；缺失/error/10 分钟超时一律按拒绝）；
  默认关闭保持自动拒绝兼容行为（`docs/rpc.md` 补协议章节）。
- `--rpc` 会话命令扩容：`get_tree` / `switch_branch`（`node` 完整 id 或 ≥4 字符前缀，
  `handoff` 摘要交接）/ `list_models`（运行时发现当前 provider 的 /models 端点）。
- REPL 会话树导航：`/tree`（渲染森林 + ◀ 活跃标记）、`/branch <id>`（摘要交接分叉，
  落点后续写走活跃路径）、`/models` 运行时发现；`/model` 接受 `[models.roles]` 角色名。
- 模型角色：`[models.roles]`（role → alias/id，含「随 `[[models]]` 段」与独立段两种
  TOML 形态的加载提升）；`resolve_role` + validate 引用校验。运行时模型发现
  `agent_llm::list_models`（OpenAI 兼容 / DeepSeek / Anthropic；其余 api 返回 Unsupported）。
- 文件式 hook：`[[hooks]]`（event: before_tool / after_tool / stop，tool 匹配器，
  timeout 1-600s 默认 10）→ `ShellHook`（stdin 事件 JSON，before_tool stdout
  `{"decision":"deny|allow"}` 可拦截；超时 kill；spawn 失败放行告警）。
- 认证链分层：`<config_dir>/auth.toml`（0600，原子写）→ 三段解析 config（含
  `${ENV}` 展开）→ auth.toml → `GYRE_<PROVIDER>_API_KEY`；`auth save` 语义的
  `agent_config::save` 供后续 `/auth` 接线。
- ACP `session/available_commands`：REPL 斜杠命令清单经
  `SessionManager::set_available_commands` 注入（serve 与 stdio 双路径）。
- MCP 工具清单磁盘缓存：启动快照落盘（原子写 0600），server 未连上时 `hydrate_from_cache`
  回填 stale 元信息并告警；MCP legacy HTTP+SSE 传输（2024-11-05 协议）作为 Streamable
  HTTP 404/405 回退。
- `--rpc` 会话控制命令面：`get_state` / `set_model` / `set_thinking` / `get_messages` /
  `compact` / `get_usage`（变更型仅空闲受理；`set_thinking` 显式 `null` 关闭思考，
  经运行期覆盖每轮解析；`docs/rpc.md` 同步补协议章节）。
- MCP 连接韧性：断连指数退避重连（500ms→4s 共 5 试）+ 爆发熔断（30s 滑窗 >5 次 → open，
  冷却后半开探活自动恢复，对齐上游 manager.ts 常量）；重连成功自动重拉工具并触发刷新回调；
  每 server 连接状态 API（供 /mcp status 后续接线）。
- `web_search` API provider：Tavily（Bearer）与 Brave（X-Subscription-Token）接入自动
  fallback 链（有 key 优先），recency 分别映射 `time_range` / `freshness`（pd/pw/pm/py）。
- hub 工具监督面：新增 `wait`（阻塞等消息/超时/`from` 过滤）/ `inbox`（peek 语义）/
  `jobs`（子任务快照）/ `cancel`（未在册 not_found 语义对齐上游）；supervisor 句柄改为
  `Arc<Supervisor>` 进程内共享（TaskTool / review / hub /agents 同源）。
- 系统提示词行为杠杆四段：`§ Tool Policy` / `# Delegation` / `§ Workflow`（六阶段）/
  `§ Delivery` + Critical 收尾（移植 oh-my-pi system-prompt，工具名适配 Gyre 面），
  在项目上下文之前注入。
- 会话文件 header + 版本化：新会话首行写 `SessionRecord::Header{version,…}` +
  `CURRENT_SESSION_VERSION` + migrations 骨架；无 header 旧文件全量兼容读入；过新版本报错。
- 密钥管线级双向脱敏：per-install key（`<config_dir>/secrets.key`，0600）+ 上游形态
  检测正则集（sk-/ghp-/AKIA/bearer/PEM 私钥块 + 通用赋值熵门）→ 出向（provider 请求）
  `<secret:N>` HMAC 占位、入向（assistant 消息）还原；会话上下文保持明文，
  `GYRE_SECRETS=off` 直通。
- Skill 跨工具发现：`.claude` / `.codex` / OpenCode / `.github` 四 provider（路径与
  优先级对齐上游），`[skills.providers]` 逐源开关，同名去重 native 优先。
- CI 发布链：cargo-deny 门（licenses/bans/sources，advisories 软门 5 项 RUSTSEC 待
  triage）+ `about.toml`/`about.hbs`（THIRD-PARTY-NOTICES 生成命令文档化，vendor 四 fork
  许可义务覆盖）+ 发布矩阵 2→5 目标（musl×2 / darwin-arm64）+ SHA256SUMS 汇总与发布后
  下载验证 job + `npm ci` / release concurrency / paths 过滤硬化。


### Fixed
- 429 限流处理：兑现 `Retry-After`，同模型指数退避（上限 3 次 / 30s），重试可被取消打断。
- `SkillsConfig.enable_commands` 死配置键移除（从未接线；`/skill:<name>` 前缀在 REPL
  恒可用）。`[skills].enable_commands` 配置键如仍存在将被忽略。
- 移除根 `Cargo.toml` 无消费者的 `tree-sitter = "0.25"` workspace 条目（`crates/ast`
  使用自有本地依赖 0.24）。
- `config.example.toml` `max_turns` 注释与默认值（1000）不符，已修正并标注 0 = 不限制。
- `docs/` 解除 gitignore：设计文档与对标分析随仓库分发。
- 跨平台命令语义分叉：模型按提示词写 bash，Linux（Debian 系 `/bin/sh` → dash）下数组/`[[ ]]`
  失败、Windows 落到 `cmd` 而提示词却宣称 PowerShell——引擎统一后消除。
- 修复：认证链分层接线把 auth.toml **文件路径**误传给期望**目录**的 `agent_config::load`
  （内部自拼 auth.toml），导致运行时 auth.toml 永远读空、只有环境变量生效（rpc 与交互
  两条路径同错）。
- 子进程与进程内输出统一 CRLF/裸 CR → LF 归一化（对齐 PTY 路径既有行为）。
- REPL `/help` 与命令表漂移修正：补齐 `/todo` `/goal` `/diff` `/fresh` `/plan` `/paste`
  `/enhance` `/suggest` `/agents` 九条帮助条目（en/zh/ru/ja 四语）；`/mode` 帮助文案补 `plan`
  模式；`/enhance` `/suggest` 空参用法提示接入 i18n；新增「帮助表覆盖内置命令全量」回归测试。

### Changed
- CI 移除 `RUSTFLAGS=-A warnings`，新增 rustfmt / clippy（`-D warnings`）/ nextest 三道执法门；
  advisor / discovery / shell 三个此前未接入 workspace lints 的 crate 已接入。
- **lint 基线校准**：`pedantic` / `nursery` 整组不再作为执法基线（83k 行存量从未在其下维护，
  整组开启产生 600+ 风格噪音；nursery 官方自述接受不稳定 lint），改为默认组
  （all / suspicious / complexity / style）warn + `correctness` / `perf` deny + 高信号
  pedantic 项显式入选（`unchecked_time_subtraction` / `mutex_integer` / `mem_forget` /
  `rc_buffer` / `vec_box` / `option_option`），修完后可逐项升回。
- **`run_command` 执行引擎统一**：进程内 brush（bash 兼容）成为三平台唯一主引擎，完整 bash 语义
  （管道/重定向/`[[ ]]`/数组/进程替换）跨平台一致；系统 shell 子进程（`/bin/sh -c` / `cmd /C`，
  新增 `GYRE_SHELL` 覆盖）降为兜底——withheld 的 rm/mv/ln、`GYRE_DISABLE_INPROC_BUILTINS` 显式
  退出、引擎初始化失败。进程内会话环境以 `PI_DISABLE_UUTILS_DESTRUCTIVE=1` 兜底禁用破坏性内建，
  管道内嵌（`xargs rm`）同样回退系统二进制。系统提示词平台段同步改口：Windows 不再宣称 PowerShell。
- PTY 会话 Windows shell 发现链（对齐上游 `resolveWindowsShell`）：`GYRE_SHELL` → Git Bash
  （`GIT_INSTALL_ROOT` / Program Files / MinGit / scoop / LocalAppData）→ PATH `bash.exe`/`sh.exe`
  → `cmd` 兜底；Windows UTF-8 环境组改为「宿主已设置（大小写不敏感）则跳过」，`env` 参数先摘除
  宿主大小写变体键再写入。
- 超限输出从「一刀切截断尾部」改为 head+tail 中段省略（头部 60% + 尾部 25%，UTF-8 边界与行首
  对齐，附省略字节数提示）。
- WebUI 模型切换器迁移：从顶栏移至消息输入框正上方的工具栏（`ModelSwitcher` 组件，向上展开的下拉
  菜单，醒目显示当前模型）；重复点击当前模型不再误开新会话；运行面板（Inspector）模型列表改为
  只读状态展示，全站唯一切换入口（工具栏 + `/model` 命令），四语文案同步清理。
- 移除 29 个 crate `lib.rs` 中与 workspace lints 冲突的 `#![warn(clippy::pedantic)]` /
  `#![warn(missing_docs)]` 属性墙（属性级别高于 Cargo.toml 配置，会架空整份 allow 名单）；
  lint 政策收敛到根 `Cargo.toml` 单一事实源。`core` 的 `missing_docs` 门禁经核实零告警，保留。
> 历史标签 v0.1.0 – v0.1.12 早于本 changelog 与可工作的 CI（彼时发布产物无法从 fresh checkout 编译），
> 不逐版补录。
