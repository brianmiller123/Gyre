//! `run_pty_command` 工具：在 PTY 中执行命令（TTY 依赖命令可运行）。
//!
//! 与 `run_command`（管道 stdio）互补：本工具走伪终端，使 `top -b`、`vim`、
//! 交互式 REPL 等**要求 TTY** 的命令能正常执行并捕获输出。

use std::collections::HashMap;

use agent_core::{ApprovalRequest, CapabilityTier, ToolError, ToolResult};
use agent_tools::{Tool, ToolContext};
use async_trait::async_trait;
use serde_json::json;

use crate::session::{run_pty_command, PtyOptions};

/// 在 PTY 中执行 shell 命令。
pub struct RunPtyTool;

#[async_trait]
impl Tool for RunPtyTool {
    fn name(&self) -> &str {
        "run_pty_command"
    }
    fn description(&self) -> &str {
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

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let command = input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `command` 参数".into()))?;
        let timeout_ms = input
            .get("timeout_ms")
            .and_then(serde_json::Value::as_u64);
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
            cwd: Some(ctx.workspace.root().to_path_buf()),
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
            _ = cancel.cancelled() => {
                return Err(ToolError::Execution("PTY 命令被取消".into()));
            }
            res = &mut run_fut => res.map_err(ToolError::Io)?,
        };

        let text = if result.timed_out {
            format!("[timed out after {}ms]\n{}", opts.timeout_ms.unwrap_or(0), result.output)
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
