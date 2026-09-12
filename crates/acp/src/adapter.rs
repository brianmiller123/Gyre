//! 事件适配器：将 [`agent_server::ServerFrame`] 转换为标准 ACP [`SessionUpdate`]。
//!
//! ACP 与 Web 前端共享同一个 `broadcast::Sender<ServerFrame>`，本模块是其子集投影——
//! 过滤 ACP 不关心的变体，并把语义映射到标准 `session/update` 的 update 类型。
//! 工具帧按「三层生命周期」granular 映射：`ToolExecutionStart/Update/End` 携带真实
//! `tool_call_id`，一一转为 ACP `tool_call` / `tool_call_update`；折叠事件 `ToolExec`
//! 无真实 id，ACP 面忽略（仅供 REPL / server 面板），避免客户端出现重复卡片。
//!
//! 终止帧（`Done` / `Error`）不映射为 update，而由 prompt 处理器消费以决定 `stopReason`。

use agent_core::ToolResult;
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
        // 折叠事件（无真实 tool_call_id）：granular 的 Start/Update/End 已按真实 id 产出
        // tool_call / tool_call_update，此处再映射会让客户端对同一次调用渲染两张卡片；
        // 折叠事件仅供 REPL / server 面板消费，ACP 面忽略。
        ServerFrame::ToolExec { .. } => return None,
        // 工具执行开始：以真实 tool_call_id 建卡（并行同名调用各自成卡，id 贯通全生命周期）。
        ServerFrame::ToolExecutionStart {
            tool_call_id,
            name,
            args,
        } => SessionUpdate::ToolCall {
            tool_call_id,
            title: tool_title(&name, &args),
            kind: tool_kind(&name).into(),
            status: "pending".into(),
            raw_output: None,
        },
        // 流式 partial：卡片转 in_progress 并滚动输出预览。
        ServerFrame::ToolExecutionUpdate {
            tool_call_id,
            partial,
            ..
        } => SessionUpdate::ToolCallUpdate {
            tool_call_id,
            status: Some("in_progress".into()),
            raw_output: Some(truncate_chars(&partial, RAW_OUTPUT_MAX_CHARS)),
        },
        // 执行结束：按 is_error 定格 completed / failed，raw_output 为截断后的结果预览。
        ServerFrame::ToolExecutionEnd {
            tool_call_id,
            result,
            is_error,
            ..
        } => SessionUpdate::ToolCallUpdate {
            tool_call_id,
            status: Some(if is_error { "failed" } else { "completed" }.into()),
            raw_output: Some(truncate_chars(&result_text(&result), RAW_OUTPUT_MAX_CHARS)),
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
        | ServerFrame::Heartbeat
        // 三层生命周期帧中，turn / message 帧暂无对应 ACP update 类型，与 Heartbeat 同策略
        // 跳过；tool_execution 三帧已改为 granular 映射（见上），不再进入本跳过清单。
        | ServerFrame::TurnStart
        | ServerFrame::TurnEnd { .. }
        | ServerFrame::MessageStart
        | ServerFrame::MessageEnd { .. }
        | ServerFrame::Steered { .. }
        // 结构化会话事件：展示文本已随配对的 Say 帧并入 agent_message_chunk，
        // 状态机字段 ACP 无对应 update 类型（与状态帧同策略）。
        | ServerFrame::Session { .. } => return None,
    })
}

/// 判断帧是否为终止帧（prompt turn 结束）。
#[must_use]
pub fn is_terminal_frame(frame: &ServerFrame) -> bool {
    matches!(frame, ServerFrame::Done { .. } | ServerFrame::Error { .. })
}

/// `rawOutput` 预览截断上限（与 `AgentEvent::ToolExec` 折叠事件的 200 字符预览策略同宽）。
const RAW_OUTPUT_MAX_CHARS: usize = 200;

/// 标题截断上限：超长命令 / 路径不至于撑爆客户端卡片。
const TITLE_MAX_CHARS: usize = 120;

/// 按工具名推断 ACP `ToolKind`（移植 oh-my-pi `mapToolKind`）。
///
/// kind 决定客户端图标与自动审批层级；未识别工具一律 `other`。
pub(crate) fn tool_kind(name: &str) -> &'static str {
    match name {
        "read_file" | "read_image" => "read",
        "write_file" | "apply_hashline" | "replace_block" | "ast_rewrite" | "lsp_apply" => "edit",
        "run_command" | "ssh" => "execute",
        "grep" | "glob" | "ast_search" | "list_files" => "search",
        // ACP `ToolKind` 还含 fetch / think：联网检索归 fetch，清单规划归 think（同 omp）。
        "web_search" => "fetch",
        "todo" => "think",
        // omp 同名工具保留归类（Gyre 当前未注册，出现时按名匹配）。
        "delete" => "delete",
        "move" => "move",
        _ => "other",
    }
}

/// 从工具参数提取人类可读标题（移植 oh-my-pi `buildToolTitle`）。
///
/// 优先级：命令文本（`$ cmd`）最高；其次关键参数主题（path/pattern/query/prompt →
/// `工具名: 主题`）；最后回退工具名。主题为内部 URI（`skill://…` 等协议限定目标）时
/// 单独展示——拼工具名前缀会被编辑器当成对不存在路径的文件操作。
pub(crate) fn tool_title(name: &str, args: &serde_json::Value) -> String {
    let field = |key: &str| {
        args.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    if name == "run_command" {
        if let Some(command) = field("command") {
            return truncate_chars(&format!("$ {command}"), TITLE_MAX_CHARS);
        }
    }
    if let Some(subject) = field("path")
        .or_else(|| field("pattern"))
        .or_else(|| field("query"))
        .or_else(|| field("prompt"))
    {
        if is_internal_uri(subject) {
            return truncate_chars(subject, TITLE_MAX_CHARS);
        }
        return truncate_chars(&format!("{name}: {subject}"), TITLE_MAX_CHARS);
    }
    name.to_string()
}

/// 判断主题是否为协议限定目标（`xd://` / `skill://` / `https://` …）。移植 oh-my-pi
/// `INTERNAL_URL_SUBJECT`：此类目标并非本地路径，前缀拼接会误导客户端聚焦不存在的文件。
fn is_internal_uri(subject: &str) -> bool {
    let Some((scheme, _)) = subject.split_once("://") else {
        return false;
    };
    let mut chars = scheme.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// 把结构化工具结果压平为展示文本：Text 原样、Image 只留占位（不拷贝二进制）、Error 取 message。
fn result_text(result: &ToolResult) -> String {
    match result {
        ToolResult::Text(text) => text.clone(),
        ToolResult::Image { mime, .. } => format!("[image/{mime}]"),
        ToolResult::Error { message, .. } => message.clone(),
    }
}

/// 按字符数截断（UTF-8 安全），超长补省略与总长说明（同 advisor 侧约定）。
fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str(&format!("…(共 {count} 字符)"));
    out
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
        match server_frame_to_acp(ServerFrame::ThinkingDelta {
            delta: "hmm".into(),
        }) {
            Some(SessionUpdate::AgentThoughtChunk { content }) => {
                assert_eq!(content_text(&content), "hmm");
            }
            other => panic!("应为 AgentThoughtChunk: {other:?}"),
        }
    }

    #[test]
    fn maps_say_as_message_chunk() {
        match server_frame_to_acp(ServerFrame::Say {
            text: "info".into(),
        }) {
            Some(SessionUpdate::AgentMessageChunk { content }) => {
                assert_eq!(content_text(&content), "info");
            }
            other => panic!("Say 应映射为 AgentMessageChunk: {other:?}"),
        }
    }

    #[test]
    fn tool_exec_collapsed_event_returns_none() {
        // 折叠事件无真实 tool_call_id：granular 三帧已按真实 id 建卡 / 更新，此处再映射
        // 会让 Zed 等客户端对同一次调用渲染「一真一假」两张卡片，故 ACP 面忽略。
        assert!(
            server_frame_to_acp(ServerFrame::ToolExec {
                name: "grep".into(),
                output: "3 hits".into(),
            })
            .is_none()
        );
    }

    #[test]
    fn maps_tool_execution_lifecycle_with_threaded_id() {
        // Start：真实 id 建卡（pending + 参数摘要标题 + kind 推断）。
        match server_frame_to_acp(ServerFrame::ToolExecutionStart {
            tool_call_id: "call-1".into(),
            name: "read_file".into(),
            args: serde_json::json!({ "path": "src/lib.rs" }),
        }) {
            Some(SessionUpdate::ToolCall {
                tool_call_id,
                title,
                kind,
                status,
                raw_output,
            }) => {
                assert_eq!(tool_call_id, "call-1");
                assert_eq!(title, "read_file: src/lib.rs");
                assert_eq!(kind, "read");
                assert_eq!(status, "pending");
                assert_eq!(raw_output, None);
            }
            other => panic!("Start 应映射为 ToolCall: {other:?}"),
        }

        // Update：同 id 转 in_progress，携带 partial 预览。
        match server_frame_to_acp(ServerFrame::ToolExecutionUpdate {
            tool_call_id: "call-1".into(),
            name: "read_file".into(),
            partial: "读取中".into(),
        }) {
            Some(SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status,
                raw_output,
            }) => {
                assert_eq!(tool_call_id, "call-1");
                assert_eq!(status.as_deref(), Some("in_progress"));
                assert_eq!(raw_output.as_deref(), Some("读取中"));
            }
            other => panic!("Update 应映射为 ToolCallUpdate: {other:?}"),
        }

        // End：同 id 定格 completed，raw_output 为结果预览。
        match server_frame_to_acp(ServerFrame::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            name: "read_file".into(),
            result: ToolResult::Text("fn main() {}".into()),
            is_error: false,
        }) {
            Some(SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status,
                raw_output,
            }) => {
                assert_eq!(tool_call_id, "call-1");
                assert_eq!(status.as_deref(), Some("completed"));
                assert_eq!(raw_output.as_deref(), Some("fn main() {}"));
            }
            other => panic!("End 应映射为 ToolCallUpdate: {other:?}"),
        }
    }

    #[test]
    fn parallel_same_name_tool_calls_keep_distinct_ids() {
        // 并行两个同名工具调用：Start 各自成卡且 id 互不串扰，End 乱序返回仍各归其卡。
        let start = |id: &str| {
            server_frame_to_acp(ServerFrame::ToolExecutionStart {
                tool_call_id: id.into(),
                name: "run_command".into(),
                args: serde_json::json!({ "command": "sleep 1" }),
            })
        };
        let end = |id: &str| {
            server_frame_to_acp(ServerFrame::ToolExecutionEnd {
                tool_call_id: id.into(),
                name: "run_command".into(),
                result: ToolResult::Text("done".into()),
                is_error: false,
            })
        };
        let starts = [start("call-a"), start("call-b")];
        let ids: Vec<&str> = starts.iter().map(update_tool_call_id).collect();
        assert_eq!(ids, ["call-a", "call-b"]);

        let ends = [end("call-b"), end("call-a")];
        let end_ids: Vec<&str> = ends.iter().map(update_tool_call_id).collect();
        assert_eq!(end_ids, ["call-b", "call-a"]);
    }

    #[test]
    fn tool_call_updates_serialize_acp_camel_case() {
        // 线格式保持 ACP 兼容：sessionUpdate 鉴别器 + toolCallId / rawOutput camelCase。
        let start = server_frame_to_acp(ServerFrame::ToolExecutionStart {
            tool_call_id: "call-1".into(),
            name: "grep".into(),
            args: serde_json::json!({ "pattern": "todo" }),
        })
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&start).unwrap()).unwrap();
        assert_eq!(v["sessionUpdate"], "tool_call");
        assert_eq!(v["toolCallId"], "call-1");
        assert_eq!(v["status"], "pending");
        // Option::is_none 的 rawOutput 整体省略（不出 null 键）。
        assert!(v.get("rawOutput").is_none());

        let end = server_frame_to_acp(ServerFrame::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            name: "grep".into(),
            result: ToolResult::Text("3 hits".into()),
            is_error: false,
        })
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&end).unwrap()).unwrap();
        assert_eq!(v["sessionUpdate"], "tool_call_update");
        assert_eq!(v["toolCallId"], "call-1");
        assert_eq!(v["status"], "completed");
        assert_eq!(v["rawOutput"], "3 hits");
    }

    #[test]
    fn tool_execution_end_error_maps_failed() {
        match server_frame_to_acp(ServerFrame::ToolExecutionEnd {
            tool_call_id: "call-err".into(),
            name: "write_file".into(),
            result: ToolResult::Error {
                recoverable: true,
                message: "权限不足".into(),
            },
            is_error: true,
        }) {
            Some(SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status,
                raw_output,
            }) => {
                assert_eq!(tool_call_id, "call-err");
                assert_eq!(status.as_deref(), Some("failed"));
                assert_eq!(raw_output.as_deref(), Some("权限不足"));
            }
            other => panic!("End(is_error) 应映射为 failed: {other:?}"),
        }
    }

    #[test]
    fn tool_end_raw_output_truncated() {
        // rawOutput 预览与 ToolExec 折叠事件同宽（200 字符），超长补省略与总长说明。
        match server_frame_to_acp(ServerFrame::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            name: "read_file".into(),
            result: ToolResult::Text("x".repeat(300)),
            is_error: false,
        }) {
            Some(SessionUpdate::ToolCallUpdate { raw_output, .. }) => {
                let out = raw_output.expect("应有 rawOutput");
                assert!(out.starts_with(&"x".repeat(200)));
                assert!(out.ends_with("…(共 300 字符)"));
            }
            other => panic!("End 应映射为 ToolCallUpdate: {other:?}"),
        }
    }

    #[test]
    fn tool_kind_inferred_from_name() {
        let cases = [
            ("read_file", "read"),
            ("read_image", "read"),
            ("write_file", "edit"),
            ("apply_hashline", "edit"),
            ("ast_rewrite", "edit"),
            ("run_command", "execute"),
            ("ssh", "execute"),
            ("grep", "search"),
            ("glob", "search"),
            ("ast_search", "search"),
            ("web_search", "fetch"),
            ("todo", "think"),
            ("delete", "delete"),
            ("move", "move"),
            ("lsp", "other"),
            ("totally_unknown", "other"),
        ];
        for (name, want) in cases {
            assert_eq!(tool_kind(name), want, "工具 {name} 的 kind 应为 {want}");
        }
    }

    #[test]
    fn tool_title_extracts_subject_from_args() {
        // 命令类：优先展示命令文本。
        assert_eq!(
            tool_title("run_command", &serde_json::json!({ "command": "ls -la" })),
            "$ ls -la"
        );
        // 关键参数主题：`工具名: 主题`。
        assert_eq!(
            tool_title("read_file", &serde_json::json!({ "path": "src/main.rs" })),
            "read_file: src/main.rs"
        );
        // 内部 URI 主题单独展示（不加工具名前缀）。
        assert_eq!(
            tool_title("read_file", &serde_json::json!({ "path": "skill://react" })),
            "skill://react"
        );
        // 无可提取参数时回退工具名。
        assert_eq!(
            tool_title("checkpoint", &serde_json::json!({})),
            "checkpoint"
        );
    }

    #[test]
    fn maps_context_usage() {
        match server_frame_to_acp(ServerFrame::ContextUsage {
            current: 10,
            limit: 100,
        }) {
            Some(SessionUpdate::UsageUpdate { used, size }) => assert_eq!((used, size), (10, 100)),
            other => panic!("应映射为 UsageUpdate: {other:?}"),
        }
    }

    #[test]
    fn terminal_frames_return_none() {
        assert!(
            server_frame_to_acp(ServerFrame::Done {
                turns: 1,
                tool_calls: 0,
                success: true
            })
            .is_none()
        );
        assert!(
            server_frame_to_acp(ServerFrame::Error {
                message: "boom".into()
            })
            .is_none()
        );
    }

    #[test]
    fn filters_internal_frames() {
        assert!(
            server_frame_to_acp(ServerFrame::StateChanged {
                state: AgentState::Running
            })
            .is_none()
        );
        assert!(server_frame_to_acp(ServerFrame::Usage(Usage::default())).is_none());
        assert!(server_frame_to_acp(ServerFrame::SubAgents { agents: vec![] }).is_none());
    }

    /// 从序列化的 TextContent 中提取 text 字段值（测试辅助）。
    fn content_text(content: &crate::types::TextContent) -> String {
        let json = serde_json::to_string(content).unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
        v.get("text")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string()
    }

    /// 提取工具卡片事件携带的 toolCallId（测试辅助）。
    fn update_tool_call_id(update: &Option<SessionUpdate>) -> &str {
        match update {
            Some(
                SessionUpdate::ToolCall { tool_call_id, .. }
                | SessionUpdate::ToolCallUpdate { tool_call_id, .. },
            ) => tool_call_id,
            other => panic!("应为工具卡片事件: {other:?}"),
        }
    }
}
