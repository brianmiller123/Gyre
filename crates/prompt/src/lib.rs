//! # agent-prompt
//!
//! Prompt 模板引擎：system prompt 以静态 `.md` 文件管理（`prompts/*.md`，编译期 `include_str!`），
//! 不在代码里拼接。`{{var}}` 轻量渲染。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

use agent_core::Mode;

// 编译期内嵌 prompt 文本（来自 prompts/*.md）
const SYSTEM_CODE: &str = include_str!("../../../prompts/system-code.md");
const SYSTEM_ARCHITECT: &str = include_str!("../../../prompts/system-architect.md");
const SYSTEM_ASK: &str = include_str!("../../../prompts/system-ask.md");
const SYSTEM_DEBUG: &str = include_str!("../../../prompts/system-debug.md");

/// Prompt 目录：返回稳定前缀主体 + 模板渲染。
#[derive(Debug, Clone, Default)]
pub struct PromptCatalog;

impl PromptCatalog {
    /// 构造。
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// 按模式返回 system prompt（稳定前缀主体，来自静态 .md）。
    #[must_use]
    pub fn system(&self, mode: Mode) -> Vec<String> {
        vec![match mode {
            Mode::Code => SYSTEM_CODE,
            Mode::Architect => SYSTEM_ARCHITECT,
            Mode::Ask => SYSTEM_ASK,
            Mode::Debug => SYSTEM_DEBUG,
        }
        .to_string()]
    }

    /// 轻量 `{{var}}` 模板渲染。
    #[must_use]
    pub fn render(&self, template: &str, vars: &[(&str, &str)]) -> String {
        let mut out = template.to_string();
        for (key, value) in vars {
            out = out.replace(&format!("{{{{{key}}}}}"), value);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_replaces_vars() {
        let cat = PromptCatalog::new();
        assert_eq!(cat.render("hello {{name}}!", &[("name", "world")]), "hello world!");
    }

    #[test]
    fn system_prompt_per_mode() {
        let cat = PromptCatalog::new();
        assert!(cat.system(Mode::Code)[0].contains("软件工程师"));
        assert!(cat.system(Mode::Ask)[0].contains("技术顾问"));
        assert!(cat.system(Mode::Architect)[0].contains("架构师"));
        assert!(cat.system(Mode::Debug)[0].contains("调试专家"));
    }
}
