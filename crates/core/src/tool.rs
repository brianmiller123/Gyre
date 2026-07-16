//! 工具相关类型与 [`ApprovalPolicy`] 端口。
//!
//! `Tool` trait 本身定义在 [`agent_tools`](https://docs.rs) crate（`crates/tools`），
//! 此处仅放跨 crate 共享的数据类型与审批端口。

use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::message::{AskMessage, AskResponse};

/// 工具调用选择（移植 oh-my-pi `ToolChoice`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    /// 自动（模型自行决定）。
    Auto,
    /// 禁止工具。
    None,
    /// 任意工具。
    Any,
    /// 必须调用工具。
    Required,
    /// 强制调用指定工具。
    Function {
        /// 工具名。
        name: String,
    },
}

/// 软工具需求：先提醒后强制，保护 provider 前缀缓存（强制 tool_choice 会使其失效）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoftToolRequirement {
    /// 稳定 id，变更才重新注入 reminder。
    pub id: String,
    /// 必须先调用的工具名。
    pub tool_name: String,
    /// 提醒消息（注入到上下文）。
    pub reminder: String,
}

/// 每轮工具选择指令：硬 [`ToolChoice`] 或软 [`SoftToolRequirement`]。
#[derive(Debug, Clone)]
pub enum ToolChoiceDirective {
    /// 硬选择（直接作用于 provider）。
    Hard(ToolChoice),
    /// 软需求（提醒 + 必要时升级为强制）。
    Soft(SoftToolRequirement),
}

/// 工具规格（提供给 LLM 的 JSON Schema 描述）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// 工具名。
    pub name: String,
    /// 工具描述。
    pub description: String,
    /// 输入参数 JSON Schema。
    pub schema: serde_json::Value,
}

impl ToolSpec {
    /// 构造工具规格。
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>, schema: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
        }
    }
}

/// 工具返回的结构化结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ToolResult {
    /// 文本结果。
    Text(String),
    /// 图像结果。
    Image {
        /// MIME 类型。
        mime: String,
        /// 二进制数据。
        data: Vec<u8>,
    },
    /// 错误结果。
    Error {
        /// 是否可恢复（循环可继续）。
        recoverable: bool,
        /// 错误信息。
        message: String,
    },
}

impl ToolResult {
    /// 从文本构造。
    #[must_use]
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    /// 渲染为供 LLM 阅读的文本（写入 Tool 消息 content）。
    #[must_use]
    pub fn to_llm_text(&self) -> String {
        match self {
            Self::Text(t) => t.clone(),
            Self::Image { mime, .. } => format!("[image/{mime}]"),
            Self::Error { message, .. } => format!("[error] {message}"),
        }
    }
}

/// 审批模式（移植 oh-my-pi `approvalMode`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    /// 总是询问写/执行类（默认；读层自动放行）。
    #[default]
    AlwaysAsk,
    /// 仅执行类询问，写类自动。
    Write,
    /// 全自动（yolo）。
    Yolo,
}

/// 工具能力分级（决定审批门槛）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityTier {
    /// 只读（read_file / grep / ast 查询）。
    ReadOnly,
    /// 写入（write_file / apply_hashline / ast 重写）。
    Write,
    /// 执行（run_command）。
    Execute,
    /// 网络。
    Network,
}

/// 审批预判定结果。
#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    /// 放行（命中 allow 规则或 yolo 模式）。
    Allow,
    /// 拒绝（命中 deny 规则）。
    Deny(&'static str),
    /// 需人工确认。
    Ask,
}

/// 一次审批请求（供 [`ApprovalPolicy::decide`] 判定）。
#[derive(Debug, Clone)]
pub struct ApprovalRequest<'a> {
    /// 工具名。
    pub tool: &'a str,
    /// 能力分级。
    pub capability: CapabilityTier,
    /// 完整命令（仅 shell 工具）。
    pub command: Option<&'a str>,
    /// 工具参数（JSON）。
    pub args: &'a serde_json::Value,
}

/// 审批策略端口：由 config 规则引擎驱动（移植 oh-my-pi approval）。
///
/// 判定链：逐工具 allow|deny|ask 覆盖 → 命令级 allow/deny/ask 规则 →
/// 能力分级（读层自动放行）→ 三档模式 always-ask / write / yolo。
///
/// 实现示例：`RulesApprovalPolicy`（config 规则）、`WebApprovalPolicy`（WS 远程确认）。
#[async_trait::async_trait]
pub trait ApprovalPolicy: Send + Sync {
    /// 同步预判定：命中 allow/deny 规则或 yolo 模式时立即返回，免去交互。
    fn decide(&self, request: &ApprovalRequest<'_>) -> ApprovalDecision;

    /// 仅当 `decide` 返回 [`ApprovalDecision::Ask`] 时调用：阻塞等待人工决议。
    /// CLI 经 stdin，Web 经 WebSocket 回执。
    ///
    /// # Errors
    /// 用户拒绝或交互失败时返回 [`ToolError`]。
    async fn prompt(&self, ask: &AskMessage) -> Result<AskResponse, ToolError>;
}
