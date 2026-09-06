//! # agent-skills
//!
//! File-backed skill 发现、聚合、过滤与 `skill://` URL 解析。
//!
//! 移植自 oh-my-pi `extensibility/skills.ts` 的 provider 优先级去重与 realpath 去重，
//! 融合 Zoo-Code 的 agentskills.io 命名规范。
//!
//! - [`NativeSkillProvider`] —— 原生 `<config_dir>/skills` + `.agent/skills` walkup 发现
//! - [`providers`] —— 跨工具 provider（Claude/Codex/OpenCode/GitHub，对齐 oh-my-pi 路径与优先级）
//! - [`SkillRegistry`] —— 聚合多 provider，priority first-wins 去重 + glob 过滤
//! - [`SkillCatalog`] —— 已加载集合，实现 [`agent_core::SkillResolver`]
//! - [`render_skills_section`] —— system prompt `<skills>` 段渲染

#![deny(unsafe_code)]

mod frontmatter;
mod native;
mod providers;
mod registry;
mod render;
mod scan;

pub use native::NativeSkillProvider;
pub use providers::{
    ProviderToggles, claude_provider, codex_provider, cross_tool_providers, github_provider,
    opencode_provider,
};
pub use registry::{SkillCatalog, SkillRegistry, resolve_skill_url};
pub use render::render_skills_section;
