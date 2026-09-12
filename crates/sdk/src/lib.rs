//! # agent-sdk —— 装配侧的单一事实来源
//!
//! 背景（差距报告 H19 / H42）：system prompt 的「基座上下文」此前在**三处各自拼装**：
//! `crates/cli/src/main.rs`（REPL / 单次任务）、`crates/cli/src/rpc.rs`（NDJSON RPC）、
//! `crates/server/src/lib.rs`（HTTP + WebSocket）。三份实现已经漂移：
//!
//! | 段 | main | rpc | server |
//! |---|---|---|---|
//! | 行为四段（Tool Policy / Delegation / Workflow / Delivery） | ✅ | ❌ | ❌ |
//! | `discover_context_files`（AGENTS.md 等） | ✅ | ✅ | ✅ |
//! | 外来配置（Codex/Cursor/Cline 指令继承） | ✅ | ✅ | ❌ |
//! | `security_scan` 使用指引 | ✅ | ❌ | ✅ |
//! | MCP server instructions | ✅ | ❌ | ❌ |
//! | 可选工具提示（ast/lsp/image/hashline/pty/ssh/browser/github） | ✅动态 | ✅动态 | ❌ |
//!
//! 本 crate 把**静态基座**收敛为一处：三个前端只提供输入（cwd / MCP 指引），
//! 顺序与内容由 [`compose_context_files`] 唯一决定。对齐 oh-my-pi `sdk.ts` 的
//! 「一个装配入口、多个宿主」结构。
//!
//! **可选工具提示**（[`optional_tool_sections`]）按「工具是否注册」动态追加：
//! REPL / RPC 可在运行期切换启用态（`/github`、`/tools`），所以在构造 Agent 时
//! 由宿主调用并追加到基座之后；server 不注册可选工具，故不追加。
//!
//! 基座顺序契约（与 oh-my-pi system-prompt 语义一致）：
//! 1. 行为四段（杠杆最前，对应 omp systemPrompt 主体）；
//! 2. 项目上下文文件（AGENTS.md / CLAUDE.md 等）；
//! 3. 外来工具配置继承段；
//! 4. `security_scan` 使用指引（恒开注册 → 恒注入）；
//! 5. MCP server 握手返回的使用指引（最后，冲突时以工具 schema 为准）。

pub mod assembly;

pub use assembly::{
    OPTIONAL_TOOL_KEYS, SnapshotStore, assemble_builtin_tools, is_known_optional_key,
    new_snapshot_store, optional_tool_switches,
};

use std::collections::HashMap;
use std::path::Path;

use agent_core::prompt_sections::{
    DELEGATION_SECTION, DELIVERY_SECTION, TOOL_POLICY_SECTION, WORKFLOW_SECTION,
};

/// 恒注入的行为契约四段（顺序即注入顺序）。
#[must_use]
pub fn behavior_sections() -> [&'static str; 4] {
    [
        TOOL_POLICY_SECTION,
        DELEGATION_SECTION,
        WORKFLOW_SECTION,
        DELIVERY_SECTION,
    ]
}

/// 基座装配输入（避免长参数列表；新增静态来源时只改本结构）。
pub struct ContextAssembly<'a> {
    /// 工作目录（项目上下文文件 / 外来配置的发现根）。
    pub cwd: &'a Path,
    /// MCP server 握手返回的使用指引（server 名，文本）；未连接时传空切片。
    pub mcp_instructions: &'a [(String, String)],
}

/// 组装结果：完整 `context_files` + 诊断计数（供宿主打印加载提示，避免重复发现）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComposedContext {
    /// 完整有序的上下文段落列表。
    pub files: Vec<String>,
    /// 命中的外来工具配置段数（`context.foreign_loaded` 提示用）。
    pub foreign_count: usize,
}

/// 外来工具配置继承段（Codex/Cursor/Cline/Gemini…），已渲染为上下文段落。
#[must_use]
pub fn foreign_sections(cwd: &Path) -> Vec<String> {
    agent_discovery::discover(cwd)
        .iter()
        .map(agent_discovery::render_section)
        .collect()
}

/// MCP server instructions 段落；无指引时返回 `None`（零 Token 开销）。
#[must_use]
pub fn mcp_instructions_section(instructions: &[(String, String)]) -> Option<String> {
    if instructions.is_empty() {
        return None;
    }
    let mut section = String::from(
        "<mcp-server-instructions>\n已连接 MCP server 随握手返回的使用指引（冲突时以工具 schema 为准）：\n",
    );
    for (server, text) in instructions {
        section.push_str(&format!(
            "\n<server name=\"{server}\">\n{}\n</server>\n",
            text.trim()
        ));
    }
    section.push_str("\n</mcp-server-instructions>");
    Some(section)
}

/// 按启用态组装可选工具提示
/// （ast / lsp / image / edit_writethrough / hashline / pty / ssh / browser / github）。
///
/// 「同一开关同时决定工具是否注册与提示词是否注入」——启用才进 system prompt，禁用完全屏蔽。
#[must_use]
pub fn optional_tool_sections(
    optional: &HashMap<String, bool>,
    github_enabled: bool,
) -> Vec<String> {
    let mut files = Vec::new();
    for p in agent_tools::OPTIONAL_TOOL_PROMPTS {
        if *optional.get(p.key).unwrap_or(&false) {
            files.push(p.prompt.to_string());
        }
    }
    if *optional.get("hashline").unwrap_or(&false) {
        files.push(agent_hashline::PROMPT_SECTION.to_string());
    }
    if *optional.get("pty").unwrap_or(&false) {
        files.push(agent_pty::PROMPT_SECTION.to_string());
    }
    if *optional.get("ssh").unwrap_or(&false) {
        files.push(agent_tools::SSH_PROMPT_SECTION.to_string());
    }
    if *optional.get("browser").unwrap_or(&false) {
        files.push(agent_browser::PROMPT_SECTION.to_string());
    }
    if github_enabled {
        files.push(agent_tools::PROMPT_SECTION.to_string());
    }
    files
}

/// 组装静态基座 `context_files`（顺序见模块文档）。
///
/// REPL / RPC / server 三处前端必须调用本函数而不是各自拼装；这是 H19 的收敛点。
/// 可选工具提示由宿主在其后追加 [`optional_tool_sections`]（可运行期变化）。
#[must_use]
pub fn compose_context_files(input: ContextAssembly<'_>) -> ComposedContext {
    let mut files: Vec<String> = Vec::new();
    // H40：SYSTEM.md 定制段（项目级覆盖用户级）位于最前——它是 system prompt 的定制，
    // 先于行为章节与项目约定（对齐 omp `systemPromptCustomization` 的块位置）。
    if let Some(custom) = agent_config::discover_system_prompt(input.cwd) {
        files.push(format!(
            "<system-prompt-customization>\n{custom}\n</system-prompt-customization>"
        ));
    }
    files.extend(behavior_sections().iter().map(|s| (*s).to_string()));
    files.extend(agent_config::discover_context_files(input.cwd));
    let foreign = foreign_sections(input.cwd);
    let foreign_count = foreign.len();
    files.extend(foreign);
    // Phase 0：security_scan 恒开注册 → 使用指引恒注入（与工具注册面一致）。
    files.push(agent_tools::SECURITY_SCAN_PROMPT_SECTION.to_string());
    if let Some(section) = mcp_instructions_section(input.mcp_instructions) {
        files.push(section);
    }
    ComposedContext {
        files,
        foreign_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compose(cwd: &Path, mcp: &[(String, String)]) -> ComposedContext {
        compose_context_files(ContextAssembly {
            cwd,
            mcp_instructions: mcp,
        })
    }

    #[test]
    fn behavior_sections_are_first_and_complete() {
        let dir = tempfile::tempdir().unwrap();
        let files = compose(dir.path(), &[]).files;
        assert_eq!(files[0], TOOL_POLICY_SECTION);
        assert_eq!(files[1], DELEGATION_SECTION);
        assert_eq!(files[2], WORKFLOW_SECTION);
        assert_eq!(files[3], DELIVERY_SECTION);
        assert!(
            files
                .iter()
                .any(|f| f == agent_tools::SECURITY_SCAN_PROMPT_SECTION)
        );
        // 可选工具提示不在静态基座内（由宿主按启用态追加）。
        assert!(!files.iter().any(|f| f.contains("<hashline>")));
        assert!(!files.iter().any(|f| f.contains("<github>")));
    }

    #[test]
    fn system_md_customization_precedes_behavior_sections() {
        let dir = tempfile::tempdir().unwrap();
        // 无 SYSTEM.md → 行为四段仍居首。
        assert_eq!(compose(dir.path(), &[]).files[0], TOOL_POLICY_SECTION);
        std::fs::write(
            dir.path().join("SYSTEM.md"),
            "始终用中文回答\n\n见 @extra.md\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("extra.md"), "额外规则\n").unwrap();
        let files = compose(dir.path(), &[]).files;
        assert!(
            files[0].starts_with("<system-prompt-customization>"),
            "定制段应最先注入: {}",
            files[0]
        );
        assert!(files[0].contains("始终用中文回答"));
        // `@import` 在 SYSTEM.md 内也展开。
        assert!(files[0].contains("额外规则"), "{}", files[0]);
        assert_eq!(files[1], TOOL_POLICY_SECTION, "行为四段紧随其后");
    }

    #[test]
    fn mcp_instructions_section_is_last() {
        let dir = tempfile::tempdir().unwrap();
        let composed = compose(
            dir.path(),
            &[
                ("fs".into(), "use it".into()),
                ("web".into(), "fetch".into()),
            ],
        );
        let last = composed.files.last().unwrap();
        assert!(last.starts_with("<mcp-server-instructions>"));
        assert!(last.contains("<server name=\"fs\">"));
        assert!(last.contains("<server name=\"web\">"));
        assert!(last.trim_end().ends_with("</mcp-server-instructions>"));
        assert_eq!(mcp_instructions_section(&[]), None);
    }

    #[test]
    fn optional_prompts_follow_enabled_state() {
        let mut optional = HashMap::new();
        let off = optional_tool_sections(&optional, false);
        assert!(!off.iter().any(|f| f.contains("<github>")));
        assert!(!off.iter().any(|f| f.contains("<hashline>")));

        // H45：pty 组的提示词必须随开关出现/消失（否则模型看不到 `shell_session`）。
        assert!(!off.iter().any(|f| f.contains("<pty>")));

        optional.insert("hashline".into(), true);
        optional.insert("pty".into(), true);
        let on = optional_tool_sections(&optional, true);
        assert!(on.iter().any(|f| f.contains("<hashline>")));
        assert!(on.iter().any(|f| f.contains("<github>")));
        assert!(
            on.iter()
                .any(|f| f.contains("<pty>") && f.contains("shell_session")),
            "启用 pty 时必须注入含 `shell_session` 的指引"
        );
    }

    #[test]
    fn project_context_files_follow_behavior_sections() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "project rule").unwrap();
        let files = compose(dir.path(), &[]).files;
        let idx = files
            .iter()
            .position(|f| f.contains("project rule"))
            .expect("AGENTS.md 应被注入");
        assert!(idx >= 4, "项目上下文应在行为四段之后");
    }
}
