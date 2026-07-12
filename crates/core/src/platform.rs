//! 跨平台路径与编译守卫（§12）。
//!
//! 平台差异集中在本模块，业务路径一律通过 [`config_dir`] 等抽象获取目录，
//! 禁止硬编码 `/` 或 `\`。

use std::path::PathBuf;

/// 返回用户级配置目录（跨平台）。
///
/// - Linux: `~/.config/agent`
/// - Windows: `%APPDATA%\agent`
/// - macOS: `~/Library/Application Support/agent`
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("agent"))
}

/// 返回项目级配置目录候选（`.agent/`，相对工作区根）。
#[must_use]
pub fn project_config_dir_name() -> &'static str {
    ".agent"
}

/// 仅支持 unix 与 windows 平台（macOS 属于 unix）。
#[cfg(not(any(unix, windows)))]
compile_error!("agent-project 仅支持 unix 与 windows 平台");
