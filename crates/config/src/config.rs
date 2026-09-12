//! 配置类型与分层加载。

use std::path::Path;

use agent_core::platform::{config_dir, project_config_dir_name};
use agent_core::{Api, ApprovalMode, ConfigError, Mode};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

/// 配置分层候选路径（低 → 高优先级）：用户 `<config_dir>/config.toml` → 项目
/// `<cwd>/.agent/config.toml`。`load` / `load_raw` / `set_key` 共用，避免分层顺序漂移。
#[must_use]
pub fn config_candidates(cwd: &Path) -> Vec<std::path::PathBuf> {
    [
        config_dir().map(|d| d.join("config.toml")),
        Some(cwd.join(project_config_dir_name()).join("config.toml")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// 读取并深合并全部存在的配置层；无任何文件时返回 `Ok(None)`。
fn load_merged_value(
    candidates: &[std::path::PathBuf],
) -> Result<Option<toml::Value>, ConfigError> {
    let mut merged: Option<toml::Value> = None;
    for path in candidates {
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
    Ok(merged)
}

/// 顶层配置。
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// 默认模型 profile。
    pub default_model: ModelProfile,
    /// 额外模型 profile（运行时 `--model <alias>` 切换）。
    #[serde(default)]
    pub models: Vec<ModelProfile>,
    /// 模型角色映射（TOML `[models.roles]`：role → alias/id）。
    /// 加载管线把 `[models.roles]` 段提升为本字段（见 [`hoist_models_roles`]）；
    /// 直接顶层键 `models_roles` 亦可。内置角色约定 default/plan/quick 仅文档层。
    #[serde(default)]
    pub models_roles: Option<RolesCfg>,
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
    /// todo 循环配置（H28：eager prelude + 完成提醒续跑）。
    #[serde(default)]
    pub todo: TodoConfig,
    /// H5 键位覆盖（`[keybindings]`；键为 action wire 名如 `app.clear`，值为键位如 `f5`）。
    #[serde(default)]
    pub keybindings: KeybindingsConfig,
    /// eval 内核配置（Python NDJSON 内核 + 环回桥）。
    #[serde(default)]
    pub eval: EvalConfig,
    /// 压缩后端配置（`[compaction]`：summarize / snapcompact 图像化压缩）。
    #[serde(default)]
    pub compaction: CompactionConfig,
    /// SOCKS5 出站代理配置（`[socks5]`；仅影响后端出站 HTTP/HTTPS，可运行时切换）。
    #[serde(default)]
    pub socks5: Socks5Config,
    /// LLM 请求 User-Agent 覆盖。缺省/空字符串 = 与 OMP 对齐的默认 UA
    /// `pi/<version> (<platform> <release>; <arch>)`（见 `agent_core::platform::default_llm_user_agent`）。
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Shell 钩子规则（TOML `[[hooks]]` 数组表；装配层经 `shell_hooks_from_config` 转为 `ShellHook`）。
    #[serde(default)]
    pub hooks: Vec<HookRule>,
}

impl Config {
    /// 分层加载 + 深度合并：项目 `<cwd>/.agent/config.toml` 覆盖用户级
    /// （Linux `~/.config/agent`、Windows `%APPDATA%\agent`、macOS `~/Library/Application Support/agent`）。
    ///
    /// # Errors
    /// 无任何配置文件，或解析失败时返回 [`ConfigError`]。
    pub fn load(cwd: &Path) -> Result<Self, ConfigError> {
        let (cfg, warnings) = Self::load_with_warnings(cwd)?;
        for warning in &warnings {
            tracing::warn!(target: "agent_config", "{warning}");
        }
        Ok(cfg)
    }

    /// [`Config::load`] 的带诊断版本（H23）：额外返回合并配置里的**未知键**告警。
    ///
    /// 未知键**不改变加载语义**（serde 仍按默认行为忽略），只用于提示拼写错误。
    /// 需要传统日志的调用方用 [`Config::load`]（它会 `tracing::warn!` 每条告警）；
    /// 需要自己渲染的调用方（CLI `config check` / 启动提示）用本方法，避免重复输出。
    ///
    /// # Errors
    /// 同 [`Config::load`]。
    pub fn load_with_warnings(
        cwd: &Path,
    ) -> Result<(Self, Vec<crate::schema::ConfigWarning>), ConfigError> {
        let candidates = config_candidates(cwd);
        let Some(merged) = load_merged_value(&candidates)? else {
            let searched = candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ConfigError::Invalid(format!(
                "未找到配置文件（已查找: {searched}）"
            )));
        };
        let mut cfg = Self::from_merged(&merged)?;
        // 多来源 `mcp.json` 合并（user → project 三级，低 → 高）；TOML `[mcp.servers]`
        // 已在上面合并，故最高优先——同名条目保留 TOML 定义。disabledServers 为
        // 跨源并集拒绝名单，合并后统一剔除（warn 与说明见下）。
        let loads = crate::mcp_json::load_mcp_json_sources(cwd);
        for note in crate::mcp_json::merge_mcp_json_sources(&mut cfg.mcp, &loads) {
            tracing::warn!(target: "agent_config::mcp", "{note}");
        }
        for load in &loads {
            for warning in &load.warnings {
                tracing::warn!(target: "agent_config::mcp", "{warning}");
            }
        }
        // H23：未知键诊断（合并后统一检查；只告警，不影响加载结果）。
        let warnings = crate::schema::unknown_keys(&merged);
        Ok((cfg, warnings))
    }

    /// 读取**合并后的原始 TOML**（不做反序列化/校验）——供 `agent config get/show`
    /// 内省任意键（包括尚未被 `Config` 结构体建模的键）。
    ///
    /// # Errors
    /// 无任何配置文件或 TOML 解析失败时返回错误。
    pub fn load_raw(cwd: &Path) -> Result<toml::Value, ConfigError> {
        let candidates = config_candidates(cwd);
        match load_merged_value(&candidates)? {
            Some(v) => Ok(v),
            None => Err(ConfigError::Invalid(format!(
                "未找到配置文件（已查找: {}）",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// `agent config set <key> <value>`：把点分键写入**用户级** `config.toml`
    /// （H23），保留文件既有注释与排版（`toml_edit`）。
    ///
    /// 语义：
    /// - 值按 TOML 字面量解析（`true` / `42` / `1.5` / `["a","b"]` / `"str"`），
    ///   解析失败则按裸字符串写入（`agent.mode = code` 与 `= "code"` 等价）；
    /// - **写入后必须能通过完整加载管线**（`from_merged` + 校验），否则**回滚**原文件并报错；
    /// - 拒绝写入数组表（`[[models]]` / `[[hooks]]`）——那类结构请直接编辑文件；
    /// - 返回写入路径（供调用方展示）。
    ///
    /// # Errors
    /// 键为空、目标是数组表、写入后校验失败或 IO 失败时返回错误。
    pub fn set_key(cwd: &Path, key: &str, value: &str) -> Result<std::path::PathBuf, ConfigError> {
        let path = config_dir().map_or_else(
            || cwd.join(project_config_dir_name()).join("config.toml"),
            |d| d.join("config.toml"),
        );
        Self::set_key_at(&path, key, value)?;
        Ok(path)
    }

    /// [`Config::set_key`] 的路径注入版本（测试可指定临时文件，无需改 `HOME`）。
    ///
    /// # Errors
    /// 同 [`Config::set_key`]。
    pub fn set_key_at(path: &Path, key: &str, value: &str) -> Result<(), ConfigError> {
        let key = key.trim();
        if key.is_empty() {
            return Err(ConfigError::Invalid("键不能为空".into()));
        }
        let segments: Vec<&str> = key
            .split('.')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if segments.is_empty() {
            return Err(ConfigError::Invalid(format!("非法键: {key:?}")));
        }
        // 数组表（`[[models]]` / `[[hooks]]`）无法用点分键表达元素语义——提前给出明确
        // 提示，而不是让用户写坏文件后再被校验拒绝。
        const ARRAY_TABLE_ROOTS: &[&str] = &["models", "hooks"];
        if segments.len() > 1 && ARRAY_TABLE_ROOTS.contains(&segments[0]) {
            return Err(ConfigError::Invalid(format!(
                "无法写入 {key:?}：`{}` 是数组表（如 [[{}]]），请直接编辑配置文件",
                segments[0], segments[0]
            )));
        }
        let original = std::fs::read_to_string(path).unwrap_or_default();
        let mut doc: toml_edit::DocumentMut = original
            .parse()
            .map_err(|e| ConfigError::Parse(format!("{}: {e}", path.display())))?;

        // 逐段下行；目标必须是普通表（数组表拒绝——无法用点分键表达元素语义）。
        // 用 `(&mut Table, key)` 游标避免在循环中重复可变借用一个 `&mut Item`。
        {
            let mut table: &mut toml_edit::Table = doc.as_table_mut();
            for (i, seg) in segments.iter().enumerate() {
                let last = i + 1 == segments.len();
                if last {
                    table.insert(seg, scalar_to_item(value));
                    break;
                }
                if table
                    .get(seg)
                    .is_some_and(toml_edit::Item::is_array_of_tables)
                {
                    return Err(ConfigError::Invalid(format!(
                        "无法写入 {key:?}：`{}` 是数组表（请直接编辑配置文件）",
                        segments[..=i].join(".")
                    )));
                }
                if table.get(seg).is_none() {
                    table.insert(seg, toml_edit::Item::Table(toml_edit::Table::new()));
                }
                table = table
                    .get_mut(seg)
                    .and_then(toml_edit::Item::as_table_mut)
                    .ok_or_else(|| {
                        ConfigError::Invalid(format!(
                            "无法写入 {key:?}：`{}` 不是普通表（数组表/标量请直接编辑配置文件）",
                            segments[..=i].join(".")
                        ))
                    })?;
            }
        }

        let written = doc.to_string();
        std::fs::write(path, &written).map_err(|source| ConfigError::Write {
            path: path.display().to_string(),
            source,
        })?;
        // 校验：完整加载管线（含合并 + 反序列化 + 语义校验）。失败则回滚。
        if let Err(e) = Self::from_merged(
            &toml::from_str::<toml::Value>(&written)
                .map_err(|e| ConfigError::Parse(format!("{}: {e}", path.display())))?,
        ) {
            let _ = std::fs::write(path, &original);
            return Err(ConfigError::Invalid(format!(
                "写入后被校验拒绝，已回滚 {}: {e}",
                path.display()
            )));
        }
        Ok(())
    }

    /// 从合并后的 `toml::Value` 构建 Config：`[models.roles]` 段提升为顶层
    /// `models_roles`（[`hoist_models_roles`]）→ round-trip 反序列化 → 校验。
    /// `load` 与测试共用此管线，保证行为一致。
    fn from_merged(merged: &toml::Value) -> Result<Self, ConfigError> {
        let mut merged = merged.clone();
        hoist_models_roles(&mut merged);
        // round-trip 通过字符串完成 toml::Value → Config（加载时一次性，开销可忽略）。
        let merged_str = toml::to_string(&merged)
            .map_err(|e| ConfigError::Parse(format!("合并配置序列化失败: {e}")))?;
        let cfg: Self = toml::from_str(&merged_str).map_err(|e| {
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
        // [[hooks]]：command 非空 + timeout 区间检查（`None` 视为默认 10，无需校验）。
        for h in &self.hooks {
            if h.command.trim().is_empty() {
                return Err(ConfigError::Invalid("hooks.command 不能为空".into()));
            }
            let t = h.effective_timeout_secs();
            if !(1..=600).contains(&t) {
                return Err(ConfigError::Invalid(format!(
                    "hooks.timeout_secs 必须在 1..=600 区间（当前 {t}）"
                )));
            }
        }
        for m in std::iter::once(&self.default_model).chain(&self.models) {
            if matches!(m.max_input_tokens, Some(0)) {
                return Err(ConfigError::Invalid("max_input_tokens 必须 > 0".into()));
            }
            // P2：fallback 引用存在性 + 防环（配置错误尽早暴露，而非运行时静默跳过）。
            self.resolve_chain(m.alias.as_deref().or(Some(m.id.as_str())))?;
        }
        // [models.roles]：每个角色值必须命中既有 profile（alias 或 id，含默认）。
        // 配置错误尽早暴露，而非运行时 resolve_role 静默返回 None。
        if let Some(roles) = &self.models_roles {
            for (role, target) in &roles.roles {
                if self.find_profile(target).is_none() {
                    return Err(ConfigError::Invalid(format!(
                        "models.roles.{role} 引用的模型不存在: {target}（须为 [[models]] 的 alias/id 或默认 profile）"
                    )));
                }
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

    /// 按角色名解析模型 profile（`[models.roles]`：role → alias/id → profile）。
    ///
    /// 角色名任意（`default` / `plan` / `quick` 为内置约定，仅文档层）；未配置
    /// `[models.roles]`、角色不存在或引用未命中时返回 `None`。
    #[must_use]
    pub fn resolve_role(&self, role: &str) -> Option<&ModelProfile> {
        let target = self.models_roles.as_ref()?.roles.get(role)?;
        self.find_profile(target)
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

/// `toml::Value` 深度合并：overlay 覆盖 base 同路径字段，table 递归合并。
/// 把命令行给的标量文本解析为 TOML 项（H23）。
///
/// 顺序：`true`/`false` → 整数 → 浮点 → `[...]` 数组 → 带引号字符串 → 裸字符串。
/// 裸字符串按字符串写入（`agent.mode = code` 与 `= "code"` 等价），避免把
/// `0123` 之类的值意外变成整数。
fn scalar_to_item(raw: &str) -> toml_edit::Item {
    let v = raw.trim();
    match v {
        "true" => return toml_edit::value(true),
        "false" => return toml_edit::value(false),
        _ => {}
    }
    if let Ok(i) = v.parse::<i64>() {
        return toml_edit::value(i);
    }
    if let Ok(f) = v.parse::<f64>() {
        return toml_edit::value(f);
    }
    if v.starts_with('[') && v.ends_with(']') {
        // 宽容解析：`[a, b]` 这类裸词列表按字符串元素处理（TOML 本身要求引号，
        // 但命令行里写引号很别扭，且配置项多为字符串列表如 `tools.enabled`）。
        let inner = &v[1..v.len() - 1];
        let mut out = toml_edit::Array::new();
        for element in inner.split(',') {
            let e = element.trim();
            if e.is_empty() {
                continue;
            }
            let unquoted = e
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| e.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
            if let Some(s) = unquoted {
                out.push(s);
            } else if e == "true" || e == "false" {
                out.push(e == "true");
            } else if let Ok(i) = e.parse::<i64>() {
                out.push(i);
            } else if let Ok(f) = e.parse::<f64>() {
                out.push(f);
            } else {
                out.push(e);
            }
        }
        return toml_edit::Item::Value(toml_edit::Value::Array(out));
    }
    let unquoted = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(v);
    toml_edit::value(unquoted)
}

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

/// `[models.roles]` 提升为顶层 `models_roles`。
///
/// TOML 规范：array-of-tables 的子表归属**最后一个元素**——`[[models]]` 之后的
/// `[models.roles]` 会被解析进最后一个模型条目；而仅写 `[models.roles]`（无
/// `[[models]]`）时 `models` 本身是 `{ roles = {…} }` 表，无法反序列化为模型数组。
/// 此处把两种形态统一提升：`roles` 表移动到顶层键 `models_roles`（已存在则保留
/// 原值），`models` 恢复为纯模型数组（提升后为空则整体移除）。
fn hoist_models_roles(value: &mut toml::Value) {
    let Some(root) = value.as_table_mut() else {
        return;
    };
    let (models_emptied, roles) = match root.get_mut("models") {
        // 形态一：`[[models]]` 存在 —— roles 挂在最后一个条目内。
        Some(toml::Value::Array(entries)) => (
            false,
            entries
                .last_mut()
                .and_then(toml::Value::as_table_mut)
                .and_then(|last| last.remove("roles")),
        ),
        // 形态二：仅 `[models.roles]` —— `models` 是 `{ roles = {…} }` 表。
        Some(toml::Value::Table(models)) => {
            let roles = models.remove("roles");
            (models.is_empty(), roles)
        }
        // 无 models 键 / 非法形态：交由后续反序列化自然报错。
        _ => return,
    };
    if models_emptied {
        root.remove("models");
    }
    if let Some(roles) = roles {
        root.entry("models_roles").or_insert(roles);
    }
}

/// 模型角色映射（TOML `[models.roles]`）。
///
/// 键为任意自定义角色名（`default` / `plan` / `quick` 为内置约定，仅文档层），
/// 值为目标模型的 `alias` 或 `id`。加载时由 [`Config::load`] 经
/// [`hoist_models_roles`] 从 `[models.roles]` 段提升到顶层 `models_roles`。
#[derive(Debug, Clone, Deserialize)]
pub struct RolesCfg {
    /// role → alias / 模型 id（flatten：段内每个键即一个角色）。
    #[serde(flatten)]
    pub roles: std::collections::HashMap<String, String>,
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
    /// H26：tokenizer 家族声明（`o200k` / `cl100k` / `heuristic[:chars_per_token]`）。
    ///
    /// 缺省按模型 id 推断；非 OpenAI provider（Claude/Gemini/DeepSeek/Qwen…）建议用
    /// `heuristic:<chars/token>` 声明近似比（CJK 语料约 1.5，英文约 4）。
    #[serde(default)]
    pub tokenizer: Option<String>,
    /// 自定义请求头（H24）：`${ENV}` 会展开；同名头覆盖内置鉴权头。
    ///
    /// ```toml
    /// [default_model.headers]
    /// "HTTP-Referer" = "https://my.app"      # OpenRouter 归因
    /// "X-Api-Version" = "2024-10-01"
    /// Authorization = "Bearer ${MY_TOKEN}"   # 非标准鉴权方案
    /// ```
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// 兼容开关（H24）：网关拒绝某参数时才开启。
    #[serde(default)]
    pub quirks: agent_core::ProviderQuirks,
    /// 内置鉴权模式（H24，对齐 omp provider `auth`）：`api_key`（默认）/ `none` / `oauth`。
    ///
    /// - `none`：不发送内置鉴权头（本地 vLLM / Ollama / 免鉴权网关）；
    /// - `oauth`：只用 OAuth 凭据存储中的令牌，缺失即报错（不回退 env / auth.toml）。
    #[serde(default)]
    pub auth: agent_core::AuthMode,
    /// omp 风格 `compat` 段（H24）：与 [`ModelProfile::quirks`] 取并集后生效。
    ///
    /// 仅收录 Gyre **实际生效**的开关（未收录的 omp 旗标会被忽略，并由 H23 未知键
    /// 诊断在 `agent config check` 里点名提示）。
    #[serde(default)]
    pub compat: CompatConfig,
}

/// omp 风格 `compat` 段（H24）：字段名沿用 oh-my-pi 的 camelCase 形状。
///
/// 映射规则（与 [`agent_core::ProviderQuirks`] 取并集，显式 `quirks` 优先级更高）：
/// - `supportsUsageInStreaming = false` → `omit_stream_options`；
/// - `supportsToolChoice = false` → `omit_tool_choice`；
/// - `supportsReasoningEffort = false` / `supportsReasoningParams = false` → `omit_reasoning`；
/// - `maxTokensField` → `max_tokens_field`。
///
/// 未知键**不报错**（omp 的完整 compat 表远大于 Gyre 实现面；报错会让既有 omp 配置
/// 直接加载失败）。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CompatConfig {
    /// 流式响应是否带 usage（`false` → 省略 `stream_options`）。
    pub supports_usage_in_streaming: Option<bool>,
    /// 是否支持 `tool_choice`（`false` → 省略）。
    pub supports_tool_choice: Option<bool>,
    /// 是否支持 `reasoning_effort`（`false` → 省略推理参数）。
    pub supports_reasoning_effort: Option<bool>,
    /// 是否支持推理参数（`false` → 省略推理参数）。
    pub supports_reasoning_params: Option<bool>,
    /// `max_tokens` 的 wire 字段名。
    pub max_tokens_field: Option<agent_core::MaxTokensField>,
}

impl CompatConfig {
    /// 映射到 Gyre 的 [`agent_core::ProviderQuirks`]（未设置的开关保持默认）。
    #[must_use]
    pub fn to_quirks(self) -> agent_core::ProviderQuirks {
        agent_core::ProviderQuirks {
            omit_stream_options: self.supports_usage_in_streaming == Some(false),
            omit_tool_choice: self.supports_tool_choice == Some(false),
            omit_reasoning: self.supports_reasoning_effort == Some(false)
                || self.supports_reasoning_params == Some(false),
            max_tokens_field: self.max_tokens_field,
            ..Default::default()
        }
    }
}

impl ModelProfile {
    /// 展开并返回真实 API key（`${ENV}` → 环境变量值）。
    #[must_use]
    pub fn resolve_api_key(&self) -> SecretString {
        use secrecy::ExposeSecret;
        let raw = self.api_key.expose_secret();
        SecretString::from(super::env::expand_env(raw))
    }

    /// 有效兼容开关（H24）：`quirks` 与 omp 风格 `compat` 的并集。
    ///
    /// `omit_*` 取「任一为 true 即 true」（更保守）；`max_tokens_field` 以显式 `quirks`
    /// 为准，`quirks` 缺省时用 `compat.maxTokensField`。
    #[must_use]
    pub fn effective_quirks(&self) -> agent_core::ProviderQuirks {
        let compat = self.compat.to_quirks();
        let mut merged = self.quirks;
        merged.omit_temperature |= compat.omit_temperature;
        merged.omit_stream_options |= compat.omit_stream_options;
        merged.omit_tool_choice |= compat.omit_tool_choice;
        merged.omit_reasoning |= compat.omit_reasoning;
        merged.max_tokens_field = merged.max_tokens_field.or(compat.max_tokens_field);
        merged
    }

    /// 有效上下文窗口（最大输入 token）：显式配置 > 内置目录（H25，按模型 id 匹配）> `128_000`。
    #[must_use]
    pub fn effective_max_input_tokens(&self) -> usize {
        self.max_input_tokens
            .or_else(|| crate::model_catalog::lookup(&self.id).map(|e| e.max_input_tokens))
            .unwrap_or(128_000)
    }

    /// 有效最大输出 token：显式配置 > 内置目录（H25）> `4096`。
    #[must_use]
    pub fn effective_max_output_tokens(&self) -> usize {
        self.max_output_tokens
            .or_else(|| crate::model_catalog::lookup(&self.id).map(|e| e.max_output_tokens))
            .unwrap_or(4096)
    }

    /// 有效思考能力位（H25）：`enable_thinking` 为总开关；目录明确声明不支持的模型
    /// （如 `gpt-4o` / `deepseek-chat`）即便总开关打开也不带思考配置；未收录的模型沿用总开关。
    #[must_use]
    pub fn effective_supports_thinking(&self, enable_thinking: bool) -> bool {
        enable_thinking
            && crate::model_catalog::lookup(&self.id).is_none_or(|e| e.supports_thinking)
    }

    /// 是否存在 ChatGPT（Codex）OAuth 凭据（`openai-responses` / `codex` store key）。
    fn has_codex_oauth() -> bool {
        let Some(dir) = config_dir() else {
            return false;
        };
        let store = super::oauth_store::load(&dir);
        store.get("openai-responses").is_some() || store.get("codex").is_some()
    }

    /// 生效的 base URL（H14）：显式 `base_url` 优先；为空时按 `api` 推导默认端点。
    ///
    /// `openai-responses` 有两种部署形态，用「是否存在 ChatGPT OAuth 凭据」区分：
    /// - 有 `openai-responses`（或 `codex`）OAuth 凭据 → ChatGPT 后端
    ///   `https://chatgpt.com/backend-api/codex`（`agent auth login codex` 后可直接用）；
    /// - 否则 → 标准 `https://api.openai.com/v1`（API key 用户）。
    ///
    /// 其余 wire 保持既有行为（空串 = 由适配器回退到各自默认端点）。
    #[must_use]
    pub fn effective_base_url(&self) -> String {
        if !self.base_url.trim().is_empty() {
            return self.base_url.clone();
        }
        if self.api == Api::OpenAiResponses {
            if Self::has_codex_oauth() {
                return "https://chatgpt.com/backend-api/codex".to_string();
            }
            return "https://api.openai.com/v1".to_string();
        }
        self.base_url.clone()
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
    ///
    /// `enable_thinking` 透传为模型能力位 `supports_thinking`——统一注入全局思考开关
    /// （`[agent].enable_thinking`），避免 CLI/RPC（曾恒 false）与 server（事后覆盖）路径分叉。
    #[must_use]
    pub fn to_model(&self, enable_thinking: bool) -> agent_core::Model {
        agent_core::Model {
            id: self.id.clone(),
            provider: "openai-compatible".into(),
            api: self.api,
            max_input_tokens: self.effective_max_input_tokens(),
            max_output_tokens: self.effective_max_output_tokens(),
            supports_tools: true,
            supports_streaming: true,
            supports_thinking: self.effective_supports_thinking(enable_thinking),
            extra_body: self.extra_body.clone(),
            tokenizer: self.tokenizer.clone(),
        }
    }
}

/// Agent 行为配置（对应 TOML `[agent]`）。
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    /// 智能体模式。
    #[serde(default)]
    pub mode: Mode,
    /// H40：追加到 system prompt 末尾的定制文本（`--append-system-prompt` 或
    /// `[agent] append_system_prompt`）。值为字面文本；需要读文件时由装配层先解析。
    #[serde(default)]
    pub append_system_prompt: Option<String>,
    /// H26：自动压缩的**绝对 token 阈值**（最高优先级；超过即触发压缩级联）。
    /// 与 `context_window_guard` / `compaction_reserve_tokens` 三选一，未设为 `None`。
    #[serde(default)]
    pub compaction_threshold_tokens: Option<u64>,
    /// H26：自动压缩的**预留余量**（次优先级；阈值 = 窗口 − 余量）。
    /// 配置值 ≥ 窗口时按窗口 15% 兜底（对齐 omp「不可能默认值」回退）。
    #[serde(default)]
    pub compaction_reserve_tokens: Option<u64>,
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
    /// 复用当前 profile 的 provider/api。`None` 时 `auto_thinking` 不生效（回退静态预算）。
    #[serde(default)]
    pub auto_thinking_model: Option<String>,
    /// 逐工具审批覆盖。
    #[serde(default)]
    pub tools: ToolsConfig,
    /// 命令级 allow/deny/ask 规则。
    #[serde(default)]
    pub commands: CommandRules,
    /// H5 流式守卫（对应 TOML `[agent.stream_guards]`）。
    #[serde(default)]
    pub stream_guards: StreamGuardsConfig,
    /// H3 异步后台执行（`run_command async:true` 与后续 task 后台化；对齐 omp
    /// `async.enabled`，默认启用）。
    #[serde(default = "default_true")]
    pub async_enabled: bool,
    /// 异步作业运行中上限（对齐 omp `DEFAULT_MAX_RUNNING_JOBS`；排队态不占槽）。
    #[serde(default = "default_async_max_jobs")]
    pub async_max_jobs: usize,
}

const fn default_async_max_jobs() -> usize {
    15
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Code,
            append_system_prompt: None,
            compaction_threshold_tokens: None,
            compaction_reserve_tokens: None,
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
            stream_guards: StreamGuardsConfig::default(),
            async_enabled: default_true(),
            async_max_jobs: default_async_max_jobs(),
        }
    }
}

const fn default_max_mistakes() -> usize {
    3
}
const fn default_max_turns() -> usize {
    1000
}
const fn default_guard() -> f32 {
    0.8
}

/// H5 流式守卫配置（对应 TOML `[agent.stream_guards]`；对齐 oh-my-pi stream-guards
/// / auto-generated-guard 语义，快照 v18.1.2）。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct StreamGuardsConfig {
    /// 生成文件编辑拦截：apply_hashline 补丁命中生成文件模式即拒绝（流式中提前
    /// 中断 + 执行前兜底；对齐 omp `edit.blockAutoGenerated`，默认 true）。
    #[serde(default = "default_true")]
    pub block_auto_generated: bool,
    /// 编辑补丁结构预检：apply_hashline 流式结束即试解析，必败（解析错误）即中断
    /// 本轮并注入错误让模型修正（对齐 omp streamingAbort 的 previewPatch 预检；
    /// stale-hash 不拦截——hashline 自带快照回放恢复，默认 true）。
    #[serde(default = "default_true")]
    pub edit_parse_guard: bool,
    /// 跨轮同参工具循环守卫（对齐 omp `model.toolCallLoopGuard.enabled`，默认 true）。
    #[serde(default = "default_true")]
    pub tool_loop_guard: bool,
    /// 连续同参调用阈值（**恰好等于**时触发一次，随后 run 不再重复；对齐 omp
    /// `model.toolCallLoopGuard.threshold`，默认 5）。
    #[serde(default = "default_tool_loop_threshold")]
    pub tool_loop_threshold: usize,
    /// 豁免工具（轮询类工具天然重复；对齐 omp 默认 `["hub"]`）。
    #[serde(default = "default_tool_loop_exempt_tools")]
    pub tool_loop_exempt_tools: Vec<String>,
    /// Gemini 思考标题连跑中断（对齐 omp `model.loopGuard`；仅 gemini 系模型生效，
    /// 默认 true）。
    #[serde(default = "default_true")]
    pub gemini_header_guard: bool,
}

impl Default for StreamGuardsConfig {
    fn default() -> Self {
        Self {
            block_auto_generated: true,
            edit_parse_guard: true,
            tool_loop_guard: true,
            tool_loop_threshold: default_tool_loop_threshold(),
            tool_loop_exempt_tools: default_tool_loop_exempt_tools(),
            gemini_header_guard: true,
        }
    }
}

const fn default_tool_loop_threshold() -> usize {
    5
}
fn default_tool_loop_exempt_tools() -> Vec<String> {
    vec!["hub".to_string()]
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

const fn default_edit_fuzzy_threshold() -> f64 {
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
            Self::Simple(s) => s,
            Self::Full { pattern } => pattern,
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
    /// 自定义扫描目录（`~` 展开；非递归 `*/SKILL.md`）。
    #[serde(default)]
    pub custom_directories: Vec<String>,
    /// 排除 glob（作用于 skill 名）。
    #[serde(default)]
    pub ignored: Vec<String>,
    /// 包含 glob（空 = 全部）。
    #[serde(default)]
    pub included: Vec<String>,
    /// 跨工具 skill 发现 provider 开关。
    #[serde(default)]
    pub providers: SkillsProviders,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            custom_directories: Vec::new(),
            ignored: Vec::new(),
            included: Vec::new(),
            providers: SkillsProviders::default(),
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

    /// 转 agent-skills 装配开关（native 恒开，不经过此表）。
    #[must_use]
    pub fn to_provider_toggles(&self) -> agent_skills::ProviderToggles {
        self.providers.to_toggles()
    }
}

/// 跨工具 skill 发现 provider 开关（对应 TOML `[skills.providers]`；缺省全开）。
///
/// 路径语义对齐 oh-my-pi `discovery/{claude,codex,opencode,github}.ts`：
/// - `claude`：`~/.claude/skills`（尊重 `CLAUDE_CONFIG_DIR`）+ 项目 `.claude/skills` walkup，priority 80
/// - `codex`：`~/.codex/skills` + 项目 `.codex/skills`，priority 70
/// - `opencode`：`~/.config/opencode/skills` + 项目 `.opencode/skills`，priority 55
/// - `github`：仅项目 `.github/skills`，priority 30
///
/// native（`<config_dir>/skills` + `.agent/skills`，priority 100）恒开，不在表内；
/// 同名 skill 按 priority first-wins 去重。
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct SkillsProviders {
    /// Claude Code（`.claude/skills`）。
    #[serde(default = "default_true")]
    pub claude: bool,
    /// OpenAI Codex（`.codex/skills`）。
    #[serde(default = "default_true")]
    pub codex: bool,
    /// OpenCode（`.opencode/skills` / `~/.config/opencode/skills`）。
    #[serde(default = "default_true")]
    pub opencode: bool,
    /// GitHub Copilot（`.github/skills`）。
    #[serde(default = "default_true")]
    pub github: bool,
}

impl Default for SkillsProviders {
    fn default() -> Self {
        Self {
            claude: true,
            codex: true,
            opencode: true,
            github: true,
        }
    }
}

impl SkillsProviders {
    /// 转 agent-skills 装配开关。
    #[must_use]
    pub fn to_toggles(self) -> agent_skills::ProviderToggles {
        agent_skills::ProviderToggles {
            claude: self.claude,
            codex: self.codex,
            opencode: self.opencode,
            github: self.github,
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
    /// 循环内自动沉淀：每 N 个停止轮把本轮 assistant 输出写入记忆（0 = 关闭）。
    /// 对齐 oh-my-pi `retainEveryNTurns`；两个后端均生效（local → raw notes，structured → 记录）。
    #[serde(default = "default_auto_retain_every_n_turns")]
    pub auto_retain_every_n_turns: usize,
    /// H30：合并改在**后台任务**执行（默认 `false` = 内联等待）。开启后停止边界不再被
    /// LLM 沉淀阻塞；并发保护仍由合并租约保证（同一时刻只有一个会话真正沉淀）。
    /// 注意：进程在后台任务完成前退出可能丢弃本轮沉淀（租约 TTL 到期后可被下次接手）。
    #[serde(default)]
    pub background_consolidate: bool,
}

const fn default_auto_consolidate() -> bool {
    true
}

const fn default_auto_retain_every_n_turns() -> usize {
    4
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: MemoryBackend::Local,
            auto_consolidate: true,
            auto_retain_every_n_turns: 4,
            background_consolidate: false,
        }
    }
}

/// MCP server 配置集合（对应 `[mcp.servers.<name>]`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpConfig {
    /// 命名 server → 启动配置（stdio 或 Streamable HTTP）。
    #[serde(default)]
    pub servers: std::collections::HashMap<String, McpServerConfig>,
}

/// 单个 MCP server 配置：stdio 子进程（`command`）或 HTTP 端点（`url`；`type = "sse"`
/// 选 legacy HTTP+SSE 传输，缺省 Streamable）。
///
/// 无标签 enum：按必填键区分——含 `command` 即 stdio（向后兼容既有配置，omp 的
/// `type = "stdio"` 键同样命中），含 `url` 即 HTTP；两者皆无时反序列化失败并提示需
/// `command` 或 `url`。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    /// stdio 子进程传输（JSON-RPC 2.0 over stdin/stdout）。
    Stdio(McpStdioConfig),
    /// Streamable HTTP 传输（POST JSON-RPC + 可选 SSE 响应流）。
    Http(McpHttpConfig),
}

impl McpServerConfig {
    /// 单次请求超时毫秒数（缺省 30000，`0` = 不限制）。
    #[must_use]
    pub fn timeout_ms(&self) -> u64 {
        match self {
            Self::Stdio(c) => c.timeout_ms.unwrap_or(30_000),
            Self::Http(c) => c.timeout_ms.unwrap_or(30_000),
        }
    }
}

/// 单个 MCP server 的 stdio 启动配置。
///
/// 序列化形状（Claude / Cursor 兼容）：`{command, args, env, timeout_ms}`，
/// 空集合与 `None` 省略。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct McpStdioConfig {
    /// 可执行命令（如 `npx` / `node` / `uvx`）。
    pub command: String,
    /// 命令参数。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// 额外环境变量。
    #[serde(
        default,
        skip_serializing_if = "std::collections::HashMap::is_empty",
        serialize_with = "serialize_sorted_map"
    )]
    pub env: std::collections::HashMap<String, String>,
    /// 单次请求超时毫秒数（缺省 30000，`0` = 不限制）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// 以键序序列化 `HashMap`（`env` / `headers`）。
///
/// `HashMap` 迭代序随进程随机，直接序列化会让 `serde_json::to_string` 每次输出不同字节：
/// 工具缓存指纹（MCP 工具集指纹）会永久 miss，写回文件也无法稳定 diff。此处按键排序输出。
fn serialize_sorted_map<S>(
    map: &std::collections::HashMap<String, String>,
    ser: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap as _;

    let mut entries: Vec<(&String, &String)> = map.iter().collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut out = ser.serialize_map(Some(entries.len()))?;
    for (k, v) in entries {
        out.serialize_entry(k, v)?;
    }
    out.end()
}

/// MCP OAuth 子配置（对齐 oh-my-pi `MCPServerConfigBase.oauth`）。
///
/// 全部可选：缺省时走 RFC 9728/8414 自动发现 + RFC 7591 动态客户端注册；
/// 显式 `client_id` 供不开放 DCR 的服务商（如 Figma MCP Catalog 白名单制）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct McpOAuthConfig {
    /// 手工指定的 OAuth client id（跳过 DCR）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// 手工指定的 client secret（机密客户端）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// 授权 scope（空格分隔）；缺省用发现结果的 scope。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// 精确 redirect URI（覆盖回环默认 `http://127.0.0.1:<port>/callback`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    /// 回调端口（缺省 3000）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
    /// 回调路径（缺省 `/callback`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_path: Option<String>,
    /// 授权请求 `prompt` 参数；缺省仅 `offline_access` 时补 `consent`（OIDC 要求），`""` 强制省略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// RFC 8707 资源指示；缺省从受保护资源/AS 元数据发现，兜底 server URL。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

/// HTTP 族 MCP 传输模式（配置键 `type`，对齐 oh-my-pi `"http" | "sse"`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum McpHttpTransport {
    /// Streamable HTTP（POST JSON-RPC + 可选 SSE 响应流；MCP 2025-03-26 规范，默认）。
    #[default]
    #[serde(rename = "http")]
    Streamable,
    /// Legacy HTTP+SSE（GET 起 SSE 读流 + POST message 端点；2024-11-05 规范前的存量 server）。
    #[serde(rename = "sse")]
    Sse,
}

/// 单个 MCP server 的 Streamable HTTP 配置（参考 oh-my-pi `MCPHttpServerConfig`）。
///
/// 序列化形状（Claude / Cursor 兼容）：`{url, headers, timeout_ms, oauth, type}`，
/// `type` 始终输出（自描述：`"http"` / `"sse"`），空集合与 `None` 省略。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct McpHttpConfig {
    /// server 端点 URL（如 `http://127.0.0.1:3000/mcp`）。
    pub url: String,
    /// 额外请求头（如 `Authorization`；`Mcp-Session-Id` / `MCP-Protocol-Version` 由传输层独占）。
    #[serde(
        default,
        skip_serializing_if = "std::collections::HashMap::is_empty",
        serialize_with = "serialize_sorted_map"
    )]
    pub headers: std::collections::HashMap<String, String>,
    /// 单次请求超时毫秒数（缺省 30000，`0` = 不限制；覆盖整个 POST + SSE 响应期）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// OAuth 授权配置（Remote MCP 授权；缺省不发授权请求）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthConfig>,
    /// 传输模式：`type = "http"`（默认 Streamable）或 `type = "sse"`（legacy HTTP+SSE）。
    #[serde(rename = "type", alias = "transport", default)]
    pub transport: McpHttpTransport,
}

const fn default_true() -> bool {
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
    if lines.first().is_some_and(|l| l.trim() == "---") {
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
#[must_use]
pub fn discover_context_files(cwd: &Path) -> Vec<String> {
    // H40：发现 → `@import` 展开 → 双通道（用户级/项目级）包含去重，再渲染为注入段落。
    crate::context_files::dedupe_contained(crate::context_files::collect_context_files(cwd))
        .iter()
        .map(crate::context_files::render_context_file)
        .collect()
}

/// H40：项目/用户级 `SYSTEM.md` 定制段（项目级覆盖用户级）；无则 `None`。
#[must_use]
pub fn discover_system_prompt(cwd: &Path) -> Option<String> {
    crate::context_files::system_prompt_file(cwd).map(|f| f.content)
}

/// GitHub 工具配置（对应 TOML `[github]`）。
///
/// `enabled = true` 时装配层注册 `GithubTool` 并把使用指引注入 system prompt；
/// `allow_write = true` 另允许 `create_pr/merge_pr/comment（提升至更高审批门禁`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GithubConfig {
    /// 是否启用 GitHub 工具（默认 `false`：显式开启，因涉及网络与 token）。
    #[serde(default)]
    pub enabled: bool,
    /// `是否允许写操作（create_pr/merge_pr/comment）；默认` `false`。
    #[serde(default)]
    pub allow_write: bool,
}

/// 可选工具开关配置（对应 TOML `[tools]`）。
///
/// 仅作用于「可选工具组」（ast / lsp / image / hashline / pty）：
/// - `核心工具（read_file` / `write_file` / `list_files` /
///   `run_command` / grep / glob）始终启用，不受此控制。
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
    /// H41：子代理名册上限（0 = 不限，**不含父 agent**）。达到上限后 `task` 在
    /// **派生之前**拒绝（`at_capacity` 预拒），避免注册表无限增长后才在投递时失败。
    #[serde(default)]
    pub max_registry: usize,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            enabled: default_subagent_enabled(),
            max_concurrent: default_max_concurrent(),
            max_registry: 0,
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

const fn default_subagent_enabled() -> bool {
    true
}

const fn default_max_concurrent() -> usize {
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
    /// 内置规则集开关（H44；`None`/缺省 = 启用）。
    ///
    /// 内置集是随二进制分发的 27 条语言约定规则（Go/Rust/TypeScript，全部
    /// `interruptMode: never`：命中只折叠为工具结果提醒、不打断流）。同名用户/项目规则
    /// 覆盖内置副本；`disabled_rules` 可逐条剔除（对内置与用户规则一视同仁）。
    /// 置 `false` 则整套不加载——此时若 `.gyre/rules` 为空，TTSR 完全不启用。
    pub builtin_rules: Option<bool>,
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
/// 累计记账口径 = input + `cache_write` + `output（cache_read` 为折扣价不计入，同 oh-my-pi
/// GoalRuntime）；墙钟自 run 首次用量起计。超限后软模式在停止边界注入一条预算提醒并
/// 续跑一轮（模型自行收尾），硬模式在下一停止边界直接结束。
#[derive(Debug, Clone, Deserialize, Default)]
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

/// 键位覆盖（对应 TOML `[keybindings]`；H5）。
///
/// 键是动作 wire 名（`app.interrupt` / `app.clear` / `app.message.dequeue` 等，见
/// `agent --help` 或 REPL `/hotkeys`），值是键位说明（`ctrl-y` / `alt-enter` / `f5`）。
/// 未知动作或非法键位在启动时告警并忽略（不阻断启动）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct KeybindingsConfig {
    /// 动作名 → 键位说明。
    #[serde(flatten)]
    pub bindings: std::collections::BTreeMap<String, String>,
}

/// todo 循环配置（对应 TOML `[todo]`；H28）。
///
/// - `eager`：首轮是否注入「先规划再动手」prelude（`off` 默认 / `preferred` 仅提醒 /
///   `always` 提醒并在首轮强制 `tool_choice=todo`）。
/// - `reminders`：模型在仍有未完成待办时停止 → 注入提醒并续跑（默认开）。
/// - `reminders_max`：单轮 prompt 内提醒次数上限（0 = 不限，默认 3）。
#[derive(Debug, Clone, Deserialize)]
pub struct TodoConfig {
    /// eager prelude 模式（缺省 `off`）。
    #[serde(default)]
    pub eager: Option<String>,
    /// 完成提醒续跑开关（默认 `true`）。
    #[serde(default = "default_true")]
    pub reminders: bool,
    /// 提醒次数上限（默认 `3`；`0` = 不限）。
    #[serde(default = "default_todo_reminders_max")]
    pub reminders_max: usize,
    /// H28：中途对账 nudge（连续变更类工具调用后提醒回归清单；默认 `true`）。
    #[serde(default = "default_true")]
    pub mid_run_nudge: bool,
}

impl Default for TodoConfig {
    fn default() -> Self {
        Self {
            eager: None,
            reminders: true,
            reminders_max: default_todo_reminders_max(),
            mid_run_nudge: true,
        }
    }
}

const fn default_todo_reminders_max() -> usize {
    3
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

const fn default_eval_idle_timeout() -> u64 {
    300
}

/// 压缩后端配置（对应 TOML `[compaction]`）。
///
/// `backend`：`summarize`（本地 LLM handoff 摘要，默认，向后兼容）或
/// `snapcompact`（本地 PNG 帧渲染——确定性、无 LLM 调用，但要求视觉模型
/// 读图回放；非视觉模型请勿启用）。
///
/// `remote_endpoint`（可选）：远程摘要端点。设置后 `summarize` 后端先 POST
/// 远程生成摘要——路径以 `/chat/completions` 结尾走 `OpenAI` 兼容格式（覆盖
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

const fn default_compaction_max_frames() -> usize {
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

const fn default_socks5_connect_timeout() -> u64 {
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
            Some('*') => inner(&p[1..], t) || (!t.is_empty() && inner(p, &t[1..])),
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
    if raw.trim() == "snapcompact" {
        agent_core::CompactionBackend::Snapcompact
    } else {
        if raw.trim() != "summarize" {
            tracing::warn!("未知压缩后端 '{raw}'，回退 summarize");
        }
        agent_core::CompactionBackend::Summarize
    }
}

/// Shell 钩子超时缺省值（秒）。
const HOOK_DEFAULT_TIMEOUT_SECS: u64 = 10;

/// 钩子事件类型（TOML 小写蛇形字符串；H20 扩展到与
/// [`agent_core::HOOK_EVENT_NAMES`] 同一集合）。
///
/// 语义分两类：
/// - **可决策**：`tool_call`（旧名 `before_tool`）——stdout 首行 JSON 的 `decision` 生效；
/// - **通知型**：其余全部——触发命令但不解析决定（输出仅记日志）。
///
/// 兼容：旧配置里的 `before_tool` / `after_tool` 仍接受（别名映射到 `tool_call` /
/// `tool_result`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEventKind {
    /// 会话开始（装配完成后、首个任务前）。
    SessionStart,
    /// 会话关闭（进程退出 / 会话销毁）。
    SessionShutdown,
    /// 首个轮次之前（可观察初始提示词）。
    BeforeAgentStart,
    /// 智能体开始运行一轮任务。
    AgentStart,
    /// 智能体结束（成功/失败/取消）。
    AgentEnd,
    /// 轮次开始。
    TurnStart,
    /// 轮次结束（携带 assistant 消息与工具结果）。
    TurnEnd,
    /// 工具执行前（可决策；旧名 `before_tool` 为其别名）。
    #[serde(alias = "before_tool")]
    ToolCall,
    /// 工具执行后（旧名 `after_tool` 为其别名）。
    #[serde(alias = "after_tool")]
    ToolResult,
    /// 任务结束（通知型）。
    Stop,
    /// 自动压缩开始。
    AutoCompactionStart,
    /// 自动压缩结束。
    AutoCompactionEnd,
    /// 自动重试开始。
    AutoRetryStart,
    /// 自动重试结束。
    AutoRetryEnd,
    /// 模型回退已应用。
    RetryFallbackApplied,
    /// 模型回退成功。
    RetryFallbackSucceeded,
    /// TTSR 流规则命中。
    TtsrTriggered,
}

impl HookEventKind {
    /// 线协议事件名（与 [`agent_core::HOOK_EVENT_NAMES`] 一致；用于匹配与 stdin `event` 字段）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionShutdown => "session_shutdown",
            Self::BeforeAgentStart => "before_agent_start",
            Self::AgentStart => "agent_start",
            Self::AgentEnd => "agent_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd => "turn_end",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::Stop => "stop",
            Self::AutoCompactionStart => "auto_compaction_start",
            Self::AutoCompactionEnd => "auto_compaction_end",
            Self::AutoRetryStart => "auto_retry_start",
            Self::AutoRetryEnd => "auto_retry_end",
            Self::RetryFallbackApplied => "retry_fallback_applied",
            Self::RetryFallbackSucceeded => "retry_fallback_succeeded",
            Self::TtsrTriggered => "ttsr_triggered",
        }
    }

    /// 是否为可决策事件（stdout 首行 JSON 的 `decision` 生效）。
    #[must_use]
    pub const fn is_decidable(self) -> bool {
        matches!(self, Self::ToolCall)
    }
}

/// 单条 shell 钩子规则（对应 TOML `[[hooks]]` 数组表）。
///
/// ```toml
/// [[hooks]]
/// event = "tool_call"              # 见 HookEventKind（旧名 before_tool/after_tool 仍接受）
/// tool = "shell"                   # 可选；缺省匹配所有工具（stop 忽略此项）
/// command = "/usr/local/bin/gate"  # `sh -c`（Windows `cmd /C`）执行
/// timeout_secs = 5                 # 可选；1..=600，缺省 10
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct HookRule {
    /// 触发事件。
    pub event: HookEventKind,
    /// 可选工具名过滤；`None` = 匹配所有工具。
    pub tool: Option<String>,
    /// shell 命令；stdin 收到事件 JSON，stdout 首行 JSON 决定拦截与否（仅 `before_tool` 解析）。
    pub command: String,
    /// 超时秒数（1..=600；缺省 10）。超时 kill 子进程并按放行处理。
    pub timeout_secs: Option<u64>,
}

impl HookRule {
    /// 生效超时秒数：未配置时取 [`HOOK_DEFAULT_TIMEOUT_SECS`]。
    #[must_use]
    pub fn effective_timeout_secs(&self) -> u64 {
        self.timeout_secs.unwrap_or(HOOK_DEFAULT_TIMEOUT_SECS)
    }

    /// 工具名过滤器是否命中（`None` 过滤器匹配一切）。
    #[must_use]
    pub fn matches_tool(&self, tool: &str) -> bool {
        self.tool.as_deref().is_none_or(|t| t == tool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// H14：`openai-responses` 的默认端点推导（显式值优先；无凭据走标准端点）。
    #[test]
    fn effective_base_url_prefers_explicit_then_api_default() {
        let mut profile = ModelProfile {
            id: "gpt-5".into(),
            alias: None,
            api: Api::OpenAiResponses,
            base_url: String::new(),
            api_key: secrecy::SecretString::from("k"),
            api_keys: Vec::new(),
            fallbacks: Vec::new(),
            temperature: None,
            max_output_tokens: None,
            max_input_tokens: None,
            extra_body: None,
            tokenizer: None,
            headers: std::collections::BTreeMap::new(),
            quirks: agent_core::ProviderQuirks::default(),
            auth: agent_core::AuthMode::ApiKey,
            compat: CompatConfig::default(),
        };
        // 无 OAuth 凭据（测试环境）→ 标准 Responses 端点。
        assert_eq!(profile.effective_base_url(), "https://api.openai.com/v1");
        // 显式配置始终优先。
        profile.base_url = "https://gateway.example/v1".into();
        assert_eq!(profile.effective_base_url(), "https://gateway.example/v1");
        // 其它 wire 保持空串（由适配器回退各自默认）。
        profile.base_url = String::new();
        profile.api = Api::OpenAiCompletions;
        assert_eq!(profile.effective_base_url(), "");
    }

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
        assert!(
            !wildcard_match("claude-*", "claude"),
            "`claude-*` 要求 `-` 前缀"
        );
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
        let cfg: CompactionConfig =
            toml::from_str("remote_endpoint = \"http://127.0.0.1:8080/v1/chat/completions\"\n")
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
    /// 验证 `round-trip（toml::Value` → `to_string` → Config）不会丢失或重复键。
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
        assert!(
            cfg.agent.commands.interceptor.enabled,
            "interceptor 默认应启用"
        );

        // 显式关闭。
        let off = format!("{toml_src}\n[agent.commands.interceptor]\nenabled = false\n");
        let cfg: Config = toml::from_str(&off).expect("解析应成功");
        assert!(
            !cfg.agent.commands.interceptor.enabled,
            "interceptor 应被关闭"
        );
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

    /// 多 key 环优先于单 key；${ENV} `展开；to_model` 正确映射。
    /// 多 key 环优先于单 `key；to_model` 正确映射。
    /// （`${ENV}` 展开逻辑由 env.rs 单测覆盖，此处用字面量避免 unsafe `set_var`。）
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

        // to_model：字段映射 + 默认输出 token + 思考能力位透传。
        let m = cfg.default_model.to_model(false);
        assert_eq!(m.id, "m");
        assert_eq!(m.api, super::Api::OpenAiCompletions);
        assert_eq!(m.max_input_tokens, 128_000);
        assert_eq!(m.max_output_tokens, 4096);
        assert!(!m.supports_thinking);
        assert!(cfg.default_model.to_model(true).supports_thinking);
    }

    /// H26：`tokenizer` 声明透传到运行时 `Model`（计数家族由上下文层消费）。
    #[test]
    fn tokenizer_declaration_reaches_runtime_model() {
        let cfg: Config = toml::from_str(
            "[default_model]\nid = \"deepseek-chat\"\napi = \"deepseek\"\nbase_url = \"https://api.deepseek.com\"\ntokenizer = \"heuristic:1.5\"\n",
        )
        .expect("解析应成功");
        let m = cfg.default_model.to_model(false);
        assert_eq!(m.tokenizer.as_deref(), Some("heuristic:1.5"));
        // 未声明 → None（上下文层按 id 推断，行为与引入前一致）。
        let cfg: Config = toml::from_str(
            "[default_model]\nid = \"gpt-4o\"\napi = \"openai-completions\"\nbase_url = \"https://api.openai.com/v1\"\n",
        )
        .expect("解析应成功");
        assert_eq!(cfg.default_model.to_model(false).tokenizer, None);
    }

    /// H25：内置目录只做补缺——显式配置优先、未知模型行为不变、目录命中的模型拿到正确默认。
    #[test]
    fn model_catalog_fills_unknown_defaults_only() {
        let parse = |toml_src: &str| -> ModelProfile {
            let cfg: Config = toml::from_str(toml_src).expect("解析应成功");
            cfg.default_model
        };
        // 目录命中：claude-sonnet-4 的 200k 窗口 / 64k 输出 / 支持思考。
        let p = parse(
            "[default_model]\nid = \"claude-sonnet-4-20250514\"\napi = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\n",
        );
        assert_eq!(p.effective_max_input_tokens(), 200_000);
        assert_eq!(p.effective_max_output_tokens(), 64_000);
        assert!(p.effective_supports_thinking(true));
        // 目录声明不支持思考：gpt-4o-mini 即便总开关打开也不带思考。
        let p = parse(
            "[default_model]\nid = \"gpt-4o-mini\"\napi = \"openai-completions\"\nbase_url = \"https://api.openai.com/v1\"\n",
        );
        assert_eq!(p.effective_max_input_tokens(), 128_000);
        assert_eq!(p.effective_max_output_tokens(), 16_384);
        assert!(!p.effective_supports_thinking(true), "gpt-4o 系列不带思考");
        assert!(!p.to_model(true).supports_thinking);
        // 显式配置覆盖目录。
        let p = parse(
            "[default_model]\nid = \"gpt-4o-mini\"\napi = \"openai-completions\"\nbase_url = \"https://x\"\nmax_input_tokens = 32000\nmax_output_tokens = 2048\n",
        );
        assert_eq!(p.effective_max_input_tokens(), 32_000);
        assert_eq!(p.effective_max_output_tokens(), 2_048);
        // 未知模型：完全沿用引入目录前的兜底（128k / 4096 / 总开关透传）。
        let p = parse(
            "[default_model]\nid = \"my-local-model\"\napi = \"openai-completions\"\nbase_url = \"http://localhost:8000/v1\"\n",
        );
        assert_eq!(p.effective_max_input_tokens(), 128_000);
        assert_eq!(p.effective_max_output_tokens(), 4_096);
        assert!(p.effective_supports_thinking(true));
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
        let cfg: Config =
            toml::from_str("[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"")
                .expect("解析");
        assert!(cfg.socks5.redacted().contains("未配置"));

        // SAFETY: 同上，仅本测试函数使用该变量。
        unsafe { std::env::remove_var("GYRE_TEST_SOCKS5_PASS") };
    }
    // MCP server 配置：无标签 enum 双形态（stdio 向后兼容 / Streamable HTTP）。
    #[test]
    fn mcp_server_config_stdio_and_http_from_toml() {
        let src = r#"
[default_model]
id = "m"
api = "deepseek"
base_url = "https://api.deepseek.com"
[mcp.servers.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
[mcp.servers.fs.env]
FOO = "bar"
[mcp.servers.remote]
url = "http://127.0.0.1:3000/mcp"
timeout_ms = 5000
[mcp.servers.remote.headers]
Authorization = "Bearer tok"
"#;
        let cfg: Config = toml::from_str(src).expect("解析应成功");
        let servers = &cfg.mcp.servers;
        assert_eq!(servers.len(), 2);

        // stdio：必填 command 命中 Stdio 变体（无 `type` 键的既有配置保持兼容）。
        let McpServerConfig::Stdio(stdio) = &servers["fs"] else {
            panic!("含 command 应解析为 Stdio");
        };
        assert_eq!(stdio.command, "npx");
        assert_eq!(stdio.args.len(), 3);
        assert_eq!(stdio.env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(servers["fs"].timeout_ms(), 30_000, "缺省超时 30s");

        // http：必填 url 命中 Http 变体。
        let McpServerConfig::Http(http) = &servers["remote"] else {
            panic!("含 url 应解析为 Http");
        };
        assert_eq!(http.url, "http://127.0.0.1:3000/mcp");
        assert_eq!(
            http.headers.get("Authorization").map(String::as_str),
            Some("Bearer tok")
        );
        assert_eq!(servers["remote"].timeout_ms(), 5_000);
    }
    #[test]
    fn mcp_server_config_sse_transport_type_key() {
        // `type = "sse"` 选 legacy 传输；`type = "http"` / 缺省为 Streamable；omp stdio 配置兼容。
        let src = r#"
[default_model]
id = "m"
api = "deepseek"
base_url = "https://api.deepseek.com"
[mcp.servers.legacy]
type = "sse"
url = "http://127.0.0.1:3000/sse"
[mcp.servers.streamable]
type = "http"
url = "http://127.0.0.1:3000/mcp"
[mcp.servers.stdio_omp]
type = "stdio"
command = "npx"
"#;
        let cfg: Config = toml::from_str(src).expect("解析应成功");
        let servers = &cfg.mcp.servers;
        let McpServerConfig::Http(legacy) = &servers["legacy"] else {
            panic!("type = \"sse\" 应解析为 Http 变体");
        };
        assert_eq!(legacy.transport, McpHttpTransport::Sse);
        let McpServerConfig::Http(streamable) = &servers["streamable"] else {
            panic!("type = \"http\" 应解析为 Http 变体");
        };
        assert_eq!(streamable.transport, McpHttpTransport::Streamable);
        let McpServerConfig::Stdio(stdio_omp) = &servers["stdio_omp"] else {
            panic!("omp 风格 type = \"stdio\" + command 应解析为 Stdio 变体");
        };
        assert_eq!(stdio_omp.command, "npx");
    }

    /// H24：`auth` 与 omp 风格 `compat` 的解析与映射。
    #[test]
    fn model_profile_auth_and_compat_mapping() {
        let base = "id = \"m\"\napi = \"openai-completions\"\nbase_url = \"x\"\n";
        // 默认：api_key + 无 compat。
        let cfg: Config = toml::from_str(&format!("[default_model]\n{base}")).unwrap();
        assert_eq!(cfg.default_model.auth, agent_core::AuthMode::ApiKey);
        assert!(cfg.default_model.effective_quirks().is_empty());

        // omp 形状 compat：false → omit_*；maxTokensField → max_tokens_field。
        let cfg: Config = toml::from_str(&format!(
            "[default_model]\n{base}auth = \"none\"\n[default_model.compat]\n\
             supportsUsageInStreaming = false\nsupportsToolChoice = false\n\
             supportsReasoningParams = false\nmaxTokensField = \"max_completion_tokens\"\n"
        ))
        .unwrap();
        assert_eq!(cfg.default_model.auth, agent_core::AuthMode::None);
        let q = cfg.default_model.effective_quirks();
        assert!(q.omit_stream_options && q.omit_tool_choice && q.omit_reasoning);
        assert!(!q.omit_temperature);
        assert_eq!(
            q.max_tokens_field,
            Some(agent_core::MaxTokensField::MaxCompletionTokens)
        );

        // 显式 quirks 与 compat 取并集；`quirks` 显式设置的 max_tokens_field 优先。
        let cfg: Config = toml::from_str(&format!(
            "[default_model]\n{base}[default_model.quirks]\nomit_temperature = true\n\
             max_tokens_field = \"max_tokens\"\n[default_model.compat]\n\
             supportsToolChoice = false\nmaxTokensField = \"max_completion_tokens\"\n"
        ))
        .unwrap();
        let q = cfg.default_model.effective_quirks();
        assert!(q.omit_temperature && q.omit_tool_choice, "并集");
        assert_eq!(
            q.max_tokens_field,
            Some(agent_core::MaxTokensField::MaxTokens),
            "显式 quirks 优先于 compat"
        );

        // omp 完整 compat 表里的**未实现**旗标不报错（否则既有 omp 配置直接加载失败）。
        let cfg: Result<Config, _> = toml::from_str(&format!(
            "[default_model]\n{base}[default_model.compat]\nsupportsStore = true\n\
             reasoningContentField = \"reasoning_content\"\nthinkingFormat = \"qwen\"\n"
        ));
        assert!(cfg.is_ok(), "未实现的 compat 旗标应被忽略: {cfg:?}");
        assert!(cfg.unwrap().default_model.effective_quirks().is_empty());
    }

    /// H44：`[ttsr] builtin_rules` 缺省 = `None`（装配层按 `true` 处理，向后兼容）；
    /// 显式 `false` 关闭整套内置规则。
    #[test]
    fn ttsr_builtin_rules_defaults_and_parse() {
        let cfg: TtsrConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.builtin_rules, None, "缺省 = None（装配层视为启用）");
        assert_eq!(cfg.enabled, None);

        let off: TtsrConfig = toml::from_str("builtin_rules = false").unwrap();
        assert_eq!(off.builtin_rules, Some(false));
        let on: TtsrConfig =
            toml::from_str("builtin_rules = true\ndisabled_rules = [\"rs-box-leak\"]").unwrap();
        assert_eq!(on.builtin_rules, Some(true));
        assert_eq!(on.disabled_rules, vec!["rs-box-leak".to_string()]);
    }

    /// H23：`load_with_warnings` 报告未知键且**不影响加载结果**（未知键被忽略）。
    #[test]
    fn load_with_warnings_reports_unknown_keys() {
        let dir = std::env::temp_dir().join(format!(
            "gyre-cfg-warn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".agent")).unwrap();
        std::fs::write(
            dir.join(".agent/config.toml"),
            "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"\n\n[agent]\nmax_turn = 3\n",
        )
        .unwrap();

        let (cfg, warnings) = Config::load_with_warnings(&dir).expect("未知键不应让加载失败");
        assert!(
            warnings
                .iter()
                .any(|w| w.path == "agent.max_turn"
                    && w.suggestion.as_deref() == Some("max_turns")),
            "应报告 agent.max_turn 并给出建议: {warnings:?}"
        );
        // 未知键被忽略：不会写入任何字段（max_turns 保持默认）。
        assert_ne!(cfg.agent.max_turns, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_server_config_timeout_zero_disables() {
        let src = "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"\n[mcp.servers.a]\ncommand = \"x\"\ntimeout_ms = 0\n";
        let cfg: Config = toml::from_str(src).expect("解析应成功");
        assert_eq!(cfg.mcp.servers["a"].timeout_ms(), 0, "0 表示不限制");
    }

    #[test]
    fn mcp_server_config_requires_command_or_url() {
        // 两者皆无 → 反序列化失败（配置错误应尽早暴露）。
        let src = "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"\n[mcp.servers.bad]\nargs = [\"x\"]\n";
        assert!(toml::from_str::<Config>(src).is_err());
    }

    // P2：[skills.providers] 跨工具 skill provider 开关。
    #[test]
    fn skills_providers_defaults_and_parse() {
        // 缺省：全开（向后兼容，无 [skills.providers] 段也可解析）。
        let cfg: SkillsConfig = toml::from_str("").unwrap();
        assert!(
            cfg.providers.claude
                && cfg.providers.codex
                && cfg.providers.opencode
                && cfg.providers.github
        );

        // 显式关闭单项；其它保持默认开。
        let cfg: SkillsConfig =
            toml::from_str("[providers]\nclaude = false\ngithub = false\n").unwrap();
        assert!(!cfg.providers.claude);
        assert!(cfg.providers.codex);
        assert!(cfg.providers.opencode);
        assert!(!cfg.providers.github);

        // 转 agent-skills 装配开关字段一一对应。
        let t = cfg.to_provider_toggles();
        assert!(!t.claude && t.codex && t.opencode && !t.github);
    }

    // ── [[hooks]]：解析 + 校验 ────────────────────────────────────────────────

    /// 合法 `[[hooks]]`：小写蛇形 event、可选字段缺省、timeout 生效值与工具过滤。
    /// H20：配置面事件名必须与 `agent-core` 的运行期事件名集合逐项一致
    /// （配置声明了却永不触发的事件会误导用户；运行期发了而配置不接受则无法订阅）。
    /// H24：`[default_model.headers]` / `quirks` 从 TOML 解析 + `${ENV}` 展开留给装配层。
    #[test]
    fn model_profile_headers_and_quirks_parse() {
        let src = r#"
[default_model]
id = "m"
api = "openai-completions"
base_url = "https://gw.example/v1"
api_key = "k"

[default_model.headers]
"HTTP-Referer" = "https://my.app"
Authorization = "Bearer ${MY_TOKEN}"

[default_model.quirks]
omit_stream_options = true
omit_temperature = true
"#;
        let cfg: Config = toml::from_str(src).expect("应可解析");
        assert_eq!(
            cfg.default_model
                .headers
                .get("HTTP-Referer")
                .map(String::as_str),
            Some("https://my.app")
        );
        assert!(cfg.default_model.quirks.omit_stream_options);
        assert!(cfg.default_model.quirks.omit_temperature);
        assert!(!cfg.default_model.quirks.omit_tool_choice);
        assert!(!cfg.default_model.quirks.is_empty());
        // 默认：无头、无开关（标准 wire）。
        let bare: Config = toml::from_str(
            "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"https://x\"\n",
        )
        .unwrap();
        assert!(bare.default_model.headers.is_empty());
        assert!(bare.default_model.quirks.is_empty());
        // 未知开关被拒（deny_unknown_fields：拼错键不会静默失效）。
        let bad = toml::from_str::<Config>(
            "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"https://x\"\n[default_model.quirks]\nomit_temperatur = true\n",
        );
        assert!(bad.is_err(), "未知 quirks 键应报错");
    }
    /// H23：`set_key_at` 写入配置（保留注释）、可回读、非法值回滚、数组表拒绝。
    #[test]
    fn set_key_writes_preserves_comments_and_rolls_back() {
        let dir = std::env::temp_dir().join(format!("gyre-cfgset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let original = "# 顶部注释必须保留\n[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"https://x\"\n";
        std::fs::write(&path, original).unwrap();

        // 标量：整数 / 浮点 / 布尔 / 裸字符串 / 引号字符串。
        Config::set_key_at(&path, "default_model.max_output_tokens", "8192").unwrap();
        Config::set_key_at(&path, "agent.context_window_guard", "0.75").unwrap();
        Config::set_key_at(&path, "agent.enable_thinking", "true").unwrap();
        Config::set_key_at(&path, "agent.mode", "code").unwrap();
        Config::set_key_at(&path, "language", "zh").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# 顶部注释必须保留"),
            "注释必须保留：\n{text}"
        );
        let merged = load_merged_value(std::slice::from_ref(&path))
            .unwrap()
            .unwrap();
        assert_eq!(
            merged["default_model"]["max_output_tokens"].as_integer(),
            Some(8192)
        );
        assert_eq!(
            merged["agent"]["context_window_guard"].as_float(),
            Some(0.75)
        );
        assert_eq!(merged["agent"]["enable_thinking"].as_bool(), Some(true));
        assert_eq!(merged["agent"]["mode"].as_str(), Some("code"));
        assert_eq!(merged["language"].as_str(), Some("zh"));

        // 数组（裸词 → 字符串元素）。
        Config::set_key_at(&path, "hooks", "[]").ok(); // 允许（空数组合法）
        let bad_before = std::fs::read_to_string(&path).unwrap();
        // 非法值（越界）→ 校验失败并回滚到写入前内容。
        let err = Config::set_key_at(&path, "agent.context_window_guard", "9.0").unwrap_err();
        assert!(err.to_string().contains("回滚"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            bad_before,
            "失败写入必须回滚"
        );

        // 数组表不可用点分键写入（明确报错而非静默破坏结构）。
        let err = Config::set_key_at(&path, "models.id", "x").unwrap_err();
        assert!(err.to_string().contains("数组表"), "{err}");
        // 空键拒绝。
        assert!(Config::set_key_at(&path, "  ", "x").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H23：标量类型推断。
    #[test]
    fn scalar_to_item_infers_types() {
        assert!(
            scalar_to_item("true")
                .as_value()
                .unwrap()
                .as_bool()
                .unwrap()
        );
        assert_eq!(
            scalar_to_item("42").as_value().unwrap().as_integer(),
            Some(42)
        );
        assert_eq!(
            scalar_to_item("1.5").as_value().unwrap().as_float(),
            Some(1.5)
        );
        assert_eq!(
            scalar_to_item("code").as_value().unwrap().as_str(),
            Some("code")
        );
        assert_eq!(
            scalar_to_item("\"quoted\"").as_value().unwrap().as_str(),
            Some("quoted")
        );
        let arr = scalar_to_item("[read_file, grep, 3]");
        let arr = arr.as_value().unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr.get(0).unwrap().as_str(), Some("read_file"));
        assert_eq!(arr.get(2).unwrap().as_integer(), Some(3));
    }

    /// H23：`config_candidates` 顺序为 用户 → 项目（项目覆盖用户）。
    #[test]
    fn config_candidates_are_user_then_project() {
        let candidates = config_candidates(Path::new("/tmp/proj"));
        assert_eq!(candidates.len(), 2, "应有两层: {candidates:?}");
        assert!(candidates[0].ends_with("config.toml"));
        assert!(
            candidates[0]
                .parent()
                .is_some_and(|p| !p.ends_with(".agent"))
        );
        assert!(candidates[1].ends_with(".agent/config.toml"));
    }
    #[test]
    fn hook_event_names_match_core_vocabulary() {
        let configured = [
            HookEventKind::SessionStart,
            HookEventKind::SessionShutdown,
            HookEventKind::BeforeAgentStart,
            HookEventKind::AgentStart,
            HookEventKind::AgentEnd,
            HookEventKind::TurnStart,
            HookEventKind::TurnEnd,
            HookEventKind::ToolCall,
            HookEventKind::ToolResult,
            HookEventKind::Stop,
            HookEventKind::AutoCompactionStart,
            HookEventKind::AutoCompactionEnd,
            HookEventKind::AutoRetryStart,
            HookEventKind::AutoRetryEnd,
            HookEventKind::RetryFallbackApplied,
            HookEventKind::RetryFallbackSucceeded,
            HookEventKind::TtsrTriggered,
        ];
        let mut names: Vec<&str> = configured.iter().map(|e| e.as_str()).collect();
        names.sort_unstable();
        let mut expected: Vec<&str> = agent_core::hook::HOOK_EVENT_NAMES.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected, "配置事件名与 core 清单漂移");
        // 全部名字都能从 TOML 字符串解析（含旧别名）。
        for name in expected {
            let src = format!("[[hooks]]\nevent = \"{name}\"\ncommand = \"true\"\n");
            let cfg: Config = toml::from_str(&format!(
                "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"https://x\"\n{src}"
            ))
            .unwrap_or_else(|e| panic!("event={name} 应可解析: {e}"));
            assert_eq!(cfg.hooks[0].event.as_str(), name);
        }
        // 旧别名仍可用（向后兼容）。
        let legacy: Config = toml::from_str(
            "[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"https://x\"\n[[hooks]]\nevent = \"before_tool\"\ncommand = \"true\"\n",
        )
        .unwrap();
        assert_eq!(legacy.hooks[0].event, HookEventKind::ToolCall);
        assert!(legacy.hooks[0].event.is_decidable());
        // 仅 tool_call 可决策。
        assert!(!HookEventKind::TurnStart.is_decidable());
        assert!(!HookEventKind::Stop.is_decidable());
    }

    #[test]
    fn hooks_parse_from_toml() {
        let src = r#"
[default_model]
id = "m"
api = "deepseek"
base_url = "https://api.deepseek.com"

[[hooks]]
event = "before_tool"
tool = "shell"
command = "/bin/gate"
timeout_secs = 5

[[hooks]]
event = "stop"
command = "/bin/notify"
"#;
        let cfg: Config = toml::from_str(src).expect("解析应成功");
        assert_eq!(cfg.hooks.len(), 2);
        let h0 = &cfg.hooks[0];
        assert_eq!(h0.event, HookEventKind::ToolCall);
        assert_eq!(h0.event.as_str(), "tool_call");
        assert!(h0.event.is_decidable());
        assert_eq!(h0.tool.as_deref(), Some("shell"));
        assert_eq!(h0.command, "/bin/gate");
        assert_eq!(h0.timeout_secs, Some(5));
        assert_eq!(h0.effective_timeout_secs(), 5);
        assert!(h0.matches_tool("shell") && !h0.matches_tool("read"));

        let h1 = &cfg.hooks[1];
        assert_eq!(h1.event, HookEventKind::Stop);
        assert!(h1.tool.is_none(), "缺省无工具过滤");
        assert_eq!(h1.timeout_secs, None);
        assert_eq!(h1.effective_timeout_secs(), 10, "缺省超时 10");
        assert!(h1.matches_tool("任意"), "None 过滤器匹配一切");
    }

    /// 非法 event 字符串 → 解析失败（serde 枚举小写蛇形）。
    #[test]
    fn hooks_reject_unknown_event() {
        let src = r#"
[default_model]
id = "m"
api = "deepseek"
base_url = "x"

[[hooks]]
event = "before"
command = "/bin/x"
"#;
        assert!(toml::from_str::<Config>(src).is_err());
    }

    /// validate：空 command 与 timeout 越界（0 / 601）拒绝；边界值（1 / 600 / 缺省）通过。
    #[test]
    fn hooks_validate_rejects_empty_command_and_bad_timeout() {
        let mk = |hooks: &str| {
            format!("[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"\n{hooks}")
        };

        let empty_cmd = mk("[[hooks]]\nevent = \"stop\"\ncommand = \"  \"\n");
        let cfg: Config = toml::from_str(&empty_cmd).unwrap();
        assert!(matches!(
            cfg.validate(),
            Err(super::ConfigError::Invalid(_))
        ));

        for bad in ["0", "601"] {
            let src = mk(&format!(
                "[[hooks]]\nevent = \"stop\"\ncommand = \"/bin/x\"\ntimeout_secs = {bad}\n"
            ));
            let cfg: Config = toml::from_str(&src).unwrap();
            assert!(
                matches!(cfg.validate(), Err(super::ConfigError::Invalid(_))),
                "timeout {bad} 应被拒绝"
            );
        }

        for ok in ["1", "600", ""] {
            let extra = if ok.is_empty() {
                String::new()
            } else {
                format!("timeout_secs = {ok}\n")
            };
            let src = mk(&format!(
                "[[hooks]]\nevent = \"stop\"\ncommand = \"/bin/x\"\n{extra}"
            ));
            let cfg: Config = toml::from_str(&src).unwrap();
            assert!(cfg.validate().is_ok(), "timeout '{ok}' 应通过");
        }
    }

    // ── [models.roles]：角色映射 ──────────────────────────────────────────────

    /// 顶层直接键 `models_roles`（不经 load 提升的裸反序列化路径）+ resolve_role。
    #[test]
    fn models_roles_direct_key_parse_and_resolve() {
        let src = r#"
[default_model]
id = "sonnet"
api = "anthropic-messages"
base_url = "https://api.anthropic.com"

[[models]]
id = "kimi"
alias = "k2"
api = "anthropic-messages"
base_url = "https://api.anthropic.com"

[models_roles]
plan = "k2"
quick = "sonnet"
"#;
        let cfg: Config = toml::from_str(src).expect("解析应成功");
        assert_eq!(cfg.models_roles.as_ref().map(|r| r.roles.len()), Some(2));
        assert_eq!(
            cfg.resolve_role("plan").map(|m| m.id.as_str()),
            Some("kimi"),
            "角色值可为 [[models]] 的 alias"
        );
        assert_eq!(
            cfg.resolve_role("quick").map(|m| m.id.as_str()),
            Some("sonnet"),
            "角色值可为默认 profile 的 id"
        );
        assert!(
            cfg.resolve_role("default").is_none(),
            "未配置的角色返回 None"
        );
        assert!(cfg.validate().is_ok());
    }

    /// `[models.roles]` 段经 load 管线提升（两种形态：随 `[[models]]` / 独立段）。
    #[test]
    fn models_roles_section_hoisted_via_load_pipeline() {
        // 形态一：`[[models]]` 存在 —— TOML 规范下 roles 挂在最后一个条目内，提升后恢复。
        let with_models = r#"
[default_model]
id = "sonnet"
api = "anthropic-messages"
base_url = "https://api.anthropic.com"

[[models]]
id = "kimi"
alias = "k2"
api = "anthropic-messages"
base_url = "https://api.anthropic.com"

[[models]]
id = "glm-5"
alias = "glm"
api = "zai"
base_url = "https://api.z.ai/api/paas/v4"

[models.roles]
plan = "k2"
quick = "glm"
"#;
        let value: toml::Value = toml::from_str(with_models).unwrap();
        let cfg = Config::from_merged(&value).expect("提升 + 解析应成功");
        assert_eq!(cfg.models.len(), 2, "提升后 models 恢复为纯模型数组");
        let roles = &cfg.models_roles.as_ref().expect("roles 应被提升").roles;
        assert_eq!(roles.get("plan").map(String::as_str), Some("k2"));
        assert_eq!(roles.get("quick").map(String::as_str), Some("glm"));
        assert_eq!(
            cfg.resolve_role("plan").map(|m| m.id.as_str()),
            Some("kimi")
        );
        assert_eq!(
            cfg.resolve_role("quick").map(|m| m.id.as_str()),
            Some("glm-5")
        );
    }

    /// 形态二：仅 `[models.roles]`（无 `[[models]]`）—— models 表整体提升后移除。
    #[test]
    fn models_roles_only_section_hoisted() {
        let roles_only = r#"
[default_model]
id = "sonnet"
api = "anthropic-messages"
base_url = "https://api.anthropic.com"

[models.roles]
plan = "sonnet"
"#;
        let value: toml::Value = toml::from_str(roles_only).unwrap();
        let cfg = Config::from_merged(&value).expect("提升 + 解析应成功");
        assert!(cfg.models.is_empty(), "提升后为空的 models 表应被移除");
        assert_eq!(
            cfg.resolve_role("plan").map(|m| m.id.as_str()),
            Some("sonnet")
        );
    }

    /// validate：roles 引用不存在的 alias/id → Invalid。
    #[test]
    fn models_roles_validate_rejects_unknown_target() {
        let src = r#"
[default_model]
id = "sonnet"
api = "anthropic-messages"
base_url = "https://api.anthropic.com"

[models_roles]
plan = "nope"
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    /// 缺省：无 roles 段 → None，resolve_role 一律 None。
    #[test]
    fn models_roles_absent_defaults_to_none() {
        let src = r#"
[default_model]
id = "m"
api = "deepseek"
base_url = "x"
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        assert!(cfg.models_roles.is_none());
        assert!(cfg.resolve_role("default").is_none());
    }
}
