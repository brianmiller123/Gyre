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
//! - [`secrets`] —— 管线级密钥双向脱敏（[`SecretsObfuscator`](secrets::SecretsObfuscator)）
//!
//! 解耦保证：本 crate 不依赖 `reqwest`/`tokio`/`tree-sitter` 等任何具体实现。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod context;
pub mod error;
pub mod hook;
pub mod hub;
pub mod jobs;
pub mod jsonrpc_frame;
pub mod llm;
pub mod memory;
pub mod message;
pub mod model;
pub mod oauth_callback;
pub mod platform;
pub mod prompt_sections;
pub mod resource;
pub mod secrets;
pub mod session_event;
pub mod skill;
pub mod todo_loop;
pub mod tool;
pub mod workspace;
pub mod write_effect;

pub use context::{
    CompactionBackend, CompactionPolicy, CompactionStrategy, ContextManager, NodeId,
    ProviderContext, SessionNode, TokenUsage,
};
pub use error::{AgentError, ConfigError, ContextError, LlmError, ToolError};
pub use hook::{Hook, HookEvent, TurnEndContext};
pub use jobs::AsyncWakeProbe;
pub use jsonrpc_frame::{FrameError, decode_frames, encode_frame, parse_content_length};
pub use llm::{
    AssistantEvent, AssistantEventStream, AuthMode, CompletionRequest, Effort, LlmProvider,
    MaxTokensField, ProviderCallContext, ProviderQuirks, ThinkingClassifier, ThinkingConfig,
    ThinkingPolicy,
};
pub use memory::{MemoryHit, MemoryNote, MemoryStore};
pub use message::{
    AgentEvent, AgentMessage, AgentRunSummary, AgentState, AskKind, AskMessage, AskResponse,
    AssistantMessage, ContentBlock, Mode, ProviderMessage, StatusKind, StatusMessage, StopDetails,
    StopReason, ToolCounters, ToolImage, ToolResultMessage, Usage, UserContent, UserMessage,
};
pub use model::{Api, Model};
pub use platform::{config_dir, forced_utf8_locale, project_config_dir_name};
pub use prompt_sections::{
    DELEGATION_SECTION, DELIVERY_SECTION, TOOL_POLICY_SECTION, WORKFLOW_SECTION,
};
pub use resource::{ResourceEntry, ResourceError, ResourceResolver};
pub use secrets::PatternRedactor;
pub use secrets::SecretsObfuscator;
pub use session_event::{CompactionAction, CompactionReason, CompactionStage, SessionEvent};
pub use skill::{
    Skill, SkillError, SkillLevel, SkillLoadOptions, SkillProvider, SkillResolver, SkillSource,
};
pub use todo_loop::{TodoLoopEntry, TodoLoopSnapshot, TodoLoopSource};
pub use tool::{
    ApprovalDecision, ApprovalMode, ApprovalPolicy, ApprovalRequest, CapabilityTier,
    SoftToolRequirement, ToolChoice, ToolChoiceDirective, ToolResult, ToolSpec,
};
pub use workspace::Workspace;
pub use write_effect::{
    DeferredDiagnosticsHandle, DiagnosticSeverity, WriteDiagnostic, WriteEffect, WriteOutcome,
};
