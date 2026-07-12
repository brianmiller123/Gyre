//! # agent-skills
//!
//! File-backed skill 发现、聚合、过滤与 `skill://` URL 解析。
//!
//! 移植自 oh-my-pi `extensibility/skills.ts` 的 provider 优先级去重与 realpath 去重，
//! 融合 Zoo-Code 的 agentskills.io 命名规范。
//!
//! - [`NativeSkillProvider`] —— 原生 `<config_dir>/skills` + `.agent/skills` walkup 发现
//! - [`SkillRegistry`] —— 聚合多 provider，priority first-wins 去重 + glob 过滤
//! - [`SkillCatalog`] —— 已加载集合，实现 [`agent_core::SkillResolver`]
//! - [`render_skills_section`] —— system prompt `<skills>` 段渲染

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

mod frontmatter;
mod native;
mod registry;
mod render;

pub use native::NativeSkillProvider;
pub use registry::{resolve_skill_url, SkillCatalog, SkillRegistry};
pub use render::render_skills_section;
