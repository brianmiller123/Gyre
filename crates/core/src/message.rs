//! 消息模型：内部富消息 vs Provider 线协议消息。
//!
//! 移植 oh-my-pi 的关键边界：内部 [`AgentMessage`]（含 UI/状态，`convert_to_llm` 会过滤）
//! 与 Provider 线 [`ProviderMessage`] 分离。融合 Zoo-Code 的 say/ask 交互语义。

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::llm::AssistantEvent;
use crate::tool::{SoftToolRequirement, ToolResult};

/// 智能体执行状态机（移植 Zoo-Code 五态机）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// 无任务。
    NoTask,
    /// 运行中。
    Running,
    /// 流式接收中。
    Streaming,
    /// 等待用户输入（审批/回答）。
    WaitingForInput,
    /// 空闲（完成或失败）。
    Idle,
    /// 可恢复（暂停中）。
    Resumable,
}

/// 智能体模式（Zoo-Code 模式系统：决定 system prompt 与工具子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// 编码模式（默认，全工具）。
    #[default]
    Code,
    /// 架构模式（只读 + 规划）。
    Architect,
    /// 问答模式（只读）。
    Ask,
    /// 调试模式。
    Debug,
}

/// 停止原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// 自然停止。
    Stop,
    /// 达到长度上限。
    Length,
    /// 触发工具调用。
    ToolUse,
    /// 非终止停顿（provider 结束响应但未完成轮次，如分段输出/进度更新）。循环应重新采样继续。
    Pause,
    /// 被中止。
    Aborted,
    /// 错误。
    Error,
}

/// token 用量与成本。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// 输入 token。
    pub input_tokens: u64,
    /// 输出 token。
    pub output_tokens: u64,
    /// 缓存命中读取 token。
    pub cache_read_tokens: u64,
    /// 缓存写入 token。
    pub cache_write_tokens: u64,
    /// 预估成本（美元）。
    pub cost_usd: f64,
}

impl Usage {
    /// 累加另一份用量。
    pub fn add(&mut self, other: &Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.cost_usd += other.cost_usd;
    }

    /// token 总数（输入 + 输出）。
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// 助手消息内容块。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// 文本。
    Text {
        /// 文本内容。
        text: String,
    },
    /// 思考（reasoning）。
    Thinking {
        /// 思考内容。
        text: String,
        /// 签名（部分 provider 返回，用于校验）。
        signature: Option<String>,
    },
    /// 工具调用。
    ToolCall {
        /// 调用 ID。
        id: String,
        /// 工具名。
        name: String,
        /// 参数（JSON）。
        arguments: serde_json::Value,
    },
}

impl ContentBlock {
    /// 若为文本块，返回其文本切片。
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        if let Self::Text { text } = self {
            Some(text)
        } else {
            None
        }
    }
}

/// 助手消息（已聚合的完整消息）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// 内容块。
    pub content: Vec<ContentBlock>,
    /// 用量。
    pub usage: Usage,
    /// 产生该消息的模型 ID。
    pub model: String,
    /// 停止原因。
    pub stop_reason: Option<StopReason>,
}

impl AssistantMessage {
    /// 提取所有工具调用块。
    #[must_use]
    pub fn tool_calls(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall { id, name, arguments } => Some((id.as_str(), name.as_str(), arguments)),
                _ => None,
            })
            .collect()
    }

    /// 是否含工具调用。
    #[must_use]
    pub fn has_tool_calls(&self) -> bool {
        self.content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. }))
    }

    /// 拼接所有文本块。
    #[must_use]
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("")
    }
}

/// 用户内容块（支持多模态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContent {
    /// 文本。
    Text {
        /// 文本内容。
        text: String,
    },
    /// 图像（base64）。
    Image {
        /// MIME 类型。
        mime: String,
        /// base64 数据。
        data: String,
    },
}

/// 用户消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    /// 内容块。
    pub content: Vec<UserContent>,
}

impl UserMessage {
    /// 从纯文本构造。
    #[must_use]
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            content: vec![UserContent::Text { text: text.into() }],
        }
    }
}

/// 工具结果消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    /// 对应的 tool_call_id。
    pub tool_call_id: String,
    /// 结构化结果。
    pub result: ToolResult,
}

/// 状态消息（Zoo-Code "say" —— 信息性，不进入 LLM 上下文）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusMessage {
    /// 文本。
    pub text: String,
    /// 类别（用于 UI 着色/路由）。
    pub kind: StatusKind,
}

impl StatusMessage {
    /// 类别的人类可读文本。
    #[must_use]
    pub fn kind_text(&self) -> &'static str {
        match self.kind {
            StatusKind::Info => "信息",
            StatusKind::Thinking => "思考",
            StatusKind::Success => "成功",
            StatusKind::Warning => "警告",
            StatusKind::Error => "错误",
        }
    }
}

/// 状态消息类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StatusKind {
    /// 普通信息（默认）。
    #[default]
    Info,
    /// 思考过程。
    Thinking,
    /// 成功。
    Success,
    /// 警告。
    Warning,
    /// 错误。
    Error,
}

/// 交互式询问（Zoo-Code "ask" —— 阻塞等待用户响应）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskMessage {
    /// 唯一 ID（用于配对响应）。
    pub id: String,
    /// 询问类型。
    pub kind: AskKind,
    /// 提示文案。
    pub prompt: String,
}

/// 询问类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    /// 工具调用审批。
    Tool {
        /// 工具名。
        tool: String,
    },
    /// 命令执行审批。
    Command {
        /// 完整命令。
        command: String,
    },
    /// 追问。
    Followup,
    /// 任务完成结果。
    CompletionResult,
}

/// 对询问的响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskResponse {
    /// 批准。
    Yes,
    /// 拒绝。
    No,
    /// 文本回答。
    Text(String),
}

/// 工具结果中的图像（多模态）。`data` 为 base64 编码。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolImage {
    /// MIME 类型（如 `image/png`）。
    pub mime: String,
    /// base64 编码数据。
    pub data: String,
}

/// Provider 线协议消息（仅 user/assistant/tool，已过滤 UI 消息）。
#[derive(Debug, Clone)]
pub enum ProviderMessage {
    /// 系统消息。
    System(String),
    /// 用户消息。
    User {
        /// 内容块。
        content: Vec<UserContent>,
    },
    /// 助手消息。
    Assistant {
        /// 内容块。
        content: Vec<ContentBlock>,
    },
    /// 工具结果。
    Tool {
        /// 对应 tool_call_id。
        tool_call_id: String,
        /// 文本结果。
        content: String,
        /// 是否为错误结果。
        is_error: bool,
        /// 附带的图像（多模态工具结果；Anthropic 端可作为 image block 真实传递，
        /// OpenAI 端 tool role 仅支持文本，自动降级为占位提示）。
        images: Vec<ToolImage>,
    },
}

/// 内部富消息（Zoo-Code say/ask + oh-my-pi Message 融合）。可序列化以支持 JSONL 持久化。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentMessage {
    /// 用户消息。
    User(UserMessage),
    /// 助手消息。
    Assistant(AssistantMessage),
    /// 工具结果。
    ToolResult(ToolResultMessage),
    /// 信息性状态（不进 LLM 上下文）。
    Status(StatusMessage),
    /// 交互式询问（阻塞）。
    Ask(AskMessage),
    /// 软工具需求提醒（注入式）。
    SoftRequirement(SoftToolRequirement),
}

impl AgentMessage {
    /// 从纯文本构造用户消息的便捷方法。
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::User(UserMessage::from_text(text))
    }

    /// 从内容块（可含图像等多模态）构造用户消息的便捷方法。
    #[must_use]
    pub fn user(content: Vec<UserContent>) -> Self {
        Self::User(UserMessage { content })
    }
}

/// 单个工具的运行计数（P2-K：可观测性，移植 oh-my-pi `run-collector` 的 [`ToolCounters`] 简化版）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCounters {
    /// 总调用次数。
    pub total: u64,
    /// 成功次数（`Text` / `Image` 结果）。
    pub ok: u64,
    /// 错误次数（`Error` 结果）。
    pub error: u64,
}

/// 运行结束摘要。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentRunSummary {
    /// 累计用量。
    pub usage: Usage,
    /// 循环轮数（LLM 请求次数）。
    pub turns: u64,
    /// 工具调用次数。
    pub tool_calls: u64,
    /// 是否成功完成。
    pub success: bool,
    /// 隔离模式下的文件变更 unified diff（仅在启用隔离时有值）。
    pub iso_diff: Option<String>,
    /// 每个工具的运行计数（P2-K）。键为工具名，BTreeMap 自然排序保证序列化稳定。
    #[serde(default)]
    pub tools_by_name: BTreeMap<String, ToolCounters>,
    /// 注册的可用工具名（排序，P2-K coverage）。
    #[serde(default)]
    pub tools_available: Vec<String>,
    /// 实际调用过的工具名集合（P2-K coverage）。`unused = available − invoked`，见 [`AgentRunSummary::unused_tools`]。
    #[serde(default)]
    pub tools_invoked: BTreeSet<String>,
}

impl AgentRunSummary {
    /// 记录一次工具执行结果（P2-K）。在工具结果回填时调用：按工具名累加 total + (ok|error)，
    /// 并把工具名加入 `tools_invoked`。
    pub fn record_tool(&mut self, name: &str, result: &ToolResult) {
        let counters = self.tools_by_name.entry(name.to_string()).or_default();
        counters.total += 1;
        if matches!(result, ToolResult::Error { .. }) {
            counters.error += 1;
        } else {
            counters.ok += 1;
        }
        self.tools_invoked.insert(name.to_string());
    }

    /// 返回「注册但从未被调用」的工具名（排序，P2-K coverage）——用于评估「该用没用」。
    #[must_use]
    pub fn unused_tools(&self) -> Vec<String> {
        self.tools_available
            .iter()
            .filter(|n| !self.tools_invoked.contains(*n))
            .cloned()
            .collect()
    }
}

/// 智能体对外发射的事件流元素（前端订阅）。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 状态机变更。
    StateChanged(AgentState),
    /// 信息性输出。
    Say(StatusMessage),
    /// 需审批/回答。
    Ask(AskMessage),
    /// 透传 LLM 流式事件。
    Assistant(AssistantEvent),
    /// 文本增量（便捷别名）。
    TextDelta(String),
    /// 思考增量（reasoning / thinking）。
    ThinkingDelta(String),
    /// 工具执行进度。
    ToolExec {
        /// 工具名。
        name: String,
        /// 阶段输出。
        output: String,
    },
    /// 用量更新。
    Usage(Usage),
    /// 任务结束。
    Done(AgentRunSummary),
    /// 错误。
    Error(String),
}
