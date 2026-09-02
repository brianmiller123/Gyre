//! 审批模式运行时控制器：Web 开关实时生效 + `.gyre/approval-mode.state` sidecar 持久化。
//!
//! 与 SOCKS5 控制器（`agent-proxy`）同构的进程级共享单例：持有原子审批模式覆盖 +
//! 可选持久化路径。Web 设置页 `PUT /api/approval-mode` 驱动 [`Self::set`]——原子写
//! 立即影响**已建会话**（[`crate::RulesEngine`] 每次 `decide` 实时读取共享原子），
//! 同时落盘 sidecar 使下次启动自动恢复。
//!
//! 优先级：CLI `--approval-mode` 显式值 ＞ `.gyre/approval-mode.state` 持久化 ＞
//! 配置 `[agent].approval_mode` 默认。
//!
//! sidecar 内容为单行文本 `always-ask` / `write` / `yolo`（非 JSON，无敏感信息）。
//! 无持久化路径（如只读装配）时仅内存生效。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use agent_core::ApprovalMode;

/// 原子编码：0 = 未设置（用配置默认），1..=3 = AlwaysAsk/Write/Yolo。
const fn encode(mode: ApprovalMode) -> u8 {
    match mode {
        ApprovalMode::AlwaysAsk => 1,
        ApprovalMode::Write => 2,
        ApprovalMode::Yolo => 3,
    }
}

const fn decode(v: u8) -> Option<ApprovalMode> {
    match v {
        1 => Some(ApprovalMode::AlwaysAsk),
        2 => Some(ApprovalMode::Write),
        3 => Some(ApprovalMode::Yolo),
        _ => None,
    }
}

const fn mode_str(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::AlwaysAsk => "always-ask",
        ApprovalMode::Write => "write",
        ApprovalMode::Yolo => "yolo",
    }
}

/// 读取 sidecar 持久化模式；缺失/损坏返回 `None`（用配置默认）。
fn read_state(path: &Path) -> Option<ApprovalMode> {
    let s = std::fs::read_to_string(path)
        .ok()?
        .trim()
        .to_ascii_lowercase();
    match s.as_str() {
        "always-ask" => Some(ApprovalMode::AlwaysAsk),
        "write" => Some(ApprovalMode::Write),
        "yolo" => Some(ApprovalMode::Yolo),
        _ => None,
    }
}

/// 尽力持久化到 sidecar（创建父目录）；失败仅告警，不影响本次生效。
fn persist_state(path: &Path, mode: ApprovalMode) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(path, mode_str(mode)) {
        tracing::warn!(error = %e, path = %path.display(), "审批模式持久化失败（仅内存生效）");
    }
}

/// 进程级审批模式运行时控制器（共享单例，经 `Arc` 分发）。
#[derive(Debug, Clone, Default)]
pub struct ApprovalModeController {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// 运行时覆盖（0 = 未设置；1..=3 = AlwaysAsk/Write/Yolo）。
    state: Arc<AtomicU8>,
    /// 持久化 sidecar（`None` = 不落盘）。
    state_path: Option<PathBuf>,
}

impl ApprovalModeController {
    /// 构造控制器：立即读取 sidecar 持久化态（若有）作为初始覆盖。
    #[must_use]
    pub fn new(state_path: Option<PathBuf>) -> Self {
        let persisted = state_path.as_deref().and_then(read_state);
        Self {
            inner: Arc::new(Inner {
                state: Arc::new(AtomicU8::new(persisted.map_or(0, encode))),
                state_path,
            }),
        }
    }

    /// 当前运行时覆盖（`None` = 未设置，应使用配置默认）。
    #[must_use]
    pub fn current(&self) -> Option<ApprovalMode> {
        decode(self.inner.state.load(Ordering::Relaxed))
    }

    /// 会话创建时取生效模式：覆盖优先，否则配置默认。
    #[must_use]
    pub fn effective(&self, config_default: ApprovalMode) -> ApprovalMode {
        self.current().unwrap_or(config_default)
    }

    /// 运行时切换：原子写 + 尽力持久化到 sidecar。`None` 恢复配置默认
    /// （清原子 + 删除 sidecar 文件）。
    pub fn set(&self, mode: Option<ApprovalMode>) {
        self.inner
            .state
            .store(mode.map_or(0, encode), Ordering::Relaxed);
        if let Some(path) = &self.inner.state_path {
            match mode {
                Some(m) => persist_state(path, m),
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }

    /// 共享原子（供 [`crate::RulesEngine::with_approval_override`] 注入；`decide`
    /// 每次实时读取，切换对已建会话立即生效）。
    #[must_use]
    pub fn shared(&self) -> Arc<AtomicU8> {
        Arc::clone(&self.inner.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_state(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gyre-approval-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("approval-mode.state")
    }

    #[test]
    fn fresh_controller_uses_config_default() {
        let c = ApprovalModeController::new(None);
        assert_eq!(c.current(), None);
        assert_eq!(
            c.effective(ApprovalMode::AlwaysAsk),
            ApprovalMode::AlwaysAsk
        );
    }

    #[test]
    fn set_persists_and_roundtrips() {
        let path = tmp_state("roundtrip");
        let _ = std::fs::remove_file(&path);
        let c = ApprovalModeController::new(Some(path.clone()));
        assert_eq!(c.current(), None);
        c.set(Some(ApprovalMode::Write));
        assert_eq!(c.current(), Some(ApprovalMode::Write));
        // 新控制器（模拟重启）从 sidecar 恢复。
        let c2 = ApprovalModeController::new(Some(path.clone()));
        assert_eq!(c2.current(), Some(ApprovalMode::Write));
        assert_eq!(c2.effective(ApprovalMode::AlwaysAsk), ApprovalMode::Write);
        // 恢复默认：清原子 + 删 sidecar。
        c2.set(None);
        assert_eq!(c2.current(), None);
        assert!(!path.exists());
    }

    #[test]
    fn corrupted_state_falls_back_to_default() {
        let path = tmp_state("corrupt");
        std::fs::write(&path, "banana").expect("写坏文件");
        let c = ApprovalModeController::new(Some(path));
        assert_eq!(c.current(), None);
    }

    #[test]
    fn shared_atomic_reflects_set() {
        let c = ApprovalModeController::new(None);
        let shared = c.shared();
        c.set(Some(ApprovalMode::Yolo));
        assert_eq!(shared.load(Ordering::Relaxed), 3);
        c.set(None);
        assert_eq!(shared.load(Ordering::Relaxed), 0);
    }
}
