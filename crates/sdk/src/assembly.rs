//! 内置工具装配（H42）：CLI/REPL、JSON-RPC、Web server 三个前端共用的**唯一**入口。
//!
//! 收敛前的漂移：`assemble_builtin_tools` 只存在于 CLI 侧（rpc 复用），server 走
//! `agent_tools::builtin_tools_with_pool_and_jobs` 并**无条件**注册 ast/image/lsp，
//! 也不注册 hashline/pty/debug/ssh/browser 与它们的提示词——同一份配置在两个前端得到
//! 不同工具面。本模块把「可选组开关推导 + 注册 + 提示词」固化为单一实现：
//!
//! - [`optional_tool_switches`]：`[tools].enabled.<key>` → 开关快照（含各组默认值）；
//! - [`assemble_builtin_tools`]：开关 → 注册表（+ 共享 `LspPool`）；
//! - [`crate::optional_tool_sections`]：同一开关 → system prompt 指引（已有，H42 前就在 sdk）。
//!
//! 快照存储 [`new_snapshot_store`] 同时供 `read_file`（记录读取版本）与 `apply_hashline`
//! （stale-hash 回放）使用；调用方还须把它通过 `AgentBuilder::snapshot_store` 交给引擎。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use agent_config::Config;
use agent_tools::{DefaultToolRegistry, InMemorySnapshotStore, LspPool, Minimizer};

/// 会话级文本快照存储（`read_file` 记录 + `apply_hashline` stale-hash 回放共享同一实例）。
pub type SnapshotStore = Arc<RwLock<InMemorySnapshotStore>>;

/// 新建空快照存储。
#[must_use]
pub fn new_snapshot_store() -> SnapshotStore {
    Arc::new(RwLock::new(InMemorySnapshotStore::new()))
}

/// 可选工具组 key 白名单（不含 `github`；github 由独立字段管理）。
pub const OPTIONAL_TOOL_KEYS: &[&str] = &[
    "ast", "lsp", "image", "hashline", "pty", "debug", "ssh", "browser",
];

/// 判断 key 是否为已知可选工具组（不含 github）。
#[must_use]
pub fn is_known_optional_key(key: &str) -> bool {
    OPTIONAL_TOOL_KEYS.contains(&key)
}

/// 从配置推导可选工具组开关快照。
///
/// 各组默认：`ast`/`lsp`/`image`/`edit_writethrough` 取
/// [`agent_tools::OPTIONAL_TOOL_PROMPTS`] 的 `default`（均 false）；`hashline` **默认 true**
/// （主推编辑格式）；`pty`/`debug`/`ssh`/`browser` 默认 false。显式
/// `[tools].enabled.<key>` 一律覆盖默认。
#[must_use]
pub fn optional_tool_switches(cfg: &Config) -> HashMap<String, bool> {
    let mut optional: HashMap<String, bool> = HashMap::new();
    for p in agent_tools::OPTIONAL_TOOL_PROMPTS {
        optional.insert(p.key.to_string(), cfg.tools.effective(p.key, p.default));
    }
    optional.insert(
        "hashline".to_string(),
        cfg.tools.effective("hashline", true),
    );
    optional.insert("pty".to_string(), cfg.tools.effective("pty", false));
    optional.insert("debug".to_string(), cfg.tools.effective("debug", false));
    optional.insert("ssh".to_string(), cfg.tools.effective("ssh", false));
    optional.insert("browser".to_string(), cfg.tools.effective("browser", false));
    optional
}

/// 装配内置工具集（H42 唯一入口，三前端共用）：核心工具 + 按开关追加的可选组 + github。
///
/// - 可选组：`ast` / `image` / `lsp` / `hashline` / `pty` / `debug` / `ssh` / `browser`；
///   开关来自 [`optional_tool_switches`]（`[tools].enabled.<key>`），未启用的组既不出现在
///   LLM 工具列表，也不占 system prompt Token（提示词由 [`crate::optional_tool_sections`] 同步）。
/// - 返回 `LspTool` 的共享 [`LspPool`]，供 `LspWriteEffect` 复用同一套语言服务器（未启用 lsp 时为 `None`）。
#[must_use]
pub fn assemble_builtin_tools(
    optional: &HashMap<String, bool>,
    github_enabled: bool,
    github_allow_write: bool,
    interceptor_enabled: bool,
    minimizer: Minimizer,
    snapshots: Option<SnapshotStore>,
    jobs: Option<Arc<agent_core::jobs::AsyncJobManager>>,
) -> (DefaultToolRegistry, Option<LspPool>) {
    let intercept = if interceptor_enabled {
        agent_tools::intercept::default_compiled()
    } else {
        Vec::new()
    };
    let mut reg = agent_tools::core_tools_with_jobs(intercept, minimizer, jobs);
    let mut lsp_pool: Option<agent_tools::LspPool> = None;
    if *optional.get("ast").unwrap_or(&false) {
        reg = agent_tools::ast_tools(reg);
    }
    if *optional.get("image").unwrap_or(&false) {
        reg = agent_tools::image_tools(reg);
    }
    if *optional.get("lsp").unwrap_or(&false) {
        // 取 LspTool 的共享 pool，供 LspWriteEffect 复用同一套语言服务器（避免两套 LSP）。
        let lsp = agent_tools::LspTool::new();
        lsp_pool = Some(lsp.pool());
        reg = reg.with(Box::new(lsp));
    }
    if *optional.get("hashline").unwrap_or(&false) {
        // 共享会话快照存储（与 engine 的 ToolContext::snapshots 同一实例——read 侧记录
        // 的版本对本工具 stale-hash 恢复可见）。None（如测试）时自建，行为不变。
        let hashline = match snapshots {
            Some(store) => agent_hashline::HashlineTool::with_snapshots(store),
            None => agent_hashline::HashlineTool::new(),
        };
        reg = reg.with(Box::new(hashline));
    }
    if *optional.get("pty").unwrap_or(&false) {
        // H45：持久会话（跨命令保持 cwd/环境）与一次性 PTY 执行并存。
        reg = reg
            .with(Box::new(agent_pty::RunPtyTool))
            .with(Box::new(agent_pty::ShellSessionTool::new()));
    }
    if *optional.get("debug").unwrap_or(&false) {
        // P1-1：DAP 调试器（lldb-dap / dlv / debugpy 自动探测，14 动作）。
        reg = reg.with(Box::new(agent_dap::DebugTool::new(
            agent_dap::DapSettings::default(),
        )));
    }
    if *optional.get("ssh").unwrap_or(&false) {
        // P2：SSH 工具（解析 ~/.ssh/config，远程命令执行；BatchMode 非交互）。
        reg = reg.with(Box::new(agent_tools::SshTool::new(None)));
    }
    if *optional.get("browser").unwrap_or(&false) {
        // P2：browser 工具（CDP 驱动 chromium；懒启动，7 动作）。
        reg = reg.with(Box::new(agent_browser::BrowserTool::new()));
    }
    if github_enabled {
        reg = reg.with(Box::new(agent_tools::GithubTool::new(github_allow_write)));
    }
    (reg, lsp_pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_tools::ToolRegistry;

    fn cfg(extra: &str) -> Config {
        let src =
            format!("[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"\n{extra}");
        toml::from_str(&src).expect("测试配置应可解析")
    }

    fn names(reg: &DefaultToolRegistry) -> Vec<String> {
        reg.specs().iter().map(|s| s.name.clone()).collect()
    }

    /// H42：开关推导的默认值与显式覆盖（三前端共用同一实现）。
    #[test]
    fn optional_tool_switches_defaults_and_overrides() {
        let defaults = optional_tool_switches(&cfg(""));
        assert_eq!(defaults.get("hashline"), Some(&true), "hashline 默认启用");
        for key in ["ast", "lsp", "image", "pty", "debug", "ssh", "browser"] {
            assert_eq!(defaults.get(key), Some(&false), "{key} 默认关闭");
        }
        assert_eq!(defaults.get("edit_writethrough"), Some(&false));

        let overridden = optional_tool_switches(&cfg(
            "[tools.enabled]\nast = true\npty = true\nhashline = false\n",
        ));
        assert_eq!(overridden.get("ast"), Some(&true));
        assert_eq!(overridden.get("pty"), Some(&true));
        assert_eq!(overridden.get("hashline"), Some(&false));
        assert_eq!(overridden.get("ssh"), Some(&false), "未覆盖的组保持默认");

        assert!(OPTIONAL_TOOL_KEYS.contains(&"pty"));
        assert!(is_known_optional_key("ast"));
        assert!(!is_known_optional_key("github"), "github 由独立字段管理");
        assert!(!is_known_optional_key("bogus"));
    }

    /// H42：装配只注册启用的组（未启用组既不进注册表，也不进提示词）。
    #[test]
    fn assemble_registers_only_enabled_groups() {
        let mut switches = optional_tool_switches(&cfg(""));
        let (reg, pool) = assemble_builtin_tools(
            &switches,
            false,
            false,
            false,
            agent_tools::disabled(),
            None,
            None,
        );
        let list = names(&reg);
        assert!(
            list.contains(&"read_file".to_string()),
            "核心工具恒在: {list:?}"
        );
        assert!(
            list.contains(&"apply_hashline".to_string()),
            "hashline 默认启用: {list:?}"
        );
        assert!(!list.contains(&"replace_block".to_string()), "ast 默认关");
        assert!(!list.contains(&"lsp".to_string()), "lsp 默认关");
        assert!(!list.contains(&"run_pty_command".to_string()));
        assert!(pool.is_none(), "未启用 lsp 时无共享池");

        // 显式打开 ast / lsp / pty + github。
        switches.insert("ast".into(), true);
        switches.insert("lsp".into(), true);
        switches.insert("pty".into(), true);
        let (reg, pool) = assemble_builtin_tools(
            &switches,
            true,
            true,
            false,
            agent_tools::disabled(),
            Some(new_snapshot_store()),
            None,
        );
        let list = names(&reg);
        assert!(list.contains(&"replace_block".to_string()), "{list:?}");
        assert!(list.contains(&"lsp".to_string()), "{list:?}");
        assert!(pool.is_some(), "启用 lsp 时返回共享池");
        assert!(list.contains(&"run_pty_command".to_string()), "{list:?}");
        assert!(list.contains(&"shell_session".to_string()), "{list:?}");
        assert!(list.contains(&"github".to_string()), "{list:?}");
    }
}
