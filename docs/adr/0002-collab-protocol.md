# ADR 0002：协同（collab）线协议——保持自有 proto=1，做显式版本拒绝

- 状态：**已接受**（2026-09-11）
- 关联：差距分析 H39（`docs/oh-my-pi-gap-analysis-2026-09-11.md` §5.1）、H38（文档）
- 决策者：Gyre 维护者；记录者：本轮差距修复

## 背景

oh-my-pi 的 `collab-web` 线协议当前是 `proto=3`（`packages/collab-web/src/wire/index.ts:397`）。
Gyre 的协同通道（`crates/collab/`）移植自同一模型，但版本号固定为 `1`
（`WireFrame::{Hello, Welcome}.proto`）。两者**帧集合与字段命名已经不同**：

| 能力 | omp proto=3 | Gyre proto=1 |
|---|---|---|
| 握手 | `hello` / `welcome` | `hello` / `welcome`（含 `read_only` / `entry_count`） |
| 快照传输 | 分块（chunked） | `snapshot_chunk` 分块 + `final_chunk` 收尾 |
| 退出 | —— | `bye`（带 reason） |
| 错误 | —— | `error`（版本/权限/乱序） |
| 传输 | 端到端密封 | 端到端密封（AES-GCM；中继密钥盲视） |

结论：**帧语义已分叉**，把版本号改成 `3` 并不能带来互操作，只会掩盖差异。

## 决策

1. **保持 `proto=1` 作为 Gyre 自有协议**（`agent_collab::PROTO_VERSION`），
   **不对齐 omp proto=3**：协同通道是 Gyre 自用的「观察/陪跑」面（Web 前端 + 只读 view 链接），
   没有跨工具互操作的真实诉求；强行对齐会让版本号撒谎。
2. **显式版本拒绝，不静默降级**：
   - Rust：`CollabClient::decode_checked` 校验 `Welcome.proto`，不匹配返回
     `CollabError::ProtoMismatch { remote, local }`（消息含两端版本与「刷新/升级」建议）；
     `proto_compatible` 只接受**完全相等**。
   - Guest 页面（`collab_guest.html`）：收到 `welcome` 时比对 `PROTO_VERSION`，不一致直接
     红色状态 + 系统提示并停止交互。
   - Host：`seal_host_welcome` 使用 `agent_collab::PROTO_VERSION` 常量（单一事实源）。
3. **协议版本只在握手帧声明**：`Chat`/`Tool`/`Presence`/`Sync`/`SnapshotChunk`/`Bye`/`Error`
   不带版本字段（`decode_checked` 只对 `Welcome` 做校验）——避免每帧重复负担。
4. **不承诺跨版本兼容**：版本号变化即视为破坏性变更，必须同时更新 Rust 常量、guest 页面
   常量与本节表格。

## 后果

- 正面：版本字段变成**可执行契约**（错配立即报错），而不是装饰；测试覆盖「错配拒绝 +
  同版本通过 + 非握手帧不受影响」。
- 负面：与 omp 的协同房间**互不可用**（本来也不可用——帧集合不同）；若未来要与 omp
  协作，必须做协议映射层而不是改版本号。
- 已知未覆盖：host 侧桥接是密钥盲视的，**无法**在 host 端检查 guest 的 `hello.proto`；
  因此协商是「guest 校验 host 的 welcome」。若将来需要 host 侧拒绝，需要引入一个
  非密封的版本前言（属于协议破坏性变更，届时 bump proto）。

## 何时重新评估（触发条件）

1. 需要与 oh-my-pi / 其它实现了同一协同协议的客户端**同房间协作**（此时应设计显式
   `proto` 映射层，并在 `welcome` 中宣告双方版本）；
2. 需要 host 侧在握手阶段拒绝不兼容 guest（引入非密封前言或签名版本声明）；
3. 协同通道需要承载新的安全语义（端到端身份、消息签名），届时帧集合将再次变化。
