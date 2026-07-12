//! 通用回退隔离：git worktree（有 .git）或递归复制。
//!
//! 移植自 [`pi_iso::rcopy`](https://github.com/can1357/oh-my-pi)。
//!
//! ## 行为
//!
//! - **Git worktree 路径**：`lower` 有 `.git` 时，用 `git worktree add --detach`
//!   创建轻量 checkout，然后种子 dirty state（staged/unstaged/untracked）
//!   使 `merged` 镜像 `lower` 的实时工作树。
//! - **非 git 路径**：递归复制，保留 mode + mtime（优化 diff 速度）。
//!
//! ## 生命周期
//!
//! `start` → agent 在 `merged` 中工作 → `stop`（`git worktree remove` + `rm -rf`）

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::{BackendKind, IsoError, IsoResult, IsolationBackend, ProbeResult};

/// Rcopy backend（首期唯一可用）。
pub struct RcopyBackend;

#[async_trait]
impl IsolationBackend for RcopyBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Rcopy
    }

    fn probe(&self) -> ProbeResult {
        // 纯 stdlib 回退始终可用。即使 `git` 缺失，
        // 非 git 分支也不需要它；git 分支会在 start 时给出清晰的 unavailable 错误。
        ProbeResult::available()
    }

    fn start(&self, lower: &Path, merged: &Path) -> IsoResult<()> {
        let lower = canonical_existing_dir(lower)?;
        let merged = absolutize(merged);
        prepare_destination(&merged)?;
        if is_git_worktree(&lower) {
            git_worktree_add(&lower, &merged)?;
            // 种子 dirty state：使 merged 镜像 lower 的实时工作树
            seed_dirty_state(&lower, &merged)
        } else {
            recursive_copy(&lower, &merged)
        }
    }

    fn stop(&self, merged: &Path) -> IsoResult<()> {
        let merged = absolutize(merged);
        // 尽力而为：识别为注册 worktree 则用 git 移除
        if is_git_worktree(&merged) {
            let _ = git_worktree_remove(&merged);
        }
        match std::fs::remove_dir_all(&merged) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(IsoError::other(format!(
                "unable to remove {}: {err}",
                merged.display()
            ))),
        }
    }
}

// ── 路径工具 ─────────────────────────────────────────────────────────────────

fn canonical_existing_dir(path: &Path) -> IsoResult<PathBuf> {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    };
    let meta = std::fs::metadata(&resolved).map_err(|err| {
        IsoError::other(format!(
            "invalid rcopy source {}: {err}",
            resolved.display()
        ))
    })?;
    if !meta.is_dir() {
        return Err(IsoError::other(format!(
            "rcopy source {} is not a directory",
            resolved.display()
        )));
    }
    Ok(std::fs::canonicalize(&resolved).unwrap_or(resolved))
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    }
}

fn prepare_destination(merged: &Path) -> IsoResult<()> {
    if let Some(parent) = merged.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            IsoError::other(format!("create parent of {}: {err}", merged.display()))
        })?;
    }
    match std::fs::remove_dir_all(merged) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(IsoError::other(format!(
                "unable to clear {} before rcopy: {err}",
                merged.display()
            )));
        }
    }
    Ok(())
}

// ── Git worktree 路径 ────────────────────────────────────────────────────────

fn is_git_worktree(path: &Path) -> bool {
    // 普通工作树 `.git` 为目录；linked worktree 为 `gitdir: …` 文本文件。
    // 无论哪种，`.git` 存在即为 git 识别的信号。
    std::fs::symlink_metadata(path.join(".git")).is_ok()
}

fn git_worktree_add(lower: &Path, merged: &Path) -> IsoResult<()> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(lower)
        .args(["worktree", "add", "--detach"])
        .arg(merged)
        .arg("HEAD")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                IsoError::unavailable(
                    "`git` not on PATH; rcopy cannot materialise a worktree from a git source",
                )
            } else {
                IsoError::other(format!("spawn git worktree add: {err}"))
            }
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(IsoError::other(format!(
        "git worktree add (exit {}): {stderr}",
        output.status.code().unwrap_or(-1)
    )))
}

fn git_worktree_remove(merged: &Path) -> IsoResult<()> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(merged)
        .args(["worktree", "remove", "--force"])
        .arg(merged)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|err| IsoError::other(format!("spawn git worktree remove: {err}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(IsoError::other(format!(
        "git worktree remove (exit {}): {stderr}",
        output.status.code().unwrap_or(-1)
    )))
}

/// 将 `lower` 的实时工作树复制到 freshly-checked-out worktree `merged`。
///
/// 三遍扫描，镜像 `git status` 在 `lower` 的报告：
/// 1. **Staged** — `git diff --binary --cached` → apply 到 index + working tree
/// 2. **Unstaged** — `git diff --binary` → apply 到 working tree
/// 3. **Untracked** — `git ls-files --others --exclude-standard -z` → 递归 copy
fn seed_dirty_state(lower: &Path, merged: &Path) -> IsoResult<()> {
    let staged = git_capture(lower, &["diff", "--binary", "--no-color", "--cached"])?;
    if !staged.is_empty() {
        git_apply(merged, &staged, &["--cached"])?;
        git_apply(merged, &staged, &[])?;
    }

    let unstaged = git_capture(lower, &["diff", "--binary", "--no-color"])?;
    if !unstaged.is_empty() {
        git_apply(merged, &unstaged, &[])?;
    }

    let untracked = git_capture(lower, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for path_bytes in untracked.split(|b| *b == 0) {
        if path_bytes.is_empty() {
            continue;
        }
        let rel = std::str::from_utf8(path_bytes)
            .map_err(|err| IsoError::other(format!("untracked path is not valid UTF-8: {err}")))?;
        let src = lower.join(rel);
        let dst = merged.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| IsoError::other(format!("create {}: {err}", parent.display())))?;
        }
        copy_path(&src, &dst)?;
    }

    Ok(())
}

fn git_capture(cwd: &Path, args: &[&str]) -> IsoResult<Vec<u8>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                IsoError::unavailable(
                    "`git` not on PATH; rcopy cannot seed dirty state from a git source",
                )
            } else {
                IsoError::other(format!(
                    "spawn git {}: {err}",
                    args.first().unwrap_or(&"<args>")
                ))
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(IsoError::other(format!(
            "git {} (exit {}): {stderr}",
            args.join(" "),
            output.status.code().unwrap_or(-1)
        )));
    }
    Ok(output.stdout)
}

fn git_apply(cwd: &Path, patch: &[u8], extra: &[&str]) -> IsoResult<()> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["apply", "--binary", "--whitespace=nowarn"])
        .args(extra)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                IsoError::unavailable(
                    "`git` not on PATH; rcopy cannot seed dirty state from a git source",
                )
            } else {
                IsoError::other(format!("spawn git apply: {err}"))
            }
        })?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| IsoError::other("git apply: child stdin was not piped".to_string()))?;
        stdin
            .write_all(patch)
            .map_err(|err| IsoError::other(format!("write patch to git apply: {err}")))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| IsoError::other(format!("wait git apply: {err}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(IsoError::other(format!(
        "git apply (exit {}): {stderr}",
        output.status.code().unwrap_or(-1)
    )))
}

// ── 递归复制路径（非 git）─────────────────────────────────────────────────────

/// 递归复制，保留 mode + mtime（优化 diff）。
fn recursive_copy(lower: &Path, merged: &Path) -> IsoResult<()> {
    std::fs::create_dir_all(merged)
        .map_err(|err| IsoError::other(format!("create {}: {err}", merged.display())))?;
    copy_dir_contents(lower, merged)
}

fn copy_dir_contents(src: &Path, dst: &Path) -> IsoResult<()> {
    let entries = std::fs::read_dir(src)
        .map_err(|err| IsoError::other(format!("read_dir {}: {err}", src.display())))?;
    for entry in entries {
        let entry = entry
            .map_err(|err| IsoError::other(format!("dir entry in {}: {err}", src.display())))?;
        let file_type = entry.file_type().map_err(|err| {
            IsoError::other(format!("file_type {}: {err}", entry.path().display()))
        })?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_symlink() {
            copy_symlink(&src_path, &dst_path)?;
        } else if file_type.is_dir() {
            std::fs::create_dir_all(&dst_path)
                .map_err(|err| IsoError::other(format!("create {}: {err}", dst_path.display())))?;
            copy_dir_contents(&src_path, &dst_path)?;
            copy_dir_mtime(&src_path, &dst_path);
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|err| {
                IsoError::other(format!(
                    "copy {} -> {}: {err}",
                    src_path.display(),
                    dst_path.display()
                ))
            })?;
            copy_file_mtime(&src_path, &dst_path);
        }
    }
    Ok(())
}

/// 复制单个路径（文件/symlink/目录），用于 untracked files pass。
fn copy_path(src: &Path, dst: &Path) -> IsoResult<()> {
    let meta = std::fs::symlink_metadata(src)
        .map_err(|err| IsoError::other(format!("stat {}: {err}", src.display())))?;
    if meta.file_type().is_symlink() {
        copy_symlink(src, dst)
    } else if meta.file_type().is_dir() {
        std::fs::create_dir_all(dst)
            .map_err(|err| IsoError::other(format!("create {}: {err}", dst.display())))?;
        copy_dir_contents(src, dst)?;
        copy_dir_mtime(src, dst);
        Ok(())
    } else {
        std::fs::copy(src, dst).map_err(|err| {
            IsoError::other(format!(
                "copy {} -> {}: {err}",
                src.display(),
                dst.display()
            ))
        })?;
        copy_file_mtime(src, dst);
        Ok(())
    }
}

// ── mtime 保留（仅优化 diff，失败不报错）───────────────────────────────────────

fn copy_file_mtime(src: &Path, dst: &Path) {
    let Ok(meta) = std::fs::metadata(src) else {
        return;
    };
    let Ok(mtime) = meta.modified() else {
        return;
    };
    let _ = filetime_set(dst, mtime);
}

fn copy_dir_mtime(src: &Path, dst: &Path) {
    let Ok(meta) = std::fs::metadata(src) else {
        return;
    };
    let Ok(mtime) = meta.modified() else {
        return;
    };
    let _ = filetime_set(dst, mtime);
}

#[cfg(unix)]
fn filetime_set(path: &Path, mtime: std::time::SystemTime) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let dur = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let times = [
        libc::timespec {
            tv_sec: dur.as_secs() as libc::time_t,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: dur.as_secs() as libc::time_t,
            tv_nsec: libc::c_long::from(dur.subsec_nanos()),
        },
    ];
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: c_path 和 times 在 syscall 期间有效。
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn filetime_set(_path: &Path, _mtime: std::time::SystemTime) -> std::io::Result<()> {
    Ok(())
}

// ── symlink 复制 ─────────────────────────────────────────────────────────────

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> IsoResult<()> {
    let target = std::fs::read_link(src)
        .map_err(|err| IsoError::other(format!("read_link {}: {err}", src.display())))?;
    std::os::unix::fs::symlink(target, dst)
        .map_err(|err| IsoError::other(format!("symlink {}: {err}", dst.display())))
}

#[cfg(not(unix))]
fn copy_symlink(_src: &Path, _dst: &Path) -> IsoResult<()> {
    Err(IsoError::other("symlink copy unsupported on this platform"))
}
