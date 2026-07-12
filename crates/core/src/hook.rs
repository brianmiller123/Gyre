//! Hook 端口：agent 执行事件钩子（before/after tool、stop）。
//!
//! Hook 仅观察事件（不阻止执行），用于日志、通知、指标、审计等副作用。
//! 装配层（cli/server）注入具体实现（如写入审计日志、推送 webhook）。

use crate::ToolResult;

/// Hook 事件。
#[derive(Debug, Clone)]
pub enum HookEvent {
    /// 工具执行前。
    BeforeTool {
        /// 工具名。
        tool: String,
        /// 工具参数。
        args: serde_json::Value,
    },
    /// 工具执行后（含成功与错误结果）。
    AfterTool {
        /// 工具名。
        tool: String,
        /// 工具结果。
        result: ToolResult,
    },
    /// 任务结束（成功/失败/取消）。
    Stop {
        /// 是否成功完成。
        success: bool,
    },
}

/// Hook 端口：观察 agent 执行事件。
#[async_trait::async_trait]
pub trait Hook: Send + Sync {
    /// 事件回调（不阻止执行，仅副作用）。
    async fn on_event(&self, event: &HookEvent);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_event_clone_and_match() {
        let e = HookEvent::BeforeTool {
            tool: "list_files".into(),
            args: serde_json::json!({"path":"."}),
        };
        match e.clone() {
            HookEvent::BeforeTool { tool, args } => {
                assert_eq!(tool, "list_files");
                assert_eq!(args["path"], ".");
            }
            _ => panic!("应为 BeforeTool"),
        }
    }

    #[test]
    fn stop_event_carries_success() {
        let e = HookEvent::Stop { success: false };
        match e {
            HookEvent::Stop { success } => assert!(!success),
            _ => panic!("应为 Stop"),
        }
    }
}
