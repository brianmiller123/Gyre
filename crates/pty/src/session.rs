//! PTY 会话：用 `portable-pty` 在伪终端中执行命令，使 TTY 依赖命令（top/vim/tmux 等）可运行。
//!
//! 移植自 [`oh-my-pi pi-shell`](../../../third/oh-my-pi/crates/pi-shell/src/shell.rs) 的运行模型
//! （原版基于 `brush` 完整 shell；本实现用 portable-pty 提供**一次性 PTY 执行** + **持久会话**）。
//!
//! 设计：
//! - [`run_pty_command`]：一次性执行 `<command>; printf MARKER_%d`，读到 EOF，解析退出码。
//!   跨平台：Unix posix openpty，Windows ConPTY。
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
const PTY_RUN_TIMEOUT: Duration = Duration::from_secs(60);

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
            _ = tokio::time::sleep(dur) => {
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
        read_fut.await.map_err(|e| io::Error::other(format!("pty 读取失败: {e}")))?
    };

    // 子进程退出状态（reader EOF 后通常已退出）
    let success = {
        let c = Arc::clone(&child);
        tokio::task::spawn_blocking(move || {
            let mut guard = c.blocking_lock();
            guard.wait().map_or(false, |s| s.success())
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
        parsed_exit.or_else(|| Some(if success { 0 } else { 1 }))
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

        // 初始化：关回声 + 同步点
        let init = "stty -echo 2>/dev/null\nprintf '\\n__AGENT_PTY_READY_0_\\n'\n";
        {
            let mut w = shell.writer.lock().await;
            w.write_all(init.as_bytes())?;
            w.flush()?;
        }
        shell.read_until_marker("__AGENT_PTY_READY_0_").await?;

        Ok(shell)
    }

    /// 执行一条命令，返回输出与退出码（跨命令保持状态）。
    pub async fn run(&self, command: &str) -> Result<PtyResult, io::Error> {
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
        let (cleaned, exit) = match tokio::time::timeout(PTY_RUN_TIMEOUT, self.read_until_marker(&marker)).await {
            Ok(res) => res?,
            Err(_) => {
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
            }
        };
        Ok(PtyResult {
            output: normalize_output(&cleaned),
            exit_code: exit,
            timed_out: false,
        })
    }

    /// 读到 marker 出现即返回（不等到 EOF）；返回 (正文, 退出码)。
    async fn read_until_marker(&self, marker: &str) -> Result<(String, Option<i32>), io::Error> {
        let reader = Arc::clone(&self.reader);
        let marker_owned = marker.to_string();
        let bytes = tokio::task::spawn_blocking(move || {
            // 字节级滑动窗口搜索：仅在「新增区段 + marker 长度重叠」内查找，
            // 复杂度 O(总字节 × marker 长度)，避免旧实现对整段 acc 反复 from_utf8_lossy +
            // contains 的 O(n²) 退化（大输出时 CPU 打满、耗时数十秒）。
            let m = marker_owned.as_bytes();
            let mlen = m.len();
            let mut acc: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            let mut r = reader.blocking_lock();
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
                            let found = (start..=end).any(|i| &acc[i..i + mlen] == m);
                            if found || acc.len() >= PTY_MAX_OUTPUT {
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
            acc
        })
        .await
        .map_err(|e| io::Error::other(format!("read join: {e}")))?;

        let raw = String::from_utf8_lossy(&bytes);
        let (cleaned, exit) = strip_marker(&raw, marker);
        Ok((cleaned, exit))
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
    #[cfg(unix)]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
    #[cfg(windows)]
    {
        "cmd".to_string()
    }
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
