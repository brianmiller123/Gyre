//! # agent-config
//!
//! 配置层：TOML 分层加载（项目 `.agent/config.toml` 覆盖用户级）、模型 profile、
//! `${ENV}` 展开、命令审批规则与 [`RulesApprovalPolicy`] 审批引擎。
//!
//! 移植 oh-my-pi 分层配置与逐工具/命令审批语义。

#![deny(unsafe_code)]

mod approval;
pub mod at_imports;
mod auth;
mod config;
pub mod context_files;
pub mod dotenv;
mod env;
mod frontmatter;
mod mcp_json;
pub mod model_catalog;
mod oauth_store;
mod rules;
pub mod schema;

pub use approval::ApprovalModeController;

pub use auth::{AuthStore, auth_path, env_var_name, load, resolve, save};

pub use oauth_store::{
    OAuthCredentials, OAuthStore, load as load_oauth, oauth_path, remove as remove_oauth,
    save as save_oauth,
};

pub use at_imports::{MAX_AT_IMPORT_DEPTH, expand_at_imports};
pub use config::{
    AcpConfig, AgentConfig, CommandPattern, CommandRules, CompactionConfig, CompatConfig, Config,
    EditToolsConfig, EvalConfig, GithubConfig, GoalsConfig, HookEventKind, HookRule,
    InterceptorConfig, KeybindingsConfig, McpConfig, McpHttpConfig, McpHttpTransport,
    McpOAuthConfig, McpServerConfig, McpStdioConfig, MemoryBackend, MemoryConfig, MinimizerConfig,
    ModelProfile, RolesCfg, ServerConfig, SkillsConfig, Socks5Config, StreamGuardsConfig,
    SubagentConfig, TodoConfig, ToolApproval, ToolsConfig, ToolsSwitchConfig, TtsrConfig,
    parse_compaction_backend, wildcard_match,
};
pub use config::{
    CustomCommand, discover_commands, discover_context_files, discover_system_prompt,
};
pub use context_files::{ContextFile, collect_context_files, dedupe_contained, system_prompt_file};
pub use dotenv::{DotEnv, load_dotenv};
pub use env::{expand_env, expand_env_with};
pub use frontmatter::{FrontmatterFields, FrontmatterValue, parse_frontmatter};
pub use mcp_json::{
    McpJsonLevel, McpJsonLoad, McpJsonSource, load_mcp_json_sources, mcp_json_sources,
    merge_mcp_json_sources, remove_mcp_server, set_mcp_server_disabled, write_mcp_server,
};
pub use model_catalog::{CATALOG, CatalogEntry};
pub use rules::{RulesApprovalPolicy, RulesEngine};
pub use schema::{
    ConfigWarning, KeyInfo, children_of, key_info, key_table, known_keys, unknown_keys,
};

/// 审批交互回调类型：前端（CLI/Web）注入，决定 `prompt()` 如何等待人工决议。
pub type PromptResolver = std::sync::Arc<
    dyn Fn(
            agent_core::AskMessage,
        ) -> futures::future::BoxFuture<
            'static,
            Result<agent_core::AskResponse, agent_core::ToolError>,
        > + Send
        + Sync,
>;
