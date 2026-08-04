//! 配置类型与分层加载。

use std::path::Path;

use agent_core::platform::{config_dir, project_config_dir_name};
use agent_core::{Api, ApprovalMode, ConfigError, Mode};
use secrecy::{ExposeSecret, SecretString};
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
    /// TTSR 流规则配置（`.gyre/rules` 发现；缺省段 = 有规则即启用）。
    #[serde(default)]
    pub ttsr: TtsrConfig,
    /// goals 目标预算配置（token / 墙钟 / 硬停开关）。
    #[serde(default)]
    pub goals: GoalsConfig,
    /// eval 内核配置（Python NDJSON 内核 + 环回桥）。
    #[serde(default)]
    pub eval: EvalConfig,
    /// 压缩后端配置（`[compaction]`：summarize / snapcompact 图像化压缩）。
    #[serde(default)]
    pub compaction: CompactionConfig,
    /// SOCKS5 出站代理配置（`[socks5]`；仅影响后端出站 HTTP/HTTPS，可运行时切换）。
    #[serde(default)]
    pub socks5: Socks5Config,
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
            // P2：fallback 引用存在性 + 防环（配置错误尽早暴露，而非运行时静默跳过）。
            self.resolve_chain(m.alias.as_deref().or(Some(m.id.as_str())))?;
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
                .find_profile(alias)
                .ok_or_else(|| ConfigError::ModelNotFound(alias.into()));
        }
        Ok(&self.default_model)
    }

    /// 解析模型 fallback 链：主 profile（`alias=None` 用默认）+ 依序展开 `fallbacks` 引用。
    ///
    /// 链中每个 profile 至多出现一次；引用不存在、成环或重复均报错（配置错误应尽早暴露，
    /// 而非在运行时静默跳过）。
    ///
    /// # Errors
    /// 主模型或任一 fallback 引用找不到 → [`ConfigError::ModelNotFound`]；
    /// 引用成环/重复 → [`ConfigError::Invalid`]。
    pub fn resolve_chain(&self, alias: Option<&str>) -> Result<Vec<&ModelProfile>, ConfigError> {
        let primary = self.resolve_model(alias)?;
        let mut chain = vec![primary];
        let mut seen = std::collections::HashSet::from([primary.id.clone()]);
        self.expand_fallbacks(primary, &mut chain, &mut seen)?;
        Ok(chain)
    }

    /// 按 alias 或 id 查找 profile（含默认 profile）。
    fn find_profile(&self, alias: &str) -> Option<&ModelProfile> {
        self.models
            .iter()
            .find(|m| m.alias.as_deref() == Some(alias) || m.id == alias)
            .or(Some(&self.default_model)
                .filter(|m| m.alias.as_deref() == Some(alias) || m.id == alias))
    }

    /// 递归展开 `profile.fallbacks` 引用到 `chain`；引用已入链 → 成环/重复错误。
    fn expand_fallbacks<'a>(
        &'a self,
        profile: &'a ModelProfile,
        chain: &mut Vec<&'a ModelProfile>,
        seen: &mut std::collections::HashSet<String>,
    ) -> Result<(), ConfigError> {
        for f in &profile.fallbacks {
            let next = self
                .find_profile(f)
                .ok_or_else(|| ConfigError::ModelNotFound(f.clone()))?;
            if !seen.insert(next.id.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "模型 fallback 引用成环或重复: {f}"
                )));
            }
            chain.push(next);
            self.expand_fallbacks(next, chain, seen)?;
        }
        Ok(())
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
    /// 多 API key 轮换环：非空时每轮请求依序取一个 key（round-robin），
    /// 任一 key 被限流/撤销时自动换下一个；为空时用单个 [`Self::api_key`]。
    /// 展开 `${ENV}` 同 `api_key`。
    #[serde(default)]
    pub api_keys: Vec<SecretString>,
    /// fallback 模型链：引用同配置的其他 profile（`alias` 或 `id`），主模型请求失败且
    /// 错误可重试（网络/5xx/429/鉴权）时依序尝试备选模型。跨线协议族亦可
    /// （如 Anthropic 主 → `OpenAI` 备）。引用须存在且不成环、不重复。
    #[serde(default)]
    pub fallbacks: Vec<String>,
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

    /// 展开后的 API key 轮换环：`api_keys` 非空用多 key 环（`${ENV}` 已展开），
    /// 否则单 key 环（`api_key`）。轮换由调用方按轮次取模。
    #[must_use]
    pub fn key_ring(&self) -> Vec<String> {
        use secrecy::ExposeSecret;
        let keys: Vec<String> = self
            .api_keys
            .iter()
            .map(|k| super::env::expand_env(k.expose_secret()))
            .collect();
        if keys.is_empty() {
            vec![self.resolve_api_key().expose_secret().to_string()]
        } else {
            keys
        }
    }

    /// 转为运行时 [`Model`](agent_core::Model)（provider 标识 `openai-compatible`）。
    #[must_use]
    pub fn to_model(&self) -> agent_core::Model {
        agent_core::Model {
            id: self.id.clone(),
            provider: "openai-compatible".into(),
            api: self.api,
            max_input_tokens: self.effective_max_input_tokens(),
            max_output_tokens: self.max_output_tokens.unwrap_or(4096),
            supports_tools: true,
            supports_streaming: true,
            supports_thinking: false,
            extra_body: self.extra_body.clone(),
        }
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
    /// 命令拦截器：把 cat/grep/find/echo-redirect 等重定向到专用工具（移植 oh-my-pi bashInterceptor）。
    #[serde(default)]
    pub interceptor: InterceptorConfig,
    /// 输出最小化器：git/cargo 等长输出压缩为摘要（省 token）。
    #[serde(default)]
    pub minimizer: MinimizerConfig,
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

/// `run_command` 命令拦截器配置（对应 TOML `[agent.commands.interceptor]`）。
///
/// 开启后，`run_command` 在 spawn 前把 `cat/head/tail`、`sed -n 'A,Bp'` → `read_file`、
/// `grep/rg` → `grep`、`find/fd -name` → `glob`、`ls` → `list_files`、`echo/printf > 文件`、
/// `sed -i` → `write_file`（移植 oh-my-pi `bashInterceptor`）。
/// 默认开启。规则集由工具层内置，目标均为始终启用的核心工具，故任何装配下都安全；如需关闭
/// （例如某些工作流确实需要 `cat`），置 `enabled = false`。
#[derive(Debug, Clone, Deserialize)]
pub struct InterceptorConfig {
    /// 是否启用命令拦截（默认 `true`）。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for InterceptorConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// `run_command` 输出最小化器配置（对应 TOML `[agent.commands.minimizer]`）。
///
/// 移植 oh-my-pi `pi-shell/src/minimizer/` 的理念：对 git/cargo 等已知冗长输出的命令，
/// 把结果压缩为摘要（保留关键信息），降低模型 token 消耗。命中过滤器的输出会附带
/// 「如需完整输出请重跑该命令」提示；`max_lines` 为通用长输出兜底（超出行数的输出
/// 折叠为 head+tail，0 = 不启用通用截断，仅按命令类型过滤）。
#[derive(Debug, Clone, Deserialize)]
pub struct MinimizerConfig {
    /// 是否启用（默认 `true`）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 通用长输出折叠阈值（行数，0 = 不折叠）。默认 `0`。
    #[serde(default)]
    pub max_lines: usize,
}

impl Default for MinimizerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_lines: 0,
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

/// 记忆后端选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryBackend {
    /// 本地 markdown 管道（MEMORY.md + notes.jsonl + 心智模型 + LLM 合并；默认，P1 既有）。
    #[default]
    Local,
    /// 结构化记忆（records.jsonl + 向量融合检索 + 心智模型；无 LLM 合并，逐条积累）。
    Structured,
}

/// 跨会话长期记忆配置（对应 `[memory]`）。
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryConfig {
    /// 总开关（默认关；启用后按项目作用域跨会话积累记忆）。
    #[serde(default)]
    pub enabled: bool,
    /// 记忆后端（`local` 默认；`structured` 启用向量记忆，需 `vec-embed` feature 编译才带 L2 语义嵌入，否则 L1 投影）。
    #[serde(default)]
    pub backend: MemoryBackend,
    /// 任务结束时是否触发 LLM 合并 raw notes → MEMORY.md（仅 local 后端生效）。
    #[serde(default = "default_auto_consolidate")]
    pub auto_consolidate: bool,
}

fn default_auto_consolidate() -> bool {
    true
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: MemoryBackend::Local,
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

/// TTSR 流规则配置（对应 TOML `[ttsr]`）。
///
/// 规则文件位于 `<cwd>/.gyre/rules/*.md`（frontmatter + Markdown 正文，语法见
/// `agent-ttsr` crate 文档）。规则不写进 system prompt——命中时才注入，零上下文成本。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TtsrConfig {
    /// 总开关（`None`/缺省 = 存在规则即启用）。
    pub enabled: Option<bool>,
    /// 禁用的规则名（按 frontmatter `name` 或文件名主干）。
    pub disabled_rules: Vec<String>,
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

/// goals 目标预算配置（对应 TOML `[goals]`）。
///
/// 累计记账口径 = input + cache_write + output（cache_read 为折扣价不计入，同 oh-my-pi
/// GoalRuntime）；墙钟自 run 首次用量起计。超限后软模式在停止边界注入一条预算提醒并
/// 续跑一轮（模型自行收尾），硬模式在下一停止边界直接结束。
#[derive(Debug, Clone, Deserialize)]
pub struct GoalsConfig {
    /// token 累计预算（0 = 不限）。
    #[serde(default)]
    pub token_budget: u64,
    /// 墙钟预算（秒，0 = 不限）。
    #[serde(default)]
    pub time_budget_secs: u64,
    /// 超限后硬停（true 停止；false 注入提醒后继续）。默认 `false`。
    #[serde(default)]
    pub hard_stop: bool,
}

impl Default for GoalsConfig {
    fn default() -> Self {
        Self {
            token_budget: 0,
            time_budget_secs: 0,
            hard_stop: false,
        }
    }
}

/// eval 内核配置（对应 TOML `[eval]`）。
///
/// Python NDJSON 持久内核 + 127.0.0.1 环回桥（bearer token 按 run 注册，abort 屏蔽）。
/// 默认关闭（工具不进 LLM 工具列表、零 Token 开销）；env `GYRE_EVAL=1` 或
/// `[eval] enabled = true` 启用。
#[derive(Debug, Clone, Deserialize)]
pub struct EvalConfig {
    /// 是否注册 eval 工具（默认 `false`）。
    #[serde(default)]
    pub enabled: bool,
    /// Python 解释器路径（默认 `python3`）。
    #[serde(default = "default_eval_python")]
    pub python: String,
    /// 内核空闲超时（秒，默认 `300`）。
    #[serde(default = "default_eval_idle_timeout")]
    pub idle_timeout_secs: u64,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            python: default_eval_python(),
            idle_timeout_secs: default_eval_idle_timeout(),
        }
    }
}

fn default_eval_python() -> String {
    "python3".into()
}

fn default_eval_idle_timeout() -> u64 {
    300
}

/// 压缩后端配置（对应 TOML `[compaction]`）。
///
/// `backend`：`summarize`（本地 LLM handoff 摘要，默认，向后兼容）或
/// `snapcompact`（本地 PNG 帧渲染——确定性、无 LLM 调用，但要求视觉模型
/// 读图回放；非视觉模型请勿启用）。
///
/// `remote_endpoint`（可选）：远程摘要端点。设置后 `summarize` 后端先 POST
/// 远程生成摘要——路径以 `/chat/completions` 结尾走 OpenAI 兼容格式（覆盖
/// llama.cpp / vLLM 自托管），其余走自定义 `{systemPrompt, prompt}` 格式；
/// 远程失败（非 2xx / 超时 / 解析失败）或摘要为空时自动回退本地 LLM。
#[derive(Debug, Clone, Deserialize)]
pub struct CompactionConfig {
    /// 压缩后端：`summarize` | `snapcompact`（默认 `summarize`）。
    #[serde(default = "default_compaction_backend")]
    pub backend: String,
    /// 远程摘要端点（默认 `None` = 纯本地 LLM 摘要）。见 struct 文档的格式约定。
    #[serde(default)]
    pub remote_endpoint: Option<String>,
    /// 视为视觉模型的 model id 通配模式（`*` 通配；仅 `snapcompact` 后端生效）。
    /// 例如 `["claude-*", "gpt-4o*", "gemini-*"]`。空列表 = 任何模型都启用。
    #[serde(default)]
    pub vision_models: Vec<String>,
    /// snapcompact 帧数预算（默认 80；超出丢中间帧保首尾）。
    #[serde(default = "default_compaction_max_frames")]
    pub max_frames: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            backend: default_compaction_backend(),
            remote_endpoint: None,
            vision_models: Vec::new(),
            max_frames: default_compaction_max_frames(),
        }
    }
}

fn default_compaction_backend() -> String {
    "summarize".into()
}

fn default_compaction_max_frames() -> usize {
    80
}

/// SOCKS5 出站代理配置（对应 TOML `[socks5]`）。
///
/// 仅影响后端发出的出站 HTTP/HTTPS 请求（LLM API 等），前端浏览器自身访问不经此代理。
/// `host`/`port` 齐全才算「已配置」；配置不完整时 [`Self::is_configured`] 返回 `false`，
/// 装配层据此自动禁用代理（不阻断启动）。密码经 `SecretString` + `${ENV}` 展开存储，
/// 永不进入日志；仅配置密码而无用户名时按 `[Self::auth]` 忽略密码并告警。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Socks5Config {
    /// 总开关（默认关闭；Web 设置页可在已配置时运行时切换，选择持久化到 sidecar）。
    pub enabled: bool,
    /// 代理主机（空 = 未配置）。
    pub host: String,
    /// 代理端口（`None`/`0` = 未配置）。
    pub port: Option<u16>,
    /// 可选用户名；设置即启用 RFC 1929 用户名/密码认证。
    pub username: Option<String>,
    /// 可选密码（`SecretString`，支持 `${ENV}` 展开，永不进日志）。
    pub password: SecretString,
    /// 代理连接（含 SOCKS5 握手）超时秒数，默认 10。
    pub connect_timeout_secs: u64,
}

impl Default for Socks5Config {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: None,
            username: None,
            password: SecretString::default(),
            connect_timeout_secs: default_socks5_connect_timeout(),
        }
    }
}

fn default_socks5_connect_timeout() -> u64 {
    10
}

impl Socks5Config {
    /// 是否已配置（host 非空且 port > 0）。不完整配置视为未配置。
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.host.trim().is_empty() && self.port.is_some_and(|p| p > 0)
    }

    /// 认证信息：`username` 有值才返回（密码经 `${ENV}` 展开）；
    /// 仅配置密码而无用户名 → 记警告并忽略密码。
    #[must_use]
    pub fn auth(&self) -> Option<(String, String)> {
        let user = self.username.as_deref().unwrap_or("").trim();
        if user.is_empty() {
            if !self.password.expose_secret().is_empty() {
                tracing::warn!("SOCKS5: 配置了密码但未配置用户名，密码将被忽略");
            }
            return None;
        }
        let pass = crate::env::expand_env(self.password.expose_secret());
        Some((user.to_string(), pass))
    }

    /// 脱敏描述（日志/API 展示用）：`socks5://user:***@host:port`，**绝不出现明文密码**。
    #[must_use]
    pub fn redacted(&self) -> String {
        if !self.is_configured() {
            return "socks5://<未配置>".into();
        }
        let auth = self
            .username
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .map_or(String::new(), |u| format!("{u}:***@"));
        format!("socks5://{auth}{}:{}", self.host, self.port.unwrap_or(0))
    }
}

/// 通配匹配（`*` 匹配任意字符序列；`?` 匹配单字符）。用于视觉模型清单匹配
/// 与 skill 文件名通配等配置驱动的模式匹配。
///
/// # Panics
/// 无 panic；空 pattern 仅匹配空 text。
#[must_use]
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    fn inner(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') => {
                inner(&p[1..], t) || (!t.is_empty() && inner(p, &t[1..]))
            }
            Some('?') => !t.is_empty() && inner(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && inner(&p[1..], &t[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    inner(&p, &t)
}

/// 解析压缩后端；未知值回退 `summarize` 并记录警告。
#[must_use]
pub fn parse_compaction_backend(raw: &str) -> agent_core::CompactionBackend {
    match raw.trim() {
        "snapcompact" => agent_core::CompactionBackend::Snapcompact,
        _ => {
            if raw.trim() != "summarize" {
                tracing::warn!("未知压缩后端 '{raw}'，回退 summarize");
            }
            agent_core::CompactionBackend::Summarize
        }
    }
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

    // P2：wildcard_match（`*` / `?` 通配，视觉模型清单匹配）。
    #[test]
    fn wildcard_match_basic() {
        assert!(wildcard_match("claude-*", "claude-sonnet-4-5"));
        assert!(!wildcard_match("claude-*", "claude"), "`claude-*` 要求 `-` 前缀");
        assert!(wildcard_match("claude*", "claude"));
        assert!(!wildcard_match("claude-*", "gpt-4o"));
        assert!(wildcard_match("gpt-4o*", "gpt-4o-mini"));
        assert!(wildcard_match("gemini-*-flash", "gemini-2.0-flash"));
        assert!(wildcard_match("?pt-4o", "gpt-4o"));
        assert!(!wildcard_match("?pt-4o", "xxpt-4o"));
        assert!(wildcard_match("*", "anything"));
        assert!(!wildcard_match("", "anything"));
        assert!(wildcard_match("", ""));
        assert!(wildcard_match("a*b*c", "aXYbZc"));
        assert!(!wildcard_match("a*b*c", "aXYbZd"));
    }

    // P2：压缩后端解析。
    #[test]
    fn compaction_backend_parse() {
        assert_eq!(
            parse_compaction_backend("summarize"),
            agent_core::CompactionBackend::Summarize
        );
        assert_eq!(
            parse_compaction_backend("snapcompact"),
            agent_core::CompactionBackend::Snapcompact
        );
        assert_eq!(
            parse_compaction_backend("  snapcompact  "),
            agent_core::CompactionBackend::Snapcompact
        );
        // 未知值回退 summarize。
        assert_eq!(
            parse_compaction_backend("bogus"),
            agent_core::CompactionBackend::Summarize
        );
    }

    // P2：CompactionConfig 默认值。
    #[test]
    fn compaction_config_defaults() {
        let c = CompactionConfig::default();
        assert_eq!(c.backend, "summarize");
        assert!(c.remote_endpoint.is_none(), "默认无远程端点");
        assert!(c.vision_models.is_empty());
        assert_eq!(c.max_frames, 80);
    }

    // P2：remote_endpoint 反序列化（远程压缩模式）。
    #[test]
    fn compaction_config_remote_endpoint_from_toml() {
        let cfg: CompactionConfig = toml::from_str(
            "remote_endpoint = \"http://127.0.0.1:8080/v1/chat/completions\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg.remote_endpoint.as_deref(),
            Some("http://127.0.0.1:8080/v1/chat/completions")
        );
        // 缺省为 None（旧配置向后兼容）。
        let cfg2: CompactionConfig = toml::from_str("backend = \"summarize\"\n").unwrap();
        assert!(cfg2.remote_endpoint.is_none());
    }

    // P2：CompactionConfig TOML 反序列化（含未知键容忍）。
    #[test]
    fn compaction_config_from_toml() {
        let cfg: CompactionConfig =
            toml::from_str("backend = \"snapcompact\"\nmax_frames = 16\n").unwrap();
        assert_eq!(cfg.backend, "snapcompact");
        assert_eq!(cfg.max_frames, 16);
        assert!(cfg.vision_models.is_empty());
    }

    // P2：memory backend 反序列化与缺省（向量记忆接线）。
    #[test]
    fn memory_backend_from_toml() {
        // 缺省 local（旧配置向后兼容）。
        let cfg: MemoryConfig = toml::from_str("enabled = true\n").unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.backend, MemoryBackend::Local);
        assert!(cfg.auto_consolidate);
        // 显式 structured。
        let cfg2: MemoryConfig =
            toml::from_str("enabled = true\nbackend = \"structured\"\n").unwrap();
        assert_eq!(cfg2.backend, MemoryBackend::Structured);
        // 非法值报错（防手滑）。
        assert!(toml::from_str::<MemoryConfig>("backend = \"sqlite\"\n").is_err());
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

    #[test]
    fn interceptor_defaults_on_and_can_disable() {
        // 未显式配置时，interceptor 默认启用。
        let toml_src = r#"
[default_model]
id       = "ds"
api      = "deepseek"
base_url = "https://api.deepseek.com"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("解析应成功");
        assert!(cfg.agent.commands.interceptor.enabled, "interceptor 默认应启用");

        // 显式关闭。
        let off = format!("{toml_src}\n[agent.commands.interceptor]\nenabled = false\n");
        let cfg: Config = toml::from_str(&off).expect("解析应成功");
        assert!(!cfg.agent.commands.interceptor.enabled, "interceptor 应被关闭");
    }

    // ── P2：fallback 链 + key 轮换 ────────────────────────────────────────

    /// `fallbacks` 依序展开（含默认模型起始与跨 api 引用）。
    #[test]
    fn resolve_chain_expands_fallbacks_in_order() {
        let toml_src = r#"
[default_model]
id       = "main"
api      = "anthropic-messages"
base_url = "https://api.anthropic.com"
fallbacks = ["backup", "third"]

[[models]]
id       = "backup"
api      = "openai-completions"
base_url = "https://api.openai.com/v1"

[[models]]
id       = "third"
alias    = "t3"
api      = "deepseek"
base_url = "https://api.deepseek.com"
fallbacks = ["backup"]
"#;
        let cfg: Config = toml::from_str(toml_src).expect("解析应成功");
        // 原始配置：third 再引 backup（已在链）→ 防环报错。
        assert!(cfg.resolve_chain(None).is_err(), "重复引用应报错");
        // 去掉 third 的 fallbacks → 无环路径：main → backup → third。
        let toml_src2 = toml_src.replace(r#"fallbacks = ["backup"]"#, "");
        let cfg2: Config = toml::from_str(&toml_src2).expect("解析2");
        let chain2 = cfg2.resolve_chain(None).expect("默认链2");
        let ids2: Vec<&str> = chain2.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids2, ["main", "backup", "third"]);
        // alias 解析：resolve_chain(Some("t3")) 命中 third。
        let chain3 = cfg2.resolve_chain(Some("t3")).expect("alias 链");
        assert_eq!(chain3[0].id, "third");
    }

    /// 缺引用报错 + 环（互相引用）报错。
    #[test]
    fn resolve_chain_rejects_missing_and_cycles() {
        let missing = r#"
[default_model]
id = "main"
api = "deepseek"
base_url = "https://api.deepseek.com"
fallbacks = ["ghost"]
"#;
        let cfg: Config = toml::from_str(missing).expect("解析应成功");
        // validate 阶段即暴露（load 时校验引用存在性）。
        assert!(matches!(
            cfg.validate(),
            Err(super::ConfigError::ModelNotFound(_))
        ));
        // resolve_chain 同样报 ModelNotFound。
        assert!(matches!(
            cfg.resolve_chain(None),
            Err(super::ConfigError::ModelNotFound(_))
        ));

        let cyc = r#"
[default_model]
id = "a"
api = "deepseek"
base_url = "https://api.deepseek.com"
fallbacks = ["b"]

[[models]]
id = "b"
api = "deepseek"
base_url = "https://api.deepseek.com"
fallbacks = ["a"]
"#;
        let cfg: Config = toml::from_str(cyc).expect("解析应成功");
        assert!(matches!(
            cfg.resolve_chain(None),
            Err(super::ConfigError::Invalid(_))
        ));
    }

    /// 多 key 环优先于单 key；${ENV} 展开；to_model 正确映射。
    /// 多 key 环优先于单 key；to_model 正确映射。
    /// （`${ENV}` 展开逻辑由 env.rs 单测覆盖，此处用字面量避免 unsafe set_var。）
    #[test]
    fn key_ring_and_to_model() {
        let toml_src = r#"
[default_model]
id = "m"
api = "openai-completions"
base_url = "https://api.openai.com/v1"
api_key = "single"
api_keys = ["k1", "k2"]
"#;
        let cfg: Config = toml::from_str(toml_src).expect("解析应成功");
        let ring = cfg.default_model.key_ring();
        assert_eq!(ring, ["k1", "k2"]);

        // 无 api_keys → 单 key 环。
        let toml_src2 = toml_src.replace("api_keys = [\"k1\", \"k2\"]", "");
        let cfg2: Config = toml::from_str(&toml_src2).expect("解析2");
        assert_eq!(cfg2.default_model.key_ring(), ["single"]);

        // to_model：字段映射 + 默认输出 token。
        let m = cfg.default_model.to_model();
        assert_eq!(m.id, "m");
        assert_eq!(m.api, super::Api::OpenAiCompletions);
        assert_eq!(m.max_input_tokens, 128_000);
        assert_eq!(m.max_output_tokens, 4096);
    }

    // SOCKS5 代理配置：默认值 / 全字段 / 缺端口 / 未知字段容忍 / ${ENV} 展开 / 脱敏。
    #[test]
    fn socks5_defaults_and_boundaries() {
        // 空段 → 默认：未配置、开关关、超时 10s。
        let cfg: Config = toml::from_str("[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"https://api.deepseek.com\"").expect("无 [socks5] 段应可解析");
        assert!(!cfg.socks5.is_configured(), "空段应视为未配置");
        assert!(!cfg.socks5.enabled);
        assert_eq!(cfg.socks5.connect_timeout_secs, 10);

        // 全字段 + 未知字段容忍（serde 默认忽略未知键）。
        let src = r#"
[default_model]
id = "m"
api = "deepseek"
base_url = "https://api.deepseek.com"
[socks5]
enabled = true
host = "127.0.0.1"
port = 1080
username = "user"
password = "s3cret"
connect_timeout_secs = 3
future_field = "ignored"
"#;
        let cfg: Config = toml::from_str(src).expect("全字段 [socks5] 应可解析");
        assert!(cfg.socks5.is_configured());
        assert!(cfg.socks5.enabled);
        assert_eq!(cfg.socks5.host, "127.0.0.1");
        assert_eq!(cfg.socks5.port, Some(1080));
        assert_eq!(cfg.socks5.connect_timeout_secs, 3);
        let (user, pass) = cfg.socks5.auth().expect("有用户名应返回认证");
        assert_eq!(user, "user");
        assert_eq!(pass, "s3cret");

        // 缺端口 / 端口 0 → 未配置。
        let src = "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"\n[socks5]\nhost = \"127.0.0.1\"";
        let cfg: Config = toml::from_str(src).expect("缺端口应可解析");
        assert!(!cfg.socks5.is_configured());
        let src = "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"\n[socks5]\nhost = \"127.0.0.1\"\nport = 0";
        let cfg: Config = toml::from_str(src).expect("port=0 应可解析");
        assert!(!cfg.socks5.is_configured());

        // 空 host → 未配置。
        let src = "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"\n[socks5]\nhost = \"\"\nport = 1080";
        let cfg: Config = toml::from_str(src).expect("空 host 应可解析");
        assert!(!cfg.socks5.is_configured());
    }

    // ${ENV} 密码展开 + 仅密码无用户名 → 忽略 + 脱敏恒不含明文密码。
    // （env.rs 同款 `#[allow(unsafe_code)]` 测试惯例：edition 2024 的 env 变更需 unsafe。）
    #[test]
    #[allow(unsafe_code)]
    fn socks5_env_password_and_redaction() {
        // SAFETY: 测试专用环境变量，单线程测试函数内读写，无并发竞争。
        unsafe { std::env::set_var("GYRE_TEST_SOCKS5_PASS", "p@ss:w/rd") };

        let src = r#"
[default_model]
id = "m"
api = "deepseek"
base_url = "x"
[socks5]
host = "proxy.example.com"
port = 1080
username = "u"
password = "${GYRE_TEST_SOCKS5_PASS}"
"#;
        let cfg: Config = toml::from_str(src).expect("解析");
        let (_, pass) = cfg.socks5.auth().expect("有用户名应返回认证");
        assert_eq!(pass, "p@ss:w/rd", "ENV 模板应展开为真实密码");

        // 脱敏描述：含用户名 → `u:***@`，不含明文密码。
        let red = cfg.socks5.redacted();
        assert!(red.contains("u:***@"), "应显示脱敏用户名: {red}");
        assert!(red.contains("proxy.example.com:1080"));
        assert!(!red.contains("p@ss"), "脱敏描述不得含明文密码");

        // 仅密码无用户名 → auth() 返回 None（密码被忽略）。
        let src = "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"\n[socks5]\nhost = \"h\"\nport = 1080\npassword = \"secret\"";
        let cfg: Config = toml::from_str(src).expect("解析");
        assert!(cfg.socks5.auth().is_none(), "仅密码时应忽略密码");

        // 未配置 → redacted 明示未配置。
        let cfg: Config = toml::from_str("[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"").expect("解析");
        assert!(cfg.socks5.redacted().contains("未配置"));

        // SAFETY: 同上，仅本测试函数使用该变量。
        unsafe { std::env::remove_var("GYRE_TEST_SOCKS5_PASS") };
    }
}
