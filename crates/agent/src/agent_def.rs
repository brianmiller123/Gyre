//! task agent 定义：从 Markdown 发现「命名子代理」并派生其能力面（H18）。
//!
//! 移植 oh-my-pi `task/{discovery,agents,read-only-policy}.ts` 的**可用子集**：
//!
//! - **文件**：`<cwd>/.agent/agents/*.md`（项目，最近祖先优先）与
//!   `<config_dir>/agents/*.md`（用户），frontmatter 声明
//!   `name` / `description` / `tools` / `model` / `thinking`；
//! - **内置**：`task`（通用，全工具）与 `scout`（只读探索）——零配置文件即可用；
//! - **只读判定**：`tools` 非空且全部落在 [`READ_ONLY_TOOLS`] 内 → 只读代理
//!   （对齐 omp `isReadOnlyAgent`：未知工具一律判**非**只读，fail-safe）；
//! - **能力裁剪**：装配层据 `tools` 用 [`agent_tools::FilteredRegistry`] 裁剪子 Agent
//!   注册表——只读代理拿不到写/执行工具（不是「注册后被审批拦住」）。
//!
//! 与 omp 的**已知偏差**（诚实记录）：
//! - omp 的 agent 正文（systemPrompt）**整体替换**子代理系统提示；Gyre 的基础提示词承载
//!   工具政策/委派契约等承重段落，故正文以 `<agent_definition>` 段**追加**在基础提示之后；
//! - `spawns`（嵌套派生策略）、`output`、`autoloadSkills`、`prewalk`、`advisor`、
//!   worktree 隔离尚未接线（§5.1 H18 剩余）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use agent_config::{FrontmatterValue, parse_frontmatter};

/// 内置只读工具名（对齐 omp `READ_ONLY_TOOL_NAMES`，按 Gyre 工具名映射）。
///
/// 判据是「工具自身 capability = ReadOnly」的**白名单**（而非黑名单）：未知/新增工具
/// 默认不算只读，代理因此不会被误判为可安全并行。
pub const READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "read_image",
    "grep",
    "glob",
    "list_files",
    "ast_search",
    "web_search",
    "todo",
    "ask_user",
    "checkpoint",
    "rewind",
    "memory_recall",
    "memory_reflect",
    "memory_retain",
    "memory_edit",
];

/// 定义来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    /// 项目 `.agent/agents`（优先级最高）。
    Project,
    /// 用户 `<config_dir>/agents`。
    User,
    /// 内置（`task` / `scout`）。
    Builtin,
}

impl AgentSource {
    /// 线协议/日志用名。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
            Self::Builtin => "builtin",
        }
    }
}

/// 一个命名子代理的定义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    /// 名称（`task` 工具的 `agent` 参数取值）。
    pub name: String,
    /// 一句话描述（供父级选择）。
    pub description: String,
    /// 正文（角色提示词）。
    pub system_prompt: String,
    /// 工具白名单；`None` = 全工具（不裁剪）。
    pub tools: Option<Vec<String>>,
    /// 模型别名覆盖（`None` = 继承父级）。
    pub model: Option<String>,
    /// 思考档位覆盖（`low`/`medium`/`high`；`None` = 继承父级）。
    pub thinking: Option<String>,
    /// 是否只读代理（由 `tools` 白名单派生；`tools` 为 `None` 时恒为 `false`）。
    pub read_only: bool,
    /// 来源。
    pub source: AgentSource,
    /// 定义文件路径（内置为 `None`）。
    pub file_path: Option<PathBuf>,
}

/// 定义解析错误。
#[derive(Debug, thiserror::Error)]
pub enum AgentDefError {
    /// 文件读取失败。
    #[error("读取 agent 定义失败 {path}: {source}")]
    Io {
        /// 路径。
        path: String,
        /// 底层错误。
        source: std::io::Error,
    },
    /// 定义缺少 `name`。
    #[error("agent 定义缺少 `name`（frontmatter）: {path}")]
    MissingName {
        /// 路径。
        path: String,
    },
}

/// 判定工具白名单是否只读（omp `isReadOnlyAgent` 语义：非空且全部命中白名单）。
#[must_use]
pub fn is_read_only_tools(tools: &[String]) -> bool {
    !tools.is_empty() && tools.iter().all(|t| READ_ONLY_TOOLS.contains(&t.as_str()))
}

/// 解析单个 agent 定义文件内容。
///
/// # Errors
/// 缺少 `name` 时返回 [`AgentDefError::MissingName`]。
pub fn parse_agent(
    file_path: &Path,
    content: &str,
    source: AgentSource,
) -> Result<AgentDefinition, AgentDefError> {
    let (fields, body) = parse_frontmatter(content);
    let name = fields
        .get("name")
        .and_then(FrontmatterValue::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| AgentDefError::MissingName {
            path: file_path.display().to_string(),
        })?;
    let description = fields
        .get("description")
        .and_then(FrontmatterValue::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let tools = fields
        .get("tools")
        .map(FrontmatterValue::as_list)
        .filter(|v| !v.is_empty());
    let read_only = tools.as_deref().is_some_and(is_read_only_tools);
    Ok(AgentDefinition {
        name,
        description,
        system_prompt: body.trim().to_string(),
        tools,
        model: fields
            .get("model")
            .and_then(FrontmatterValue::as_str)
            .map(|s| s.trim().trim_start_matches('@').to_string())
            .filter(|s| !s.is_empty()),
        thinking: fields
            .get("thinking")
            .or_else(|| fields.get("thinkinglevel"))
            .or_else(|| fields.get("thinking_level"))
            .and_then(FrontmatterValue::as_str)
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty()),
        read_only,
        source,
        file_path: Some(file_path.to_path_buf()),
    })
}

/// 内置定义：`task`（通用）与 `scout`（只读探索）。
#[must_use]
pub fn builtin_agents() -> Vec<AgentDefinition> {
    vec![
        AgentDefinition {
            name: "task".to_string(),
            description: "通用子代理：多步委派任务，拥有全部工具".to_string(),
            system_prompt: String::new(),
            tools: None,
            model: None,
            thinking: None,
            read_only: false,
            source: AgentSource::Builtin,
            file_path: None,
        },
        AgentDefinition {
            name: "scout".to_string(),
            description: "只读侦察子代理：只检索与阅读，绝不修改工作区".to_string(),
            system_prompt: concat!(
                "你是只读侦察子代理。目标：快速、准确地回答被指派的问题，并给出可核对的证据。\n",
                "- 只用检索/阅读类工具；**不要**尝试写文件或执行命令（你没有这些工具）。\n",
                "- 结论必须附 `file:line` 引用；找不到就明说找不到，不要猜测。\n",
                "- 输出精炼：先结论，再证据清单。"
            )
            .to_string(),
            tools: Some(READ_ONLY_TOOLS.iter().map(|s| (*s).to_string()).collect()),
            model: None,
            thinking: None,
            read_only: true,
            source: AgentSource::Builtin,
            file_path: None,
        },
    ]
}

/// 发现定义：项目（最近祖先的 `.agent/agents`）→ 用户（`<config_dir>/agents`）→ 内置，
/// 同名**先到者胜**（对齐 omp 优先级：project > user > bundled）。
#[must_use]
pub fn discover_agents(cwd: &Path) -> Vec<AgentDefinition> {
    let mut out: Vec<AgentDefinition> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    if let Some(project_dir) = nearest_project_agents_dir(cwd) {
        load_dir(&project_dir, AgentSource::Project, &mut out, &mut seen);
    }
    if let Some(config_dir) = agent_core::platform::config_dir() {
        load_dir(
            &config_dir.join("agents"),
            AgentSource::User,
            &mut out,
            &mut seen,
        );
    }
    for builtin in builtin_agents() {
        if seen.insert(builtin.name.clone()) {
            out.push(builtin);
        }
    }
    out
}

/// 自 `cwd` 向上寻找最近的 `.agent/agents` 目录。
fn nearest_project_agents_dir(cwd: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        let candidate = dir
            .join(agent_core::platform::project_config_dir_name())
            .join("agents");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if home.as_deref() == Some(dir) {
            break;
        }
        current = dir.parent();
    }
    None
}

/// 加载目录下的 `*.md`（按文件名字典序，便于复现）。
fn load_dir(
    dir: &Path,
    source: AgentSource,
    out: &mut Vec<AgentDefinition>,
    seen: &mut HashSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md") && p.is_file())
        .collect();
    files.sort();
    for path in files {
        let Ok(content) = std::fs::read_to_string(&path) else {
            tracing::warn!(path = %path.display(), "读取 agent 定义失败，跳过");
            continue;
        };
        match parse_agent(&path, &content, source) {
            Ok(def) => {
                if seen.insert(def.name.clone()) {
                    out.push(def);
                } else {
                    tracing::debug!(
                        name = %def.name,
                        path = %path.display(),
                        "同名 agent 定义已存在（高优先级来源胜），跳过"
                    );
                }
            }
            Err(e) => tracing::warn!(error = %e, "解析 agent 定义失败，跳过"),
        }
    }
}

/// 按名查找定义。
#[must_use]
pub fn find_agent<'a>(agents: &'a [AgentDefinition], name: &str) -> Option<&'a AgentDefinition> {
    agents.iter().find(|a| a.name == name)
}

/// 供父级提示词使用的清单段（名称 + 描述 + 是否只读）。
#[must_use]
pub fn render_agent_catalog(agents: &[AgentDefinition]) -> Option<String> {
    if agents.is_empty() {
        return None;
    }
    let mut out = String::from(
        "\n\n<subagents>\n可用命名子代理（`task` 工具的 `agent` 参数取值；缺省 `task`）：\n",
    );
    for a in agents {
        let tag = if a.read_only { " [只读]" } else { "" };
        out.push_str(&format!("- {}{}: {}\n", a.name, tag, a.description));
    }
    out.push_str("</subagents>\n");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_def(dir: &Path, file: &str, content: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(file);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn parses_definition_and_derives_read_only() {
        let dir = std::env::temp_dir().join(format!("agent-def-{}", std::process::id()));
        let path = write_def(
            &dir,
            "scout.md",
            "---\nname: scout\ndescription: 只读探索\ntools: [read_file, grep]\nmodel: \"@smol\"\nthinking: low\n---\n你是侦察兵。",
        );
        let def = parse_agent(
            &path,
            &std::fs::read_to_string(&path).unwrap(),
            AgentSource::Project,
        )
        .unwrap();
        assert_eq!(def.name, "scout");
        assert_eq!(def.description, "只读探索");
        assert_eq!(
            def.tools.as_deref(),
            Some(&["read_file".to_string(), "grep".to_string()][..])
        );
        assert!(def.read_only, "全为只读工具 → 只读代理");
        assert_eq!(def.model.as_deref(), Some("smol"), "`@` 前缀剥离");
        assert_eq!(def.thinking.as_deref(), Some("low"));
        assert!(def.system_prompt.contains("侦察兵"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_tool_makes_agent_not_read_only() {
        // fail-safe：含未知/写工具 → 非只读。
        assert!(!is_read_only_tools(&[
            "read_file".into(),
            "write_file".into()
        ]));
        assert!(!is_read_only_tools(&["read_file".into(), "mystery".into()]));
        assert!(
            !is_read_only_tools(&[]),
            "空白名单不算只读（= 无工具，非只读语义）"
        );
        assert!(is_read_only_tools(&["read_file".into(), "grep".into()]));
    }

    #[test]
    fn missing_name_is_rejected() {
        let dir = std::env::temp_dir().join(format!("agent-def-noname-{}", std::process::id()));
        let path = write_def(&dir, "x.md", "---\ndescription: 无名\n---\n正文");
        let err = parse_agent(
            &path,
            &std::fs::read_to_string(&path).unwrap(),
            AgentSource::User,
        )
        .unwrap_err();
        assert!(matches!(err, AgentDefError::MissingName { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_prefers_project_then_builtin_and_dedupes() {
        let root = std::env::temp_dir().join(format!("agent-def-disc-{}", std::process::id()));
        let agents_dir = root.join(".agent").join("agents");
        // 项目定义覆盖内置 `scout`，并新增 `reviewer`。
        write_def(
            &agents_dir,
            "scout.md",
            "---\nname: scout\ndescription: 项目版侦察\ntools: [grep]\n---\n项目提示",
        );
        write_def(
            &agents_dir,
            "reviewer.md",
            "---\nname: reviewer\ndescription: 代码评审\n---\n评审提示",
        );
        let inner = root.join("a").join("b");
        std::fs::create_dir_all(&inner).unwrap();
        let found = discover_agents(&inner);
        let by = |n: &str| found.iter().find(|d| d.name == n).cloned().unwrap();
        // 项目版 scout 胜出（覆盖内置）。
        assert_eq!(by("scout").description, "项目版侦察");
        assert_eq!(by("scout").source, AgentSource::Project);
        // 内置 task 仍在。
        assert_eq!(by("task").source, AgentSource::Builtin);
        assert!(by("reviewer").description.contains("评审"));
        // 无重复。
        let mut names: Vec<&str> = found.iter().map(|d| d.name.as_str()).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn catalog_lists_names_with_read_only_tag() {
        let agents = builtin_agents();
        let text = render_agent_catalog(&agents).unwrap();
        assert!(text.contains("- task:"));
        assert!(text.contains("- scout [只读]:"));
        assert!(render_agent_catalog(&[]).is_none());
    }
}
