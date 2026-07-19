//! 事件适配器：将 [`agent_server::ServerFrame`] 转换为标准 ACP [`SessionUpdate`]。
//!
//! ACP 与 Web 前端共享同一个 `broadcast::Sender<ServerFrame>`，本模块是其子集投影——
//! 过滤 ACP 不关心的变体，并把语义映射到标准 `session/update` 的 update 类型。
//!
//! 终止帧（`Done` / `Error`）不映射为 update，而由 prompt 处理器消费以决定 `stopReason`。

use agent_server::ServerFrame;

use crate::types::SessionUpdate;

/// 将 [`ServerFrame`] 转换为 [`SessionUpdate`]。
///
/// 返回 `None` 表示该帧无对应 update（终止帧 / 内部帧 / 状态帧 / 审批帧）。
#[must_use]
pub fn server_frame_to_acp(frame: ServerFrame) -> Option<SessionUpdate> {
    Some(match frame {
        ServerFrame::TextDelta { delta } => SessionUpdate::AgentMessageChunk {
            content: crate::types::TextContent::new(delta),
        },
        ServerFrame::ThinkingDelta { delta } => SessionUpdate::AgentThoughtChunk {
            content: crate::types::TextContent::new(delta),
        },
        // Say 是信息性文本，ACP 无独立事件，并入 agent_message_chunk。
        ServerFrame::Say { text } => SessionUpdate::AgentMessageChunk {
            content: crate::types::TextContent::new(text),
        },
        ServerFrame::ToolExec { name, output } => SessionUpdate::ToolCall {
            tool_call_id: name.clone(),
            title: name,
            kind: "other".into(),
            status: "completed".into(),
            raw_output: Some(output),
        },
        ServerFrame::ContextUsage { current, limit } => SessionUpdate::UsageUpdate {
            used: current as u64,
            size: limit as u64,
        },
        // 终止帧由 prompt 处理器消费，不映射为 update。
        ServerFrame::Done { .. } | ServerFrame::Error { .. } => return None,
        // 状态变更 / 审批 / 用量统计（增量与快照）/ 子 Agent 快照 / 应用层心跳：ACP 无独立 update 类型，跳过。
        ServerFrame::StateChanged { .. }
        | ServerFrame::Ask { .. }
        | ServerFrame::Usage(_)
        | ServerFrame::UsageSnapshot(_)
        | ServerFrame::SubAgents { .. }
        | ServerFrame::Heartbeat => return None,
    })
}

/// 判断帧是否为终止帧（prompt turn 结束）。
#[must_use]
pub fn is_terminal_frame(frame: &ServerFrame) -> bool {
    matches!(
        frame,
        ServerFrame::Done { .. } | ServerFrame::Error { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AgentState, Usage};
    use agent_server::ServerFrame;

    #[test]
    fn maps_text_delta() {
        match server_frame_to_acp(ServerFrame::TextDelta { delta: "hi".into() }) {
            Some(SessionUpdate::AgentMessageChunk { content }) => {
                assert_eq!(content_text(&content), "hi");
            }
            other => panic!("应为 AgentMessageChunk: {other:?}"),
        }
    }

    #[test]
    fn maps_thinking_delta() {
        match server_frame_to_acp(ServerFrame::ThinkingDelta { delta: "hmm".into() }) {
            Some(SessionUpdate::AgentThoughtChunk { content }) => {
                assert_eq!(content_text(&content), "hmm");
            }
            other => panic!("应为 AgentThoughtChunk: {other:?}"),
        }
    }

    #[test]
    fn maps_say_as_message_chunk() {
        match server_frame_to_acp(ServerFrame::Say { text: "info".into() }) {
            Some(SessionUpdate::AgentMessageChunk { content }) => {
                assert_eq!(content_text(&content), "info");
            }
            other => panic!("Say 应映射为 AgentMessageChunk: {other:?}"),
        }
    }

    #[test]
    fn maps_tool_exec() {
        match server_frame_to_acp(ServerFrame::ToolExec { name: "grep".into(), output: "3 hits".into() }) {
            Some(SessionUpdate::ToolCall { tool_call_id, title, status, raw_output, .. }) => {
                assert_eq!(tool_call_id, "grep");
                assert_eq!(title, "grep");
                assert_eq!(status, "completed");
                assert_eq!(raw_output.as_deref(), Some("3 hits"));
            }
            other => panic!("应映射为 ToolCall: {other:?}"),
        }
    }

    #[test]
    fn maps_context_usage() {
        match server_frame_to_acp(ServerFrame::ContextUsage { current: 10, limit: 100 }) {
            Some(SessionUpdate::UsageUpdate { used, size }) => assert_eq!((used, size), (10, 100)),
            other => panic!("应映射为 UsageUpdate: {other:?}"),
        }
    }

    #[test]
    fn terminal_frames_return_none() {
        assert!(server_frame_to_acp(ServerFrame::Done { turns: 1, tool_calls: 0, success: true }).is_none());
        assert!(server_frame_to_acp(ServerFrame::Error { message: "boom".into() }).is_none());
    }

    #[test]
    fn filters_internal_frames() {
        assert!(server_frame_to_acp(ServerFrame::StateChanged { state: AgentState::Running }).is_none());
        assert!(server_frame_to_acp(ServerFrame::Usage(Usage::default())).is_none());
        assert!(server_frame_to_acp(ServerFrame::SubAgents { agents: vec![] }).is_none());
    }

    /// 从序列化的 TextContent 中提取 text 字段值（测试辅助）。
    fn content_text(content: &crate::types::TextContent) -> String {
        let json = serde_json::to_string(content).unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
        v.get("text").and_then(|t| t.as_str()).unwrap_or_default().to_string()
    }
}
