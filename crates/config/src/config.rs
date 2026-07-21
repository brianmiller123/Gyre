//! 配置类型与分层加载。

use std::path::Path;

use agent_core::platform::{config_dir, project_config_dir_name};
use agent_core::{Api, ApprovalMode, ConfigError, Mode};
use secrecy::SecretString;
use serde::Deserialize;

/// 顶层配置。
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// 默认模型 profile。
    pub default_model: ModelProfile,
    /// 额外模型 profile（运行时 `--model <alias>` 切换）。
    #[serde(default)]
    pub models: Vec<ModelProfile>,
    /// Agent 行为配置。
    #[serde(default)]
    pub agent: AgentConfig,
    /// Web 服务配置。
    #[serde(default)]
    pub server: ServerConfig,
    /// Skill 系统配置。
    #[serde(default)]
    pub skills: SkillsConfig,
    /// MCP server 配置。
    #[serde(default)]
    pub mcp: McpConfig,
    /// 跨会话长期记忆配置。
    #[serde(default)]
    pub memory: MemoryConfig,
    /// GitHub 工具配置（启用开关与写权限）。
    #[serde(default)]
    pub github: GithubConfig,
    /// 可选工具开关（ast / lsp / image / hashline / pty）；核心工具与 github 不受此控制。
    #[serde(default)]
    pub tools: ToolsSwitchConfig,
    /// 子 Agent 配置（task 委派 + swarm 并发护栏）。
    #[serde(default)]
    pub subagent: SubagentConfig,
    /// 界面语言覆盖（en / zh / ru / ja …）；留空则自动探测系统语言（LANG/LC_*）。
    #[serde(default)]
    pub language: Option<String>,
    /// ACP（Agent Client Protocol）服务端配置。
    #[serde(default)]
    pub acp: AcpConfig,
}

impl Config {
    /// 分层加载 + 深度合并：项目 `<cwd>/.agent/config.toml` 覆盖用户级
    /// （Linux `~/.config/agent`、Windows `%APPDATA%\agent`、macOS `~/Library/Application Support/agent`）。
    ///
    /// # Errors
    /// 无任何配置文件，或解析失败时返回 [`ConfigError`]。
    pub fn load(cwd: &Path) -> Result<Self, ConfigError> {
        let candidates: Vec<std::path::PathBuf> = [
            config_dir().map(|d| d.join("config.toml")),
            Some(cwd.join(project_config_dir_name()).join("config.toml")),
        ]
        .into_iter()
        .flatten()
        .collect();

        let mut merged: Option<toml::Value> = None;
        for path in &candidates {
            if !path.exists() {
                continue;
            }
            let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                path: path.display().to_string(),
                source,
            })?;
            let value: toml::Value = toml::from_str(&text)
                .map_err(|e| ConfigError::Parse(format!("{}: {e}", path.display())))?;
            match &mut merged {
                Some(base) => merge_value(base, &value),
                None => merged = Some(value),
            }
        }

        let Some(merged) = merged else {
            let searched = candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ConfigError::Invalid(format!(
                "未找到配置文件（已查找: {searched}）"
            )));
        };

        // round-trip 通过字符串完成 toml::Value → Config（加载时一次性，开销可忽略）。
        let merged_str = toml::to_string(&merged)
            .map_err(|e| ConfigError::Parse(format!("合并配置序列化失败: {e}")))?;
        let cfg: Config = toml::from_str(&merged_str).map_err(|e| {
            ConfigError::Parse(format!(
                "合并后配置解析失败（可能为项目级与用户级配置键冲突）: {e}"
            ))
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// 语义校验。
    fn validate(&self) -> Result<(), ConfigError> {
        if self.agent.context_window_guard <= 0.0 || self.agent.context_window_guard > 1.0 {
            return Err(ConfigError::Invalid(
                "context_window_guard 必须在 (0.0, 1.0] 区间".into(),
            ));
        }
        if self.subagent.max_concurrent == 0 {
            return Err(ConfigError::Invalid(
                "subagent.max_concurrent 必须 ≥ 1".into(),
            ));
        }
        for m in std::iter::once(&self.default_model).chain(&self.models) {
            if matches!(m.max_input_tokens, Some(0)) {
                return Err(ConfigError::Invalid("max_input_tokens 必须 > 0".into()));
            }
        }
        Ok(())
    }

    /// 按 alias（或 id）解析模型 profile；`alias=None` 返回默认 profile。
    ///
    /// # Errors
    /// 指定 alias 找不到时返回 [`ConfigError::ModelNotFound`]。
    pub fn resolve_model(&self, alias: Option<&str>) -> Result<&ModelProfile, ConfigError> {
        if let Some(alias) = alias {
            return self
                .models
                .iter()
                .find(|m| m.alias.as_deref() == Some(alias) || m.id == alias)
                .or(Some(&self.default_model)
                    .filter(|m| m.alias.as_deref() == Some(alias) || m.id == alias))
                .ok_or_else(|| ConfigError::ModelNotFound(alias.into()));
        }
        Ok(&self.default_model)
    }
}

/// toml::Value 深度合并：overlay 覆盖 base 同路径字段，table 递归合并。
fn merge_value(base: &mut toml::Value, overlay: &toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base_t), toml::Value::Table(overlay_t)) => {
            for (k, v) in overlay_t {
                match base_t.get_mut(k) {
                    Some(existing) => merge_value(existing, v),
                    None => {
                        base_t.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, overlay) => *slot = overlay.clone(),
    }
}

/// 模型 profile（对应 TOML `[default_model]` / `[[models]]`）。
#[derive(Debug, Clone, Deserialize)]
pub struct ModelProfile {
    /// 模型 ID。
    pub id: String,
    /// 别名（`--model <alias>`）。
    #[serde(default)]
    pub alias: Option<String>,
    /// 线协议族，决定 Provider 路由。
    pub api: Api,
    /// base URL（自定义网关 / 本地 vLLM）。
    pub base_url: String,
    /// API key 模板（SecretString 包裹，永不进日志；可为 `${ENV}` 形式）。
    #[serde(default)]
    pub api_key: SecretString,
    /// 温度。
    #[serde(default)]
    pub temperature: Option<f32>,
    /// 最大输出 token。
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
    /// 最大输入 token（上下文窗口大小），决定上下文压缩（shake/summarize/prune）的触发时机。
    /// 未指定时回退到内置默认 `128_000`。按模型实际窗口设置（如 32k / 200k / 1M）。
    #[serde(default)]
    pub max_input_tokens: Option<usize>,
    /// 额外请求体字段（per-model）：发送 LLM 请求时合并到请求体顶层。
    ///
    /// 用于传递 Provider 特有的非标准参数，如 vLLM 的 `chat_template_kwargs`：
    /// ```toml
    /// extra_body = { chat_template_kwargs = { thinking = true } }
    /// ```
    #[serde(default)]
    pub extra_body: Option<serde_json::Value>,
}

impl ModelProfile {
    /// 展开并返回真实 API key（`${ENV}` → 环境变量值）。
    #[must_use]
    pub fn resolve_api_key(&self) -> SecretString {
        use secrecy::ExposeSecret;
        let raw = self.api_key.expose_secret();
        SecretString::from(super::env::expand_env(raw))
    }

    /// 有效上下文窗口（最大输入 token）：显式指定则用之，否则回退内置默认 `128_000`。
    #[must_use]
    pub fn effective_max_input_tokens(&self) -> usize {
        self.max_input_tokens.unwrap_or(128_000)
    }
}

/// Agent 行为配置（对应 TOML `[agent]`）。
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    /// 智能体模式。
    #[serde(default)]
    pub mode: Mode,
    /// 审批模式。
    #[serde(default)]
    pub approval_mode: ApprovalMode,
    /// 最大连续错误次数。
    #[serde(default = "default_max_mistakes")]
    pub max_mistakes: usize,
    /// 单任务最大轮次（硬上限），0 表示不限制。防止模型陷入无限工具循环。
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    /// 上下文窗口占用阈值比例（触发压缩）。
    #[serde(default = "default_guard")]
    pub context_window_guard: f32,
    /// 是否启用思考模式（reasoning/thinking），由支持思考的模型消费。
    #[serde(default)]
    pub enable_thinking: bool,
    /// 思考 token 预算（None 表示用模型默认）。
    #[serde(default)]
    pub reasoning_budget: Option<usize>,
    /// P1-K：自适应思考——每轮按用户 prompt 难度经 tiny 模型分类，动态调整思考预算
    ///（简单问题省 token/降延迟，难题深度推理）。需配合 `auto_thinking_model`。
    #[serde(default)]
    pub auto_thinking: bool,
    /// P1-K：自适应思考用的 tiny 模型 id（如 "gpt-4o-mini" / "glm-4-flash"）。
    /// 复用当前 profile 的 provider/api。`None` 时 auto_thinking 不生效（回退静态预算）。
    #[serde(default)]
    pub auto_thinking_model: Option<String>,
    /// 逐工具审批覆盖。
    #[serde(default)]
    pub tools: ToolsConfig,
    /// 命令级 allow/deny/ask 规则。
    #[serde(default)]
    pub commands: CommandRules,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Code,
            approval_mode: ApprovalMode::AlwaysAsk,
            max_mistakes: default_max_mistakes(),
            max_turns: default_max_turns(),
            context_window_guard: default_guard(),
            enable_thinking: false,
            reasoning_budget: None,
            auto_thinking: false,
            auto_thinking_model: None,
            tools: ToolsConfig::default(),
            commands: CommandRules::default(),
        }
    }
}

fn default_max_mistakes() -> usize {
    3
}
fn default_max_turns() -> usize {
    1000
}
fn default_guard() -> f32 {
    0.8
}

/// 工具相关配置。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolsConfig {
    /// 逐工具审批：`allow` / `prompt` / `deny` / `ask`。
    #[serde(default)]
    pub approval: std::collections::HashMap<String, ToolApproval>,
    /// 编辑工具配置（编辑后 LSP writethrough + SEARCH 块模糊匹配）。
    #[serde(default)]
    pub edit: EditToolsConfig,
}

/// 编辑工具配置（对应 TOML `[tools].edit`）。
///
/// 控制编辑后的 LSP writethrough（format/diagnostics）与 SEARCH 块模糊匹配。
/// 装配层据此构造 `LspWriteEffect` 并注入 `ToolContext`。
#[derive(Debug, Clone, Deserialize)]
pub struct EditToolsConfig {
    /// 模糊匹配开关：`"off"` | `"on"` | `"auto"`（`auto` 读 `PI_EDIT_FUZZY` 环境变量）。默认 `"off"`。
    #[serde(default = "default_edit_fuzzy")]
    pub fuzzy: String,
    /// 模糊匹配相似度阈值（0.0..=1.0）。默认 `0.9`。
    #[serde(default = "default_edit_fuzzy_threshold")]
    pub fuzzy_threshold: f64,
    /// 编辑后是否自动 LSP format。默认 `false`。
    #[serde(default)]
    pub format_on_write: bool,
    /// 编辑后是否回写诊断。默认 `false`（无 LSP 时不产生噪声）。
    #[serde(default)]
    pub diagnostics_on_write: bool,
    /// 是否对诊断去重（`source|message` 身份，忽略行号）。默认 `true`。
    #[serde(default = "default_true")]
    pub diagnostics_deduplicate: bool,
    /// 是否启用异步诊断合并（当前同步收集，预留）。默认 `false`。
    #[serde(default)]
    pub defer_diagnostics: bool,
}

impl Default for EditToolsConfig {
    fn default() -> Self {
        Self {
            fuzzy: default_edit_fuzzy(),
            fuzzy_threshold: default_edit_fuzzy_threshold(),
            format_on_write: false,
            diagnostics_on_write: false,
            diagnostics_deduplicate: true,
            defer_diagnostics: false,
        }
    }
}

fn default_edit_fuzzy() -> String {
    "off".into()
}

fn default_edit_fuzzy_threshold() -> f64 {
    0.9
}

/// 逐工具审批取值。
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolApproval {
    /// 放行。
    Allow,
    /// 需确认。
    Prompt,
    /// 拒绝。
    Deny,
    /// 需询问。
    Ask,
}

/// 命令规则集合。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CommandRules {
    /// 命令白名单（glob）。
    #[serde(default)]
    pub allow: Vec<CommandPattern>,
    /// 命令黑名单（glob，不可越权）。
    #[serde(default)]
    pub deny: Vec<CommandPattern>,
    /// 需询问的命令（glob）。
    #[serde(default)]
    pub ask: Vec<CommandPattern>,
}

/// 单条命令规则。
///
/// 支持两种 TOML 写法（经 `#[serde(untagged)]` 自动识别）：
/// - 简写：裸字符串，如 `allow = ["ls", "cat"]`
/// - 完整：含 `pattern` 字段的 table，如 `[[agent.commands.allow]] pattern = "cargo *"`
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CommandPattern {
    /// 简写：裸字符串（如 `"ls"`），等价于 `{ pattern = "ls" }`。
    Simple(String),
    /// 完整形式：`{ pattern = "cargo *" }`。
    Full {
        /// glob 模式（如 `git status`、`cargo *`）。
        pattern: String,
    },
}

impl CommandPattern {
    /// 获取 glob 模式字符串。
    #[must_use]
    pub fn pattern(&self) -> &str {
        match self {
            CommandPattern::Simple(s) => s,
            CommandPattern::Full { pattern } => pattern,
        }
    }
}

/// Web 服务配置（对应 TOML `[server]`）。
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// 监听地址。
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Web UI 鉴权 token（可为 `${ENV}`）。
    #[serde(default)]
    pub auth_token: Option<String>,
}

fn default_bind() -> String {
    "127.0.0.1:8080".into()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            auth_token: None,
        }
    }
}

/// Skill 系统配置（对应 TOML `[skills]`）。
#[derive(Debug, Clone, Deserialize)]
pub struct SkillsConfig {
    /// 总开关；`false` 则不发现、不注入、`skill://` 一律失败。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 注册 `/skill:<name>` slash command（远期，当前未实现）。
    #[serde(default)]
    pub enable_commands: bool,
    /// 自定义扫描目录（`~` 展开；非递归 `*/SKILL.md`）。
    #[serde(default)]
    pub custom_directories: Vec<String>,
    /// 排除 glob（作用于 skill 名）。
    #[serde(default)]
    pub ignored: Vec<String>,
    /// 包含 glob（空 = 全部）。
    #[serde(default)]
    pub included: Vec<String>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            enable_commands: false,
            custom_directories: Vec::new(),
            ignored: Vec::new(),
            included: Vec::new(),
        }
    }
}

impl SkillsConfig {
    /// 构造 skill 加载选项（展开 `~`、转 `PathBuf`）。
    #[must_use]
    pub fn to_load_options(&self) -> agent_core::SkillLoadOptions {
        agent_core::SkillLoadOptions {
            enabled: self.enabled,
            custom_directories: self
                .custom_directories
                .iter()
                .map(|s| expand_tilde_path(s))
                .collect(),
            ignored: self.ignored.clone(),
            included: self.included.clone(),
        }
    }
}

/// 跨会话长期记忆配置（对应 `[memory]`）。
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryConfig {
    /// 总开关（默认关；启用后按项目作用域跨会话积累记忆）。
    #[serde(default)]
    pub enabled: bool,
    /// 任务结束时是否触发 LLM 合并 raw notes → MEMORY.md。
    #[serde(default)]
    pub auto_consolidate: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_consolidate: true,
        }
    }
}

/// MCP server 配置集合（对应 `[mcp.servers.<name>]`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpConfig {
    /// 命名 server → stdio 启动配置。
    #[serde(default)]
    pub servers: std::collections::HashMap<String, McpServerConfig>,
}

/// 单个 MCP server 的 stdio 启动配置。
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    /// 可执行命令（如 `npx` / `node` / `uvx`）。
    pub command: String,
    /// 命令参数。
    #[serde(default)]
    pub args: Vec<String>,
    /// 额外环境变量。
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

fn default_true() -> bool {
    true
}

/// 展开 `~/...` 为 home 目录（跨平台经 `dirs`）。
fn expand_tilde_path(p: &str) -> std::path::PathBuf {
    if let Some(rest) = p.strip_prefix('~') {
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    std::path::PathBuf::from(p)
}

/// 自定义 slash 命令（来自 `.agent/commands/*.md`）。
#[derive(Debug, Clone)]
pub struct CustomCommand {
    /// 命令名（文件名去 `.md`，如 `test` → `/test`）。
    pub name: String,
    /// 描述（frontmatter `description`，缺省空）。
    pub description: String,
    /// 正文（剥离 frontmatter）。
    pub body: String,
}

/// 发现自定义 slash 命令（`.agent/commands/*.md` + 用户级 `<config_dir>/commands/*.md`）。
///
/// 返回命令表；命令名 = 文件名去 `.md`。frontmatter 解析 `description`，其余正文。
/// 缺失/读取失败静默跳过。
#[must_use]
pub fn discover_commands(cwd: &Path) -> Vec<CustomCommand> {
    let mut out = Vec::new();
    let mut scan = |dir: &Path, level: &str| {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (description, body) = split_command_frontmatter(&content);
            out.push(CustomCommand {
                name,
                description,
                body,
            });
            let _ = level;
        }
    };
    if let Some(cfg) = config_dir() {
        scan(&cfg.join("commands"), "user");
    }
    // 项目级 cwd 向上 walkup 的 .agent/commands
    let home = dirs::home_dir();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        scan(&dir.join(".agent").join("commands"), "project");
        if let Some(h) = &home {
            if dir == h.as_path() {
                break;
            }
        }
        current = dir.parent();
    }
    out
}

/// 从命令文件内容分离 frontmatter description 与正文。
fn split_command_frontmatter(content: &str) -> (String, String) {
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().map_or(false, |l| l.trim() == "---") {
        let mut desc = String::new();
        let mut idx = 1;
        while idx < lines.len() && lines[idx].trim() != "---" {
            if let Some(rest) = lines[idx].strip_prefix("description:") {
                desc = rest.trim().trim_matches('"').to_string();
            }
            idx += 1;
        }
        if idx < lines.len() {
            let body = lines[idx + 1..]
                .join("\n")
                .trim_start_matches('\n')
                .to_string();
            return (desc, body);
        }
    }
    (String::new(), content.to_string())
}

/// 发现并读取上下文约定文件（`AGENTS.md`），返回适合注入 system prompt 的段列表。
///
/// 发现位置（均按 `<name>/AGENTS.md`，注入顺序 = 优先级递增）：
/// 1. 用户级 `<config_dir>/AGENTS.md`
/// 2. 项目级：自 cwd 向上 walkup 的 `<ancestor>/.agent/AGENTS.md`（cwd 最近者排最后，覆盖更远者）
///
/// 缺失或读取失败静默跳过。移植自 oh-my-pi context-files（AGENTS.md）能力。
pub fn discover_context_files(cwd: &Path) -> Vec<String> {
    let mut out = Vec::new();
    // 用户级
    if let Some(cfg) = config_dir() {
        let p = cfg.join("AGENTS.md");
        if let Ok(content) = std::fs::read_to_string(&p) {
            out.push(format!("项目约定（用户级）:\n\n{content}"));
        }
    }
    // 项目级 walkup（cwd 在前 → 收集后反转，使 cwd 最近者排最后注入）
    let home = dirs::home_dir();
    let mut project: Vec<(std::path::PathBuf, String)> = Vec::new();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        let p = dir.join(".agent").join("AGENTS.md");
        if let Ok(content) = std::fs::read_to_string(&p) {
            project.push((p, content));
        }
        if let Some(h) = &home {
            if dir == h.as_path() {
                break;
            }
        }
        current = dir.parent();
    }
    // cwd 最近者最后注入（覆盖语义：后注入的段在 prompt 更靠后/优先）
    for (p, content) in project.into_iter().rev() {
        out.push(format!("项目约定（{}）:\n\n{content}", p.display()));
    }
    out
}

/// GitHub 工具配置（对应 TOML `[github]`）。
///
/// `enabled = true` 时装配层注册 `GithubTool` 并把使用指引注入 system prompt；
/// `allow_write = true` 另允许 create_pr/merge_pr/comment（提升至更高审批门禁）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GithubConfig {
    /// 是否启用 GitHub 工具（默认 `false`：显式开启，因涉及网络与 token）。
    #[serde(default)]
    pub enabled: bool,
    /// 是否允许写操作（create_pr/merge_pr/comment）；默认 `false`。
    #[serde(default)]
    pub allow_write: bool,
}

/// 可选工具开关配置（对应 TOML `[tools]`）。
///
/// 仅作用于「可选工具组」（ast / lsp / image / hashline / pty）：
/// - 核心工具（read_file / write_file / list_files /
///   run_command / grep / glob）始终启用，不受此控制。
/// - GitHub 工具仍由 `[github] enabled` 控制（保留其 `allow_write` 子选项）。
///
/// 语义：键为工具组 key，值为是否启用；未列出的组使用各组内置默认（默认关闭，
/// 以严格控制初始上下文长度）。`true` 启用并注入对应操作提示词，`false` 完全屏蔽。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolsSwitchConfig {
    /// 逐可选工具组启用开关：`<key> = true|false`（如 `ast = true`）。
    #[serde(default)]
    pub enabled: std::collections::HashMap<String, bool>,
}

impl ToolsSwitchConfig {
    /// 解析某可选工具组的有效启用态：配置显式指定则用之，否则回退到 `default`。
    #[must_use]
    pub fn effective(&self, key: &str, default: bool) -> bool {
        self.enabled.get(key).copied().unwrap_or(default)
    }
}

/// 子 Agent 配置（对应 TOML `[subagent]`）。
///
/// 统一管控「task 委派」与「swarm 多代理」两类子 Agent：
/// - `enabled`：总开关；`false` 则不注册 `task` 工具、`/swarm` 命令拒绝执行。
/// - `max_concurrent`：swarm 同波并发护栏（信号量许可数）；`task` 并行子任务亦受此约束。
/// - `inherit_parent`：子 Agent 是否继承父 Agent 的 temperature / thinking（默认 `true`，
///   消除「父开思考、子不思考」的隐性割裂）。
/// - `max_output_tokens`：子 Agent 独立输出 token 预算；`None` 则回退到父 profile 值。
#[derive(Debug, Clone, Deserialize)]
pub struct SubagentConfig {
    /// 是否启用子 Agent（task 委派 + swarm 编排）。
    #[serde(default = "default_subagent_enabled")]
    pub enabled: bool,
    /// 同波/并行子 Agent 的最大并发数（≥1）。
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// 是否继承父 Agent 的 temperature / thinking。
    #[serde(default = "default_true")]
    pub inherit_parent: bool,
    /// 子 Agent 独立输出 token 预算（`None` 回退到父 profile）。
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            enabled: default_subagent_enabled(),
            max_concurrent: default_max_concurrent(),
            inherit_parent: true,
            max_output_tokens: None,
        }
    }
}

impl SubagentConfig {
    /// 解析有效输出 token 预算：显式则用之，否则回退 `parent_fallback`。
    #[must_use]
    pub fn effective_max_output(&self, parent_fallback: usize) -> usize {
        self.max_output_tokens.unwrap_or(parent_fallback)
    }
}

fn default_subagent_enabled() -> bool {
    true
}

fn default_max_concurrent() -> usize {
    4
}

/// ACP（Agent Client Protocol）服务端配置（对应 TOML `[acp]`）。
///
/// 控制 ACP 标准协议端点的启用与传输模式。`--acp` CLI flag 可运行时覆盖
/// （单独使用为纯 stdio 模式；与 `--serve` 配合为 HTTP+SSE）。
#[derive(Debug, Clone, Deserialize)]
pub struct AcpConfig {
    /// 是否启用 ACP 服务端（默认 `false`；启用后 `--serve` 挂载 `/acp/*` 路由）。
    #[serde(default)]
    pub enabled: bool,
    /// 传输模式：`"http"`（HTTP+SSE）/ `"stdio"` / `"both"`。默认 `"http"`。
    #[serde(default = "default_acp_transport")]
    pub transport: String,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: default_acp_transport(),
        }
    }
}

fn default_acp_transport() -> String {
    "http".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nano() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn discovers_project_agents_md() {
        let root =
            std::env::temp_dir().join(format!("agent-cf-{}-{:#x}", std::process::id(), nano()));
        std::fs::create_dir_all(root.join(".agent")).unwrap();
        std::fs::write(
            root.join(".agent").join("AGENTS.md"),
            "use rust 2024 edition",
        )
        .unwrap();
        let files = discover_context_files(&root);
        assert!(
            files.iter().any(|f| f.contains("use rust 2024 edition")),
            "应发现项目级 AGENTS.md: {files:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_agents_md_yields_only_user_level_at_most() {
        // 无 AGENTS.md 的目录：项目级贡献为空（用户级可能存在，不阻断）
        let root = std::env::temp_dir().join(format!(
            "agent-cf-empty-{}-{:#x}",
            std::process::id(),
            nano()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let files = discover_context_files(&root);
        let project_files: Vec<_> = files.iter().filter(|f| f.contains(".agent")).collect();
        assert!(
            project_files.is_empty(),
            "无 .agent/AGENTS.md 时不应有项目级条目"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn discovers_custom_commands_with_frontmatter() {
        let root =
            std::env::temp_dir().join(format!("agent-cmd-{}-{:#x}", std::process::id(), nano()));
        std::fs::create_dir_all(root.join(".agent").join("commands")).unwrap();
        std::fs::write(
            root.join(".agent").join("commands").join("test.md"),
            "---\ndescription: 运行测试\n---\n请执行 cargo test",
        )
        .unwrap();
        std::fs::write(
            root.join(".agent").join("commands").join("review.md"),
            "审查代码",
        )
        .unwrap();
        let cmds = discover_commands(&root);
        // 不断言总数：discover_commands 会合并用户级全局 commands 目录
        // （config_dir()/commands），运行环境可能存在额外命令文件，故此处只
        // 验证本测试创建的两个命令被正确发现与解析，避免环境耦合导致的脆弱断言。
        let test_cmd = cmds.iter().find(|c| c.name == "test").unwrap();
        assert_eq!(test_cmd.description, "运行测试");
        assert!(test_cmd.body.contains("cargo test"));
        let review_cmd = cmds.iter().find(|c| c.name == "review").unwrap();
        assert_eq!(review_cmd.description, "");
        assert_eq!(review_cmd.body, "审查代码");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tools_switch_effective_falls_back_to_default() {
        let cfg = ToolsSwitchConfig::default();
        assert!(!cfg.effective("ast", false)); // 未指定 → 默认 false
        assert!(cfg.effective("lsp", true)); // 未指定 → 默认 true
    }

    #[test]
    fn tools_switch_effective_respects_explicit() {
        let mut enabled = std::collections::HashMap::new();
        enabled.insert("ast".to_string(), true);
        let cfg = ToolsSwitchConfig { enabled };
        assert!(cfg.effective("ast", false)); // 显式 true 覆盖默认
        assert!(!cfg.effective("image", false)); // 未指定 → 默认
    }

    #[test]
    fn subagent_config_defaults_and_effective_output() {
        let cfg = SubagentConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_concurrent, 4);
        assert!(cfg.inherit_parent);
        assert!(cfg.max_output_tokens.is_none());
        assert_eq!(cfg.effective_max_output(4096), 4096); // 无覆盖 → 回退父值

        let parsed: SubagentConfig = toml::from_str(
            "enabled = false\nmax_concurrent = 8\ninherit_parent = false\nmax_output_tokens = 2048\n",
        )
        .expect("parse");
        assert!(!parsed.enabled);
        assert_eq!(parsed.max_concurrent, 8);
        assert!(!parsed.inherit_parent);
        assert_eq!(parsed.effective_max_output(4096), 2048); // 显式覆盖
    }

    #[test]
    fn model_profile_parses_extra_body_inline_table() {
        let toml_src = r#"
id = "Qwen/Qwen3"
api = "openai-completions"
base_url = "http://localhost:8000/v1"
extra_body = { chat_template_kwargs = { thinking = true } }
"#;
        let parsed: ModelProfile = toml::from_str(toml_src).expect("parse");
        assert_eq!(parsed.id, "Qwen/Qwen3");
        let eb = parsed.extra_body.expect("extra_body 应存在");
        assert_eq!(eb["chat_template_kwargs"]["thinking"], true);
    }

    #[test]
    fn model_profile_extra_body_defaults_to_none() {
        let toml_src = r#"
id = "gpt-4o"
api = "openai-completions"
base_url = "https://api.openai.com/v1"
"#;
        let parsed: ModelProfile = toml::from_str(toml_src).expect("parse");
        assert!(parsed.extra_body.is_none());
    }

    /// 复现用户配置：`agent.commands` 同时含简写 allow（字符串数组）与
    /// 数组表 deny/ask，且 deny/ask 定义在 `[agent.tools.approval]` 之后。
    /// 验证 round-trip（toml::Value → to_string → Config）不会丢失或重复键。
    #[test]
    fn roundtrip_mixed_command_rules_after_tools_approval() {
        let toml_src = r#"
[default_model]
id        = "ds"
api       = "deepseek"
base_url  = "https://api.deepseek.com"

[agent]
mode = "code"

[agent.commands]
allow = ["ls", "cat", "grep", "cargo", "git"]

[agent.tools.approval]
read_file   = "allow"
run_command = "ask"

[[agent.commands.deny]]
pattern = "rm -rf *"
[[agent.commands.ask]]
pattern = "docker *"
"#;

        // 第一步：解析成 toml::Value（模拟 load 的第一段）
        let value: toml::Value = toml::from_str(toml_src).expect("第一步：解析原始 TOML 应成功");

        // 第二步：序列化回字符串（模拟 load 的 round-trip）
        let merged_str = toml::to_string(&value).expect("第二步：序列化 toml::Value 应成功");

        // 第三步：反序列化成 Config（模拟 load 的最后一步）
        let cfg: Config = toml::from_str(&merged_str).expect("第三步：反序列化成 Config 应成功");

        // 验证命令规则正确保留
        assert_eq!(cfg.agent.commands.allow.len(), 5);
        assert_eq!(cfg.agent.commands.deny.len(), 1);
        assert_eq!(cfg.agent.commands.deny[0].pattern(), "rm -rf *");
        assert_eq!(cfg.agent.commands.ask.len(), 1);
        assert_eq!(cfg.agent.commands.ask[0].pattern(), "docker *");
    }
}
