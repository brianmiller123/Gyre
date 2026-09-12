//! `run_pty_command` 工具：在 PTY 中执行命令（TTY 依赖命令可运行）。
//!
//! 与 `run_command`（管道 stdio）互补：本工具走伪终端，使 `top -b`、`vim`、
//! 交互式 REPL 等**要求 TTY** 的命令能正常执行并捕获输出。

use std::collections::HashMap;

use agent_core::{ApprovalRequest, CapabilityTier, ToolError, ToolResult};
use agent_tools::{Tool, ToolContext};
use async_trait::async_trait;
use serde_json::json;

use crate::session::{PtyOptions, PtyShell, run_pty_command};

/// 在 PTY 中执行 shell 命令。
pub struct RunPtyTool;

#[async_trait]
impl Tool for RunPtyTool {
    fn name(&self) -> &'static str {
        "run_pty_command"
    }
    fn description(&self) -> &'static str {
        "在伪终端（PTY）中执行 shell 命令，返回合并 stdout/stderr 与退出码。\
         适用于需要 TTY 的命令（top/vim/交互式 REPL 等）。属执行类操作，默认需审批。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "完整命令字符串" },
                "timeout_ms": { "type": "integer", "description": "可选超时（毫秒）" },
                "rows": { "type": "integer", "description": "可选终端行数（默认 24）" },
                "cols": { "type": "integer", "description": "可选终端列数（默认 80）" }
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
        let timeout_ms = input.get("timeout_ms").and_then(serde_json::Value::as_u64);
        let rows = input
            .get("rows")
            .and_then(serde_json::Value::as_u64)
            .map_or(24u16, |n| n as u16);
        let cols = input
            .get("cols")
            .and_then(serde_json::Value::as_u64)
            .map_or(80u16, |n| n as u16);

        let opts = PtyOptions {
            command: command.to_string(),
            cwd: Some(ctx.workspace.root()),
            env: HashMap::new(),
            timeout_ms,
            rows,
            cols,
        };

        let cancel = ctx.cancel;
        let run_fut = run_pty_command(&opts);
        tokio::pin!(run_fut);
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(ToolError::Execution("PTY 命令被取消".into()));
            }
            res = &mut run_fut => res.map_err(ToolError::Io)?,
        };

        let text = if result.timed_out {
            format!(
                "[timed out after {}ms]\n{}",
                opts.timeout_ms.unwrap_or(0),
                result.output
            )
        } else {
            let code = result.exit_code.unwrap_or(-1);
            if code == 0 {
                result.output
            } else {
                format!("[exit {code}]\n{}", result.output)
            }
        };
        Ok(ToolResult::text(text))
    }
}

/// `shell_session` 工具：**持久** PTY 会话（跨命令保持 cwd / 环境变量）。
///
/// 与 [`RunPtyTool`] 的分工：后者每条命令一个独立 PTY（无状态）；本工具把 [`PtyShell`]
/// 常驻，`cd` / `export` 等状态在后续 `run` 中继续生效——这是「先 cd 再构建」这类多步
/// 交互流程的正确执行方式，也是 omp `bash-executor.ts` 用 `PtyShell` 作持久执行后端的语义。
///
/// 生命周期：会话在首次 `run` 时按工作区根惰性启动；`close` 显式回收；**超时会杀掉并
/// 置失效**（fail-closed），下一次 `run` 自动重建（`status` 会显示 `poisoned`）。
#[derive(Default)]
pub struct ShellSessionTool {
    /// 常驻会话（`tokio::sync::Mutex`：跨 await 持锁，串行化同一会话上的命令）。
    session: tokio::sync::Mutex<Option<PtyShell>>,
}

impl ShellSessionTool {
    /// 新建（无会话，首次 `run` 惰性启动）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 确保会话可用（惰性启动；失效会话自动重建）。
    async fn ensure_session<'a>(
        guard: &'a mut Option<PtyShell>,
        cwd: &std::path::Path,
    ) -> Result<&'a PtyShell, ToolError> {
        let needs_restart = guard.as_ref().is_none_or(PtyShell::is_poisoned);
        if needs_restart {
            if let Some(old) = guard.take() {
                drop(old); // 显式回收失效 shell（Drop kill+wait）。
            }
            let shell = PtyShell::spawn(Some(cwd)).await.map_err(ToolError::Io)?;
            *guard = Some(shell);
        }
        Ok(guard.as_ref().expect("会话已确保存在"))
    }
}

#[async_trait]
impl Tool for ShellSessionTool {
    fn name(&self) -> &'static str {
        "shell_session"
    }
    fn description(&self) -> &'static str {
        "在**持久** PTY 会话中执行 shell 命令：`cd` / `export` 等状态跨命令保留（多条命令共享同一 shell）。\
         op=run 执行 / status 查看会话状态 / close 回收会话。适合「先切目录再构建」这类多步流程；\
         一次性无状态命令用 run_pty_command 或 run_command。属执行类操作，默认需审批。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "op": { "type": "string", "enum": ["run", "status", "close"], "default": "run",
                        "description": "run 执行命令 / status 查看会话（是否存活、是否失效）/ close 回收" },
                "command": { "type": "string", "description": "op=run：命令字符串（在持久 shell 中执行）" },
                "timeout_ms": { "type": "integer",
                                "description": "op=run 超时（毫秒，默认 60000）；超时会杀掉会话，下次 run 自动重建" }
            },
            "required": ["op"]
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
        let op = input
            .get("op")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("run");
        match op {
            "status" => {
                let guard = self.session.lock().await;
                let text = match guard.as_ref() {
                    None => "会话未启动（首次 op=run 时惰性创建）".to_string(),
                    Some(s) if s.is_poisoned() => {
                        "会话已失效（此前命令超时被杀）：下次 op=run 会自动重建".to_string()
                    }
                    Some(_) => "会话存活（cwd/环境变量跨命令保留）".to_string(),
                };
                Ok(ToolResult::text(text))
            }
            "close" => {
                let mut guard = self.session.lock().await;
                let had = guard.take().is_some();
                Ok(ToolResult::text(if had {
                    "会话已回收（下次 op=run 会重新启动）"
                } else {
                    "无活动会话"
                }))
            }
            "run" => {
                let command = input
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .filter(|c| !c.trim().is_empty())
                    .ok_or_else(|| ToolError::InvalidArgs("op=run 需要 `command` 参数".into()))?;
                let timeout = input
                    .get("timeout_ms")
                    .and_then(serde_json::Value::as_u64)
                    .filter(|ms| *ms > 0)
                    .map(std::time::Duration::from_millis);
                let cwd = ctx.workspace.root();
                let mut guard = self.session.lock().await;
                // 借用作用域：`shell` 的可变借用必须在 `guard.take()` 之前结束。
                let result = {
                    let shell = Self::ensure_session(&mut guard, &cwd).await?;
                    let run_fut = shell.run_with_timeout(command, timeout);
                    tokio::pin!(run_fut);
                    tokio::select! {
                        biased;
                        () = ctx.cancel.cancelled() => {
                            return Err(ToolError::Execution("PTY 会话命令被取消".into()));
                        }
                        res = &mut run_fut => res.map_err(ToolError::Io)?,
                    }
                };
                let text = if result.timed_out {
                    // 会话已被杀掉并置失效：显式回收，避免留下失效实例。
                    let _ = guard.take();
                    format!(
                        "[timed out after {}ms；会话已失效，下次 op=run 自动重建]\n{}",
                        timeout.map_or(60_000, |d| d.as_millis()),
                        result.output
                    )
                } else {
                    let code = result.exit_code.unwrap_or(-1);
                    if code == 0 {
                        result.output
                    } else {
                        format!("[exit {code}]\n{}", result.output)
                    }
                };
                Ok(ToolResult::text(text))
            }
            other => Err(ToolError::InvalidArgs(format!("未知 op：{other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ApprovalDecision, ApprovalPolicy, AskMessage, AskResponse};

    struct NoopApproval;
    #[async_trait]
    impl ApprovalPolicy for NoopApproval {
        fn decide(&self, _r: &ApprovalRequest<'_>) -> ApprovalDecision {
            ApprovalDecision::Allow
        }
        async fn prompt(&self, _a: &AskMessage) -> Result<AskResponse, ToolError> {
            Ok(AskResponse::Yes)
        }
    }

    fn ctx<'a>(
        ws: &'a agent_core::Workspace,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> ToolContext<'a> {
        ToolContext {
            workspace: ws,
            approval: &NoopApproval,
            cancel,
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
            update_tx: None,
            conflicts: None,
            pending_rewrites: None,
            context: None,
            snapshots: None,
            tool_call_id: None,
        }
    }

    /// 沙箱里 PTY 可能不可用：首个命令失败即跳过（与 session.rs 的一次性用例同策略）。
    async fn run(
        tool: &ShellSessionTool,
        ws: &agent_core::Workspace,
        cancel: &tokio_util::sync::CancellationToken,
        input: serde_json::Value,
    ) -> Option<String> {
        match tool.execute(input, &ctx(ws, cancel)).await {
            Ok(r) => Some(r.to_llm_text()),
            Err(e) if format!("{e}").to_lowercase().contains("pty") => {
                eprintln!("skipping: pty unavailable ({e})");
                None
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    /// H45：持久会话——`cd` 的效果跨命令保留（无状态执行会回到工作区根）。
    #[tokio::test]
    async fn shell_session_persists_cwd_across_runs() {
        let root = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let ws = agent_core::Workspace::new(root.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let tool = ShellSessionTool::new();
        let target_path = target.path().display().to_string();

        let Some(first) = run(
            &tool,
            &ws,
            &cancel,
            json!({"op": "run", "command": format!("cd {target_path} && pwd")}),
        )
        .await
        else {
            return;
        };
        assert!(first.contains(&target_path), "首次 cd 应生效: {first}");
        let second = run(&tool, &ws, &cancel, json!({"op": "run", "command": "pwd"}))
            .await
            .expect("会话应存活");
        assert!(
            second.contains(&target_path),
            "第二次 pwd 应仍在 {target_path}（持久会话）: {second}"
        );
        assert!(
            !second.contains(&root.path().display().to_string()),
            "不应回到工作区根: {second}"
        );
    }

    /// H45：status/close 生命周期 + 非零退出码透出。
    #[tokio::test]
    async fn shell_session_status_close_and_exit_code() {
        let root = tempfile::tempdir().unwrap();
        let ws = agent_core::Workspace::new(root.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let tool = ShellSessionTool::new();

        let status = run(&tool, &ws, &cancel, json!({"op": "status"}))
            .await
            .expect("status 不应依赖 PTY");
        assert!(status.contains("未启动"), "{status}");

        let Some(out) = run(
            &tool,
            &ws,
            &cancel,
            json!({"op": "run", "command": "exit 3"}),
        )
        .await
        else {
            return;
        };
        // `exit 3` 会结束持久 shell：子进程真实退出码必须被透出（而非 -1），
        // 且会话随之失效。
        assert!(out.contains("[exit 3]"), "应透出真实退出码 3: {out}");

        let status = run(&tool, &ws, &cancel, json!({"op": "status"}))
            .await
            .expect("status");
        assert!(status.contains("失效"), "shell 退出后应显示失效: {status}");
        let closed = run(&tool, &ws, &cancel, json!({"op": "close"}))
            .await
            .expect("close");
        assert!(
            closed.contains("已回收") || closed.contains("无活动会话"),
            "{closed}"
        );
        let status = run(&tool, &ws, &cancel, json!({"op": "status"}))
            .await
            .expect("status");
        assert!(status.contains("未启动"), "{status}");
        // 未知 op / 缺 command 参数。
        assert!(
            tool.execute(json!({"op": "bogus"}), &ctx(&ws, &cancel))
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"op": "run"}), &ctx(&ws, &cancel))
                .await
                .is_err()
        );
    }

    /// H45：超时杀掉会话并标记失效，**下一次 run 自动重建**（自愈）。
    #[tokio::test]
    async fn shell_session_timeout_poisons_then_self_heals() {
        let root = tempfile::tempdir().unwrap();
        let ws = agent_core::Workspace::new(root.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let tool = ShellSessionTool::new();
        let Some(out) = run(
            &tool,
            &ws,
            &cancel,
            json!({"op": "run", "command": "sleep 5", "timeout_ms": 200}),
        )
        .await
        else {
            return;
        };
        assert!(out.contains("timed out"), "应报告超时: {out}");
        // 自愈：下一次 run 重建会话并正常执行。
        let healed = run(
            &tool,
            &ws,
            &cancel,
            json!({"op": "run", "command": "echo healed"}),
        )
        .await
        .expect("应自动重建会话");
        assert!(healed.contains("healed"), "{healed}");
    }
}
