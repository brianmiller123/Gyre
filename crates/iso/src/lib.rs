//! 跨平台文件系统隔离 PAL（Platform Abstraction Layer）。
//!
//! 移植自 oh-my-pi [`pi-iso`](https://github.com/can1357/oh-my-pi)，
//! 遵循相同的 `lower`（只读源）→ `merged`（可写视图）→ `diff`（变更捕获）契约。
//!
//! ## Backend 优先级（Linux）
//! btrfs → zfs → linux-reflink → overlayfs → rcopy
//!
//! ## 首期实现
//! 仅 [`Rcopy`](BackendKind::Rcopy)（git worktree / 递归复制），其余 backend
//! 预留 enum 变体与 trait 实现占位。

use std::{fmt, path::Path};

use async_trait::async_trait;

mod diff;
mod rcopy;

pub use diff::{ChangeKind, Diff, FileChange};

// ── 预留 backend 模块（P1） ──────────────────────────────────────────────────
// mod overlayfs;
// mod apfs;

/// 隔离后端标识。首期仅 [`Rcopy`] 可用，其余为预留变体供 P1 实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// macOS APFS `clonefile(2)` `CoW` 克隆（P1）。
    Apfs,
    /// Linux btrfs subvolume snapshot（P1）。
    Btrfs,
    /// ZFS dataset clone（P1）。
    Zfs,
    /// Linux `FICLONE` per-file reflink（P1）。
    LinuxReflink,
    /// Linux kernel overlay + fuse-overlayfs 回退（P1）。
    Overlayfs,
    /// Windows `FSCTL_DUPLICATE_EXTENTS_TO_FILE` block clone（P1）。
    WindowsBlockClone,
    /// Windows ProjFS（P1）。
    Projfs,
    /// 通用回退：git worktree（有 .git）或递归复制。✅ 首期已实现。
    Rcopy,
}

impl BackendKind {
    /// 稳定短标识符。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Apfs => "apfs",
            Self::Btrfs => "btrfs",
            Self::Zfs => "zfs",
            Self::LinuxReflink => "linux-reflink",
            Self::Overlayfs => "overlayfs",
            Self::WindowsBlockClone => "windows-block-clone",
            Self::Projfs => "projfs",
            Self::Rcopy => "rcopy",
        }
    }

    /// 从字符串解析。未知返回 `None`。
    ///
    /// 刻意不实现 [`std::str::FromStr`]：该 trait 要求返回 `Result`，
    /// 而此处“未知即 `None`”用 `Option` 表达更直接。
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "apfs" => Self::Apfs,
            "btrfs" => Self::Btrfs,
            "zfs" => Self::Zfs,
            "linux-reflink" | "reflink" => Self::LinuxReflink,
            "overlayfs" => Self::Overlayfs,
            "windows-block-clone" | "block-clone" => Self::WindowsBlockClone,
            "projfs" => Self::Projfs,
            "rcopy" => Self::Rcopy,
            _ => return None,
        })
    }

    /// 当前编译目标原生 backend（首期统一返回 Rcopy，P1 按平台返回最佳）。
    pub const fn native() -> Self {
        // P1: #[cfg(target_os = "macos")] { Self::Apfs }
        // P1: #[cfg(target_os = "linux")]  { Self::Overlayfs }
        // P1: #[cfg(windows)]             { Self::Projfs }
        Self::Rcopy
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── 错误类型 ─────────────────────────────────────────────────────────────────

/// 隔离操作错误。
///
/// `Unavailable` 表示 backend 前提条件缺失（如 `git` 不在 PATH），
/// 调用方可据此回退到下一个候选 backend。
#[derive(Debug, Clone)]
pub enum IsoError {
    /// Backend 不可用（可回退）。
    Unavailable(String),
    /// 其他硬错误。
    Other(String),
}

impl IsoError {
    /// 构造不可用错误。
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::Unavailable(msg.into())
    }

    /// 构造其他错误。
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    /// 是否为不可用错误。
    #[must_use]
    pub const fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    /// 错误消息文本。
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable(m) | Self::Other(m) => m,
        }
    }
}

impl fmt::Display for IsoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for IsoError {}

/// 隔离操作结果。
pub type IsoResult<T> = Result<T, IsoError>;

// ── Backend trait ────────────────────────────────────────────────────────────

/// Backend 契约：创建 writable `merged` 视图 → 工作 → 捕获变更 → 清理。
///
/// `start` / `stop` 为同步（底层 syscall 是阻塞的），
/// 调用方应从 `spawn_blocking` 中驱动。
/// [`diff`](IsolationBackend::diff) 为异步（重 I/O：walk 树、读文件、spawn git）。
#[async_trait]
pub trait IsolationBackend: Send + Sync {
    /// Backend 标识。
    fn kind(&self) -> BackendKind;

    /// 探测 backend 是否可用。
    fn probe(&self) -> ProbeResult;

    /// 创建隔离视图。`lower` 为只读源目录，`merged` 为可写目标路径。
    ///
    /// # Errors
    /// 路径无效、backend 不可用或系统调用失败时返回错误。
    fn start(&self, lower: &Path, merged: &Path) -> IsoResult<()>;

    /// 清理隔离视图并回收资源。
    ///
    /// # Errors
    /// 卸载/清理失败时返回错误。
    fn stop(&self, merged: &Path) -> IsoResult<()>;

    /// 捕获 `lower` 与 `merged` 之间的变更。
    ///
    /// 默认实现：git repo → `git diff`；否则 mtime-skipped tree walk。
    async fn diff(&self, lower: &Path, merged: &Path) -> IsoResult<Diff> {
        diff::default_diff(lower, merged).await
    }
}

// ── Probe ────────────────────────────────────────────────────────────────────

/// Backend 探测结果。
#[derive(Debug, Clone)]
pub struct ProbeResult {
    /// 是否可用。
    pub available: bool,
    /// 不可用原因（人类可读）。
    pub reason: Option<String>,
}

impl ProbeResult {
    /// 构造可用结果。
    pub const fn available() -> Self {
        Self {
            available: true,
            reason: None,
        }
    }

    /// 构造不可用结果。
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            reason: Some(reason.into()),
        }
    }
}

// ── Backend 查表 ─────────────────────────────────────────────────────────────

// ── 占位 backend（未实现的 [`BackendKind`]）─────────────────────────────────
// probe 诚实报告不可用，使 [`resolve`] 能据此回退到 `Rcopy` 并标记 `fell_back`；
// P1 逐个替换为真实实现。

/// 不可用占位 backend：诚实报告自身 `kind`，但 probe/start 失败。
struct StubBackend(BackendKind);

impl StubBackend {
    const fn new(kind: BackendKind) -> Self {
        Self(kind)
    }
}

#[async_trait]
impl IsolationBackend for StubBackend {
    fn kind(&self) -> BackendKind {
        self.0
    }

    fn probe(&self) -> ProbeResult {
        ProbeResult::unavailable(format!(
            "{} backend not yet implemented; resolves to rcopy fallback",
            self.0
        ))
    }

    fn start(&self, _lower: &Path, _merged: &Path) -> IsoResult<()> {
        Err(IsoError::unavailable(format!(
            "{} backend not yet implemented",
            self.0
        )))
    }

    fn stop(&self, _merged: &Path) -> IsoResult<()> {
        // 未实现 backend 从未 start，stop 视为无操作。
        Ok(())
    }
}

static STUB_APFS: StubBackend = StubBackend::new(BackendKind::Apfs);
static STUB_BTRFS: StubBackend = StubBackend::new(BackendKind::Btrfs);
static STUB_ZFS: StubBackend = StubBackend::new(BackendKind::Zfs);
static STUB_LINUX_REFLINK: StubBackend = StubBackend::new(BackendKind::LinuxReflink);
static STUB_OVERLAYFS: StubBackend = StubBackend::new(BackendKind::Overlayfs);
static STUB_WINDOWS_BLOCK_CLONE: StubBackend = StubBackend::new(BackendKind::WindowsBlockClone);
static STUB_PROJFS: StubBackend = StubBackend::new(BackendKind::Projfs);

/// 按 [`BackendKind`] 查 backend 实现。
///
/// 已实现的变体返回真实 backend；未实现的（`Apfs`/`Btrfs`/`Zfs`/
/// `LinuxReflink`/`Overlayfs`/`WindowsBlockClone`/`Projfs`）返回
/// [`StubBackend`]，其 [`probe`](IsolationBackend::probe) 诚实报告不可用，
/// 由 [`resolve`] 据此回退到 [`Rcopy`](BackendKind::Rcopy) 并标记 `fell_back`。
/// P1 将逐个替换 stub 为真实实现。
pub fn backend(kind: BackendKind) -> &'static dyn IsolationBackend {
    match kind {
        BackendKind::Rcopy => &rcopy::RcopyBackend,
        BackendKind::Apfs => &STUB_APFS,
        BackendKind::Btrfs => &STUB_BTRFS,
        BackendKind::Zfs => &STUB_ZFS,
        BackendKind::LinuxReflink => &STUB_LINUX_REFLINK,
        BackendKind::Overlayfs => &STUB_OVERLAYFS,
        BackendKind::WindowsBlockClone => &STUB_WINDOWS_BLOCK_CLONE,
        BackendKind::Projfs => &STUB_PROJFS,
    }
}

/// 默认 backend（当前编译目标原生）。
pub fn default_backend() -> &'static dyn IsolationBackend {
    backend(BackendKind::native())
}

/// 默认 backend 的 [`BackendKind`]。
#[must_use]
pub fn backend_kind() -> BackendKind {
    default_backend().kind()
}

// ── Resolve ──────────────────────────────────────────────────────────────────

/// Backend 解析结果。
#[derive(Debug, Clone)]
pub struct Resolution {
    /// 首选可用 backend。
    pub kind: BackendKind,
    /// 所有可用 backend（回退顺序）。
    pub candidates: Vec<BackendKind>,
    /// 是否从 preferred 回退。
    pub fell_back: bool,
    /// 第一个不可用 backend 的原因（如有）。
    pub reason: Option<String>,
}

/// 自动探测顺序（首期仅 rcopy）。
const AUTO_ORDER: &[BackendKind] = &[BackendKind::Rcopy];

/// 选择最佳可用 backend。
///
/// 1. 若 `preferred` 提供且可用 → 直接用。
/// 2. 否则按 [`AUTO_ORDER`] 遍历，跳过已探测的 preferred。
/// 3. [`BackendKind::Rcopy`] 作为最终回退（首期也是唯一）。
pub fn resolve(preferred: Option<BackendKind>) -> Resolution {
    let mut reason = None;
    let mut candidates = Vec::with_capacity(AUTO_ORDER.len() + usize::from(preferred.is_some()));

    if let Some(p) = preferred {
        let probe = backend(p).probe();
        if probe.available {
            candidates.push(p);
        } else {
            reason = probe.reason;
        }
    }

    for candidate in AUTO_ORDER {
        if Some(*candidate) == preferred {
            continue;
        }
        let probe = backend(*candidate).probe();
        if probe.available {
            candidates.push(*candidate);
        } else if reason.is_none() {
            reason = probe.reason;
        }
    }

    if candidates.is_empty() {
        candidates.push(BackendKind::Rcopy);
    }
    let kind = candidates[0];
    let fell_back = preferred.is_some_and(|p| p != kind);

    Resolution {
        kind,
        candidates,
        fell_back,
        reason,
    }
}

#[cfg(test)]
mod backend_tests {
    use super::*;

    #[test]
    fn unimplemented_backends_probe_unavailable() {
        for kind in [
            BackendKind::Apfs,
            BackendKind::Btrfs,
            BackendKind::Zfs,
            BackendKind::LinuxReflink,
            BackendKind::Overlayfs,
            BackendKind::WindowsBlockClone,
            BackendKind::Projfs,
        ] {
            let backend = backend(kind);
            assert_eq!(backend.kind(), kind, "{kind} 应诚实报告自身 kind");
            let probe = backend.probe();
            assert!(!probe.available, "{kind} 应报告不可用而非静默回退");
        }
    }

    #[test]
    fn rcopy_probe_available() {
        assert!(backend(BackendKind::Rcopy).probe().available);
    }

    #[test]
    fn resolve_falls_back_from_unimplemented() {
        let res = resolve(Some(BackendKind::Btrfs));
        assert!(res.fell_back, "应从 btrfs 回退到 rcopy");
        assert_eq!(res.kind, BackendKind::Rcopy);
        assert_eq!(res.candidates, vec![BackendKind::Rcopy]);
        assert!(res.reason.is_some(), "应携带不可用原因");
    }

    #[test]
    fn resolve_preferred_rcopy_no_fallback() {
        let res = resolve(Some(BackendKind::Rcopy));
        assert!(!res.fell_back);
        assert_eq!(res.kind, BackendKind::Rcopy);
    }

    #[test]
    fn resolve_default_is_rcopy() {
        let res = resolve(None);
        assert!(!res.fell_back);
        assert_eq!(res.kind, BackendKind::Rcopy);
    }
}
