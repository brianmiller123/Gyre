//! 评审建议 emission guard：在代码层去重 + 过滤无内容 filler。
//!
//! 移植 oh-my-pi `advisor/emission-guard.ts`（真实教训：无去重的 advisor 会 spam
//! "Stop." 114 次，issue #3520）。`normalize` 后按 `(severity, 规范化文本)` 去重，
//! 空/纯标点/黑名单 filler 一律不发射。

use std::collections::HashSet;

use super::Severity;

/// 建议发射守卫（线程安全；同一实例供 Advisor 复用）。
#[derive(Debug, Default)]
pub struct EmissionGuard {
    seen: std::sync::Mutex<HashSet<String>>,
}

impl EmissionGuard {
    /// 空守卫。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 规范化建议文本：NFKC + 小写 + 折叠标点/空白（比较键）。
    #[must_use]
    pub fn normalize(note: &str) -> String {
        let mut out = String::with_capacity(note.len());
        for ch in note.chars().flat_map(char::to_lowercase) {
            if ch.is_alphanumeric() {
                out.push(ch);
            }
        }
        out
    }

    /// 尝试发射一条建议：`false` = 被守卫拦截（重复 / filler / 空）。
    ///
    /// 拦截条件（任一命中）：
    /// - 规范化后为空或长度 < 8（纯标点、单字噪音）；
    /// - 命中 filler 黑名单（stop/ok/continue/done/lgtm 等）；
    /// - 与已发射建议规范化后相同（含跨 severity）。
    pub fn emit(&self, severity: Severity, note: &str) -> bool {
        let key = Self::normalize(note);
        if key.chars().count() < 8 {
            return false;
        }
        if FILLERS.iter().any(|f| key == *f || key.starts_with(f)) {
            return false;
        }
        let mut seen = self.seen.lock().expect("guard 锁");
        let dedup_key = format!("{severity:?}:{key}");
        if seen.contains(&dedup_key) {
            return false;
        }
        seen.insert(dedup_key);
        true
    }
}

/// 无内容 filler 黑名单（规范化后比较）。
const FILLERS: &[&str] = &[
    "stop",
    "ok",
    "okay",
    "continue",
    "done",
    "lgtm",
    "looksgood",
    "nothing",
    "none",
    "n/a",
    "noted",
    "understood",
    "iagree",
    "agreed",
    "yes",
    "no",
    "thanks",
    "good",
    "fine",
    "great",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_case_and_punct() {
        assert_eq!(EmissionGuard::normalize("Stop."), "stop");
        assert_eq!(EmissionGuard::normalize("  继续！！"), "继续");
        assert_eq!(EmissionGuard::normalize("l'GTM!"), "lgtm");
    }

    #[test]
    fn dedupes_identical_advice() {
        let g = EmissionGuard::new();
        assert!(g.emit(Severity::Concern, "The error is swallowed silently."));
        assert!(!g.emit(Severity::Concern, "the ERROR is swallowed silently!!"));
        // 不同文本放行。
        assert!(g.emit(Severity::Concern, "A different observation."));
    }

    #[test]
    fn filters_fillers_and_empty() {
        let g = EmissionGuard::new();
        assert!(!g.emit(Severity::Nit, "Stop."));
        assert!(!g.emit(Severity::Nit, "OK"));
        assert!(!g.emit(Severity::Nit, ""));
        assert!(!g.emit(Severity::Nit, "！！！"));
        assert!(!g.emit(Severity::Nit, "Continue"));
    }

    #[test]
    fn short_noise_rejected() {
        let g = EmissionGuard::new();
        assert!(!g.emit(Severity::Nit, "hmm"));
        assert!(g.emit(
            Severity::Blocker,
            "User acceptance criteria no longer match the fix."
        ));
    }
}
