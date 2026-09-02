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

### Fixed
- 429 限流处理：兑现 `Retry-After`，同模型指数退避（上限 3 次 / 30s），重试可被取消打断。
- README 漂移修正：MCP 能力表述（实际为 stdio 传输 + 4 方法）、工具注册机制表述、模块树（23 → 33 crates）、仓库 URL。
- `docs/` 解除 gitignore：设计文档与对标分析随仓库分发。

### Changed
- CI 移除 `RUSTFLAGS=-A warnings`，新增 rustfmt / clippy（`-D warnings`）/ nextest 三道执法门；
  advisor / discovery / shell 三个此前未接入 workspace lints 的 crate 已接入。
- **lint 基线校准**：`pedantic` / `nursery` 整组不再作为执法基线（83k 行存量从未在其下维护，
  整组开启产生 600+ 风格噪音；nursery 官方自述接受不稳定 lint），改为默认组
  （all / suspicious / complexity / style）warn + `correctness` / `perf` deny + 高信号
  pedantic 项显式入选（`unchecked_time_subtraction` / `mutex_integer` / `mem_forget` /
  `rc_buffer` / `vec_box` / `option_option`），修完后可逐项升回。
- 移除 29 个 crate `lib.rs` 中与 workspace lints 冲突的 `#![warn(clippy::pedantic)]` /
  `#![warn(missing_docs)]` 属性墙（属性级别高于 Cargo.toml 配置，会架空整份 allow 名单）；
  lint 政策收敛到根 `Cargo.toml` 单一事实源。`core` 的 `missing_docs` 门禁经核实零告警，保留。
> 历史标签 v0.1.0 – v0.1.12 早于本 changelog 与可工作的 CI（彼时发布产物无法从 fresh checkout 编译），
> 不逐版补录。
