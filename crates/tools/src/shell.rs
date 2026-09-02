//! Shell `工具：run_command（跨平台`）。

use std::process::Stdio;
use std::time::Duration;

use agent_core::{ApprovalRequest, CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::intercept::{self, CompiledRule};
use crate::minimizer::{self, Minimized, Minimizer};
use crate::{Tool, ToolContext};

/// 在工作区执行 shell 命令。
///
/// 跨平台：Unix 走 `/bin/sh -c`，Windows 走 `cmd /C`。`kill_on_drop` 确保取消时子进程被回收。
pub struct RunCommandTool {
    /// 命令拦截规则（命中即在 spawn 前重定向到专用工具）。空 Vec = 不拦截。
    intercept: Vec<CompiledRule>,
    /// 输出最小化器：git/cargo/python 等冗长命令输出压缩为摘要。传 [`minimizer::disabled`] 关闭。
    minimizer: Minimizer,
}

impl RunCommandTool {
    /// 构造带指定拦截规则与输出最小化器的 `run_command` 工具。
    #[must_use]
    pub const fn new(intercept: Vec<CompiledRule>, minimizer: Minimizer) -> Self {
        Self {
            intercept,
            minimizer,
        }
    }
}

impl Default for RunCommandTool {
    /// 默认启用内置规则集（cat/grep/find/echo-redirect → 专用工具）；输出最小化默认关闭。
    fn default() -> Self {
        Self {
            intercept: intercept::default_compiled(),
            minimizer: minimizer::disabled(),
        }
    }
}

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &'static str {
        "run_command"
    }
    fn description(&self) -> &'static str {
        "在工作区目录执行 shell 命令并返回合并的 stdout/stderr。属于执行类操作，默认需审批。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "完整命令字符串" }
            },
            "required": ["command"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        CapabilityTier::Execute
    }

    /// `run_command` 可被 steering 中途打断：execute 内已用 `select! { cancel.cancelled() => .. }`
    /// 响应 `ctx.cancel`（批级 token），故 Immediate 模式下用户中途发消息会尽快中止在途命令
    /// （`kill_on_drop` 回收子进程），steering 随后在下轮被处理。
    fn interruptible(&self) -> bool {
        true
    }

    fn describe<'a>(&'a self, input: &'a serde_json::Value) -> ApprovalRequest<'a> {
        ApprovalRequest {
            tool: self.name(),
            capability: self.capability(),
            command: input.get("command").and_then(|v| v.as_str()),
            args: input,
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let command = input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `command` 参数".into()))?;

        // 命令拦截：把 cat/grep/find/echo-redirect 等有专用工具等价物的命令，在 spawn 前重定向
        // 到专用工具（移植 oh-my-pi bash 拦截器）。命中即返回可恢复错误，回灌模型使其改用专用
        // 工具——不计入连续错误预算（`ToolError::Execution` 的 `is_recoverable` 为真）。
        if let Some(rule) = intercept::check(command, &self.intercept) {
            return Err(ToolError::Execution(format!(
                "已拦截：该命令有更合适的专用工具——改用 `{}` 工具（{}）。\n原始命令：{command}",
                rule.tool, rule.message
            )));
        }

        // 拆分命令名与参数，供输出最小化器做过滤器匹配（只读，命中才改写结果文本）。
        let (cmd_name, cmd_args) = minimizer::split_command(command);

        // 进程内内建优先：命中 pi-builtins 注册表的命令（cat/sed/jq/sponge/… 50+ 工具
        // 与 POSIX/bash 内建）在宿主进程内直接执行，不 fork 外部二进制，跨平台行为一致
        // （Windows 不依赖宿主安装 coreutils）。`GYRE_DISABLE_INPROC_BUILTINS` 环境变量
        // 可整体退回子进程路径。破坏性工具（rm/mv/ln）不在表内，仍走系统二进制。
        let (exit_success, exit_code, combined) = if inproc_enabled()
            && agent_shell::is_inproc_command(&cmd_name)
        {
            let output =
                run_inproc_command(command, &ctx.workspace.root(), ctx.cancel, CMD_TIMEOUT).await?;
            (
                output.exit_code == Some(0),
                output.exit_code,
                output.combined,
            )
        } else {
            let mut cmd = shell_command(command);
            cmd.current_dir(ctx.workspace.root())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            let cancel = ctx.cancel;
            // 墙钟超时 + 取消双门禁 + 流式有界读取：任一触发都会 drop run_command_capped future，
            // 借助 `kill_on_drop(true)` 回收子进程；stdout/stderr 各按 CMD_MAX_OUTPUT 上限读取，
            // 杜绝命令瞬时产出海量数据撑爆内存（修复 OOM）。
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return Err(ToolError::Execution("命令被取消".into()));
                }
                output = tokio::time::timeout(CMD_TIMEOUT, run_command_capped(cmd)) => match output {
                    Ok(Ok(CmdOutput { status, combined })) => (
                        status.is_some_and(|s| s.success()),
                        status.and_then(|s| s.code()),
                        combined,
                    ),
                    Ok(Err(e)) => return Err(ToolError::Io(e)),
                    Err(_) => return Err(ToolError::Execution(format!("命令超时（{CMD_TIMEOUT:?}）"))),
                }
            }
        };

        // 输出最小化：命中过滤器时以摘要替代完整输出（退出码语义由下方分支保留）。
        // 空输出不压缩（过滤器对空输出返回 None，落到原路径）。
        let text = match self.minimizer.apply(&cmd_name, &cmd_args, &combined) {
            Some(Minimized { summary, filter }) => format!(
                "{summary}\n> 输出已由 minimizer 压缩（{filter}）；如需完整输出请重跑该命令。"
            ),
            None if combined.is_empty() => {
                // 成功且无输出（mkdir/touch/git config 等静默命令）：显式标注。
                // 既让模型明确「命令已成功执行」避免误判/重试，又在源头消除空文本
                // （序列化层另有兜底，此处为语义与 UI 改善）。
                "(命令成功，无输出)".to_string()
            }
            None => combined,
        };
        if exit_success {
            Ok(ToolResult::text(text))
        } else {
            Ok(ToolResult::text(format!(
                "[exit {}]\n{text}",
                exit_code.unwrap_or(-1)
            )))
        }
    }
}

/// 子进程执行结果（流式有界读取后）。
struct CmdOutput {
    /// 退出状态。
    status: Option<std::process::ExitStatus>,
    /// 合并后的输出文本（已截断）。
    combined: String,
}

/// 启动子进程并并发读取 stdout/stderr（各自按 `CMD_MAX_OUTPUT` 上限，防 OOM），
/// 等待退出后合并为单段文本。
///
/// 注意：[`read_capped`] 达上限后会继续**丢弃式读取**直到 EOF（仅保留前 `max` 字节），
/// 确保子进程不会因管道写满而阻塞——否则 `child.wait()` 将死锁，只能靠墙钟超时兜底。
async fn run_command_capped(mut cmd: tokio::process::Command) -> Result<CmdOutput, std::io::Error> {
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // 并发读取 stdout/stderr 与等待子进程退出——使用 join 而非 spawn，
    // 确保 future 被 drop（取消/超时）时读取任务也随之取消，不留孤儿。
    let (status, out_bytes, err_bytes) = tokio::join!(
        child.wait(),
        read_capped(stdout, CMD_MAX_OUTPUT),
        read_capped(stderr, CMD_MAX_OUTPUT),
    );
    Ok(CmdOutput {
        status: Some(status?),
        combined: combine_capped(out_bytes, err_bytes),
    })
}

/// 读取流的前 `max` 字节保留；触及上限后继续**丢弃式读取**直到 EOF，
/// 保证管道持续排空、子进程不阻塞（仅返回给上层的文本被截断到 `max`）。
async fn read_capped<R>(reader: Option<R>, max: usize) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let Some(mut r) = reader else {
        return Vec::new();
    };
    let mut buf = Vec::with_capacity(max.min(64 * 1024));
    let mut tmp = [0u8; 8192];
    let mut capped = false;
    loop {
        match r.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                if !capped {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.len() >= max {
                        buf.truncate(max);
                        capped = true;
                    }
                }
                // capped 后继续读但不保存：保持管道排空，子进程不阻塞。
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!(target: "tools::shell", "流读取错误（返回已读部分）: {e}");
                break;
            }
        }
    }
    buf
}

/// 合并 stdout/stderr 为单段文本，并按总上限截断。
fn combine_capped(out: Vec<u8>, err: Vec<u8>) -> String {
    let mut combined = String::new();
    if !out.is_empty() {
        combined.push_str(&decode_output(&out));
    }
    if !err.is_empty() {
        if !combined.is_empty() {
            combined.push_str("\n[stderr]\n");
        }
        combined.push_str(&decode_output(&err));
    }
    if combined.len() > CMD_MAX_OUTPUT {
        // 回退到最近的 UTF-8 字符边界，避免 truncate panic。
        let mut cut = CMD_MAX_OUTPUT;
        while cut > 0 && !combined.is_char_boundary(cut) {
            cut -= 1;
        }
        combined.truncate(cut);
        combined.push_str("\n...(输出过长，已截断)");
    }
    combined
}
/// 进程内执行结果（与子进程路径同构的退出码 + 合并文本）。
struct InprocOutput {
    exit_code: Option<i32>,
    combined: String,
}

/// 进程内执行命令（agent-shell/brush 内建，不 fork）。
///
/// 取消：`cancel`（批级令牌）经桥接任务转成 brush 的 [`agent_shell::AbortReason::User`]；
/// 超时：`timeout` 墙钟注入 [`agent_shell::CancelToken`]。输出合并格式与子进程路径一致
/// （`[stderr]` 分隔、`CMD_MAX_OUTPUT` 截断）。
async fn run_inproc_command(
    command: &str,
    cwd: &std::path::Path,
    cancel: &tokio_util::sync::CancellationToken,
    timeout: Duration,
) -> Result<InprocOutput, ToolError> {
    // 会话环境继承宿主进程（$PATH/$HOME 等在进程内命令中可用）。
    let shell = agent_shell::InProcShell::new(Some(std::env::vars().collect()));

    let mut ct = agent_shell::CancelToken::with_timeout(Some(timeout));
    let abort = ct.emplace_abort_token();
    let bridge = tokio::spawn({
        let abort = abort.clone();
        let cancel = cancel.clone();
        async move {
            cancel.cancelled().await;
            abort.abort(agent_shell::AbortReason::User);
        }
    });
    let outcome = shell
        .run_with_cancel(
            command,
            &agent_shell::RunOptions {
                cwd: Some(cwd.to_path_buf()),
                env: None,
                timeout_ms: None, // 超时已由 CancelToken 承担（含取消桥接）
            },
            &mut ct,
        )
        .await
        .map_err(|e| ToolError::Execution(format!("进程内命令执行失败: {e}")))?;
    bridge.abort();

    if outcome.timed_out {
        return Err(ToolError::Execution(format!("命令超时（{timeout:?}）")));
    }
    // brush 的 cancelled 只映射 Signal 中止；经桥接的 User 中止（批级取消）表现为
    // 无退出码——两者统一按「命令被取消」上报。
    if outcome.cancelled || outcome.exit_code.is_none() {
        return Err(ToolError::Execution("命令被取消".into()));
    }

    Ok(InprocOutput {
        exit_code: outcome.exit_code,
        combined: combine_inproc(&outcome.stdout, &outcome.stderr),
    })
}

/// 合并进程内执行的 stdout/stderr 为单段文本（与子进程路径相同的 `[stderr]` 分隔
/// 格式），并按总上限截断。
fn combine_inproc(stdout: &str, stderr: &str) -> String {
    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push_str("\n[stderr]\n");
        }
        combined.push_str(stderr);
    }
    if combined.len() > CMD_MAX_OUTPUT {
        // 回退到最近的 UTF-8 字符边界，避免 truncate panic。
        let mut cut = CMD_MAX_OUTPUT;
        while cut > 0 && !combined.is_char_boundary(cut) {
            cut -= 1;
        }
        combined.truncate(cut);
        combined.push_str("\n...(输出过长，已截断)");
    }
    combined
}

/// 进程内内建开关：`GYRE_DISABLE_INPROC_BUILTINS` 环境变量存在时退回子进程路径。
fn inproc_enabled() -> bool {
    std::env::var_os("GYRE_DISABLE_INPROC_BUILTINS").is_none()
}

/// 构造 Windows 命令前置：`chcp 65001 >nul && <command>`，把 cmd 控制台代码页切到 UTF-8。
///
/// 单独成函数便于在非 Windows 平台单测包裹格式。`>nul` 抑制 `chcp` 自身的
/// "Active code page: 65001" 提示行，避免污染命令输出。
// 非 Windows 构建无调用点（仅 Windows 走 `chcp` 路径）；保留供跨平台单测断言包裹格式。
#[allow(dead_code)]
fn windows_utf8_wrapper(command: &str) -> String {
    format!("chcp 65001 >nul && {command}")
}

/// 解码子进程输出字节为字符串。
///
/// 优先严格 UTF-8——force-UTF-8（Unix 经 `forced_utf8_locale` 注入 `LC_ALL/LANG；Windows` 经
/// `chcp 65001` + Python env）生效时输出即 UTF-8，此路径零损耗。失败时按 `gbk_fallback`
/// 决定兜底：Windows 下按 GBK（中文 Windows 的 OEM 代码页 CP936）解码以正确显示中文；
/// 其余情形退化为 lossy，避免把罕见非 UTF-8 字节误判为 GBK。
fn decode_bytes(bytes: &[u8], gbk_fallback: bool) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    if gbk_fallback {
        encoding_rs::GBK
            .decode_without_bom_handling(bytes)
            .0
            .into_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// 解码子进程输出：严格 UTF-8 优先；Windows 下失败回退 GBK，其余回退 lossy。
fn decode_output(bytes: &[u8]) -> String {
    decode_bytes(bytes, cfg!(windows))
}

/// 命令墙钟超时上限。
const CMD_TIMEOUT: Duration = Duration::from_secs(120);
/// 合并输出大小上限（超出截断，防 OOM）。
const CMD_MAX_OUTPUT: usize = 256 * 1024;

/// 跨平台构造 shell 命令。
fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(unix)]
    {
        let mut c = tokio::process::Command::new("/bin/sh");
        c.arg("-c").arg(command);
        // 保障 UTF-8 输出：继承的 locale 非 UTF-8（服务端常由 systemd / 容器 / 后台启动器
        // 拉起）时，中文等程序会以 GBK 编码输出，经 from_utf8_lossy 解码即乱码。
        if let Some(loc) = agent_core::forced_utf8_locale() {
            c.env("LC_ALL", loc);
            c.env("LANG", loc);
        }
        c
    }
    #[cfg(windows)]
    {
        let mut c = tokio::process::Command::new("cmd");
        // 强制 UTF-8：前置 `chcp 65001` 把 cmd 控制台代码页切到 UTF-8，使 cmd 内建命令
        // （echo/dir…）及尊重输出代码页的程序以 UTF-8 输出（`>nul` 抑制 chcp 自身提示行）。
        // 与 Unix 的 `forced_utf8_locale` 注入对齐，确保下游 `from_utf8` 解码不乱码。
        c.arg("/C").arg(windows_utf8_wrapper(command));
        // Python：强制 UTF-8 stdio（覆盖 Windows 默认的 ANSI 代码页）。
        c.env("PYTHONUTF8", "1");
        c.env("PYTHONIOENCODING", "utf-8");
        c
    }
    #[cfg(not(any(unix, windows)))]
    {
        compile_error!("run_command 仅支持 unix 与 windows");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_wrapper_prepends_utf8_codepage() {
        assert_eq!(
            windows_utf8_wrapper("echo 你好"),
            "chcp 65001 >nul && echo 你好"
        );
    }

    #[test]
    fn decode_bytes_passthrough_valid_utf8() {
        assert_eq!(decode_bytes("你好".as_bytes(), true), "你好");
        assert_eq!(decode_bytes(b"ascii", false), "ascii");
    }

    #[test]
    fn decode_bytes_gbk_fallback_recovers_chinese() {
        // "你好" in GBK (CP936): C4 E3 BA C3 —— 既非合法 UTF-8，应触发 GBK 兜底。
        let gbk = [0xC4, 0xE3, 0xBA, 0xC3];
        assert_eq!(decode_bytes(&gbk, true), "你好");
    }

    #[test]
    fn decode_bytes_lossy_when_fallback_disabled() {
        // 同样的 GBK 字节，关闭兜底时走 lossy（含 U+FFFD），不抛错。
        let gbk = [0xC4, 0xE3, 0xBA, 0xC3];
        let s = decode_bytes(&gbk, false);
        assert!(s.contains('\u{FFFD}'), "lossy 应含替换字符，实际: {s:?}");
    }
    #[test]
    fn inproc_command_table() {
        // 常用工具与内建命中进程内路径。
        for name in [
            "cat", "sed", "grep", "jq", "wc", "find", "cd", "echo", "printf", "sponge",
        ] {
            assert!(
                agent_shell::is_inproc_command(name),
                "{name} 应可进程内执行"
            );
        }
        // 破坏性三件套 withheld，仍走系统二进制。
        for name in ["rm", "mv", "ln"] {
            assert!(
                !agent_shell::is_inproc_command(name),
                "{name} 不应进程内执行"
            );
        }
        // 外部程序不受影响。
        for name in ["git", "python3", "cargo", "node"] {
            assert!(!agent_shell::is_inproc_command(name), "{name} 应走子进程");
        }
    }

    #[tokio::test]
    async fn inproc_executes_pipeline() {
        let cwd = std::env::current_dir().unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let out = run_inproc_command(
            "echo 'hello world' | tr a-z A-Z",
            &cwd,
            &cancel,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.combined, "HELLO WORLD\n");
    }

    #[tokio::test]
    async fn inproc_reports_failure() {
        let cwd = std::env::current_dir().unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let out = run_inproc_command(
            "cat /nonexistent/definitely-missing",
            &cwd,
            &cancel,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_ne!(out.exit_code, Some(0));
        assert!(
            out.combined.to_lowercase().contains("no such file"),
            "stderr 应含错误信息，实际: {:?}",
            out.combined
        );
    }

    #[tokio::test]
    async fn inproc_honours_timeout() {
        let cwd = std::env::current_dir().unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let err = run_inproc_command("sleep 30", &cwd, &cancel, Duration::from_millis(100)).await;
        assert!(err.is_err(), "sleep 应被超时中断");
    }

    #[tokio::test]
    async fn inproc_respects_cancellation() {
        let cwd = std::env::current_dir().unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let handle = tokio::spawn({
            let cwd = cwd.clone();
            let cancel = cancel.clone();
            async move { run_inproc_command("sleep 30", &cwd, &cancel, Duration::from_secs(30)).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        let err = handle.await.unwrap();
        assert!(err.is_err(), "批级取消应中断进程内命令");
    }
}
