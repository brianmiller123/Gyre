//! # agent-core
//!
//! 基础契约层：被所有业务 crate 依赖，自身零业务依赖。
//!
//! 包含：
//! - [`error`] —— 分层错误枚举（`AgentError` 总线 + 各子错误）
//! - [`model`] —— 模型身份与线协议族（`Model` / `Api`）
//! - [`message`] —— 内部富消息 vs Provider 线协议消息
//! - [`llm`] —— [`LlmProvider`] 端口 trait
//! - [`tool`] —— `ToolChoice` / `ToolSpec` / [`ApprovalPolicy`] 等
//! - [`context`] —— [`ContextManager`] 端口 trait
//! - [`workspace`] —— 工作区抽象
//! - [`platform`] —— 跨平台路径与编译守卫
//! - [`skill`] —— file-backed skill 端口（[`SkillProvider`](skill::SkillProvider) / [`SkillResolver`](skill::SkillResolver)）
//! - [`resource`] —— 外部资源读取端口（[`ResourceResolver`](resource::ResourceResolver)，`mcp://` 路由用）
//!
//! 解耦保证：本 crate 不依赖 `reqwest`/`tokio`/`tree-sitter` 等任何具体实现。

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::pedantic)]

pub mod context;
pub mod error;
pub mod hook;
pub mod llm;
pub mod memory;
pub mod message;
pub mod model;
pub mod platform;
pub mod resource;
pub mod skill;
pub mod tool;
pub mod write_effect;
pub mod workspace;

pub use context::{CompactionStrategy, ContextManager, NodeId, ProviderContext, SessionNode, TokenUsage};
pub use error::{AgentError, ConfigError, ContextError, LlmError, ToolError};
pub use hook::{Hook, HookEvent, TurnEndContext};
pub use memory::{MemoryNote, MemoryStore};
pub use llm::{
    AssistantEvent, AssistantEventStream, CompletionRequest, Effort, LlmProvider, ProviderCallContext,
    ThinkingClassifier, ThinkingConfig, ThinkingPolicy,
};
pub use message::{
    AgentEvent, AgentMessage, AgentRunSummary, AgentState, AskKind, AskMessage, AskResponse,
    AssistantMessage, ContentBlock, Mode, ProviderMessage, StatusKind, StatusMessage, StopDetails,
    StopReason, ToolCounters, ToolImage, ToolResultMessage, Usage, UserContent, UserMessage,
};
pub use model::{Api, Model};
pub use platform::{config_dir, forced_utf8_locale};
pub use resource::{ResourceEntry, ResourceError, ResourceResolver};
pub use skill::{
    Skill, SkillError, SkillLevel, SkillLoadOptions, SkillProvider, SkillResolver, SkillSource,
};
pub use tool::{
    ApprovalDecision, ApprovalMode, ApprovalPolicy, ApprovalRequest, CapabilityTier, SoftToolRequirement,
    ToolChoice, ToolChoiceDirective, ToolResult, ToolSpec,
};
pub use write_effect::{
    DeferredDiagnosticsHandle, DiagnosticSeverity, WriteDiagnostic, WriteEffect, WriteOutcome,
};
pub use workspace::Workspace;
