# Gyre × oh-my-pi 全面对标差距分析（2026-09-02）

> 调研对象：`third/oh-my-pi`（badlogic/pi-mono fork，MIT）vs 本仓库 Gyre（Rust workspace，33 crates + vendor 4 crates）。
> 调研日期：2026-09-02；审查基线为**工作区当前状态**（全部工作位于未提交改动，最后提交 08-06 `64242e1`，未提交 ~6900 行）。
> 方法：以 `docs/oh-my-pi-agent-gap-analysis-2026-08-23.md`（对同一工作树审计，功能矩阵经本轮抽验仍准确）为功能基线，本轮新增 6 路并行只读 scout 对工程维度（架构/质量/测试/构建发布/API-DX/性能/文档/扩展）做源码级核查；关键承重结论（vendor 忽略、CI 配置、测试量、协议面、Phase 0 落地）均经直接复核。**源码为唯一事实源**。
> 证据格式：omp 路径相对 `third/oh-my-pi/`，Gyre 路径相对仓库根。

---

## 〇、结论速览

1. **规模对比**：Gyre = 33 crates / 83,557 行自有 Rust + 158,719 行 vendored（pi-builtins 89,784 / pi-shell 37,044 / brush-core 26,295 / pi-walker 5,596）；omp = 17 TS packages / 778k 行 + 8 Rust crates / 195k 行。
2. **功能基线核实**：08-23 文档矩阵仍准确——Phase 0 四项接线已亲验落地（`crates/cli/src/main.rs:730-742`、`crates/server/src/lib.rs:1146-1149`、429 Retry-After 于 `crates/llm/src/lib.rs:56-72` + `crates/agent/src/engine.rs:476-523`）；P1 项（plan propose / RPC v2 / MCP HTTP / manage_skill）grep 确认零进展。
3. **本轮核心发现**：Gyre 的短板已从「功能缺什么」转移到「**工程资产是否被保护、仓库可否被协作**」——lint/测试/基准配置齐备但 CI 零执法；**vendor/ 被 .gitignore 却是硬构建依赖，fresh clone 编译失败（唯一阻断级缺陷）**；文档面 8 vs 126 篇且 README 存在 3 处实现漂移。
4. **总量估算**：P0 ≈ 4–6 人日；P1 ≈ 40–55 人日（收敛 ~80% 用户可见差距）；P2 ≈ 40–60 人日。引擎与功能面不必大动，剩余差距全部是可排期的增量工作。

---

## 一、十维度总览

| # | 维度 | 差距定性 | 一句话结论 |
|---|------|---------|-----------|
| 1 | 架构与目录结构 | 🟢 基本对齐 | 分层 DAG 无环、core 零业务依赖；差距在「执法」不在「设计」 |
| 2 | 核心功能完整度 | 🟡 ~85% | 引擎层等价+局部超集；缺口集中在接口纵深/provider 生态/外围工具 |
| 3 | 代码质量与工程规范 | 🔴 结构好、执法零 | lint 配置同级但 CI `RUSTFLAGS=-A warnings`，全是「愿望配置」 |
| 4 | API 设计与 DX | 🟡 明显落后 | RPC 3 vs 42 命令、无握手无错误码；配置 2 级 vs 5 层 schema 单源 |
| 5 | 性能与资源效率 | 🟡 有真热点 | token 计数每轮全量 BPE×2 + 非 OpenAI 退 chars/4；克隆密度高 |
| 6 | 错误处理与健壮性 | 🟢 局部领先 | catch_unwind/超时矩阵/fail-closed 齐全；omp 领先在恢复语义广度 |
| 7 | 测试与 QA | 🔴 量质尚可、零保护 | 1060 个行为级测试质量好，但 CI 一个都不跑 |
| 8 | 文档与示例 | 🔴 数量级差距 | docs 8 vs 126；README 3 处漂移；零 authoring 文档/示例 |
| 9 | 构建与发布 | 🔴 阻断级缺陷 | **vendor/ 被 gitignore 却是硬依赖——fresh clone 编译失败** |
| 10 | 可扩展性与插件 | 🟡 结构性差距 | 3+2 拦截点 vs 70 事件；MCP stdio-only；零分发载体（部分为既定边界） |

---

## 二、逐维度详析

### 1. 架构设计与目录结构 🟢

**Gyre**：依赖方向 iso→core→域 crate→tools/agent→server→cli，无环无越层反依赖；core 仅依赖 iso（`crates/core/Cargo.toml:8`）；3 条 dev-dependency 环（core dev→context、tools dev→memory、agent dev→context）均有注释论证；平均 2.5k 行/crate 粒度细。最大文件 `crates/agent/src/lib.rs` 5,108 行（`mod engine` 声明在 L971，状态机内联）、`crates/server/src/lib.rs` 4,295 行。

**omp**：utils→catalog→ai→agent→coding-agent 分层靠成文约定 + per-package 依赖声明；自有最大文件 `coding-agent/src/session/agent-session.ts` 10,230 行——**标杆同样不控制模块大小，且更容忍巨文件**。

**差距与借鉴**：设计层面无实质差距。可借鉴两点：
- omp 把 vendored fork 收编为 workspace 成员但按 manifest 分级豁免（Gyre 的 vendor/ 完全游离于 workspace lints 之外，是「法外之地」）；
- omp 的 AGENTS.md 把分层规则写成明文（Gyre 靠口头传统）。

`agent/lib.rs` 内联大块宜下沉子模块（已有 engine.rs 先例），非紧急。

### 2. 核心功能完整度与特性覆盖 🟡

**已对齐**（08-23 矩阵 15✅ + Phase 0 修复落地）：双循环/steering/压缩/恢复/审批内核等价；审批 7 步链、含元字符命令永不自动放行、记忆中文调优、i18n 四语、Harmony 泄漏自愈为**超集**；todo/ask/checkpoint/rewind/security_scan 已接线；429 Retry-After 已兑现。

**真实缺口（五类）**：

1. **接口纵深**：RPC 3 命令 vs 42；无 ready 握手/版本协商；ACP 10 方法 vs 12+扩展（缺 list/resume/configOption/elicitation）；无 Python 客户端/SDK。
2. **provider 生态**：5 适配器 vs 68 描述符/4409 模型/4 种认证形态（OAuth/coding-plan/keyless 全缺）；无模型目录数据化。
3. **产品流半成品**：plan propose 批准流（`xd://propose` 零命中）、checkpoint turn_end 延迟应用、hub v2（ACK/唤醒/jobs/launch）。
4. **外围能力**：computer/tts/PDF 读取/自动标题/会话导入/postmortem/本地 cost 定价表（`cost_usd` 恒 0）/telemetry metrics。
5. **恢复语义广度**：跨进程 in-flight 限流、empty-stop reparent、凭证 403/usage-limit 直轮、模型冷却。

**借鉴**：omp 的 `builtin-names.ts` 单点工具清单 + `rpc-types.ts` 协议类型单源值得照搬形态；模型目录不必照抄 4409 条，提炼实际使用的 5–8 家 + 字段自动判定即可。

### 3. 代码质量与工程规范 🔴（本轮最大意外）

**Gyre**：`[workspace.lints]` 配置与 omp 同级（pedantic+nursery warn、correctness deny，注释自称「与 oh-my-pi 一致」），**但**：

- CI `RUSTFLAGS: "-A warnings"`（`.github/workflows/rust.yml:12/17`）主动压掉全部警告，无 clippy/fmt/test job——**执法为零**；
- advisor/discovery/shell 3/33 crate 未继承 workspace lints（各自 Cargo.toml 无 `[lints]` 段）；
- 无 rustfmt.toml、clippy.toml、deny.toml、AGENTS.md——零成文规范；
- 好的一面：生产 unsafe 仅 1 处（`crates/iso/src/rcopy.rs:393` utimensat）；unwrap/expect 89 处中真问题约 8 处；错误架构（thiserror 分层枚举 ×16 + `AgentError` `#[from]` 总线 + `is_fallbackable/is_recoverable` 语义谓词，`crates/core/src/error.rs`，anyhow 严格限于 cli/shell 边界）**实际优于** omp 的 TS Error 类机制。

**omp**：同一配置 `-D warnings` 全量执法（`scripts/run-rs-task.ts:30-41`）+ cargo-deny 许可证/来源双门（`ci.yml:152-162`）+ rustfmt.toml 60+ 项 + lint-staged + Bazel clippy 三档（strict/default/ported，vendored fork 受控豁免仍保持零 rustc 警告）+ 26KB AGENTS.md 工程宪法（禁 inline import、中央工具优先「两个实现=bug」、测试反模式清单、构建决策带测量记录）+ `allow_attributes_without_reason` / `undocumented_unsafe_blocks`。

**借鉴**：**最大可借鉴点是「执法链」而非配置本身**——两边 lint 基线几乎相同，差别只在是否进 CI。次借鉴：allow 必须写理由；错误设计模式写进成文规范。

### 4. API 设计与开发者体验 🟡

**Gyre**：

- CLI RPC 3 命令/6 事件，无握手、无版本、错误为扁平 `{type:"error",message}` 无机器可读码（`crates/cli/src/rpc.rs:110`）；忙时重复 prompt 直接报错（而 server WS 已移植 steering——**自家三个面行为不一致**）；
- ACP 10 方法，`session/load`/`set_mode` 以重建会话实现、返回新 sessionId（`crates/acp/src/rpc.rs:283-299`）；HTTP+SSE 面 2 端点 + Bearer 鉴权 + fail-closed 600s；
- REST 26 路径（`crates/server/src/lib.rs:1558-1594`）+ WS ServerFrame 20 帧变体**零契约文档**；
- 配置仅 2 级覆盖（项目覆盖用户级）、无 schema 无校验；
- repo URL 是占位符 `agent-project/agent-project`（`Cargo.toml:10`），未发布任何库。

**omp**：42 命令 12 组 + ready 帧携带 `supportedProtocolVersions:[1,2]`（`rpc-types.ts:144-149`）+ negotiate 后超 1MiB 帧 `rpc_chunk` 分片传输 + 错误带可选机器可读 `code`；宿主可实现工具（`set_host_tools`）+ extension UI 11 方法往返；Python 客户端 1,830 行协议 + 2,154 行客户端（手写 TypedDict + 运行时校验，无 codegen）；`SETTINGS_SCHEMA` 单一事实源驱动 `omp config get/set/reset`（运行时内省永不与代码漂移）+ 5 层覆盖 + 坏文件 `.broken-*` 备份；npm v18.1.2 直接发布 TS 源。

**Gyre 优势面**（应保持）：rpc.md 与代码零漂移（单页可全文核对）、CLI 帮助 4 语言（`cli/main.rs:124-133`）、`config.example.toml` 18 节逐键注释、REPL 30 命令补全。

**借鉴**：① RPC 加 1 行 ready 帧 + negotiate（向后兼容零成本）+ error `code` 字段；② 把 server 已有的 steering 移植回 CLI RPC；③ 配置 schema 单源是最大结构性 DX 差距；④ `ServerFrame` serde tag 已是机器可读来源，REST/WS 契约文档可生成。

### 5. 性能与资源效率 🟡

**Gyre 最大热点——token 计数双输**：`count_context_for`（`crates/context/src/token.rs:80-133`）每轮全量逐段 BPE 重编码（build + token_usage 各一遍），非 OpenAI 模型退化为 chars/4（`token.rs:67-70`）——长会话 O(n) 成本 + 精度双输。**omp** 用原生 utok 六族词表（openai/cl100k/o200k/qwen/deepseek/kimi/claude）+ 非消息段实例身份缓存（`coding-agent/src/utils/context-usage.ts:196-225`）+ provider usage 回报做锚点、本地估算仅做下限（`session/session-advisors.ts:1573-1585`）。

**次级差距**：

- 上下文管线克隆密度高（`active_path_messages` 每次克隆全部消息 `context/src/lib.rs:94-96`、`snapshot_nodes` 全森林克隆 `:288`、compact 双份重建 `:118,185-194`）；
- 工具更新 `unbounded_channel`（`engine.rs:1349`）无背压；steering 以 `len()` 非消费轮询（`engine.rs:1658-1668`）；
- fs_cache 精简版（Mutex<HashMap>/16 条目/TTL 1s，`tools/src/fs_cache.rs` vs omp DashMap+rayon 四方共享）；
- InProcShell oneshot 无会话状态（omp 为持久 brush 会话 + 117 内建零 fork）；
- criterion 4 基准（ttsr/search/tokenize/compaction）已建但 **CI 不跑，形同虚设**；二进制体积无台账（双方都缺）。

**不必追**：omp 25k 行 NAPI addon（grep/AST/diff/fuzzy/utok/SIXEL）是在补 JS 的慢——Rust 本身原生、无 FFI 边界，属路线差异非缺陷。

**借鉴**：token 计数改「usage 锚点 + 尾部增量」+ 补非 OpenAI 词表；snapcompact 升级 provider 计费感知帧形状（Gyre v1 单形状 CJK 折 `?` 是自认可读性硬伤，`crates/snapcompact/src/lib.rs:8-10`）；**「eval 驱动性能参数」方法论**（omp 用 SQuAD recall eval 调帧形状，替代拍脑袋常量）。

### 6. 错误处理与健壮性 🟢（Gyre 强项）

**已到位**：

- 工具执行 `catch_unwind` panic 归一化（`engine.rs:1553-1557`，防第三方工具失控终止整个 run，不污染会话文件）；
- MCP/LSP/DAP/CDP 请求级超时 30s/30s/30s/10s 全覆盖；权限请求 fail-closed 600s；
- SSE 逐 chunk 360s 空闲超时（`llm/src/lib.rs:108-110`）替代总超时（不误杀慢速长流）+ 1MB 行上限防 OOM + 跨 chunk UTF-8 缓冲；
- MCP stdout 4MB 行缓冲上限；429 退避 + key 轮换（重试重走 key 轮换，等待可被取消打断）；Harmony 泄漏自愈双计数器；refusal-like 不重放。

**omp 领先项**（= 08-23 矩阵 #39-45 剩余部分）：TurnRecovery 的 empty-stop reparent + 模型冷却；**跨进程 in-flight 限流**（文件租约：锁 10s/过期 30s/心跳 5s——多实例共享配额，`stream.ts:177-264`）；a/b/c 凭证 64 次轮换 + 403/usage-limit 直轮 sibling；崩溃 postmortem 分类器。

**结论**：单进程健壮性 Gyre 已达或超标杆；差距全在**恢复语义广度**，属 P1 功能件而非重构。

### 7. 测试覆盖率与质量保障 🔴

**Gyre**：1060 个测试（1044 内嵌 `#[test]`/`#[tokio::test]` + 16 集成），35 个测试目标；行为级风格好（假 provider + 调用计数断言可观察行为：`agent/src/lib.rs:977+` 并发/steering/429/stop_reason 回填；`shell/tests/inproc.rs` 黑盒验证 50+ 内建真实可执行）——**但 CI 一个测试都不跑**（rust.yml 仅 build+release）。分布 Top：tools 166、agent 91、llm 85、memory 79、context 77、hashline 67、cli 63、config 58；telemetry 零测试；supervisor/pty 各 3。无覆盖率工具、无 e2e、无模型在环 eval。vendor 内另有 2,108 个测试（口径应分开：08-23 文档「3127 通过」= 1060 自研 + 2108 vendored − 41 漂移）。

**omp**：TS 23,290 用例（2365 个 `*.test.ts`）分桶分片（`scripts/ci-test-ts.ts`：singleton/ui chunk=5/runtime chunk=10，按 OOM 与 bun GC SIGTRAP 实测调参）+ Rust 2,534 走 nextest（`.config/nextest.toml`：fail-fast=false、**retries=0「flaky 是 bug 不是遮羞布」**、slow-timeout 60s terminate-after 4）+ **metaharness**（experiment→run→trace + SQLite + REST :4700 的基准管理器）+ typescript-edit-benchmark（Babel 变异源码量化 edit 成功率）+ release_gate 聚合门 + 发布后 codesign 验证。覆盖率/变异测试双方皆无，不构成差距。

**借鉴**（性价比序）：① rust.yml 加 test/clippy/fmt——测试已存在，零编写成本；② nextest.toml 近乎照搬；③ vendor 测试分口径统计；④ 中期补 2–3 个跨 crate 集成目录 + 最小 agentic eval。

### 8. 文档与示例 🔴

**Gyre**：docs/ 8 文件中 7 个是历史分析，唯一开发文档 rpc.md 质量好；README 三语 492 行完全平行手工同步；**3 处实现漂移亲验**——

1. `README.md:73,472` 称 provider/**tool** 均免改中央注册，实际 tool 是手工 `DefaultToolRegistry.with()` 装配（`cli/main.rs:722,1618`），inventory 全仓仅 provider 一处（`llm/src/plugin.rs:38-52`）；
2. `README.md:33`「any MCP server's tools can be mounted」，实际 stdio-only 且仅 initialize/tools/list/tools/call 四方法（`mcp/src/lib.rs:5-9`），README:384 的 `mcpCapabilities{http:false,stdio:true}` 自相矛盾；
3. 模块树列 23 crates，实际 33（acp/advisor/browser/dap/discovery/eval/proxy/shell/snapcompact/ttsr 等 10 个缺席）。

另有占位 clone URL（README:90 vs Cargo.toml:10）与断链（`tests/understand/README.md:7` 引用已删除的 plans/ 文件——plans/ 整个被 gitignore）。无 CONTRIBUTING、无 AGENTS.md、无 examples/。rustdoc 头部质量高但仅 core 开 `#![warn(missing_docs)]`。

**omp**：126 个 md 按受众分层（用户 settings.md 840 行 / 第三方 authoring 三部曲 + docs/tools 每工具一页 + toolconv 每协议一页 / 内部 \*-internals ×9）；3 个可运行扩展示例（hello-extension/safety-hook/mini-marketplace）；文档内嵌「Implementation files」交叉引用清单对抗漂移；文档-代码同步靠 schema 驱动的运行时内省 + 手工维护（无 codegen）。

**借鉴**：文档内嵌实现文件清单模式（低成本防漂移）；authoring 文档三部曲（给「扩框架者」——Gyre 目前只覆盖「用框架者」）。

### 9. 构建工具链与发布流程 🔴（含唯一阻断级缺陷）

**🔴 vendor 断链（已亲验）**：`.gitignore:22` 忽略 `vendor/`，`git ls-files vendor/` = 0；但 `Cargo.toml:171-177` 将 pi-shell/pi-builtins/pi-walker/brush-core 声明为 path 依赖 + `[patch.crates-io] brush-core`。四个 crate 实际仅 **6.2MB**（pi-shell 2.0M / pi-builtins 2.9M / pi-walker 0.2M / brush-core 1.1M），其余 982MB（718 个目录）是无引用的 cargo-vendor 残留。**后果：fresh clone 编译失败，GitHub 上 CI 与 tag 发布从未真正可工作——发布链是纸面功能。**

**其余差距**：CI 仅 2 平台（linux/win x64）裸二进制，无校验和/签名/冒烟/发布后验证；无 CHANGELOG、无 cargo-deny、无 Docker/Nix/安装脚本；版本 0.1.0 无 bump 工具。

**omp**：一条 `bun scripts/release.ts` → 版本批量重写 + 锁文件重生成 + 原子 tag 推送 + CI 盯守（486 行 + release.test.ts 进 CI）；7 目标二进制（linux-x64/musl-x64/arm64/musl-arm64/win32-x64/darwin-x64/arm64，macOS Developer ID 签名+公证）+ SHA256SUMS.txt + 发布后下载验证；GitHub/npm（OIDC trusted publishing）/Homebrew 三渠道；deny.toml→about.toml→THIRD-PARTY-NOTICES.txt（22,909 行随产物分发）→SECURITY.md 合规闭环；bazel 仅作 CI 产物流水线（cargo 仍是本地权威——**不要求 Gyre 引入 bazel**）。

**借鉴**：release_gate 聚合 job 模式；产物 `--version` 冒烟 + 校验和是最低成本高收益件；cargo-deny 骨架直接抄。

### 10. 可扩展性与插件机制 🟡

**Gyre**：Hook trait 4 方法——3 事件（BeforeTool/AfterTool/Stop）+ `before_tool_intercept`（可阻断）+ `after_tool_override`（可改写）（`core/src/hook.rs:50-76`，较 08-23 基线已从纯观察升级为带拦截）；但 hooks 仅内部注入 3 个记忆 hook（`cli/main.rs:923-955`），**无外部加载路径**；inventory 仅 provider 一个扩展点；MCP stdio-only 4 方法；`xd://` 仅 3 个硬编码路径（pending/resolve/reject，`tools/src/fs.rs:439-457`）；自定义命令单一来源（`.agent/commands/*.md`，`config/src/config.rs:708-757`）；零分发载体。

**omp**：HookAPI 25 事件（9 个可阻断/改写，覆盖 session 生命周期/压缩/上下文，`extensibility/hooks/types.ts:479-506`）+ ExtensionAPI 45 事件（~15 个带 Result，含 `before_provider_request` 改写 LLM 请求、流式三段、审批，`extensibility/extensions/types.ts:1236-1295`）+ 运行时 TS 磁盘加载 + 热重载 + marketplace + slash 命令 7 源优先级（native100>omp-plugins90>claude80>claude-plugins/agents/codex70>opencode55，兼容三方 harness 命令生态）+ MCP 三传输 + OAuth 自动发现刷新 + resources/prompts + xdev 16 个 discoverable 工具统一挂载（`tools/xdev.ts:83-86`）。

**判定**：动态加载/市场在 Rust 侧**不可 1:1**（既定边界，替代 = inventory + Hook + 文件级命令）；**可追的是事件面广度**（Gyre 3→10-15 事件即可覆盖 provider 请求前后、turn 生命周期、审批）、MCP HTTP/OAuth 纵深、xd:// 通用化。Gyre 局部优势：类型安全同步拦截（Rust trait 显式签名）、审批门禁与 hook 职责分离。

---

## 三、改进路线图（按优先级）

### P0 · 高优先级 · 短期速赢（本周，合计 4–6 人日）

| # | 项 | 步骤 | 工作量 | 收益 |
|---|---|---|---|---|
| 1 | **vendor 入库修复（阻断级）** | `git add -f vendor/{pi-shell,pi-builtins,pi-walker,brush-core}`（6.2MB）；`.gitignore` 精确排除其余；删除 982MB 无引用残留；验收 = fresh clone `cargo check --workspace` 通过 | 0.5–1d | 仓库从「不可克隆构建」恢复为可用；一切下游项的前提 |
| 2 | **CI 执法链** | rust.yml 增三 step：`cargo fmt --check`、`cargo clippy --workspace --no-deps -- -D warnings`、`cargo test --workspace`（或 nextest）；删 `RUSTFLAGS=-A warnings`；advisor/discovery/shell 补 `[lints] workspace=true`；加 `.config/nextest.toml`（照搬 omp：fail-fast=false、retries=0、slow-timeout 60s） | 1–3d（浮动在存量警告清理量） | 1060 测试 + 4 基准 + 严格 lint 从摆设变为防线；回归无法再静默进入 |
| 3 | **README/元数据一致性** | 修 tool 注册表述、MCP 能力表述、模块树 23→33；repo URL 落定；清断链；同步三语 | 0.5–1d | 能力声明恢复可信；新人上手不再被误导 |
| 4 | **CHANGELOG + 发布最小加固** | 根 CHANGELOG（Keep-a-Changelog + Unreleased 约定）；release job 加产物 `--version` 冒烟 + SHA256SUMS.txt | 1d | 发布从「裸文件」变为可校验、可追溯 |

### P1 · 中优先级 · 补齐（1–2 月，合计 40–55 人日）

**工程件（~12–18d）**：

- cargo-deny（licenses+advisories，抄 omp 骨架）0.5–1d；
- 发布矩阵扩 arm64/musl + macOS 1–2d；
- criterion 基准进 CI（threshold 检测或至少趋势记录）+ release 二进制体积台账 1–2d；
- **Gyre 版 AGENTS.md**：把 lints 意图、错误设计模式（AgentError 总线 + 语义谓词）、vendor 策略、CI 门禁成文化 1–2d；
- **配置 schema 单源**：`SettingsSchema` 结构驱动启动校验 + `agent config get/set`（不必先做 5 层覆盖）3–5d；
- REST/WS 契约文档（从 serde tag 生成或手写一页）1–2d；
- telemetry crate 补测试 + metrics 导出 2–3d。

**性能件（~7–10d）**：

- token 计数改「provider usage 锚点 + 尾部增量」+ 补 claude/deepseek/glm 词表（借鉴 utok）3–5d；
- 上下文管线热点 Arc 化/借用视图（`active_path_messages`/`snapshot_nodes`/compact 路径）3–5d；
- `unbounded_channel` 加上限 + 满时策略 1d。

**功能件（衔接 08-23 P1，~21–27d）**，两个**顺序硬约束**：

1. **RPC 先版本握手后扩命令**：ready 帧 + negotiate + error code + 把 server 已有 steering 移植回 CLI RPC（2–4d）→ 命令扩至 15–20 条（8–12d）→ Python 类型化客户端（8–12d，手写 TypedDict 模式已验证可行）；
2. **checkpoint 先 pinning 后延迟应用**（压缩 pinning 语义先行，否则 rewind 可能回卷到被压缩吞噬的位置——08-23 §3.2-5 结论仍有效，3–5d）；
3. plan propose 批准流（复用 xd:// 设备骨架）4–6d；
4. MCP Streamable HTTP + OAuth discovery 5–8d；
5. hub v2（扩展现有 core/hub + supervisor，勿另立）5–8d；
6. manage_skill / 自动标题 / 本地定价表等小件插空 4–6d；
7. 模型目录数据化（提炼版 5–8 家 + vision/thinking/context 上限自动判定）4–6d。

**文档件（~5–8d）**：per-tool 文档（核心 10 工具先做）+ 扩展 authoring 指南 + 2 个可运行 hook 示例；每篇内嵌「实现文件」清单防漂移。

### P2 · 低优先级 · 长期重构（按资源排期，合计 40–60 人日）

| 项 | 工作量 | 说明 |
|---|---|---|
| Hook 事件面 3→10–15（补 turn 生命周期/审批/provider 前后） | 3–6d | 事件签名沿用现有 trait 模式，类型安全是相对 omp 的差异化优势 |
| 最小 agentic eval 设施（metaharness 思路：experiment→run→trace + 报表） | 8–12d | 先服务 snapcompact 帧形状与 edit 工具调参 |
| snapcompact 计费感知帧形状 + recall eval | 5–8d | 依赖 eval 设施 |
| InProcShell 持久会话（省重复初始化 + 状态延续） | 3–5d | |
| PAL 接线（task 隔离 opt-in）+ reflink/overlayfs 后端 | 2–4d / 8–12d | rcopy 先行，大仓库成本高是已知限制 |
| OAuth 设备流 v1（1–2 个 provider） | 5–8d | 账号基础设施 |
| secrets 工具参数 HMAC 占位符 + 执行前反混淆 | 3–5d | 密钥进对话仍达 provider 是真实暴露面 |
| 跨 crate 集成测试层 + postmortem 分类器 | 8–14d | |

### 可接受 / 不迁移项（附理由）

| 项 | 判定 | 理由 |
|---|---|---|
| TUI 差分引擎 | ✅ 接受不迁移 | 与 rustyline + Web 双前端路线冲突；借鉴其 append-only 回滚契约到 Web diff 推送即可 |
| TS 动态插件/市场/7 源 slash | ✅ 接受边界 | Rust 无运行时模块加载，结构性不可 1:1；替代（inventory + Hook + 文件命令）已定且可用，需在 README 明示边界（顺带修复漂移 #1） |
| Node SDK / robomp | ✅ 接受 | 语言栈差异；对应物是 `--rpc` + Python 客户端（P1） |
| mnemopi 深度记忆语义（情景图/三元组/Weibull） | ✅ 接受 | Gyre structured + 向量 + MMR 中文调优已覆盖主路径且为中文超集 |
| 25k 行 NAPI 原生化路线 | ✅ 接受 | omp 在补 JS 短板；Rust 天然原生。仅借鉴 utok 词表与缓存策略 |
| computer/tts/voice-WebRTC | ✅ 暂缓 | 生态与真机矩阵成本高，收益低 |
| 巨文件执法 | ✅ 接受现状 | 标杆自己的 10,230 行比 Gyre 更大；仅 agent/lib.rs 内联块宜择机下沉 |
| 覆盖率/变异测试 | ✅ 双方皆无 | 不构成对标差距；nextest + 行为级测试已够 |
| vendor 残留 982MB | 🗑 删除 | 无引用的 cargo-vendor 副产品，纯仓库噪声 |

### 必须优先补齐的 5 项及理由

1. **vendor 入库（P0-1）**——当前仓库对任何第二人/任何 CI 都不可构建，发布链从未真正工作；这是唯一阻断级缺陷，且修复成本不到 1 天。
2. **CI 执法链（P0-2）**——Gyre 的测试量与质量、lint 配置都已达到甚至超过典型水准，但全部无保护；执法链是让既有资产产生复利的前提，也是后续每一项 P1/P2 的安全网。
3. **RPC 版本握手先于命令扩充**——协议一旦有第三方客户端（Python/编辑器插件），事后加握手就是破坏性变更；1 行 ready 帧的成本必须现在付。
4. **压缩 pinning 先于 checkpoint 延迟应用**——正确性风险：Gyre 压缩会删节点而 omp 条目树永不删；顺序颠倒会产出「rewind 到已消失位置」的静默数据错误。
5. **README/文档一致性（P0-3）**——能力声明的可信度是开源项目的入口资产；3 处漂移中 MCP 与 tool 注册两条直接误导集成者，修复成本以小时计。

---

## 附：本轮审查证据方法说明

- omp 侧计数源自直接核验：HookAPI 25 事件（`extensibility/hooks/types.ts:479-506`）、ExtensionAPI 45 事件（`extensibility/extensions/types.ts:1236-1295`）、RPC 42 命令（`modes/rpc/rpc-types.ts:28-93`）、docs 126 个 md（根 81 + tools 30 + toolconv 12 + skills 3）、TS 测试 23,290 用例（2365 个 `*.test.ts` 行锚定统计）、Rust 测试 2,534。
- Gyre 侧：33 crates 83,557 LOC（wc -l 汇总）；1060 测试函数静态 grep；vendor 引用关系经 `grep vendor/ crates/*/Cargo.toml Cargo.toml` 确认仅 4 crate（6.2MB），`git ls-files vendor/` = 0，`du` 确认残留 982MB；Phase 0 落地与 429 兑现经 grep 逐处核验（`cli/main.rs:730-742`、`server/lib.rs:1146-1149`、`engine.rs:476-523`、`llm/lib.rs:56-72`）。
- P1 未动工项经反向 grep 确认零命中：`negotiate`、`xd://propose`、`manage_skill`、`streamable`。
- 6 路并行只读 scout 分域：ArchQuality（架构+质量）、TestQa（测试）、BuildRelease（构建发布）、ApiDx（API/DX）、Perf（性能）、DocsExt（文档+扩展）。
