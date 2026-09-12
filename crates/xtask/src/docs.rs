//! 工具参考文档生成（H38）。
//!
//! 数据源是**真实装配的工具实例**（不是手抄清单）：核心工具、可选工具组、会话工具、
//! 宿主工具各自用与前端相同的构造器建出来，再读 `Tool::name/description/capability/schema`
//! 渲染 Markdown。因此工具的 schema/描述变更会立刻反映到文档，`--check` 即时发现漂移。
//!
//! 输出：`docs/tools.md`（概览表 + 逐工具参数表）。

use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{CapabilityTier, ToolSpec};
use agent_tools::{DefaultToolRegistry, Tool, ToolRegistry};

/// 一个工具在文档中的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDoc {
    /// 工具名（模型可见的 wire 名）。
    pub name: String,
    /// 分组标签（核心 / 可选 / 会话 / 宿主）。
    pub group: &'static str,
    /// 可选开关 key（`[tools] <key> = true`）；核心工具为 `None`。
    pub optional_key: Option<&'static str>,
    /// 能力档（审批门禁依据）。
    pub capability: CapabilityTier,
    /// 工具描述（可多行）。
    pub description: String,
    /// 参数 schema。
    pub schema: serde_json::Value,
}

/// 从注册表收集工具（按注册顺序）。
fn collect_from(
    reg: &dyn ToolRegistry,
    group: &'static str,
    optional_key: Option<&'static str>,
    out: &mut Vec<ToolDoc>,
) {
    for spec in reg.specs() {
        let Some(tool) = reg.get(&spec.name) else {
            continue;
        };
        out.push(ToolDoc {
            name: tool.name().to_string(),
            group,
            optional_key,
            capability: tool.capability(),
            description: tool.description().to_string(),
            schema: tool.schema(),
        });
    }
}

/// 装配全部工具并收集文档数据。
///
/// 与前端装配保持一致：核心 = `core_tools_with_jobs`；可选组按组单独装配（文档需要
/// **全部**可选工具，故不按开关过滤）；会话/宿主工具用与 CLI 相同的构造器。
#[must_use]
pub fn collect_tools() -> Vec<ToolDoc> {
    let mut out: Vec<ToolDoc> = Vec::new();

    // 核心工具（只读/写/执行/搜索）。
    let core = agent_tools::core_tools_with_jobs(Vec::new(), agent_tools::disabled(), None);
    collect_from(&core, "核心", None, &mut out);

    // 可选工具组：逐组装配（文档列全量，实际是否注册由 `[tools] <key>` 决定）。
    collect_from(
        &agent_tools::ast_tools(DefaultToolRegistry::new()),
        "可选",
        Some("ast"),
        &mut out,
    );
    collect_from(
        &agent_tools::image_tools(DefaultToolRegistry::new()),
        "可选",
        Some("image"),
        &mut out,
    );
    collect_from(
        &agent_tools::lsp_tool(DefaultToolRegistry::new()),
        "可选",
        Some("lsp"),
        &mut out,
    );
    collect_from(
        &DefaultToolRegistry::new().with(Box::new(agent_hashline::HashlineTool::new())),
        "可选",
        Some("hashline"),
        &mut out,
    );
    collect_from(
        &DefaultToolRegistry::new()
            .with(Box::new(agent_pty::RunPtyTool))
            .with(Box::new(agent_pty::ShellSessionTool::new())),
        "可选",
        Some("pty"),
        &mut out,
    );
    collect_from(
        &DefaultToolRegistry::new().with(Box::new(agent_dap::DebugTool::new(
            agent_dap::DapSettings::default(),
        ))),
        "可选",
        Some("debug"),
        &mut out,
    );
    collect_from(
        &DefaultToolRegistry::new().with(Box::new(agent_tools::SshTool::new(None))),
        "可选",
        Some("ssh"),
        &mut out,
    );
    collect_from(
        &DefaultToolRegistry::new().with(Box::new(agent_browser::BrowserTool::new())),
        "可选",
        Some("browser"),
        &mut out,
    );
    collect_from(
        &DefaultToolRegistry::new().with(Box::new(agent_tools::GithubTool::new(false))),
        "可选",
        Some("github"),
        &mut out,
    );

    // 会话 / 宿主工具（恒开或按运行时状态），构造器与三前端一致。
    let todo = agent_tools::TodoState::in_memory().shared();
    let checkpoints = agent_tools::CheckpointState::new().shared();
    let hub = agent_core::hub::Hub::new().shared();
    let memory: Arc<dyn agent_core::MemoryStore> = Arc::new(
        agent_memory::LocalMemoryStore::with_root(std::env::temp_dir().join("xtask-docs-memory")),
    );
    let session: DefaultToolRegistry = DefaultToolRegistry::new()
        .with(Box::new(agent_tools::TodoTool::new(todo)))
        .with(Box::new(agent_tools::AskUserTool::new()))
        .with(Box::new(agent_tools::CheckpointTool::new(Arc::clone(
            &checkpoints,
        ))))
        .with(Box::new(agent_tools::RewindTool::new(Arc::clone(
            &checkpoints,
        ))))
        .with(Box::new(agent_tools::SecurityScanTool::new(
            agent_tools::SecurityScanState::new().shared(),
        )))
        .with(Box::new(agent_tools::HubTool::register(hub, "main")))
        .with(Box::new(agent_tools::MemoryRecallTool::new(Arc::clone(
            &memory,
        ))))
        .with(Box::new(agent_tools::MemoryRetainTool::new(Arc::clone(
            &memory,
        ))))
        .with(Box::new(agent_tools::MemoryReflectTool::new(Arc::clone(
            &memory,
        ))))
        .with(Box::new(agent_tools::MemoryEditTool::new(Arc::clone(
            &memory,
        ))))
        .with(Box::new(agent_tools::MemoryLearnTool::new(memory)));
    collect_from(&session, "会话", None, &mut out);

    // 宿主工具的 schema 由实现给出；`task` 需要一个 provider（此处用 no-op 桩）——
    // schema/描述与 provider 无关，仅为拿到实例。
    out.push(goal_doc());
    out.push(task_doc());
    out
}

fn goal_doc() -> ToolDoc {
    let state = Arc::new(std::sync::Mutex::new(agent::GoalState::new(
        agent::GoalBudget::unlimited(),
    )));
    let tool = agent::GoalTool::new(state);
    ToolDoc {
        name: tool.name().to_string(),
        group: "宿主",
        optional_key: None,
        capability: tool.capability(),
        description: tool.description().to_string(),
        schema: tool.schema(),
    }
}

/// `task` 的 no-op provider（只为构造 TaskTool 实例，不发起任何调用）。
struct NoopProvider;

#[async_trait::async_trait]
impl agent_core::LlmProvider for NoopProvider {
    fn id(&self) -> &'static str {
        "xtask-docs-noop"
    }
    fn supports(&self) -> &[agent_core::Api] {
        &[]
    }
    async fn stream(
        &self,
        _request: agent_core::CompletionRequest,
        _ctx: &agent_core::ProviderCallContext,
    ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
        Err(agent_core::LlmError::Unsupported("noop".into()))
    }
}

fn task_doc() -> ToolDoc {
    struct EmptyRegistry;
    impl ToolRegistry for EmptyRegistry {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }
        fn get(&self, _name: &str) -> Option<Arc<dyn Tool>> {
            None
        }
    }
    let tool = agent::TaskTool::new(
        Arc::new(NoopProvider),
        Arc::new(EmptyRegistry),
        Arc::new(agent_prompt::PromptCatalog::new()),
        Arc::new(agent_core::Workspace::new(std::env::temp_dir())),
        agent_core::Model::with_defaults("docs", "docs", agent_core::Api::OpenAiCompletions),
        agent_core::ProviderCallContext::default(),
        agent_core::Mode::Code,
        3,
        0.8,
        4096,
        Arc::new(|| {
            Arc::new(agent_context::InMemoryContext::new(vec![]))
                as Arc<dyn agent_core::ContextManager>
        }),
        None,
        None,
        4,
    );
    ToolDoc {
        name: tool.name().to_string(),
        group: "宿主",
        optional_key: None,
        capability: tool.capability(),
        description: tool.description().to_string(),
        schema: tool.schema(),
    }
}

/// 能力档的文档名（与审批语义对应）。
fn capability_label(tier: CapabilityTier) -> &'static str {
    match tier {
        CapabilityTier::ReadOnly => "read_only",
        CapabilityTier::Write => "write",
        CapabilityTier::Execute => "execute",
        CapabilityTier::Network => "network",
    }
}

/// 首行摘要（概览表用）。
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > 120 {
        format!("{}…", line.chars().take(119).collect::<String>())
    } else {
        line.to_string()
    }
}

/// 转义 Markdown 表格里的竖线（描述里偶有 `a|b` 写法）。
fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|")
}

/// schema 的参数行：`(名称, 类型, 必填, 说明)`。
fn params(schema: &serde_json::Value) -> Vec<(String, String, bool, String)> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    let Some(props) = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, spec) in props {
        let ty = spec
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                spec.get("enum")
                    .and_then(serde_json::Value::as_array)
                    .map(|vals| {
                        let head = vals
                            .iter()
                            .take(3)
                            .filter_map(serde_json::Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\\|");
                        format!("enum: {head}")
                    })
            })
            .unwrap_or_else(|| "any".to_string());
        let desc = spec
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        out.push((name.clone(), ty, required.contains(&name.as_str()), desc));
    }
    out
}

/// 渲染 Markdown。
#[must_use]
pub fn render(tools: &[ToolDoc]) -> String {
    let mut core = 0usize;
    let mut optional = 0usize;
    let mut session = 0usize;
    let mut host = 0usize;
    for t in tools {
        match t.group {
            "核心" => core += 1,
            "可选" => optional += 1,
            "会话" => session += 1,
            _ => host += 1,
        }
    }
    let mut out = String::new();
    out.push_str("# 工具参考\n\n");
    out.push_str(
        "> 本文件由 `cargo xtask docs --tools` 生成（数据源：真实装配的工具实例）。\n\
         > 请勿手工编辑——工具 schema/描述变更后重跑该命令；CI 以 `--check` 校验漂移。\n\n",
    );
    out.push_str(&format!(
        "共 **{}** 个工具：核心 {} / 可选 {}（需 `[tools] <key> = true` 启用）/ 会话 {} / 宿主 {}。\n\n",
        tools.len(),
        core,
        optional,
        session,
        host
    ));

    out.push_str("## 概览\n\n");
    out.push_str("| 工具 | 分组 | 能力 | 启用 | 说明 |\n|---|---|---|---|---|\n");
    for t in tools {
        let enable = match t.optional_key {
            Some(k) => format!("`[tools] {k} = true`"),
            None => "恒开".to_string(),
        };
        out.push_str(&format!(
            "| `{}` | {} | `{}` | {} | {} |\n",
            t.name,
            t.group,
            capability_label(t.capability),
            enable,
            escape_cell(&first_line(&t.description))
        ));
    }

    out.push_str("\n## 参数详情\n");
    for t in tools {
        out.push_str(&format!("\n### `{}`\n\n", t.name));
        out.push_str(&format!("{}\n\n", t.description.trim()));
        let enable = match t.optional_key {
            Some(k) => format!("`[tools] {k} = true`"),
            None => "恒开".to_string(),
        };
        out.push_str(&format!(
            "- 分组：{} · 能力：`{}` · 启用：{}\n",
            t.group,
            capability_label(t.capability),
            enable
        ));
        let rows = params(&t.schema);
        if rows.is_empty() {
            out.push_str("- 参数：无（工具不接受参数）\n");
            continue;
        }
        out.push_str("\n| 参数 | 类型 | 必填 | 说明 |\n|---|---|---|---|\n");
        for (name, ty, required, desc) in rows {
            out.push_str(&format!(
                "| `{name}` | `{ty}` | {} | {} |\n",
                if required { "是" } else { "否" },
                escape_cell(&desc)
            ));
        }
    }
    out
}

/// 默认输出路径。
#[must_use]
pub fn default_output() -> PathBuf {
    crate::notices::workspace_root()
        .join("docs")
        .join("tools.md")
}

/// `docs` 子命令（当前仅 `--tools`）。
///
/// # Errors
/// 参数非法、写文件失败，或 `--check` 发现漂移。
pub fn run(args: &[String]) -> Result<(), String> {
    let mut check = false;
    let mut output = default_output();
    let mut tools_only = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => check = true,
            "--tools" => tools_only = true,
            "--output" => {
                output = PathBuf::from(
                    it.next()
                        .ok_or_else(|| "--output 需要一个路径参数".to_string())?,
                );
            }
            other => return Err(format!("未知参数: {other}")),
        }
    }
    if !tools_only {
        return Err("请指定要生成的文档：`cargo xtask docs --tools`".into());
    }
    let tools = collect_tools();
    let rendered = render(&tools);
    if check {
        let current = std::fs::read_to_string(&output).map_err(|e| {
            format!(
                "读取 {} 失败: {e}（先运行 `cargo xtask docs --tools`）",
                output.display()
            )
        })?;
        if current == rendered {
            println!("docs/tools.md 已是最新（{} 个工具）", tools.len());
            return Ok(());
        }
        return Err(format!(
            "{} 与当前工具面不一致——重跑 `cargo xtask docs --tools` 并提交",
            output.display()
        ));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    std::fs::write(&output, &rendered)
        .map_err(|e| format!("写入 {} 失败: {e}", output.display()))?;
    println!("已生成 {}（{} 个工具）", output.display(), tools.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_extract_type_required_and_description() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "文件路径"},
                "mode": {"enum": ["a", "b", "c", "d"]}
            },
            "required": ["path"]
        });
        let rows = params(&schema);
        assert_eq!(rows.len(), 2);
        let path = rows.iter().find(|r| r.0 == "path").unwrap();
        assert_eq!(path.1, "string");
        assert!(path.2, "path 必填");
        assert_eq!(path.3, "文件路径");
        let mode = rows.iter().find(|r| r.0 == "mode").unwrap();
        assert!(mode.1.starts_with("enum: a"), "枚举应列出取值: {}", mode.1);
        assert!(!mode.2);
    }

    #[test]
    fn params_empty_when_no_properties() {
        assert!(params(&serde_json::json!({"type": "object"})).is_empty());
        assert!(params(&serde_json::json!(null)).is_empty());
    }

    #[test]
    fn render_groups_and_escapes_table_cells() {
        let tools = vec![ToolDoc {
            name: "demo".into(),
            group: "核心",
            optional_key: None,
            capability: CapabilityTier::ReadOnly,
            description: "读 | 写".into(),
            schema: serde_json::json!({"type": "object"}),
        }];
        let md = render(&tools);
        assert!(md.starts_with("# 工具参考"));
        assert!(md.contains("共 **1** 个工具"));
        assert!(md.contains("读 \\| 写"), "表格竖线应转义: {md}");
        assert!(md.contains("### `demo`"));
        assert!(md.contains("参数：无（工具不接受参数）"));
    }
}
