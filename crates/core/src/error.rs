//! 分层错误体系。
//!
//! 设计：每层一个 `thiserror` 枚举；`AgentError` 作为总线，通过 `#[from]` 收敛各子错误，
//! 调用方用 `?` 向上传播；除真正不可恢复情形外不 `panic`（配合 release `panic = "abort"`）。

use thiserror::Error;

/// 智能体顶层错误总线：聚合所有子系统的错误。
#[derive(Debug, Error)]
pub enum AgentError {
    /// LLM 调用错误。
    #[error(transparent)]
    Llm(#[from] LlmError),
    /// 工具执行错误。
    #[error(transparent)]
    Tool(#[from] ToolError),
    /// 上下文管理错误。
    #[error(transparent)]
    Context(#[from] ContextError),
    /// 配置错误。
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// 连续错误次数达到上限（mistake limit）。
    #[error("连续错误次数达到上限: {0}")]
    MistakeLimit(usize),
    /// 任务被取消。
    #[error("任务被取消: {0}")]
    Aborted(String),
    /// 标准库 IO 错误。
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// JSON 序列化/反序列化错误。
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// 其他未分类错误。
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// LLM 调用相关错误。
#[derive(Debug, thiserror::Error, Clone)]
pub enum LlmError {
    /// HTTP 层错误（状态码 + 响应体）。
    #[error("HTTP {status}: {body}")]
    Http {
        /// HTTP 状态码。
        status: u16,
        /// 响应体摘要。
        body: String,
    },
    /// 网络/传输错误。
    #[error("网络错误: {0}")]
    Transport(String),
    /// 认证失败。
    #[error("认证失败: {0}")]
    Auth(String),
    /// 速率限制，`retry_after_ms` 后可重试。
    #[error("速率限制，{retry_after_ms}ms 后重试")]
    RateLimit {
        /// 建议重试等待毫秒数。
        retry_after_ms: u64,
    },
    /// 流式传输中途中断。
    #[error("流式中断: {0}")]
    StreamInterrupted(String),
    /// 反序列化失败（转字符串，使本枚举可 Clone）。
    #[error("反序列化失败: {0}")]
    Decode(String),
    /// 不支持的模型/能力。
    #[error("不支持: {0}")]
    Unsupported(String),
}

/// 工具执行相关错误。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// 参数校验失败。
    #[error("参数校验失败: {0}")]
    InvalidArgs(String),
    /// 操作被用户拒绝。
    #[error("操作被用户拒绝")]
    Rejected,
    /// 工具执行内部失败。
    #[error("工具执行失败: {0}")]
    Execution(String),
    /// 标准库 IO 错误。
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// JSON 错误。
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl ToolError {
    /// 该错误是否可恢复（循环可继续，而非致命）。
    pub fn is_recoverable(&self) -> bool {
        !matches!(self, Self::Rejected)
    }
}

/// 上下文管理相关错误。
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    /// 超出上下文窗口。
    #[error("超出上下文窗口: 需 {needed}, 上限 {limit}")]
    Overflow {
        /// 实际需要的 token 数。
        needed: usize,
        /// 模型上限。
        limit: usize,
    },
    /// 压缩失败。
    #[error("压缩失败: {0}")]
    Compaction(String),
    /// 标准库 IO 错误。
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// 配置相关错误。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// 配置文件读取失败。
    #[error("读取配置失败 {path}: {source}")]
    Read {
        /// 配置文件路径。
        path: String,
        /// 底层 IO 错误。
        #[source]
        source: std::io::Error,
    },
    /// 配置解析失败（转字符串，避免 core 依赖 toml）。
    #[error("解析配置失败: {0}")]
    Parse(String),
    /// 语义校验失败（如缺少必填字段）。
    #[error("配置校验失败: {0}")]
    Invalid(String),
    /// 找不到请求的模型 profile。
    #[error("找不到模型 profile: {0}")]
    ModelNotFound(String),
}
