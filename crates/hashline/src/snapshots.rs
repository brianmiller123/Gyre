//! 会话级文件版本快照存储。
//!
//! 移植自 [`oh-my-pi hashline/snapshots.ts`](https://github.com/can1357/oh-my-pi/blob/master/packages/hashline/src/snapshots.ts)（精简：有界内存版）。
//!
//! 类型实体在 `agent-tools::snapshot`：[`ToolContext`](agent_tools::ToolContext) 的
//! `snapshots` 字段须在 `agent-tools` 内命名该类型，而本 crate 依赖 `agent-tools`
//! （反向会成环）。此处仅再导出，`agent_hashline::InMemorySnapshotStore` 等
//! 既有对外路径保持不变。
//!
//! 写工具在落盘前把「原始正文」记录为该路径的一个版本（按内容指纹去重）；
//! `read_file` 读取真实工作区文本时同样记录（共享同一 store 实例）。
//! 后续当某区段带 stale hash 到来时，recovery 可凭 hash 找回对应历史版本，
//! 重放编辑到当前正文——典型场景是「模型连读带改，第二次编辑仍引用第一次读到的 hash」。

pub use agent_tools::snapshot::{InMemorySnapshotStore, Snapshot};
