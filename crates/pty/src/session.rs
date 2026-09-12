//! PTY 会话：用 `portable-pty` 在伪终端中执行命令，使 TTY 依赖命令（top/vim/tmux 等）可运行。
//!
//! 移植自 [`oh-my-pi pi-shell`](../../../third/oh-my-pi/crates/pi-shell/src/shell.rs) 的运行模型
//! （原版基于 `brush` 完整 shell；本实现用 portable-pty 提供**一次性 PTY 执行** + **持久会话**）。
//!
//! 设计：
//! - [`run_pty_command`]：一次性执行 `<command>; printf MARKER_%d`，读到 EOF，解析退出码。
//!   跨平台：Unix posix openpty，Windows `ConPTY`。
//! - [`PtyShell`]：持久 shell（`stty -echo` + 唯一 marker 协议），跨命令保持 cwd/环境。

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use agent_core::forced_utf8_locale;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::Mutex;

const MARKER_PREFIX: &str = "__AGENT_PTY_EXIT_";

/// PTY 输出累积上限（超出即停止读取，防 OOM）。
const PTY_MAX_OUTPUT: usize = 8 * 1024 * 1024; // 8 MiB
/// 持久 shell 单命令墙钟超时（超时即杀 shell，fail-closed）。
const PTY_RUN_TIMEOUT: Duration = Duration::from_mins(1);

/// 把 portable-pty 的 anyhow 风格错误转为 [`io::Error`]。
fn pty_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

/// 一次性 PTY 命令选项。
#[derive(Debug, Clone)]
pub struct PtyOptions {
    /// 命令字符串（交给 `sh -c`）。
    pub command: String,
    /// 工作目录。
    pub cwd: Option<PathBuf>,
    /// 额外环境变量。
    pub env: HashMap<String, String>,
    /// 超时（毫秒）。
    pub timeout_ms: Option<u64>,
    /// 终端行数。
    pub rows: u16,
    /// 终端列数。
    pub cols: u16,
}

impl Default for PtyOptions {
    fn default() -> Self {
        Self {
            command: String::new(),
            cwd: None,
            env: HashMap::new(),
            timeout_ms: None,
            rows: 24,
            cols: 80,
        }
    }
}

/// PTY 执行结果。
#[derive(Debug, Clone, Default)]
pub struct PtyResult {
    /// 合并的 stdout/stderr（已去回声 marker、归一化换行、剥离 ANSI）。
    pub output: String,
    /// 退出码。
    pub exit_code: Option<i32>,
    /// 是否超时。
    pub timed_out: bool,
}

/// 在 PTY 中一次性执行命令。
///
/// # Errors
/// PTY 创建、spawn 或读取底层失败时返回 IO 错误。
pub async fn run_pty_command(opts: &PtyOptions) -> Result<PtyResult, io::Error> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: opts.rows,
            cols: opts.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(pty_err)?;

    let marker = format!("{MARKER_PREFIX}{}_", next_id());
    let script = format!("{}\nprintf '\\n{marker}%d\\n' $?\n", opts.command);

    let mut cmd = CommandBuilder::new(shell_program());
    cmd.arg("-c");
    cmd.arg(&script);
    if let Some(cwd) = &opts.cwd {
        cmd.cwd(cwd);
    }
    for (k, v) in &opts.env {
        cmd.env(k, v);
    }
    // 保障 UTF-8 输出（与 run_command 一致）：继承 locale 非 UTF-8 时注入 C.UTF-8，
    // 避免 PTY 子进程以 GBK 等编码输出中文，捕获后解码产生乱码。
    if let Some(loc) = forced_utf8_locale() {
        cmd.env("LC_ALL", loc);
        cmd.env("LANG", loc);
    }

    let child = pair.slave.spawn_command(cmd).map_err(pty_err)?;
    let reader = pair.master.try_clone_reader().map_err(pty_err)?;
    // 关闭 slave 句柄，使子进程退出后 reader 收到 EOF
    drop(pair.slave);
    drop(pair.master);

    let child = Arc::new(Mutex::new(child));
    let killer = Arc::clone(&child);

    let mut read_fut = tokio::task::spawn_blocking(move || read_to_eof(reader));

    let timed_out;
    let bytes = if let Some(ms) = opts.timeout_ms {
        let dur = Duration::from_millis(ms);
        tokio::select! {
            res = &mut read_fut => {
                timed_out = false;
                res.map_err(|e| io::Error::other(format!("pty 读取失败: {e}")))?
            }
            () = tokio::time::sleep(dur) => {
                // 超时：杀子进程 → reader 收 EOF → 阻塞读取线程解除
                {
                    let mut c = killer.lock().await;
                    let _ = c.kill();
                }
                timed_out = true;
                (&mut read_fut).await.unwrap_or_default()
            }
        }
    } else {
        timed_out = false;
        read_fut
            .await
            .map_err(|e| io::Error::other(format!("pty 读取失败: {e}")))?
    };

    // 子进程退出状态（reader EOF 后通常已退出）
    let success = {
        let c = Arc::clone(&child);
        tokio::task::spawn_blocking(move || {
            let mut guard = c.blocking_lock();
            guard.wait().is_ok_and(|s| s.success())
        })
        .await
        .unwrap_or(false)
    };

    let raw = String::from_utf8_lossy(&bytes);
    let (cleaned, parsed_exit) = strip_marker(&raw, &marker);
    let output = normalize_output(&cleaned);

    let exit_code = if timed_out {
        None
    } else {
        parsed_exit.or_else(|| Some(i32::from(!success)))
    };

    Ok(PtyResult {
        output,
        exit_code,
        timed_out,
    })
}

/// 持久 PTY Shell：跨命令保持 cwd / 环境变量。
///
/// 协议：启动即发 `stty -echo`（关闭输入回声），每条命令后发唯一 marker；
/// 读到匹配 `marker_<id>_<exit>` 的行即认为该命令结束。
pub struct PtyShell {
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    reader: Arc<Mutex<Box<dyn Read + Send>>>,
    /// 持久 shell 子进程（std Mutex：仅在 Drop 中同步 kill+wait，不跨 await）。
    _child: std::sync::Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    counter: AtomicU64,
    /// 会话失效标记：run 超时杀 shell 后置位，后续 run 直接报错而非死锁于 reader 锁。
    poisoned: Arc<AtomicBool>,
}

impl PtyShell {
    /// 在 `cwd` 启动持久 shell。
    ///
    /// # Errors
    /// PTY 创建/spawn 失败时返回 IO 错误。
    pub async fn spawn(cwd: Option<&Path>) -> Result<Self, io::Error> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(pty_err)?;

        let mut cmd = CommandBuilder::new(shell_program());
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        // 保障 UTF-8 输出（持久 shell 同样需要：env 注入会被交互式 shell 继承）。
        if let Some(loc) = forced_utf8_locale() {
            cmd.env("LC_ALL", loc);
            cmd.env("LANG", loc);
        }
        let child = pair.slave.spawn_command(cmd).map_err(pty_err)?;
        let writer = pair.master.take_writer().map_err(pty_err)?;
        let reader = pair.master.try_clone_reader().map_err(pty_err)?;
        drop(pair.slave);
        drop(pair.master);

        let shell = Self {
            writer: Arc::new(Mutex::new(writer)),
            reader: Arc::new(Mutex::new(reader)),
            _child: std::sync::Mutex::new(child),
            counter: AtomicU64::new(1),
            poisoned: Arc::new(AtomicBool::new(false)),
        };

        // 初始化：关回声 + 清提示符（避免交互式 shell 的 PS1 污染输出）+ 同步点。
        let init = "stty -echo 2>/dev/null\nPS1=''\nPROMPT_COMMAND=''\nprintf '\\n__AGENT_PTY_READY_0_\\n'\n";
        {
            let mut w = shell.writer.lock().await;
            w.write_all(init.as_bytes())?;
            w.flush()?;
        }
        let _ = shell.read_until_marker("__AGENT_PTY_READY_0_").await?;

        Ok(shell)
    }

    /// 执行一条命令，返回输出与退出码（跨命令保持状态）。
    ///
    /// # Errors
    /// 会话失效或底层读写失败。
    pub async fn run(&self, command: &str) -> Result<PtyResult, io::Error> {
        self.run_with_timeout(command, None).await
    }

    /// 会话是否已失效（此前命令超时被杀，需重建）。
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }

    /// [`PtyShell::run`] 的显式超时变体（`None` = 默认 [`PTY_RUN_TIMEOUT`]）。
    ///
    /// 超时即杀掉持久 shell 并置失效标记（fail-closed）——调用方应丢弃本会话并重建，
    /// 而不是继续复用（reader 锁可能仍被阻塞读持有）。
    ///
    /// # Errors
    /// 会话失效或底层读写失败。
    pub async fn run_with_timeout(
        &self,
        command: &str,
        timeout: Option<Duration>,
    ) -> Result<PtyResult, io::Error> {
        // 失效会话不可复用：此前命令超时杀 shell 后 reader 锁可能仍被旧阻塞读持有，
        // 继续调用将死锁——直接 fail-fast。
        if self.poisoned.load(Ordering::SeqCst) {
            return Err(io::Error::other(
                "PTY 会话已失效（此前命令超时被杀），需新建 PtyShell",
            ));
        }
        let id = self.counter.fetch_add(1, Ordering::SeqCst);
        let marker = format!("{MARKER_PREFIX}{id}_");
        let script = format!("{command}\nprintf '\\n{marker}%d\\n' $?\n");
        {
            let mut w = self.writer.lock().await;
            w.write_all(script.as_bytes())?;
            w.flush()?;
        }
        // 墙钟超时：防止交互式命令（vim/top/挂起）永久阻塞读线程。
        // 超时即杀掉持久 shell（fail-closed），会话随后不可复用，需新建 PtyShell。
        let (cleaned, mut exit, found) = if let Ok(res) = tokio::time::timeout(
            timeout.unwrap_or(PTY_RUN_TIMEOUT),
            self.read_until_marker(&marker),
        )
        .await
        {
            res?
        } else {
            if let Ok(mut child) = self._child.lock() {
                let _ = child.kill();
            }
            // 标记失效：防止后续 run 在 reader 锁上死锁。
            self.poisoned.store(true, Ordering::SeqCst);
            return Ok(PtyResult {
                output: String::new(),
                exit_code: None,
                timed_out: true,
            });
        };
        if !found {
            // 未读到 marker：两种可能——
            // (a) shell 已退出（`exit N`）：PTY master 读到 EOF，此时向子进程回收真实退出码；
            // (b) 输出超过 PTY_MAX_OUTPUT 上限：shell 仍活着但流已失步（残余输出会被下一条
            //     命令误读为自己的输出）。
            // 两者都必须置失效（fail-closed），区别仅在 (a) 能报出真实退出码。
            exit = self.reap_exit_code().or(exit);
            self.poisoned.store(true, Ordering::SeqCst);
        }
        Ok(PtyResult {
            output: normalize_output(&cleaned),
            exit_code: exit,
            timed_out: false,
        })
    }

    /// 回收持久 shell 子进程的退出码（仅在已读到 EOF 时调用）。
    ///
    /// EOF 表示 slave 端已关闭，但子进程可能尚未被 wait 到，故做有界重试（约 100ms）。
    fn reap_exit_code(&self) -> Option<i32> {
        let mut child = self._child.lock().ok()?;
        for _ in 0..10 {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status.exit_code() as i32),
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => {
                    tracing::warn!(target: "pty", "回收持久 shell 退出码失败: {e}");
                    return None;
                }
            }
        }
        None
    }

    /// 读到 marker 出现即返回（不等到 EOF）；返回 `(正文, 退出码, 是否命中 marker)`。
    ///
    /// `found == false` 意味着在读到 marker 前就退出循环（EOF 或输出超限），调用方必须据此
    /// 判定会话失效，见 [`PtyShell::run_with_timeout`]。
    async fn read_until_marker(
        &self,
        marker: &str,
    ) -> Result<(String, Option<i32>, bool), io::Error> {
        let reader = Arc::clone(&self.reader);
        let marker_owned = marker.to_string();
        let bytes = tokio::task::spawn_blocking(move || {
            // 字节级滑动窗口搜索：仅在「新增区段 + marker 长度重叠」内查找，
            // 复杂度 O(总字节 × marker 长度)，避免旧实现对整段 acc 反复 from_utf8_lossy +
            // contains 的 O(n²) 退化（大输出时 CPU 打满、耗时数十秒）。
            // 匹配「行首 marker」（`\n` + marker）：真实 marker 前有 printf 输出的换行，
            // 而**回显的命令行**里 marker 前缀只会出现在 `printf '\n<marker>...` 中间，
            // 前面是空格而非换行——按行首匹配即可避免把回显误判为完成（持久 shell 的
            // `stty -echo` 在某些 shell/终端组合下不生效，必须对回显免疫）。
            let mut m = Vec::with_capacity(marker_owned.len() + 1);
            m.push(b'\n');
            m.extend_from_slice(marker_owned.as_bytes());
            let mlen = m.len();
            let mut acc: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            let mut r = reader.blocking_lock();
            let mut found = false;
            loop {
                let prev_len = acc.len();
                match r.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        acc.extend_from_slice(&tmp[..n]);
                        // 仅当累积数据 ≥ marker 长度时才搜索，避免索引越界 panic。
                        if acc.len() >= mlen {
                            // 搜索起点回退 mlen-1 字节以覆盖跨 chunk 边界的 marker。
                            let start = prev_len.saturating_sub(mlen.saturating_sub(1));
                            let end = acc.len().saturating_sub(mlen);
                            // start > end 时 range 为空，.any() 返回 false（安全）。
                            let hit = (start..=end).any(|i| acc[i..i + mlen] == m[..]);
                            if hit || acc.len() >= PTY_MAX_OUTPUT {
                                found = hit;
                                break;
                            }
                        } else if acc.len() >= PTY_MAX_OUTPUT {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        tracing::warn!(target: "pty", "读取错误（返回已读部分）: {e}");
                        break;
                    }
                }
            }
            (acc, found)
        })
        .await
        .map_err(|e| io::Error::other(format!("read join: {e}")))?;

        let (bytes, found) = bytes;
        let raw = String::from_utf8_lossy(&bytes);
        let (cleaned, exit) = strip_marker(&raw, marker);
        Ok((cleaned, exit, found))
    }
}

impl Drop for PtyShell {
    fn drop(&mut self) {
        // 回收持久 shell 子进程，避免僵尸/孤儿（portable-pty Child 不会自动 kill）。
        if let Ok(child) = self._child.get_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn read_to_eof(mut reader: Box<dyn Read + Send>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 4096];
    let mut capped = false;
    loop {
        match reader.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                if !capped {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.len() >= PTY_MAX_OUTPUT {
                        buf.truncate(PTY_MAX_OUTPUT);
                        capped = true;
                    }
                }
                // capped 后继续读丢弃：保持 PTY 排空，子进程不阻塞在写端。
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!(target: "pty", "read_to_eof 错误（返回已读部分）: {e}");
                break;
            }
        }
    }
    buf
}

/// 去掉尾部 marker 行，返回 (正文, 退出码)。
fn strip_marker(text: &str, marker: &str) -> (String, Option<i32>) {
    let needle = format!("\n{marker}");
    if let Some(idx) = text.rfind(&needle) {
        let body = &text[..idx];
        let tail = &text[idx + needle.len()..];
        let code = tail
            .trim_start_matches(['\r', '\n'])
            .lines()
            .next()
            .and_then(|line| line.trim().parse::<i32>().ok());
        return (body.to_string(), code);
    }
    (text.to_string(), None)
}

/// 归一化输出：`\r\n` → `\n`，剥离基本 ANSI CSI 序列，丢弃裸 CR。
fn normalize_output(s: &str) -> String {
    let without_crlf = s.replace("\r\n", "\n");
    strip_ansi(&without_crlf)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && matches!(chars.peek(), Some('[')) {
            chars.next(); // consume '['
            for cc in chars.by_ref() {
                if cc.is_ascii() && (0x40..=0x7e).contains(&(cc as u32)) {
                    break;
                }
            }
        } else if c == '\r' {
            // 裸 CR：丢弃
        } else {
            out.push(c);
        }
    }
    out
}

fn shell_program() -> String {
    // 用户显式覆盖优先（三平台一致）。
    if let Ok(s) = std::env::var("GYRE_SHELL")
        && !s.trim().is_empty()
    {
        return s;
    }
    #[cfg(unix)]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
    #[cfg(windows)]
    {
        windows_shell_program()
    }
}

/// Windows PTY shell 发现链（对齐上游 `resolveWindowsShell`，procmgr.ts:130-176）：
/// `GIT_INSTALL_ROOT` → Program Files/MinGit → scoop → LocalAppData →
/// PATH 上的 `bash.exe`/`sh.exe`（Cygwin/MSYS2 若在 PATH 即被覆盖）→ `cmd` 兜底。
#[cfg(windows)]
fn windows_shell_program() -> String {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(root) = std::env::var("GIT_INSTALL_ROOT") {
        let root = PathBuf::from(root);
        candidates.push(root.join("bin").join("bash.exe"));
        candidates.push(root.join("usr").join("bin").join("bash.exe"));
    }
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(dir) = std::env::var(var) {
            for sub in ["Git", "MinGit"] {
                let base = PathBuf::from(&dir).join(sub);
                candidates.push(base.join("bin").join("bash.exe"));
                candidates.push(base.join("usr").join("bin").join("bash.exe"));
            }
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        let home = PathBuf::from(home);
        candidates.push(
            home.join("scoop")
                .join("apps")
                .join("git")
                .join("current")
                .join("bin")
                .join("bash.exe"),
        );
    }
    if let Ok(lad) = std::env::var("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(lad)
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            candidates.push(dir.join("bash.exe"));
            candidates.push(dir.join("sh.exe"));
        }
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .map_or_else(|| "cmd".to_string(), |p| p.to_string_lossy().into_owned())
}

fn next_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_marker_extracts_exit() {
        let text = "hello\n__AGENT_PTY_EXIT_42_0\n";
        let (body, code) = strip_marker(text, "__AGENT_PTY_EXIT_42_");
        assert_eq!(body, "hello");
        assert_eq!(code, Some(0));
    }

    #[test]
    fn ansi_and_crlf_normalized() {
        let raw = "a\r\nb\x1b[32mc\x1b[0m\r\nd";
        let out = normalize_output(raw);
        assert_eq!(out, "a\nbc\nd");
        assert!(!out.contains('\u{1b}'));
        assert!(!out.contains('\r'));
    }

    /// H45：`exit N` 结束持久 shell 时，必须从子进程回收**真实**退出码并置失效，
    /// 而不是因缺 marker 而退化为 `None`（-1），更不能让后续 run 在 reader 锁上死锁。
    #[tokio::test]
    async fn shell_exit_reports_child_code_and_poisons() {
        let Ok(shell) = PtyShell::spawn(None).await else {
            eprintln!("skipping: pty unavailable");
            return;
        };
        let res = shell.run("exit 7").await.expect("run");
        assert_eq!(res.exit_code, Some(7), "应回收子进程退出码: {res:?}");
        assert!(shell.is_poisoned(), "shell 退出后会话必须置失效");
        // 失效会话必须 fail-fast（报错），而不是死锁或静默复用。
        assert!(shell.run("echo hi").await.is_err(), "失效会话应拒绝复用");
    }

    #[tokio::test]
    async fn one_shot_echo() {
        let opts = PtyOptions {
            command: "printf hi".into(),
            ..Default::default()
        };
        let res = run_pty_command(&opts).await;
        let Ok(res) = res else {
            eprintln!("skipping: pty unavailable");
            return;
        };
        assert!(res.output.contains("hi"));
        assert_eq!(res.exit_code, Some(0));
    }
}
