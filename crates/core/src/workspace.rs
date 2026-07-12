//! 工作区抽象：文件系统隔离感知的根基准。
//!
//! `Workspace` 是工具执行的目标目录。当启用隔离（`IsoMode`）时，
//! 它透明地将 `root` 指向隔离的 `merged` 视图，并在 drop 时清理。

use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

use agent_iso::BackendKind;

/// 隔离模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsoMode {
    /// 不隔离：直接在源目录操作（默认）。
    Off,
    /// 创建隔离视图：源目录只读，工具在 merged 副本中工作。
    On,
}

/// 隔离句柄：记录 lower（源）与 merged（可写视图）路径。
#[derive(Debug)]
struct IsoHandle {
    /// 只读源目录。
    lower: PathBuf,
    /// 可写隔离视图（也是 root）。
    merged: PathBuf,
    /// 使用的 backend。
    kind: BackendKind,
}

/// 工作区：文件系统操作的根目录。
///
/// 隔离状态通过内部 [`Mutex`] 保护，允许通过 `&self` 在 `Arc<Workspace>` 中关闭隔离。
#[derive(Debug)]
pub struct Workspace {
    /// 工作区根目录（绝对路径）。隔离模式下指向 merged 视图。
    root: Mutex<PathBuf>,
    /// 隔离句柄（仅在启用隔离时有值）。
    iso: Mutex<Option<IsoHandle>>,
}

impl Workspace {
    /// 构造工作区（不隔离）。
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Mutex::new(root.into()),
            iso: Mutex::new(None),
        }
    }

    /// 以当前进程工作目录构造（不隔离）。
    #[must_use]
    pub fn current_dir() -> Self {
        Self {
            root: Mutex::new(
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            ),
            iso: Mutex::new(None),
        }
    }

    /// 构造隔离工作区。
    ///
    /// 在 `merged_dir` 创建 `lower` 的可写视图，`root` 指向 merged。
    /// Backend 通过 [`agent_iso::resolve`] 自动选择最佳可用实现。
    ///
    /// # Errors
    /// 隔离创建失败时返回错误。
    pub fn with_isolation(
        lower: impl Into<PathBuf>,
        merged_dir: impl Into<PathBuf>,
    ) -> Result<Self, agent_iso::IsoError> {
        let lower = lower.into();
        let merged = merged_dir.into();
        let resolution = agent_iso::resolve(None);
        let backend = agent_iso::backend(resolution.kind);
        backend.start(&lower, &merged)?;
        Ok(Self {
            root: Mutex::new(merged.clone()),
            iso: Mutex::new(Some(IsoHandle {
                lower,
                merged,
                kind: resolution.kind,
            })),
        })
    }

    /// 根目录路径（clone 出的 PathBuf，避免持有 Mutex guard 跨 await）。
    #[must_use]
    pub fn root(&self) -> PathBuf {
        self.root.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 是否启用隔离。
    #[must_use]
    pub fn is_isolated(&self) -> bool {
        self.iso.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    /// 解析路径为**工作区内**绝对路径（沙箱化）。
    ///
    /// 安全性：无论传入绝对路径还是含 `..` 的相对路径，结果都被词法收敛到工作区根之内，
    /// 杜绝 Agent 工具的路径穿越（读取/写入工作区外文件）。绝对前缀被视为「从根重新开始」，
    /// 故指向根之外的绝对路径会被映射到根下（读得到 not-found，写被限制在根内）。
    #[must_use]
    pub fn resolve(&self, relative: &Path) -> PathBuf {
        let root = self
            .root
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        sandbox_within(&root, relative)
    }

    /// 捕获隔离变更（仅在 `is_isolated()` 时产生 diff）。
    ///
    /// 调用方负责在 agent 完成任务后调用，收集 lower → merged 的所有文件变更。
    ///
    /// # Errors
    /// diff 执行失败时返回错误。
    pub async fn diff(&self) -> Option<Result<agent_iso::Diff, agent_iso::IsoError>> {
        let handle = {
            let guard = self.iso.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().map(|h| (h.lower.clone(), h.merged.clone(), h.kind))
        };
        let (lower, merged, kind) = handle?;
        let backend = agent_iso::backend(kind);
        Some(backend.diff(&lower, &merged).await)
    }

    /// 关闭隔离并清理 merged 视图（幂等）。
    ///
    /// 调用后 `root` 指向 lower 源目录。通常在 [`diff`](Self::diff) 之后调用。
    /// 由于内部使用 [`Mutex`]，此方法接受 `&self` 而非 `&mut self`。
    pub fn close_isolation(&self) -> Result<(), agent_iso::IsoError> {
        let handle = {
            let mut guard = self.iso.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        let Some(handle) = handle else {
            return Ok(());
        };
        let backend = agent_iso::backend(handle.kind);
        backend.stop(&handle.merged)?;
        *self.root.lock().unwrap_or_else(|e| e.into_inner()) = handle.lower;
        Ok(())
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // 与其它方法一致：即使 Mutex 中毒也恢复内部数据，避免 Drop 期间 panic（→ abort）。
        let handle = self
            .iso
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let backend = agent_iso::backend(handle.kind);
            let _ = backend.stop(&handle.merged);
        }
    }
}

/// 将 `input` 词法收敛到 `root` 之内（不要求目标已存在），并叠加符号链接防御。
///
/// 规则：先以（best-effort 规范化的）`root` 为基底词法收敛（`.` 忽略、普通分量追加、
/// `..` 仅当当前路径仍位于根下时回退一层否则钳制在根、绝对前缀重置到根）；
/// 再对其做符号链接校验（[`resolve_symlinks_within`]），杜绝工作区内软链指向根外文件。
fn sandbox_within(root: &Path, input: &Path) -> PathBuf {
    let base = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let lexical = lexically_clamp(&base, input);
    resolve_symlinks_within(&base, &lexical)
}

/// 纯词法收敛：逐分量处理 `input`，结果必位于 `base` 词法之下。
fn lexically_clamp(base: &Path, input: &Path) -> PathBuf {
    let mut out = base.to_path_buf();
    for comp in input.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if out != *base {
                    out.pop();
                }
            }
            Component::Normal(c) => out.push(c),
            // 绝对前缀：从根重新开始，丢弃原绝对基底以防逃逸。
            Component::RootDir | Component::Prefix(_) => {
                out = base.to_path_buf();
            }
        }
    }
    out
}

/// 在词法收敛结果之上叠加符号链接防御：取最长「存在前缀」规范化并校验仍位于 `base` 之内；
/// 不存在的尾部（写入目标尚未生成）原样保留；经软链逃出根则钳制到根（fail-safe）。
///
/// 这样可拦截工作区内形如 `evil → /etc` 的符号链接：词法结果 `base/evil` 会被规范化为
/// `/etc`，`starts_with(base)` 不成立 → 钳制回根，工具读取/写入安全失败。
fn resolve_symlinks_within(base: &Path, candidate: &Path) -> PathBuf {
    if candidate == base {
        return base.to_path_buf();
    }
    // 从 candidate 向上收集尾部分量，直到首个可规范化的祖先（最晚在 base 处命中）。
    let mut tail: Vec<OsString> = Vec::new();
    let mut cur = candidate.to_path_buf();
    let ancestor = loop {
        if let Ok(c) = cur.canonicalize() {
            break c;
        }
        match cur.file_name().map(OsString::from) {
            Some(name) => tail.push(name),
            None => return base.to_path_buf(),
        }
        let Some(parent) = cur.parent() else {
            return base.to_path_buf();
        };
        if parent == cur {
            return base.to_path_buf();
        }
        cur = parent.to_path_buf();
    };
    if !ancestor.starts_with(base) {
        // 经符号链接逃出根 → 钳制到根（fail-safe）
        return base.to_path_buf();
    }
    let mut out = ancestor;
    for name in tail.into_iter().rev() {
        out.push(&name);
        // 逐级检查：若某一级是符号链接，则可能指向 base 外（即使目标不存在），
        // fail-safe 钳制到根，杜绝写入跟随符号链接逃逸。
        if out.symlink_metadata().map_or(false, |m| m.file_type().is_symlink()) {
            return base.to_path_buf();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> Workspace {
        // 用真实临时目录，便于 canonicalize。
        let dir = std::env::temp_dir().join(format!(
            "agent-ws-resolve-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Workspace::new(&dir)
    }

    #[test]
    fn relative_stays_in_root() {
        let w = ws();
        let root = w.root();
        let got = w.resolve(Path::new("a/b.txt"));
        assert!(got.starts_with(&root) || got == root.join("a").join("b.txt"));
        assert!(got.ends_with("a/b.txt"));
    }

    #[test]
    fn parent_traversal_is_clamped() {
        let w = ws();
        let root = w.root();
        // `../../etc/passwd` 不得逃出根
        let got = w.resolve(Path::new("../../etc/passwd"));
        assert!(got.starts_with(&root), "{} 逃出了根 {}", got.display(), root.display());
        // 结果应被钳制到根内（passwd 作为根下普通文件名）
        assert!(got.ends_with("passwd"));
    }

    #[test]
    fn absolute_outside_root_is_contained() {
        let w = ws();
        let root = w.root();
        // 绝对路径指向根外 → 被映射到根下，不得读 /etc/passwd
        let got = w.resolve(Path::new("/etc/passwd"));
        assert!(got.starts_with(&root), "{} 逃出了根", got.display());
    }

    #[test]
    fn dotdot_after_normal_clamps() {
        let w = ws();
        let root = w.root();
        let got = w.resolve(Path::new("a/../../escape"));
        assert!(got.starts_with(&root));
        assert!(got.ends_with("escape"));
    }
}
