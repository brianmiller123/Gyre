# Changelog

本项目的所有显著变更将记录在此文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)，
版本号遵循 [Semantic Versioning](https://semver.org/spec/v2.0.0.html)。

## [Unreleased]

### Added
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
- 危险命令硬拦截清单：移植上游 `CRITICAL_BASH_PATTERNS` 21 条（`rm -rf /`、`--no-preserve-root`、
  fork 炸弹、dd/mkfs/shred、`> /etc/passwd`、`curl | sh` 及进程替换/eval 变体、关机、`nc -e` 等），
  命中即拒并附原因提示。
- `run_command` 非交互环境基线（移植上游 `NON_INTERACTIVE_ENV`）：pagers→cat、`TERM=dumb`、
  `NO_COLOR`、编辑器→`true`、`GIT_TERMINAL_PROMPT=0`、包管理器非交互；`GYRE_BASH_NO_CI` 可退出
  `CI=true` 注入。

### Fixed
- 429 限流处理：兑现 `Retry-After`，同模型指数退避（上限 3 次 / 30s），重试可被取消打断。
- README 漂移修正：MCP 能力表述（实际为 stdio 传输 + 4 方法）、工具注册机制表述、模块树（23 → 33 crates）、仓库 URL。
- `docs/` 解除 gitignore：设计文档与对标分析随仓库分发。
- 跨平台命令语义分叉：模型按提示词写 bash，Linux（Debian 系 `/bin/sh` → dash）下数组/`[[ ]]`
  失败、Windows 落到 `cmd` 而提示词却宣称 PowerShell——引擎统一后消除。
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
- 移除 29 个 crate `lib.rs` 中与 workspace lints 冲突的 `#![warn(clippy::pedantic)]` /
  `#![warn(missing_docs)]` 属性墙（属性级别高于 Cargo.toml 配置，会架空整份 allow 名单）；
  lint 政策收敛到根 `Cargo.toml` 单一事实源。`core` 的 `missing_docs` 门禁经核实零告警，保留。
> 历史标签 v0.1.0 – v0.1.12 早于本 changelog 与可工作的 CI（彼时发布产物无法从 fresh checkout 编译），
> 不逐版补录。
