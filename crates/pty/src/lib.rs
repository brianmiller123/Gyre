//! # agent-pty
//!
//! PTY 交互式 Shell：跨平台伪终端（`portable-pty`：Unix posix openpty / Windows ConPTY），
//! 让 `top`/`vim`/交互式 REPL 等**要求 TTY** 的命令可在 agent 工具中运行。
//!
//! 移植自 [`oh-my-pi pi-shell`](../../../third/oh-my-pi/crates/pi-shell)（运行模型）。
//!
//! - [`run_pty_command`]：一次性 PTY 执行（marker 协议解析退出码）
//! - [`PtyShell`]：持久会话（`stty -echo` + marker，跨命令保持 cwd/环境）
//! - [`RunPtyTool`]：`run_pty_command` 工具（与 `run_command` 互补）

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

pub mod session;
pub mod tool;

pub use session::{PtyOptions, PtyResult, PtyShell, run_pty_command};
pub use tool::RunPtyTool;

/// 注入 system prompt 的 PTY 工具使用指引（启用时由装配层追加）。
pub const PROMPT_SECTION: &str = "<pty>\n\
PTY 工具 `run_pty_command` 已启用：在伪终端执行命令，返回合并 stdout/stderr 与退出码。\n\
适用于需要 TTY 的命令（top/vim/交互式 REPL 等），与 run_command（管道 stdio）互补。属执行类，默认需审批。\n\
</pty>";
