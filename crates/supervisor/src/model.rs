//! 子 Agent 监控数据模型。

use agent_core::Usage;
use serde::{Deserialize, Serialize};

/// 子 Agent 生命周期阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentPhase {
    /// 排队中（等待并发许可）。
    #[default]
    Pending,
    /// 运行中。
    Running,
    /// 流式接收 LLM 输出中。
    Streaming,
    /// 等待工具执行 / 审批。
    WaitingTool,
    /// 成功完成。
    Done,
    /// 失败。
    Failed,
    /// 被取消。
    Cancelled,
}

impl SubAgentPhase {
    /// 是否终态（不会再变化）。
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    /// 人类可读标签（中文）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pending => "排队",
            Self::Running => "运行中",
            Self::Streaming => "流式中",
            Self::WaitingTool => "等待工具",
            Self::Done => "完成",
            Self::Failed => "失败",
            Self::Cancelled => "已取消",
        }
    }
}

/// 日志级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// 普通信息（默认）。
    #[default]
    Info,
    /// 调试 / 思考。
    Debug,
    /// 警告。
    Warn,
    /// 错误。
    Error,
}

/// 单条日志（环形缓冲中的一行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    /// 时间戳（毫秒 epoch）。
    pub ts: u64,
    /// 级别。
    pub level: LogLevel,
    /// 文本。
    pub text: String,
}

/// 单个子 Agent 的实时状态快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentStatus {
    /// 唯一 id（进程内单调）。
    pub id: String,
    /// 父 Agent id（顶层委派为 `None`；嵌套委派时填父 id）。
    pub parent_id: Option<String>,
    /// 人类可读标签（任务首部截断）。
    pub label: String,
    /// 被委派的任务描述。
    pub task: String,
    /// 当前阶段。
    pub phase: SubAgentPhase,
    /// 进度估计 `[0.0, 1.0]`。
    pub progress: f32,
    /// 已完成轮次。
    pub turns: u64,
    /// 已调用工具次数。
    pub tool_calls: u64,
    /// 累计用量。
    pub usage: Usage,
    /// 启动时间（毫秒 epoch）。
    pub started_at: u64,
    /// 最近更新时间（毫秒 epoch）。
    pub updated_at: u64,
    /// 当前活动描述（如 `调用工具: shell`）。
    pub current_activity: Option<String>,
    /// 失败原因（仅终态失败时）。
    pub error: Option<String>,
    /// 日志尾部（环形缓冲，按时间顺序，最新在后）。
    pub logs: Vec<LogLine>,
}
