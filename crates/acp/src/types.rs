//! ACP 线协议类型：JSON-RPC 2.0 消息 + 标准 ACP session/update 事件。
//!
//! 遵循 Agent Client Protocol v1（agentclientprotocol.com）规范。
//! 事件枚举是内部 `ServerFrame` 的子集投影，映射到标准 `SessionUpdate`。

use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 id（数字或字符串；通知时为 None / null）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum JsonRpcId {
    /// 数字 id。
    Num(i64),
    /// 字符串 id。
    Str(String),
}

/// JSON-RPC 2.0 请求 / 通知。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// 固定 `"2.0"`。
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    /// 请求 id；`None` 表示通知（无响应）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonRpcId>,
    /// 方法名（`initialize` / `session/new` / `session/prompt` / `session/cancel` …）。
    pub method: String,
    /// 参数（任意 JSON 对象）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 成功响应。
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    /// 固定 `"2.0"`。
    pub jsonrpc: String,
    /// 对应请求 id。
    pub id: Option<JsonRpcId>,
    /// 结果。
    pub result: serde_json::Value,
}

/// JSON-RPC 2.0 错误响应。
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    /// 固定 `"2.0"`。
    pub jsonrpc: String,
    /// 对应请求 id。
    pub id: Option<JsonRpcId>,
    /// 错误对象。
    pub error: RpcError,
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    /// 错误码（JSON-RPC 标准码或实现自定义）。
    pub code: i32,
    /// 错误消息。
    pub message: String,
    /// 附加数据（可选）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// ACP `session/available_commands` 响应中的命令条目。
///
/// 清单由装配层经 [`agent_server::SessionManager::set_available_commands`] 注入
/// （共享可写存储，未注入时为空列表），分发层读取后投影为本类型序列化。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandInfo {
    /// 命令名（不含前导斜杠，如 `review`）。
    pub name: String,
    /// 一行描述（客户端命令菜单的提示文案）。
    pub description: String,
}

// ──────────────────────────────────────────────────────────────────────────────
// 标准 ACP session/update 事件类型
// ──────────────────────────────────────────────────────────────────────────────

/// 简化的文本内容块（`ContentBlock::Text`）。
///
/// 完整 ACP ContentBlock 支持 text/image/audio/resource 等变体，
/// 此处仅实现 text（覆盖 agent 输出的绝大多数场景）。
#[derive(Debug, Clone, Serialize)]
pub struct TextContent {
    /// 固定 `"text"`。
    #[serde(rename = "type")]
    kind: &'static str,
    /// 文本内容。
    text: String,
}

impl TextContent {
    /// 构造文本内容块。
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            kind: "text",
            text: text.into(),
        }
    }
}

/// 标准 ACP `SessionUpdate`——`session/update` 通知的 `update` 载荷。
///
/// 按 `sessionUpdate` 字段做鉴别（oneOf discriminator）。
/// 参见 <https://agentclientprotocol.com/protocol/prompt-turn#3-agent-reports-output>。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    /// 智能体回复文本增量。
    AgentMessageChunk {
        /// 内容块。
        content: TextContent,
    },
    /// 智能体思考 / reasoning 增量。
    AgentThoughtChunk {
        /// 内容块。
        content: TextContent,
    },
    /// 工具调用（创建 + 初始状态）。
    ToolCall {
        /// 工具调用 id。
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// 人类可读标题（参数摘要，如 `$ ls -la`、`read_file: src/main.rs`）。
        title: String,
        /// 工具类别（`read`/`edit`/`execute`/`search`/…）。
        kind: String,
        /// 执行状态。
        status: String,
        /// 原始输出（可选）。
        #[serde(rename = "rawOutput", skip_serializing_if = "Option::is_none")]
        raw_output: Option<String>,
    },
    /// 工具调用更新（状态/输出变更）。
    ToolCallUpdate {
        /// 工具调用 id。
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// 更新后的状态（可选）。
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// 更新后的原始输出（可选）。
        #[serde(rename = "rawOutput", skip_serializing_if = "Option::is_none")]
        raw_output: Option<String>,
    },
    /// 上下文窗口用量更新。
    UsageUpdate {
        /// 当前已用 token。
        used: u64,
        /// 上下文窗口上限。
        size: u64,
    },
}

/// 标准 ACP `session/update` 通知参数。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotificationParams {
    /// 会话 id。
    pub session_id: String,
    /// 更新内容。
    pub update: SessionUpdate,
}

/// 标准 ACP `session/update` 通知（JSON-RPC notification，无 id）。
#[derive(Debug, Clone, Serialize)]
pub struct SessionNotification {
    /// 固定 `"2.0"`。
    pub jsonrpc: String,
    /// 固定 `"session/update"`。
    pub method: String,
    /// 通知参数。
    pub params: SessionNotificationParams,
}

impl SessionNotification {
    /// 构造一条 `session/update` 通知。
    #[must_use]
    pub fn new(session_id: impl Into<String>, update: SessionUpdate) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: "session/update".into(),
            params: SessionNotificationParams {
                session_id: session_id.into(),
                update,
            },
        }
    }
}

/// ACP 传输层错误。
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// I/O 错误（stdio 读写）。
    #[error("I/O 错误: {0}")]
    Io(#[from] std::io::Error),
}

fn default_jsonrpc() -> String {
    "2.0".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_notification_serializes_to_standard_acp() {
        let notif = SessionNotification::new(
            "sess-1",
            SessionUpdate::AgentMessageChunk {
                content: TextContent::new("hello"),
            },
        );
        let json = serde_json::to_string(&notif).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();

        // 顶层信封
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "session/update");
        // camelCase: session_id → sessionId
        assert_eq!(v["params"]["sessionId"], "sess-1");
        // 鉴别器字段
        assert_eq!(
            v["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        // 内容块
        assert_eq!(v["params"]["update"]["content"]["type"], "text");
        assert_eq!(v["params"]["update"]["content"]["text"], "hello");
    }

    #[test]
    fn thought_chunk_serializes_correctly() {
        let notif = SessionNotification::new(
            "s",
            SessionUpdate::AgentThoughtChunk {
                content: TextContent::new("thinking..."),
            },
        );
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&notif).unwrap()).unwrap();
        assert_eq!(
            v["params"]["update"]["sessionUpdate"],
            "agent_thought_chunk"
        );
    }

    #[test]
    fn tool_call_serializes_camel_case_fields() {
        let notif = SessionNotification::new(
            "s",
            SessionUpdate::ToolCall {
                tool_call_id: "tc-1".into(),
                title: "grep".into(),
                kind: "search".into(),
                status: "completed".into(),
                raw_output: Some("3 hits".into()),
            },
        );
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&notif).unwrap()).unwrap();
        let update = &v["params"]["update"];
        assert_eq!(update["sessionUpdate"], "tool_call");
        // camelCase: tool_call_id → toolCallId, raw_output → rawOutput
        assert_eq!(update["toolCallId"], "tc-1");
        assert_eq!(update["rawOutput"], "3 hits");
        assert_eq!(update["title"], "grep");
        assert_eq!(update["status"], "completed");
    }

    #[test]
    fn tool_call_update_serializes_camel_case_fields() {
        let notif = SessionNotification::new(
            "s",
            SessionUpdate::ToolCallUpdate {
                tool_call_id: "tc-2".into(),
                status: Some("failed".into()),
                raw_output: Some("boom".into()),
            },
        );
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&notif).unwrap()).unwrap();
        let update = &v["params"]["update"];
        assert_eq!(update["sessionUpdate"], "tool_call_update");
        // camelCase: tool_call_id → toolCallId, raw_output → rawOutput
        assert_eq!(update["toolCallId"], "tc-2");
        assert_eq!(update["status"], "failed");
        assert_eq!(update["rawOutput"], "boom");
    }

    #[test]
    fn tool_call_update_omits_absent_option_fields() {
        // 仅更新 id（无状态 / 输出变化）时省略可空键，不产 null 字段。
        let notif = SessionNotification::new(
            "s",
            SessionUpdate::ToolCallUpdate {
                tool_call_id: "tc-3".into(),
                status: None,
                raw_output: None,
            },
        );
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&notif).unwrap()).unwrap();
        let update = &v["params"]["update"];
        assert_eq!(update["sessionUpdate"], "tool_call_update");
        assert_eq!(update["toolCallId"], "tc-3");
        assert!(update.get("status").is_none());
        assert!(update.get("rawOutput").is_none());
    }

    #[test]
    fn usage_update_serializes_correctly() {
        let notif = SessionNotification::new(
            "s",
            SessionUpdate::UsageUpdate {
                used: 100,
                size: 1000,
            },
        );
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&notif).unwrap()).unwrap();
        let update = &v["params"]["update"];
        assert_eq!(update["sessionUpdate"], "usage_update");
        assert_eq!(update["used"], 100);
        assert_eq!(update["size"], 1000);
    }

    #[test]
    fn command_info_serializes_name_and_description() {
        let cmd = CommandInfo {
            name: "review".into(),
            description: "代码评审".into(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
        // 仅 name/description 两键，camelCase（单词形与 snake_case 同形，锁定键名）。
        assert_eq!(v["name"], "review");
        assert_eq!(v["description"], "代码评审");
        assert_eq!(v.as_object().map(|o| o.len()), Some(2));
    }
}
