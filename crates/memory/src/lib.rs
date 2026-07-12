//! # agent-memory
//!
//! 跨会话长期记忆（移植 oh-my-pi `memory.backend: local` 的 local summary pipeline，去 SQLite/embedding）。
//!
//! - [`LocalMemoryStore`]：按项目（cwd）作用域，markdown 存储 + LLM 合并
//! - 启动注入 `summary()` 到 system prompt；任务后 `append_note()` 追加事实；
//!   `consolidate()` 用 LLM 把 raw notes + 旧 MEMORY.md 合并为新 MEMORY.md + memory_summary.md
//!
//! 存储布局：`<config_dir>/memory/<cwd 哈希十六进制>/{MEMORY.md, memory_summary.md, notes.jsonl}`。
//! Zoo-Code 无此能力，本项目补齐项目作用域的跨会话记忆。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

mod store;
mod structured;

pub use store::{consolidation_prompt, LocalMemoryStore};
pub use structured::{
    MemoryRecord, MemoryStats, RecallHit, RecallOptions, SearchFilter, StructuredMemoryStore,
};
