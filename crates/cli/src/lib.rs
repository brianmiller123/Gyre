//! # agent-cli（库面）
//!
//! 二进制 `agent` 之外的独立组件层：供装配层（main）与测试复用。
//! 只收录不反向依赖 repl / rpc / main 模块树的纯功能模块。

pub mod hooks_cfg;
pub mod keybindings;
pub mod manage;
pub mod queue;
pub mod tree_ui;
