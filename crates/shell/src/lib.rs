//! 进程内命令行工具执行器。
//!
//! 基于 oh-my-pi 的 `pi-shell`（brush shell + 50+ 进程内 coreutils 内建，
//! vendored 自 `third/oh-my-pi`，见 `vendor/`）封装：在宿主进程内直接执行
//! `cat`、`grep`、`sed`、`ls`、`find`、`jq`、`sponge` 等工具，**不 fork 外部
//! 二进制**，行为跨平台一致（Windows 上同样可用，不依赖宿主安装 coreutils）。
//!
//! 与 `agent-tools` 的 `run_command`（`/bin/sh -c` 子进程）互补：本 crate 面向
//! 长驻会话内的零 fork 执行与可编程输出裁剪；`run_command` 保留给必须真正
//! 落到子进程的场景。
//!
//! # 示例
//!
//! ```
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! use agent_shell::{InProcShell, RunOptions};
//!
//! let shell = InProcShell::new(None);
//! let out = shell
//!     .run("echo 'hello' | tr a-z A-Z", &RunOptions::default())
//!     .await
//!     .unwrap();
//! assert_eq!(out.exit_code, Some(0));
//! assert_eq!(out.stdout, "HELLO\n");
//! # })
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use anyhow::Result;
use brush_core::extensions::DefaultShellExtensions;
use pi_builtins::{BuiltinSet, default_builtins, process_builtins, utility_builtins};
use pi_shell::{ShellExecuteOptions, StreamSinks};

pub use pi_shell::cancel::AbortReason;

pub use pi_shell::cancel::CancelToken;

/// 单次进程内命令执行的可选参数。
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// 工作目录；`None` = 宿主进程当前目录。
    pub cwd: Option<std::path::PathBuf>,
    /// 命令级环境变量，覆盖会话环境中的同名项。
    pub env: Option<HashMap<String, String>>,
    /// 超时（毫秒）；`None` = 不限时。
    pub timeout_ms: Option<u32>,
}

/// 一次进程内命令执行的完整结果。
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    /// 退出码；`None` 表示未正常退出（如被信号终止）。
    pub exit_code: Option<i32>,
    /// stdout 文本。
    pub stdout: String,
    /// stderr 文本。
    pub stderr: String,
    /// 是否被取消。
    pub cancelled: bool,
    /// 是否超时。
    pub timed_out: bool,
    /// 执行结束后的实际工作目录（相对路径已按 `cwd` 解析）。
    pub working_dir: Option<String>,
}

/// 进程内命令行执行门面：注册了 POSIX/bash 内建与 50+ 进程内工具，
/// 每次 [`run`](Self::run) 是一次独立的 oneshot 执行（无会话状态）。
pub struct InProcShell {
    session_env: Option<HashMap<String, String>>,
}

impl InProcShell {
    /// 创建一个新的进程内 shell。
    ///
    /// `session_env` 提供会话级环境（如 `PATH`、`HOME`），所有命令共享；
    /// 命令级覆盖见 [`RunOptions::env`]。
    pub const fn new(session_env: Option<HashMap<String, String>>) -> Self {
        Self { session_env }
    }

    /// 执行一条完整命令行（支持管道、重定向、内建与工具），stdout/stderr 分离捕获。
    pub async fn run(&self, command: &str, options: &RunOptions) -> Result<CommandOutcome> {
        let mut cancel = CancelToken::new(options.timeout_ms);
        self.run_with_cancel(command, options, &mut cancel).await
    }

    /// 同 [`run`](Self::run)，但取消令牌由调用方提供：可先 `emplace_abort_token()`
    /// 把外部取消（如批级取消令牌）桥接为 [`AbortReason::User`]，或携带自己的超时。
    pub async fn run_with_cancel(
        &self,
        command: &str,
        options: &RunOptions,
        cancel: &mut CancelToken,
    ) -> Result<CommandOutcome> {
        let (stdout_tx, stdout_rx) = flume::bounded::<bytes::Bytes>(64);
        let (stderr_tx, stderr_rx) = flume::bounded::<bytes::Bytes>(64);

        let stdout_task = tokio::spawn(drain_stream(stdout_rx));
        let stderr_task = tokio::spawn(drain_stream(stderr_rx));

        let result = pi_shell::execute_shell_streams(
            ShellExecuteOptions {
                command: command.to_owned(),
                cwd: options
                    .cwd
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                env: options.env.clone(),
                session_env: self.session_env.clone(),
                timeout_ms: options.timeout_ms,
                snapshot_path: None,
                minimizer: None,
            },
            StreamSinks {
                stdout: Some(stdout_tx),
                stderr: Some(stderr_tx),
            },
            cancel.clone(),
        )
        .await?;

        let stdout = stdout_task.await?;
        let stderr = stderr_task.await?;

        Ok(CommandOutcome {
            exit_code: result.exit_code,
            stdout,
            stderr,
            cancelled: result.cancelled,
            timed_out: result.timed_out,
            working_dir: result.working_dir,
        })
    }

    /// 执行单个工具，参数自动做 shell 引用转义。
    ///
    /// `name` 可以是内建（`cd`、`echo`、`printf`…）或进程内工具（`cat`、
    /// `grep`、`sed`、`jq`、`wc`…）；参数中的空白、引号等特殊字符会被安全
    /// 引用，不会被当作语法解析。等价于
    /// `run(&agent_shell::command_line(name, args), options)`。
    pub async fn run_utility(
        &self,
        name: &str,
        args: &[String],
        options: &RunOptions,
    ) -> Result<CommandOutcome> {
        self.run(&command_line(name, args), options).await
    }
}

/// 把 `name` 与参数拼成命令行，参数逐个做 POSIX 单引号转义。
pub fn command_line(name: &str, args: &[String]) -> String {
    let capacity = name.len() + args.iter().map(String::len).sum::<usize>() + args.len();
    let mut line = String::with_capacity(capacity);
    line.push_str(name);
    for arg in args {
        line.push(' ');
        line.push_str(&shell_quote(arg));
    }
    line
}

/// 仅含这些字节的参数可原样拼接，其余一律单引号包裹（内部 `'` 转义为
/// `'\''`，POSIX 标准做法）。
fn shell_quote(arg: &str) -> String {
    const SAFE: &[u8] = b"-._/:@%+=,";
    if arg.is_empty() {
        return "''".to_owned();
    }
    if arg
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || SAFE.contains(&b))
    {
        return arg.to_owned();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

async fn drain_stream(rx: flume::Receiver<bytes::Bytes>) -> String {
    let mut buf = String::new();
    while let Ok(chunk) = rx.recv_async().await {
        buf.push_str(&String::from_utf8_lossy(&chunk));
    }
    buf
}

/// 破坏性工具三件套：与 omp 嵌入方默认一致，这些命令仍走系统二进制
/// （agent-shell 不暴露进程内实现，避免在宿主进程内误伤文件）。
const WITHHELD_UTILITIES: &[&str] = &["rm", "mv", "ln"];

/// 全部可进程内执行的命令名（POSIX/bash 内建 + 进程内工具 + 进程工具，扣除
/// [`WITHHELD_UTILITIES`]）。构建自 pi-builtins 注册表，随 vendored 代码自动同步。
#[must_use]
pub fn inproc_command_names() -> &'static HashSet<String> {
    static NAMES: LazyLock<HashSet<String>> = LazyLock::new(|| {
        let mut names: HashSet<String> =
            default_builtins::<DefaultShellExtensions>(BuiltinSet::BashMode)
                .into_keys()
                .collect();
        names.extend(
            utility_builtins::<DefaultShellExtensions>()
                .into_iter()
                .map(|(n, _)| n.to_owned()),
        );
        names.extend(
            process_builtins::<DefaultShellExtensions>()
                .into_iter()
                .map(|(n, _)| n.to_owned()),
        );
        for withheld in WITHHELD_UTILITIES {
            names.remove(*withheld);
        }
        names
    });
    &NAMES
}

/// `name` 是否可进程内执行（不 fork 外部二进制）。
#[must_use]
pub fn is_inproc_command(name: &str) -> bool {
    inproc_command_names().contains(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_special_characters() {
        assert_eq!(shell_quote("plain"), "plain");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_quote("a;b"), "'a;b'");
        assert_eq!(shell_quote("-n"), "-n");
        assert_eq!(shell_quote("路径"), "'路径'");
    }

    #[test]
    fn builds_command_line() {
        let args = ["a b".to_owned(), "c'd".to_owned()];
        assert_eq!(command_line("printf", &args), "printf 'a b' 'c'\\''d'");
        assert_eq!(command_line("cat", &[]), "cat");
    }
}
