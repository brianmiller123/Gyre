//! 调试适配器可执行文件的 PATH 探测。
//!
//! 探测语义：命令含路径分隔符 → 直接检查存在且可执行；否则按 `PATH` 环境变量
//! 逐目录查找可执行文件。`DebugTool` 在 launch/attach（resolve 时）逐个适配器探测，
//! 失败时给出明确错误（含尝试列表）。

use std::path::{Path, PathBuf};

/// 单条 PATH 目录解析（空段 → 当前目录，与 `split_paths` 语义一致）。
fn path_dirs(path_env: &str) -> impl Iterator<Item = PathBuf> {
    std::env::split_paths(path_env)
}

/// 判断路径是否为可执行文件。
#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && p.metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// 探测命令在给定 `PATH` 中的可执行文件绝对路径。
///
/// - 命令含路径分隔符（`/` 或 `\`）→ 直接检查（相对路径按当前目录解析）；
/// - 否则遍历 `PATH` 目录，返回首个可执行者。
#[must_use]
pub fn probe_executable(command: &str, path_env: &str) -> Option<PathBuf> {
    if command.contains('/') || command.contains('\\') {
        let p = Path::new(command);
        return is_executable(p).then(|| p.to_path_buf());
    }
    path_dirs(path_env)
        .map(|dir| dir.join(command))
        .find(|p| is_executable(p))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;

    /// 建临时目录（含清理）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-dap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录失败");
        dir
    }

    #[test]
    #[cfg(unix)]
    fn probe_finds_executable_in_path() {
        let dir = temp_dir("probe");
        let script = dir.join("fake-dap");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("写脚本失败");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod 失败");
        let path_env = dir.to_string_lossy().into_owned();
        let found = probe_executable("fake-dap", &path_env).expect("应能找到脚本");
        assert_eq!(found, script);
        // 不在 PATH 中 → 找不到
        assert!(probe_executable("fake-dap", "").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_absolute_path() {
        let found = probe_executable("/bin/sh", "").expect("绝对路径应直接可用");
        assert_eq!(found, PathBuf::from("/bin/sh"));
        assert!(probe_executable("/nonexistent/nope", "").is_none());
    }

    #[test]
    fn probe_unknown_command_is_none() {
        assert!(probe_executable("definitely-not-a-real-cmd-xyz", "").is_none());
    }
}
