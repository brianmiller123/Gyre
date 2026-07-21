//! # agent-prompt
//!
//! Prompt 模板引擎：system prompt 以静态 `.md` 文件管理（`prompts/*.md`，编译期 `include_str!`），
//! 不在代码里拼接。`{{var}}` 轻量渲染。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

use std::path::Path;

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

    /// 检测当前操作系统与架构，生成平台感知系统提示词段落。
    ///
    /// 让模型知晓运行环境，从而使用正确的 `shell` 语法、路径分隔符与原生命令，
    /// 避免在 `Windows` 上生成 `bash` 语法或在 `unix` 上生成 `PowerShell` 语法。
    /// 输出在进程运行期内固定不变，属稳定前缀，不破坏 `provider` 前缀缓存。
    #[must_use]
    pub fn platform_section(&self) -> String {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        let (platform_name, shell, notes): (&str, &str, &[&str]) = match os {
            "windows" => (
                "Windows",
                "PowerShell",
                &[
                    "路径分隔符为反斜杠 `\\`",
                    "列出文件用 `dir` 或 `Get-ChildItem`，而非 `ls`",
                    "读取环境变量用 `$env:VAR`（而非 `$VAR` 或 `%VAR%`）",
                    "换行符为 CRLF（`\\r\\n`）",
                ],
            ),
            "macos" => (
                "macOS",
                "zsh / bash",
                &[
                    "路径分隔符为正斜杠 `/`",
                    "文件系统默认大小写不敏感（APFS）",
                    "macOS 专有命令：`open`、`pbcopy`/`pbpaste`、`defaults`",
                ],
            ),
            "linux" => (
                "Linux",
                "bash / sh",
                &["路径分隔符为正斜杠 `/`", "文件系统大小写敏感"],
            ),
            _ => ("类 Unix", "sh", &["路径分隔符为正斜杠 `/`"]),
        };
        let notes_text = notes
            .iter()
            .map(|n| format!("- {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\n\n<environment>\n当前运行环境：{platform_name}（{os}/{arch}）。\n默认 Shell：{shell}。\n平台注意事项：\n{notes_text}\n执行命令与编写脚本时须遵循上述平台约定。\n</environment>\n"
        )
    }

    /// 生成工作目录感知系统提示词段落（稳定前缀）。
    ///
    /// 让模型知晓当前 workspace 根（cwd）：避免盲猜工作目录而执行 `cd /workspace` 之类的
    /// 命令，并明确相对路径的基准。会话期内 cwd 不变，属稳定前缀，不破坏 provider 前缀缓存。
    #[must_use]
    pub fn workspace_section(&self, cwd: &Path) -> String {
        format!(
            "\n\n<workspace>\n当前工作目录：{cwd}。\n所有文件路径默认相对此目录解释；无需执行 `cd` 切换目录，如需访问子目录请在工具参数中直接给出相对或绝对路径。\n</workspace>\n",
            cwd = cwd.display()
        )
    }

    /// 按模式返回 system prompt，并自动追加平台感知段落（稳定前缀）。
    ///
    /// 平台段位于模式主体之后，适合作为 `system` 首部之后的固定块。
    #[must_use]
    pub fn system_with_platform(&self, mode: Mode) -> Vec<String> {
        let mut system = self.system(mode);
        system.push(self.platform_section());
        system
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

    #[test]
    fn platform_section_contains_environment_tag() {
        let cat = PromptCatalog::new();
        let section = cat.platform_section();
        assert!(section.contains("<environment>"), "应含 <environment> 标签");
        assert!(section.contains("</environment>"));
        // 应含当前 OS 与架构常量
        assert!(section.contains(std::env::consts::OS), "应含当前 OS");
        assert!(section.contains(std::env::consts::ARCH), "应含当前 ARCH");
        // 应含 Shell 提示
        assert!(section.contains("Shell"));
    }

    #[test]
    fn workspace_section_contains_cwd() {
        let cat = PromptCatalog::new();
        let section = cat.workspace_section(std::path::Path::new("/home/user/project"));
        assert!(section.contains("<workspace>"), "应含 <workspace> 标签");
        assert!(section.contains("</workspace>"));
        assert!(
            section.contains("/home/user/project"),
            "应含传入的 cwd 路径"
        );
    }

    #[test]
    fn system_with_platform_appends_section() {
        let cat = PromptCatalog::new();
        let sys = cat.system_with_platform(Mode::Code);
        assert_eq!(sys.len(), 2, "应为基础 prompt + 平台段");
        assert!(sys[0].contains("软件工程师"), "首段为模式主体");
        assert!(sys[1].contains("<environment>"), "次段为平台感知");
    }
}
