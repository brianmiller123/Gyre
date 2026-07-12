//! # agent-config
//!
//! 配置层：TOML 分层加载（项目 `.agent/config.toml` 覆盖用户级）、模型 profile、
//! `${ENV}` 展开、命令审批规则与 [`RulesApprovalPolicy`] 审批引擎。
//!
//! 移植 oh-my-pi 分层配置与逐工具/命令审批语义。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

mod config;
mod env;
mod rules;

pub use config::{
    AgentConfig, AcpConfig, CommandPattern, CommandRules, Config, GithubConfig, McpConfig, McpServerConfig,
    MemoryConfig, ModelProfile, ServerConfig, SkillsConfig, SubagentConfig, ToolApproval,
    ToolsConfig, EditToolsConfig,
};
pub use config::{discover_commands, discover_context_files, CustomCommand};
pub use env::expand_env;
pub use rules::{RulesApprovalPolicy, RulesEngine};

/// 审批交互回调类型：前端（CLI/Web）注入，决定 `prompt()` 如何等待人工决议。
pub type PromptResolver = std::sync::Arc<
    dyn Fn(agent_core::AskMessage) -> futures::future::BoxFuture<'static, Result<agent_core::AskResponse, agent_core::ToolError>>
        + Send
        + Sync,
>;
