//! # agent-tools
//!
//! [`Tool`] 端口（具体实现）、[`ToolRegistry`] 与内置 fs/shell/lsp 工具。
//! 工具经 [`Tool::capability`] + [`Tool::describe`] 自描述审批请求，
//! 由执行循环（`crates/agent`）集中做 say/ask 审批门禁。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

mod ast_tool;
mod fs;
mod fuzzy_match;
mod github;
mod image;
mod list_tool;
mod lsp_tool;
mod lsp_write_effect;
mod search;
mod shell;
mod write;

use agent_core::{
    ApprovalPolicy, ApprovalRequest, CapabilityTier, ToolResult, ToolSpec, Workspace, WriteEffect,
};

pub use ast_tool::{AstRewriteTool, AstSearchTool, ReplaceBlockTool};
pub use fs::{ReadFileTool, WriteFileTool};
pub use fuzzy_match::{
    find_unique_match, set_fuzzy_opts, FuzzyOpts, MatchError, MatchMethod, MatchOutcome,
};
pub use github::{GithubTool, PROMPT_SECTION};
pub use image::{ImageGenTool, ReadImageTool};
pub use list_tool::ListFilesTool;
pub use lsp_tool::{LspPool, LspTool};
pub use lsp_write_effect::LspWriteEffect;
pub use search::{GlobTool, GrepTool};
pub use shell::RunCommandTool;
pub use write::{render_diagnostics, write_with_effects, NoopWriteEffect, WriteReport};

/// 工具执行上下文：工作区、审批策略、取消令牌，及内部协议解析器。
pub struct ToolContext<'a> {
    /// 工作区根。
    pub workspace: &'a Workspace,
    /// 审批策略（工具可二次细查，主门禁由循环负责）。
    pub approval: &'a dyn ApprovalPolicy,
    /// 取消令牌（中断长时工具）。
    pub cancel: &'a tokio_util::sync::CancellationToken,
    /// Skill 解析器（可选；read_file 遇 `skill://` URL 时使用）。
    pub skills: Option<&'a dyn agent_core::SkillResolver>,
    /// 跨会话记忆（可选；read_file 遇 `memory://` URL 时使用）。
    pub memory: Option<&'a dyn agent_core::MemoryStore>,
    /// 外部资源解析器（可选；read_file 遇 `mcp://` URL 时使用）。
    pub resources: Option<&'a dyn agent_core::ResourceResolver>,
    /// 写入效果（可选；写工具经 `write_with_effects` 在写盘后触发 LSP format/diagnostics）。
    pub write_effect: Option<&'a dyn WriteEffect>,
}

/// 工具端口。
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// 工具名。
    fn name(&self) -> &str;
    /// 描述（供 LLM）。
    fn description(&self) -> &str;
    /// 输入参数 JSON Schema。
    fn schema(&self) -> serde_json::Value;
    /// 能力分级（决定审批门槛）。
    fn capability(&self) -> CapabilityTier;

    /// 构造审批请求描述（默认：工具名 + 能力 + 无命令）；shell 工具重写以带 command。
    fn describe<'a>(&'a self, input: &'a serde_json::Value) -> ApprovalRequest<'a> {
        ApprovalRequest {
            tool: self.name(),
            capability: self.capability(),
            command: None,
            args: input,
        }
    }

    /// 执行。
    ///
    /// # Errors
    /// 参数校验/执行失败时返回 [`agent_core::ToolError`]。
    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, agent_core::ToolError>;
}

/// 工具注册表端口。
pub trait ToolRegistry: Send + Sync {
    /// 所有工具的线 spec（供 LLM 工具列表）。
    fn specs(&self) -> Vec<ToolSpec>;
    /// 按名查找工具。
    fn get(&self, name: &str) -> Option<&dyn Tool>;
}

/// 默认注册表实现。
pub struct DefaultToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl DefaultToolRegistry {
    /// 空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// 注册工具。
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// 构建器：注册并返回自身。
    #[must_use]
    pub fn with(mut self, tool: Box<dyn Tool>) -> Self {
        self.register(tool);
        self
    }
}

impl Default for DefaultToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry for DefaultToolRegistry {
    fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|t| ToolSpec::new(t.name(), t.description(), t.schema()))
            .collect()
    }

    fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(Box::as_ref)
    }
}

/// 核心工具集（始终启用，不受 `[tools]` 开关控制）：
/// read_file / write_file / list_files / run_command / grep / glob。
/// 编辑经可选的 `apply_hashline`（主推）完成；`write_file` 负责整文件创建/覆写。
#[must_use]
pub fn core_tools() -> DefaultToolRegistry {
    DefaultToolRegistry::new()
        .with(Box::new(ReadFileTool))
        .with(Box::new(WriteFileTool))
        .with(Box::new(ListFilesTool))
        .with(Box::new(RunCommandTool))
        .with(Box::new(GrepTool))
        .with(Box::new(GlobTool))
}

/// AST 可选工具组：`replace_block` / `ast_search` / `ast_rewrite`（受 `[tools].ast` 控制）。
#[must_use]
pub fn ast_tools(reg: DefaultToolRegistry) -> DefaultToolRegistry {
    reg.with(Box::new(ReplaceBlockTool))
        .with(Box::new(AstSearchTool))
        .with(Box::new(AstRewriteTool))
}

/// 图像可选工具组：`read_image` / `image_gen`（受 `[tools].image` 控制）。
#[must_use]
pub fn image_tools(reg: DefaultToolRegistry) -> DefaultToolRegistry {
    reg.with(Box::new(ReadImageTool))
        .with(Box::new(ImageGenTool))
}

/// LSP 可选工具（受 `[tools].lsp` 控制）。
#[must_use]
pub fn lsp_tool(reg: DefaultToolRegistry) -> DefaultToolRegistry {
    reg.with(Box::new(LspTool::new()))
}

/// 全部内置工具（core + ast + image + lsp），向后兼容入口。
///
/// 注意：可被装配层按需裁剪。生产装配应使用 [`core_tools`] + 各可选组构造器，
/// 仅启用已配置的工具组，以严格控制初始上下文长度（避免无用 Token 开销）。
#[must_use]
pub fn builtin_tools() -> DefaultToolRegistry {
    let reg = core_tools();
    let reg = ast_tools(reg);
    let reg = image_tools(reg);
    lsp_tool(reg)
}

/// 与 [`builtin_tools`] 相同，但额外返回 `LspTool` 的共享 [`LspPool`]（供 `LspWriteEffect` 复用同一套语言服务器）。
#[must_use]
pub fn builtin_tools_with_pool() -> (DefaultToolRegistry, LspPool) {
    let reg = core_tools();
    let reg = ast_tools(reg);
    let reg = image_tools(reg);
    let lsp = LspTool::new();
    let pool = lsp.pool();
    let reg = reg.with(Box::new(lsp));
    (reg, pool)
}

// ── 可选工具提示词（启用时动态注入 system prompt，禁用完全屏蔽）──────────────────

/// 单个可选工具组的提示词元数据。
#[derive(Debug, Clone, Copy)]
pub struct OptionalToolPrompt {
    /// 配置 key（对应 `[tools].enabled.<key>`）。
    pub key: &'static str,
    /// 人类可读标签（供 `/tools` 展示）。
    pub label: &'static str,
    /// 内置默认开关（未在配置显式指定时使用）。
    pub default: bool,
    /// 启用时注入 system prompt 的操作指引。
    pub prompt: &'static str,
}

/// AST 组使用指引（覆盖 replace_block / ast_search / ast_rewrite）。
const AST_PROMPT: &str = "<ast>\n\
AST 结构化工具已启用：基于 tree-sitter 的句法感知编辑与检索。\n\
- replace_block：按行号定位，整体替换一个句法块（函数/结构体/方法），自动确定块边界，比纯文本替换更稳。\n\
- ast_search：按 AST 模式检索（如「所有函数定义」「所有方法调用」），精确于文本 grep。\n\
- ast_rewrite：按 AST 模式做结构化重写。\n\
当前主要支持 Rust；行号须基于最新 read 结果（编辑后重编号）。\n\
</ast>";

/// LSP 组使用指引。
const LSP_PROMPT: &str = "<lsp>\n\
LSP 工具 `lsp` 已启用：经语言服务器获取语义信息。\n\
action ∈ {diagnostics, goto_definition, find_references, hover, document_symbols, workspace_symbols, rename, code_actions}。\n\
用 diagnostics 拿编译/类型错误，goto_definition/find_references 做精准跳转，hover 看类型；\n\
rename 做跨文件语义重命名。uri 用 file:/// 绝对路径；语言服务器须已在工作区可被发现。\n\
</lsp>";

/// 图像组使用指引（覆盖 read_image / image_gen）。
const IMAGE_PROMPT: &str = "<image>\n\
图像工具已启用：read_image（读取本地图片给模型查看，png/jpeg/gif/webp）与 image_gen（OpenAI 兼容图像生成）。\n\
image_gen 需环境变量 IMAGE_API_KEY 或 OPENAI_API_KEY。\n\
</image>";

/// 编辑后 writethrough 组使用指引。
const EDIT_PROMPT: &str = "<edit_writethrough>\n\
编辑后 LSP writethrough 已启用：写工具（write_file/apply_hashline/replace_block/ast_rewrite）\n\
落盘后自动触发 LSP format（若服务器支持）与诊断回写，结果附在工具返回末尾。据此诊断修正代码。\n\
SEARCH 块模糊匹配可经 PI_EDIT_FUZZY=on 启用（归一化 + 相似度容错），命中非精确时结果会标注相似度。\n\
</edit_writethrough>";

/// agent-tools 内可选工具组的提示词元数据（ast / lsp / image）。
///
/// `hashline` / `pty` / `github` 由各自 crate 导出的 `PROMPT_SECTION` 提供，
/// 由装配层在启用时一并注入——统一原则：**仅在启用态进入 system prompt，禁用时连同工具一并移除**。
pub const OPTIONAL_TOOL_PROMPTS: &[OptionalToolPrompt] = &[
    OptionalToolPrompt {
        key: "ast",
        label: "AST 结构化检索/重写 (replace_block/ast_search/ast_rewrite)",
        default: false,
        prompt: AST_PROMPT,
    },
    OptionalToolPrompt {
        key: "lsp",
        label: "LSP 语言服务器 (诊断/定义/引用/重命名)",
        default: false,
        prompt: LSP_PROMPT,
    },
    OptionalToolPrompt {
        key: "image",
        label: "图像读取/生成 (read_image/image_gen)",
        default: false,
        prompt: IMAGE_PROMPT,
    },
    OptionalToolPrompt {
        key: "edit_writethrough",
        label: "编辑后 LSP writethrough (format/diagnostics 回写)",
        default: false,
        prompt: EDIT_PROMPT,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lists_and_finds() {
        let reg = builtin_tools();
        let specs = reg.specs();
        assert!(specs.iter().any(|s| s.name == "read_file"));
        assert!(reg.get("write_file").is_some());
        // github 默认禁用：由装配层在 config [github] enabled 时条件注册。
        assert!(!reg.get("github").is_some());
        assert!(reg.get("nope").is_none());
    }

    #[test]
    fn core_tools_excludes_optional() {
        let reg = core_tools();
        let specs = reg.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        for core in [
            "read_file",
            "write_file",
            "list_files",
            "run_command",
            "grep",
            "glob",
        ] {
            assert!(names.contains(&core), "核心工具 {core} 应在 core_tools");
        }
        for opt in [
            "replace_block",
            "ast_search",
            "ast_rewrite",
            "read_image",
            "image_gen",
            "lsp",
        ] {
            assert!(!names.contains(&opt), "可选工具 {opt} 不应在 core_tools");
        }
    }

    #[test]
    fn ast_tools_adds_ast_group() {
        let reg = ast_tools(core_tools());
        let specs = reg.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"replace_block"));
        assert!(names.contains(&"ast_search"));
        assert!(names.contains(&"ast_rewrite"));
    }

    #[test]
    fn optional_tool_prompts_cover_known_keys() {
        let keys: Vec<&str> = OPTIONAL_TOOL_PROMPTS.iter().map(|p| p.key).collect();
        for k in ["ast", "lsp", "image"] {
            assert!(keys.contains(&k), "缺可选工具提示词 {k}");
        }
        // 每条提示词以 <key>…</key> 包裹，便于注入时识别
        for p in OPTIONAL_TOOL_PROMPTS {
            assert!(p.prompt.starts_with(&format!("<{}>", p.key)), "{} 头标记", p.key);
            assert!(p.prompt.ends_with(&format!("</{}>", p.key)), "{} 尾标记", p.key);
        }
    }
}
