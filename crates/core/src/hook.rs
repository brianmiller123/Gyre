//! Hook 端口：agent 执行事件钩子（before/after tool、stop）。
//!
//! Hook 仅观察事件（不阻止执行），用于日志、通知、指标、审计等副作用。
//! 装配层（cli/server）注入具体实现（如写入审计日志、推送 webhook）。

use crate::ToolResult;
use crate::message::{AssistantMessage, ToolResultMessage};

/// Gyre 当前**实际会发射**的 hook 事件名全集（H20）。
///
/// 配置面（`[[hooks]] event = "…"`）只接受这些名字——刻意**不**预先声明「有空壳但永不触发」
/// 的事件（omp 的 26 事件里，依赖扩展宿主 / 会话树 / plan 模式的那些在 Gyre 尚无运行点）。
/// 已覆盖：会话与智能体生命周期、轮次、工具、自动压缩、自动重试与回退、TTSR、todo 提醒。
pub const HOOK_EVENT_NAMES: &[&str] = &[
    // 生命周期
    "session_start",
    "session_shutdown",
    "before_agent_start",
    "agent_start",
    "agent_end",
    "turn_start",
    "turn_end",
    "stop",
    // 工具
    "tool_call",
    "tool_result",
    // 会话级结构化事件（与 [`crate::SessionEvent`] 同名镜像）
    "auto_compaction_start",
    "auto_compaction_end",
    "auto_retry_start",
    "auto_retry_end",
    "retry_fallback_applied",
    "retry_fallback_succeeded",
    "ttsr_triggered",
];

/// Hook 事件。
#[derive(Debug, Clone)]
pub enum HookEvent {
    /// 工具执行前（`tool_call`）。
    BeforeTool {
        /// 工具名。
        tool: String,
        /// 工具参数。
        args: serde_json::Value,
    },
    /// 工具执行后（含成功与错误结果；`tool_result`）。
    AfterTool {
        /// 工具名。
        tool: String,
        /// 工具结果。
        result: ToolResult,
    },
    /// 任务结束（成功/失败/取消；`stop`）。
    Stop {
        /// 是否成功完成。
        success: bool,
    },
    /// 通用命名事件（H20）：`event` 为线协议名（见 [`HOOK_EVENT_NAMES`]），
    /// `payload` 为结构化负载（已含 `event` 字段，直接作为 hook 命令的 stdin JSON）。
    ///
    /// 用单一变体承载扩展事件面，避免为每个生命周期点新增 enum 变体 + 每个 Hook 实现
    /// 都要跟着改 match（既有实现只需处理自己订阅的名字）。
    Named {
        /// 事件名（如 `turn_start` / `auto_retry_start`）。
        event: String,
        /// 事件负载（结构化 JSON）。
        payload: serde_json::Value,
    },
}

impl HookEvent {
    /// 事件名（写进 hook 命令 stdin 的 `event` 字段，亦用于 `[[hooks]] event=` 匹配）。
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::BeforeTool { .. } => "tool_call",
            Self::AfterTool { .. } => "tool_result",
            Self::Stop { .. } => "stop",
            Self::Named { event, .. } => event.as_str(),
        }
    }

    /// 构造命名事件（负载自动补 `event` 字段，保证 hook 脚本读到的形状一致）。
    #[must_use]
    pub fn named(event: impl Into<String>, mut payload: serde_json::Value) -> Self {
        let event = event.into();
        if let Some(obj) = payload.as_object_mut() {
            obj.insert(
                "event".to_string(),
                serde_json::Value::String(event.clone()),
            );
        } else {
            payload = serde_json::json!({ "event": event, "data": payload });
        }
        Self::Named { event, payload }
    }

    /// 线协议负载（hook 命令 stdin 的 JSON）。
    #[must_use]
    pub fn to_payload(&self) -> serde_json::Value {
        match self {
            Self::BeforeTool { tool, args } => {
                serde_json::json!({ "event": "tool_call", "tool": tool, "args": args })
            }
            Self::AfterTool { tool, result } => {
                serde_json::json!({ "event": "tool_result", "tool": tool, "result": result })
            }
            Self::Stop { success } => serde_json::json!({ "event": "stop", "success": success }),
            Self::Named { payload, .. } => payload.clone(),
        }
    }
}

/// turn 结束钩子上下文：每轮模型响应 + 工具处理完毕后的快照（移植 oh-my-pi `onTurnEnd`）。
///
/// 与 [`crate::AgentEvent::TurnEnd`] 事件配对，但面向**不经事件流的程序化 hook**
///（审计、指标、memory 更新、telemetry span 等）。事件消费者（如 server 的 `to_server_frame`）
/// 已能从 `TurnEnd` 事件观测；本钩子供 agent 内部 / 装配层注入的程序化副作用使用。
#[derive(Debug)]
pub struct TurnEndContext<'a> {
    /// 本轮最终化的 assistant 消息。
    pub message: &'a AssistantMessage,
    /// 本轮工具结果（含实际执行与占位 skipped）。
    pub tool_results: &'a [ToolResultMessage],
    /// 是否将继续下一轮（有工具调用且未触达 deadline / cancel / `max_turns`）。
    pub will_continue: bool,
}

/// Hook 端口：观察 agent 执行事件，并可选地拦截/改写工具执行（P2-I）。
#[async_trait::async_trait]
pub trait Hook: Send + Sync {
    /// 事件回调（不阻止执行，仅副作用）。
    async fn on_event(&self, event: &HookEvent);

    /// 每轮结束（assistant 消息 + 工具处理完毕，即将 continue 或 stop）。移植 oh-my-pi
    /// `onTurnEnd`。默认空实现（既有 Hook 实现无需改动）。
    async fn on_turn_end(&self, _ctx: &TurnEndContext<'_>) {}

    /// 工具执行前拦截（P2-I）：返回 `Some(reason)` 则阻止执行——回填可恢复错误给模型，
    /// **不调用** `Tool::execute`。默认 `None`（不阻止，与既有观察语义兼容，现有实现无需改动）。
    /// 用于扩展 / MCP 程序化门禁危险工具（区别于交互式审批 `ApprovalPolicy`）。
    async fn before_tool_intercept(
        &self,
        _tool: &str,
        _args: &serde_json::Value,
    ) -> Option<String> {
        None
    }

    /// 工具执行后改写（P2-I）：接收真实 result，返回 `Some(new)` 替换；`None` 保留原结果。
    /// 默认 `None`（不改写）。在 `AfterTool` 观察事件**之前**应用，故观察事件看到的是最终结果。
    /// 用于结果脱敏、归一化、附加纠正提示等。
    async fn after_tool_override(&self, _tool: &str, _result: &ToolResult) -> Option<ToolResult> {
        None
    }
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
        match e {
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
