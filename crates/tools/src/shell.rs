//! Shell 工具：run_command（跨平台）。

use std::process::Stdio;
use std::time::Duration;

use agent_core::{ApprovalRequest, CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::{Tool, ToolContext};

/// 在工作区执行 shell 命令。
///
/// 跨平台：Unix 走 `/bin/sh -c`，Windows 走 `cmd /C`。`kill_on_drop` 确保取消时子进程被回收。
pub struct RunCommandTool;

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }
    fn description(&self) -> &str {
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
            _ = cancel.cancelled() => {
                Err(ToolError::Execution("命令被取消".into()))
            }
            output = tokio::time::timeout(CMD_TIMEOUT, run_command_capped(cmd)) => match output {
                Ok(Ok(CmdOutput { status, combined })) => {
                    if status.map_or(false, |s| s.success()) {
                        // 成功且无输出（mkdir/touch/git config 等静默命令）：显式标注。
                        // 既让模型明确「命令已成功执行」避免误判/重试，又在源头消除空文本
                        // （序列化层另有兜底，此处为语义与 UI 改善）。
                        let text = if combined.is_empty() {
                            "(命令成功，无输出)".to_string()
                        } else {
                            combined
                        };
                        Ok(ToolResult::text(text))
                    } else {
                        Ok(ToolResult::text(format!(
                            "[exit {}]\n{combined}",
                            status.and_then(|s| s.code()).unwrap_or(-1)
                        )))
                    }
                }
                Ok(Err(e)) => Err(ToolError::Io(e)),
                Err(_) => Err(ToolError::Execution(format!(
                    "命令超时（{CMD_TIMEOUT:?}）"
                ))),
            }
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
async fn run_command_capped(
    mut cmd: tokio::process::Command,
) -> Result<CmdOutput, std::io::Error> {
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
        combined.push_str(&String::from_utf8_lossy(&out));
    }
    if !err.is_empty() {
        if !combined.is_empty() {
            combined.push_str("\n[stderr]\n");
        }
        combined.push_str(&String::from_utf8_lossy(&err));
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
        c.arg("/C").arg(command);
        c
    }
    #[cfg(not(any(unix, windows)))]
    {
        compile_error!("run_command 仅支持 unix 与 windows");
    }
}
