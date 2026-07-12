//! # Skill 端口
//!
//! File-backed skill 系统的端口类型与 trait。
//!
//! - [`Skill`] / [`SkillSource`] / [`SkillLevel`] —— 已发现 skill 的数据模型
//! - [`SkillProvider`] —— 发现 provider 端口（可插拔；首批仅 native 实现，见 `crates/skills`）
//! - [`SkillResolver`] —— `skill://` URL 解析端口（供 `ReadFileTool` 注入，避免 `tools`→`skills` 依赖）
//! - [`SkillError`] —— skill 子系统错误
//!
//! 解耦保证：本模块仅放跨 crate 共享的端口，实现位于 `crates/skills`；
//! `crates/tools` 经 [`SkillResolver`] trait 解耦，不直接依赖 `crates/skills`。

use std::path::PathBuf;

use crate::message::Mode;

/// skill 发现层级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillLevel {
    /// 用户级（`<config_dir>/skills`）。
    User,
    /// 项目级（自 cwd 向上 walkup 的 `.agent/skills`）。
    Project,
}

/// skill 来源元数据。
#[derive(Debug, Clone)]
pub struct SkillSource {
    /// provider 稳定 id（如 "native"）。
    pub provider: String,
    /// 发现层级。
    pub level: SkillLevel,
}

/// 一个已发现的 skill。
#[derive(Debug, Clone)]
pub struct Skill {
    /// 名称（须符合 agentskills.io 规范：`^[a-z0-9]+(-[a-z0-9]+)*$`，1–64 字符）。
    pub name: String,
    /// 描述（注入 system prompt 的 `<skills>` 段）。
    pub description: String,
    /// `SKILL.md` 绝对路径。
    pub file_path: PathBuf,
    /// skill 目录（`skill://` URL 的 baseDir）。
    pub base_dir: PathBuf,
    /// 来源。
    pub source: SkillSource,
    /// `true` = 不进 prompt 列表，但仍可 `skill://` 访问（frontmatter `hide` / `disable-model-invocation`）。
    pub hide: bool,
    /// 限定模式；`None` 或空 `Vec` 表示所有模式可用。
    pub modes: Option<Vec<Mode>>,
}

impl Skill {
    /// 该 skill 在给定模式下是否可见（未限定 modes，或 modes 含 `mode`）。
    #[must_use]
    pub fn available_for_mode(&self, mode: Mode) -> bool {
        match &self.modes {
            None => true,
            Some(modes) => modes.is_empty() || modes.contains(&mode),
        }
    }
}

/// skill 加载选项（来自配置 `[skills]` 段）。
#[derive(Debug, Clone)]
pub struct SkillLoadOptions {
    /// 总开关；`false` 则不发现、不注入、`skill://` 一律失败。
    pub enabled: bool,
    /// 自定义扫描目录（`~` 已展开）；非递归 `*/SKILL.md`。
    pub custom_directories: Vec<PathBuf>,
    /// 排除 glob（作用于 skill 名）。
    pub ignored: Vec<String>,
    /// 包含 glob（作用于 skill 名；空 = 全部）。
    pub included: Vec<String>,
}

impl Default for SkillLoadOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            custom_directories: Vec::new(),
            ignored: Vec::new(),
            included: Vec::new(),
        }
    }
}

/// skill 发现 provider 端口（可插拔）。
///
/// 实现按 [`SkillProvider::priority`] 聚合；同名 skill 高 priority 者胜（first-wins）。
/// 首批仅 native provider（`crates/skills`）；未来可零侵入新增 `.claude` / `.codex` / `.github` provider。
#[async_trait::async_trait]
pub trait SkillProvider: Send + Sync {
    /// provider 稳定 id（如 "native"）。
    fn id(&self) -> &str;
    /// 优先级（数值大者优先）。
    fn priority(&self) -> u32;
    /// 发现该 provider 的全部 skill（未去重、未过滤）。
    ///
    /// # Errors
    /// 底层 IO 失败时返回 [`SkillError`]。
    async fn discover(&self, opts: &SkillLoadOptions) -> Result<Vec<Skill>, SkillError>;
}

/// `skill://` URL 解析端口。
///
/// 供 `ReadFileTool`（`crates/tools`）注入，使其无需依赖 `crates/skills`：
/// 装配层把 `SkillCatalog` 作为本 trait 的实现注入 read 工具。
///
/// URL 形式：
/// - `skill://<name>` → 该 skill 的 `SKILL.md`
/// - `skill://<name>/<rel>` → skill 目录内相对路径
///
/// 解析为纯路径计算（不做磁盘 stat），同步、零 IO。
pub trait SkillResolver: Send + Sync {
    /// 解析 `skill://` URL 为磁盘路径。
    ///
    /// # Errors
    /// URL 缺名 / 未知名 / 绝对路径 / 路径遍历时返回 [`SkillError`]。
    fn resolve(&self, url: &str) -> Result<PathBuf, SkillError>;
}

/// skill 子系统错误。
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// `skill://` URL 缺少 skill 名。
    #[error("skill:// URL 缺少 skill 名")]
    MissingName,
    /// 未知 skill 名（含可用列表，逗号分隔）。
    #[error("未知 skill: {name}（可用: {available}）")]
    Unknown {
        /// 请求的 skill 名。
        name: String,
        /// 可用 skill 名（逗号分隔）。
        available: String,
    },
    /// `skill://` URL 含绝对路径。
    #[error("skill:// 不允许绝对路径")]
    AbsolutePath,
    /// `skill://` URL 含路径遍历 `..`。
    #[error("skill:// 不允许路径遍历 ..")]
    Traversal,
    /// 解析出的路径不在 skill baseDir 之下（逃逸）。
    #[error("skill:// 解析路径逃逸 baseDir")]
    Escape,
    /// 解析出的路径不存在。
    #[error("文件未找到: {0}")]
    NotFound(PathBuf),
    /// 底层 IO 错误。
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
