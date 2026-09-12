//! # agent-pty
//!
//! PTY 交互式 Shell：跨平台伪终端（`portable-pty`：Unix posix openpty / Windows `ConPTY`），
//! 让 `top`/`vim`/交互式 REPL 等**要求 TTY** 的命令可在 agent 工具中运行。
//!
//! 移植自 [`oh-my-pi pi-shell`](../../../third/oh-my-pi/crates/pi-shell)（运行模型）。
//!
//! - [`run_pty_command`]：一次性 PTY 执行（marker 协议解析退出码）
//! - [`PtyShell`]：持久会话（`stty -echo` + marker，跨命令保持 cwd/环境）
//! - [`RunPtyTool`]：`run_pty_command` 工具（一次性、无状态；与 `run_command` 互补）
//! - [`ShellSessionTool`]：`shell_session` 工具（持久会话，跨命令保持 cwd/环境变量）

#![deny(unsafe_code)]

pub mod session;
pub mod tool;

pub use session::{PtyOptions, PtyResult, PtyShell, run_pty_command};
pub use tool::{RunPtyTool, ShellSessionTool};

/// PTY 执行器适配器（H33）：把本 crate 的一次性 PTY 执行接入
/// [`agent_tools::PtyExecutor`] 端口，供 `run_command(pty: true)` 使用。
pub struct PtyAdapter;

impl agent_tools::PtyExecutor for PtyAdapter {
    fn run(
        &self,
        command: String,
        cwd: Option<std::path::PathBuf>,
        env: std::collections::HashMap<String, String>,
        timeout_ms: Option<u64>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<agent_tools::PtyExecOutcome, String>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let opts = PtyOptions {
                command,
                cwd,
                env,
                timeout_ms,
                ..PtyOptions::default()
            };
            let out = run_pty_command(&opts).await.map_err(|e| e.to_string())?;
            Ok(agent_tools::PtyExecOutcome {
                output: out.output,
                exit_code: out.exit_code,
                timed_out: out.timed_out,
            })
        })
    }
}

/// 构造 PTY 执行器句柄（装配层 `agent_tools::set_pty_executor(agent_pty::executor())`）。
#[must_use]
pub fn executor() -> std::sync::Arc<dyn agent_tools::PtyExecutor> {
    std::sync::Arc::new(PtyAdapter)
}

/// 注入 system prompt 的 PTY 工具使用指引（启用时由装配层追加）。
pub const PROMPT_SECTION: &str = "<pty>\n\
PTY 工具已启用：`run_pty_command` 一次性伪终端执行（返回合并 stdout/stderr 与退出码，\n\
适合需 TTY 的命令如 top/vim/交互式 REPL）；`shell_session` 持久会话（op=run/status/close，\n\
`cd`/`export` 跨命令保留，适合「先切目录再构建」的多步流程）。两者与 run_command（管道 stdio）互补；\n\
均属执行类，默认需审批。\n\
</pty>";
