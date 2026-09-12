//! `.env` 加载（H36）。
//!
//! 移植 oh-my-pi `packages/utils/src/env.ts` 的 dotenv 语义：
//!
//! - **来源与优先级**：`$GYRE_ENV_FILE`（显式指定）→ `<cwd>/.env` → `<cwd>/.agent/.env`
//!   → `<config_dir>/.env` → `$HOME/.env`；同一键**先到先得**（项目覆盖用户）。
//! - **真实环境优先**：进程已有的环境变量永远胜过 `.env` 值（同 omp `!Bun.env[key]`），
//!   因此 `.env` 只做「补缺」，不会覆盖 shell 里显式导出的值。
//! - **行语义**（对齐 Bun/omp `parseEnvLine`）：可选的 `export ` 前缀、整行 `#` 注释、
//!   未加引号值时「空白 + `#`」起行内注释、单/双/反引号包裹（引号内 `#` 原样保留）。
//! - **安全过滤**：变量名不得含 `=`/`\0`、值不得含 `\0`；跳过 macOS 的
//!   `MallocStackLogging*`（被继承到子进程会带来巨大开销）。
//!
//! 与进程级环境不同，这里**不写回** `std::env`（edition 2024 下 `set_var` 是 unsafe，
//! 且运行期改环境在多线程下本身不安全）：值存在进程级只读表里，由
//! [`crate::env::expand_env`] 在 `${VAR}` 展开时回落查询。`GYRE_DOTENV=off|0|false`
//! 关闭加载。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 解析出的 `.env` 变量表与来源文件（按优先级从高到低）。
#[derive(Debug, Default, Clone)]
pub struct DotEnv {
    vars: BTreeMap<String, String>,
    files: Vec<PathBuf>,
}

impl DotEnv {
    /// 按给定顺序解析文件（先到先得；不存在/不可读的文件静默跳过）。
    #[must_use]
    pub fn from_files(paths: &[PathBuf]) -> Self {
        let mut vars = BTreeMap::new();
        let mut files = Vec::new();
        for path in paths {
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            let mut loaded_any = false;
            for line in content.lines() {
                let Some((key, value)) = parse_env_line(line) else {
                    continue;
                };
                if !is_safe_env_name(&key) || !is_safe_env_value(&value) || is_malloc_logging(&key)
                {
                    continue;
                }
                // 先到先得：后续（更靠近用户级）文件不覆盖已有键。
                vars.entry(key).or_insert(value);
                loaded_any = true;
            }
            if loaded_any {
                files.push(path.clone());
            }
        }
        Self { vars, files }
    }

    /// 查一个变量（不含真实进程环境回落）。
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// 变量条数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// 实际生效的来源文件（解析出至少一个变量者）。
    #[must_use]
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }
}

/// 解析一行 dotenv；空白/注释/畸形行返回 `None`。
///
/// 与 omp `parseEnvLine` 一致：`export ` 前缀、三种引号、未引号值的「空白 + #」行内注释。
#[must_use]
pub fn parse_env_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let eq = trimmed.find('=')?;
    let mut key = trimmed[..eq].trim();
    if let Some(rest) = key.strip_prefix("export") {
        // `export` 与名字之间至少一个空白（避免匹配 `exportFOO`）。
        if rest.starts_with([' ', '\t']) {
            key = rest.trim_start();
        }
    }
    if key.is_empty() {
        return None;
    }
    let raw = trimmed[eq + 1..].trim_start();
    let value = match raw.chars().next() {
        Some(q @ ('"' | '\'' | '`')) => {
            // 找未被反斜杠转义的闭合引号；找不到则取到行尾（同 omp）。
            let mut close = raw[1..].find(q).map(|i| i + 1);
            while let Some(idx) = close {
                if !is_escaped(raw, idx) {
                    break;
                }
                close = raw[idx + 1..].find(q).map(|i| i + idx + 1);
            }
            close.map_or_else(|| raw[1..].to_string(), |idx| raw[1..idx].to_string())
        }
        _ => {
            // 未引号值：` #` 起注释。
            let comment = raw
                .char_indices()
                .find(|(i, c)| {
                    (*c == '#') && *i > 0 && matches!(raw.as_bytes()[i - 1], b' ' | b'\t')
                })
                .map(|(i, _)| i);
            comment.map_or_else(
                || raw.trim_end().to_string(),
                |i| raw[..i].trim_end().to_string(),
            )
        }
    };
    Some((key.to_string(), value))
}

/// `idx` 位置的字符是否被反斜杠转义（奇数个连续反斜杠）。
fn is_escaped(s: &str, idx: usize) -> bool {
    let bytes = s.as_bytes();
    let mut count = 0usize;
    let mut i = idx;
    while i > 0 && bytes[i - 1] == b'\\' {
        count += 1;
        i -= 1;
    }
    count % 2 == 1
}

/// 变量名安全（非空、不含 `=`/`\0`，同 omp `isSafeEnvName`）。
#[must_use]
pub fn is_safe_env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('=') && !name.contains('\0')
}

/// 变量值安全（不含 `\0`，同 omp `isSafeEnvValue`）。
#[must_use]
pub fn is_safe_env_value(value: &str) -> bool {
    !value.contains('\0')
}

/// macOS malloc 栈记录开关（继承到子进程会拖垮性能，跳过）。
#[must_use]
pub fn is_malloc_logging(name: &str) -> bool {
    matches!(name, "MallocStackLogging" | "MallocStackLoggingNoCompact")
}

/// 候选 `.env` 路径（优先级从高到低）。
///
/// `$GYRE_ENV_FILE`（可多个，用 `:` 分隔）显式指定时排在最前。
#[must_use]
pub fn candidate_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(explicit) = std::env::var("GYRE_ENV_FILE") {
        for p in explicit.split(':').filter(|p| !p.trim().is_empty()) {
            paths.push(PathBuf::from(p.trim()));
        }
    }
    paths.push(cwd.join(".env"));
    paths.push(cwd.join(agent_core::project_config_dir_name()).join(".env"));
    if let Some(cfg) = agent_core::config_dir() {
        paths.push(cfg.join(".env"));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".env"));
    }
    paths
}

static DOTENV: OnceLock<DotEnv> = OnceLock::new();

/// 安装进程级 dotenv（幂等：仅首次生效）。`GYRE_DOTENV=off|0|false` 时装载空表。
///
/// 返回生效表（供宿主打印一行来源摘要）。
pub fn load_dotenv(cwd: &Path) -> &'static DotEnv {
    DOTENV.get_or_init(|| {
        if dotenv_disabled() {
            return DotEnv::default();
        }
        DotEnv::from_files(&candidate_paths(cwd))
    })
}

/// 进程级 dotenv（未安装时 `None`）。
#[must_use]
pub fn dotenv() -> Option<&'static DotEnv> {
    DOTENV.get()
}

/// `GYRE_DOTENV` 是否为关闭值。
fn dotenv_disabled() -> bool {
    std::env::var("GYRE_DOTENV").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_export_quoted_and_commented_lines() {
        assert_eq!(
            parse_env_line("KEY=value"),
            Some(("KEY".into(), "value".into()))
        );
        assert_eq!(
            parse_env_line("export KEY = value "),
            Some(("KEY".into(), "value".into()))
        );
        // 未引号值的行内注释（空白 + #）。
        assert_eq!(
            parse_env_line("KEY=value # trailing"),
            Some(("KEY".into(), "value".into()))
        );
        // 引号内的 `#` 原样保留；引号本身剥掉。
        assert_eq!(
            parse_env_line(r#"KEY="a # b""#),
            Some(("KEY".into(), "a # b".into()))
        );
        assert_eq!(
            parse_env_line("KEY='single'"),
            Some(("KEY".into(), "single".into()))
        );
        // 注释/空行/畸形行。
        assert_eq!(parse_env_line("  # comment"), None);
        assert_eq!(parse_env_line("   "), None);
        assert_eq!(parse_env_line("NOEQUALS"), None);
        assert_eq!(parse_env_line("=novalue"), None);
        // `exportFOO` 不是 export 前缀。
        assert_eq!(
            parse_env_line("exportFOO=1"),
            Some(("exportFOO".into(), "1".into()))
        );
    }

    #[test]
    fn from_files_first_wins_and_skips_unsafe() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project.env");
        let user = dir.path().join("user.env");
        std::fs::write(
            &project,
            "A=project\nB=1\nMallocStackLogging=1\n# c\nC=keep # x\n",
        )
        .unwrap();
        std::fs::write(&user, "A=user\nD=2\n").unwrap();
        let env = DotEnv::from_files(&[project.clone(), user.clone()]);
        // 先到先得：项目值胜出；缺失的键由用户文件补。
        assert_eq!(env.get("A"), Some("project"));
        assert_eq!(env.get("B"), Some("1"));
        assert_eq!(env.get("C"), Some("keep"));
        assert_eq!(env.get("D"), Some("2"));
        assert_eq!(env.get("MallocStackLogging"), None, "malloc 开关被过滤");
        assert_eq!(env.len(), 4);
        assert_eq!(env.files(), [project, user]);
        // 不存在的文件静默跳过，且不计入来源。
        let missing = DotEnv::from_files(&[dir.path().join("nope.env")]);
        assert!(missing.is_empty());
        assert!(missing.files().is_empty());
    }

    /// 端到端：`load_dotenv` → `expand_env`（`:-` 默认值 / dotenv 值 / 真实环境优先）。
    ///
    /// 进程级 `OnceLock` 只允许本测试安装一次，故这是唯一调用 `load_dotenv` 的用例。
    #[test]
    #[allow(unsafe_code)] // edition 2024：env 变更需 unsafe；仅测试
    fn load_dotenv_feeds_expand_env() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("explicit.env");
        std::fs::write(
            &file,
            "H36_TEST_FROM_DOTENV=dotenv-value\nH36_TEST_EMPTY=\nH36_TEST_REAL_WINS=dotenv-loses\n",
        )
        .unwrap();
        unsafe { std::env::set_var("GYRE_ENV_FILE", &file) };
        unsafe { std::env::set_var("H36_TEST_REAL_WINS", "shell-value") };
        let loaded = load_dotenv(dir.path());
        assert_eq!(loaded.len(), 3, "{:?}", loaded.files());
        // dotenv 提供值。
        assert_eq!(
            crate::env::expand_env("k=${H36_TEST_FROM_DOTENV}"),
            "k=dotenv-value"
        );
        // `:-` 默认值：dotenv 中为空 → 取默认；未设置 → 取默认。
        assert_eq!(
            crate::env::expand_env("${H36_TEST_EMPTY:-fallback}"),
            "fallback"
        );
        assert_eq!(
            crate::env::expand_env("${H36_TEST_ABSENT:-fallback}"),
            "fallback"
        );
        // 真实环境变量优先于 dotenv。
        assert_eq!(
            crate::env::expand_env("${H36_TEST_REAL_WINS}"),
            "shell-value"
        );
        unsafe {
            std::env::remove_var("GYRE_ENV_FILE");
            std::env::remove_var("H36_TEST_REAL_WINS");
        }
    }

    #[test]
    fn candidate_paths_put_cwd_before_user_scope() {
        let cwd = Path::new("/tmp/ws");
        let paths = candidate_paths(cwd);
        let cwd_idx = paths.iter().position(|p| p == &cwd.join(".env")).unwrap();
        let agent_idx = paths
            .iter()
            .position(|p| p == &cwd.join(".agent").join(".env"))
            .unwrap();
        assert!(cwd_idx < agent_idx, "cwd/.env 优先于 cwd/.agent/.env");
        assert!(paths.len() >= 2);
    }

    #[test]
    fn escapes_are_respected_when_finding_closing_quote() {
        // `\"` 不是闭合引号。
        assert_eq!(
            parse_env_line(r#"KEY="a\"b""#),
            Some(("KEY".into(), r#"a\"b"#.into()))
        );
        // 未闭合引号：取到行尾（同 omp）。
        assert_eq!(
            parse_env_line(r#"KEY="unterminated"#),
            Some(("KEY".into(), "unterminated".into()))
        );
    }
}
