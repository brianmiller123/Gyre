//! Chromium 启动器。
//!
//! - PATH 探测 chromium/chrome 可执行文件（`BROWSER_PATH` 环境变量可显式指定）；
//! - spawn `--headless=new --remote-debugging-port=0 --user-data-dir=<临时目录>`
//!   （`--no-sandbox` 由 `BROWSER_NO_SANDBOX` 环境变量开关，或 [`LaunchOptions::no_sandbox`]）；
//! - 解析 stderr 中 `DevTools listening on ws://…` 行，拿到端口与 WebSocket 端点；
//! - [`BrowserProcess::close`] 击杀**整个进程组**（launch 时 `process_group(0)` 建独立组）。

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 启动错误。
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// 未找到 chromium/chrome 可执行文件。
    #[error(
        "未找到 chromium/chrome 可执行文件（PATH 探测失败；可用 BROWSER_PATH 环境变量或 LaunchOptions::binary 显式指定）"
    )]
    BinaryNotFound,
    /// 创建临时用户数据目录失败。
    #[error("创建临时目录失败: {0}")]
    TempDir(#[from] std::io::Error),
    /// spawn 失败。
    #[error("spawn 失败: {0}")]
    Spawn(String),
    /// 等待 DevTools 端点超时。
    #[error("等待 DevTools 端点超时（{0:?}）")]
    Timeout(Duration),
    /// 进程提前退出，未输出 DevTools 端点。
    #[error("chromium 进程提前退出: {0}")]
    Exited(String),
}

/// 解析出的 DevTools 端点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevToolsEndpoint {
    /// 浏览器级 WebSocket 端点（`ws://127.0.0.1:<port>/devtools/browser/<id>`）。
    pub ws_url: String,
    /// 调试端口。
    pub port: Option<u16>,
}

/// 启动选项。
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// 显式可执行文件路径；`None` 时依次尝试 `BROWSER_PATH` 环境变量与 PATH 探测。
    pub binary: Option<PathBuf>,
    /// 是否追加 `--no-sandbox`；`None` 时读取 `BROWSER_NO_SANDBOX` 环境变量。
    pub no_sandbox: Option<bool>,
    /// 等待 DevTools 端点的超时；默认 15s。
    pub timeout: Option<Duration>,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            binary: None,
            no_sandbox: None,
            timeout: None,
        }
    }
}

impl LaunchOptions {
    fn no_sandbox(&self) -> bool {
        match self.no_sandbox {
            Some(v) => v,
            None => env_flag("BROWSER_NO_SANDBOX"),
        }
    }

    fn timeout(&self) -> Duration {
        self.timeout.unwrap_or(Duration::from_secs(15))
    }
}

/// 环境变量布尔开关：`1` / `true` / `on` / `yes`（大小写不敏感）视为开启。
fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        )
    })
}

/// PATH 候选浏览器可执行文件名。
const BROWSER_CANDIDATES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "chrome",
    "msedge",
    "microsoft-edge",
];

/// 在 `path_list`（unix 用 `:` 分隔，Windows 用 `;`）中查找可执行文件。
fn find_on_path(name: &str, path_list: &str) -> Option<PathBuf> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    path_list
        .split(sep)
        .map(Path::new)
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// 解析浏览器可执行文件：显式路径 → `BROWSER_PATH` → PATH 探测。
fn resolve_binary(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Ok(p) = std::env::var("BROWSER_PATH") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let path = std::env::var("PATH").unwrap_or_default();
    BROWSER_CANDIDATES
        .iter()
        .find_map(|name| find_on_path(name, &path))
}

/// 解析 chromium stderr 的 `DevTools listening on ws://…` 行。
#[must_use]
pub fn parse_devtools_line(line: &str) -> Option<DevToolsEndpoint> {
    const MARKER: &str = "DevTools listening on ";
    let rest = line.find(MARKER)?;
    let ws_url = line[rest + MARKER.len()..].trim();
    let url = url::Url::parse(ws_url).ok()?;
    if !matches!(url.scheme(), "ws" | "wss") || url.host_str().is_none() {
        return None;
    }
    Some(DevToolsEndpoint {
        ws_url: ws_url.to_string(),
        port: url.port(),
    })
}

/// 构造 chromium 启动参数（纯函数，供测试）。
#[must_use]
pub fn build_args(data_dir: &Path, no_sandbox: bool) -> Vec<String> {
    let mut args = vec![
        "--headless=new".to_string(),
        "--remote-debugging-port=0".to_string(),
        format!("--user-data-dir={}", data_dir.display()),
    ];
    if no_sandbox {
        args.push("--no-sandbox".to_string());
    }
    args
}

/// 已启动的 chromium 进程句柄。
pub struct BrowserProcess {
    child: Option<tokio::process::Child>,
    endpoint: DevToolsEndpoint,
    /// 临时用户数据目录（保持存活到进程关闭，随 drop 自动清理）。
    _data_dir: tempfile::TempDir,
}

impl BrowserProcess {
    /// 启动 headless chromium 并等待 DevTools 端点就绪。
    ///
    /// # Errors
    /// 二进制未找到 / spawn 失败 / 等待端点超时 / 进程提前退出时返回 [`LaunchError`]。
    pub async fn launch(opts: LaunchOptions) -> Result<Self, LaunchError> {
        let binary = resolve_binary(opts.binary.as_deref()).ok_or(LaunchError::BinaryNotFound)?;
        let data_dir = tempfile::tempdir()?;
        let args = build_args(data_dir.path(), opts.no_sandbox());
        tracing::debug!(binary = %binary.display(), args = ?args, "启动 headless chromium");

        let mut cmd = tokio::process::Command::new(&binary);
        cmd.args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        // 独立进程组：close 时对整树（含 zygote/renderer/GPU 子进程）统一击杀
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| LaunchError::Spawn(e.to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| LaunchError::Spawn("stderr 不可用".into()))?;
        let endpoint = match read_devtools_endpoint(stderr, opts.timeout()).await {
            Ok(ep) => ep,
            Err(e) => {
                // 端点未就绪即失败：回收进程，避免孤儿
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(e);
            }
        };
        Ok(Self {
            child: Some(child),
            endpoint,
            _data_dir: data_dir,
        })
    }

    /// 浏览器级 DevTools WebSocket 端点。
    #[must_use]
    pub fn ws_url(&self) -> &str {
        &self.endpoint.ws_url
    }

    /// 调试端口。
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.endpoint.port
    }

    /// 子进程 PID。
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(tokio::process::Child::id)
    }

    /// 关闭浏览器：unix 先 SIGTERM 进程组，宽限 2s 后 SIGKILL；Windows 直接 kill 子进程。
    ///
    /// # Errors
    /// 清理过程几乎不会失败；保留 `Result` 以便未来扩展。
    pub async fn close(mut self) -> Result<(), LaunchError> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        #[cfg(unix)]
        {
            if let Some(pid) = child.id() {
                let group = nix::unistd::Pid::from_raw(pid as i32);
                let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGTERM);
                let grace = tokio::time::sleep(Duration::from_secs(2));
                tokio::pin!(grace);
                tokio::select! {
                    _ = child.wait() => return Ok(()),
                    _ = &mut grace => {
                        let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
                        let _ = child.wait().await;
                        return Ok(());
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        Ok(())
    }
}

impl Drop for BrowserProcess {
    /// 兜底：未显式 close 时同步击杀进程组，避免孤儿 chromium。
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.start_kill();
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

/// 逐行读 stderr，等待 DevTools 端点（带超时）。
async fn read_devtools_endpoint(
    mut stderr: tokio::process::ChildStderr,
    timeout: Duration,
) -> Result<DevToolsEndpoint, LaunchError> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut lines = BufReader::new(&mut stderr).lines();
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return Err(LaunchError::Timeout(timeout)),
            line = lines.next_line() => {
                let line = line.map_err(|e| LaunchError::Spawn(format!("读取 stderr 失败: {e}")))?;
                match line {
                    Some(l) => {
                        if let Some(ep) = parse_devtools_line(&l) {
                            tracing::info!(ws = %ep.ws_url, port = ?ep.port, "DevTools 端点就绪");
                            return Ok(ep);
                        }
                        tracing::debug!(line = %l, "chromium stderr");
                    }
                    None => return Err(LaunchError::Exited("进程退出且未输出 DevTools 端点".into())),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_contains_headless_and_debug_port() {
        let dir = Path::new("/tmp/fake-profile");
        let args = build_args(dir, false);
        assert!(args.iter().any(|a| a == "--headless=new"));
        assert!(args.iter().any(|a| a == "--remote-debugging-port=0"));
        assert!(
            args.iter()
                .any(|a| a.starts_with("--user-data-dir=") && a.ends_with("fake-profile"))
        );
        assert!(
            !args.iter().any(|a| a == "--no-sandbox"),
            "默认不应带 --no-sandbox"
        );

        let args2 = build_args(dir, true);
        assert!(
            args2.iter().any(|a| a == "--no-sandbox"),
            "开启后应带 --no-sandbox"
        );
    }

    #[test]
    fn parse_devtools_line_extracts_endpoint() {
        let ep = parse_devtools_line(
            "DevTools listening on ws://127.0.0.1:9222/devtools/browser/abc123",
        )
        .unwrap();
        assert_eq!(ep.ws_url, "ws://127.0.0.1:9222/devtools/browser/abc123");
        assert_eq!(ep.port, Some(9222));

        assert!(parse_devtools_line("random stderr line").is_none());
        assert!(parse_devtools_line("DevTools listening on http://127.0.0.1:1/x").is_none());
        assert!(parse_devtools_line("DevTools listening on ").is_none());
    }

    #[test]
    fn find_on_path_resolves_executable() {
        let dir = std::env::temp_dir().join(format!(
            "agent-browser-probe-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("chromium");
        std::fs::write(&exe, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
            let found = find_on_path("chromium", &format!("{}:/nonexistent", dir.display()))
                .expect("应命中可执行文件");
            assert_eq!(found, exe);

            // 去掉执行位后不应命中
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(find_on_path("chromium", &format!("{}:/nonexistent", dir.display())).is_none());
        }
        #[cfg(not(unix))]
        {
            let found = find_on_path("chromium", &format!("{}:/nonexistent", dir.display()))
                .expect("应命中文件");
            assert_eq!(found, exe);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 伪 chromium：打印 DevTools 行后长驻，并记录自身与孙进程 pid 供整树击杀断言。
    #[cfg(unix)]
    #[tokio::test]
    async fn fake_chromium_launch_parse_and_tree_kill() {
        let dir = std::env::temp_dir().join(format!(
            "agent-browser-fake-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("self.pid");
        let grand_file = dir.join("grand.pid");
        let script = dir.join("chromium");
        let body = format!(
            "#!/bin/sh\nsleep 60 &\necho $! > {grand}\necho $$ > {pid}\n\
             echo 'DevTools listening on ws://127.0.0.1:43210/devtools/browser/fake' >&2\nsleep 60\n",
            grand = grand_file.display(),
            pid = pid_file.display(),
        );
        std::fs::write(&script, body).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let proc = BrowserProcess::launch(LaunchOptions {
            binary: Some(script),
            no_sandbox: Some(false),
            timeout: Some(Duration::from_secs(10)),
        })
        .await
        .expect("伪 chromium 应可启动");
        assert_eq!(proc.ws_url(), "ws://127.0.0.1:43210/devtools/browser/fake");
        assert_eq!(proc.port(), Some(43210));
        proc.close().await.expect("close 应成功");

        // 进程树整树击杀：主进程与 sleep 孙进程都应已消失
        let main_pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let grand_pid = std::fs::read_to_string(&grand_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        for pid in [main_pid, grand_pid] {
            // 孙进程短暂为僵尸态等待 init 回收，轮询容忍该窗口
            let mut dead = false;
            for _ in 0..20 {
                if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
                    dead = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(dead, "pid {pid} 应已被击杀");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
