//! 诊断去重账本：跟踪每文件已报告诊断，仅返回新增（fresh）诊断。
//!
//! 移植自 oh-my-pi [`diagnostics-ledger.ts`](../../../third/oh-my-pi/packages/coding-agent/src/lsp/diagnostics-ledger.ts)。
//! 避免同一诊断在每次编辑后重复呈现给模型，减少上下文噪声。
//!
//! 诊断身份以 `source|message` 为键（忽略具体行号），故同类错误移行仍视为重复。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use agent_core::WriteDiagnostic;

/// 诊断去重账本：按文件记录已见诊断身份，[`Self::reduce`] 返回新增诊断。
pub struct DiagnosticsLedger {
    seen: Mutex<HashMap<String, HashSet<String>>>,
}

impl DiagnosticsLedger {
    /// 构造空账本。
    #[must_use]
    pub fn new() -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// 对某文件的诊断列表去重：返回未在之前报告过的新增诊断，并更新已见集合。
    pub fn reduce(&self, abs_path: &str, diagnostics: &[WriteDiagnostic]) -> Vec<WriteDiagnostic> {
        let mut seen = self.seen.lock().expect("ledger mutex poisoned");
        let previous = seen.get(abs_path).cloned();
        let mut current = HashSet::new();
        let mut fresh = Vec::new();
        for d in diagnostics {
            let identity = diagnostic_identity(d);
            current.insert(identity.clone());
            let is_fresh = previous.as_ref().map_or(true, |p| !p.contains(&identity));
            if is_fresh {
                fresh.push(d.clone());
            }
        }
        if current.is_empty() {
            seen.remove(abs_path);
        } else {
            seen.insert(abs_path.to_string(), current);
        }
        fresh
    }
}

impl Default for DiagnosticsLedger {
    fn default() -> Self {
        Self::new()
    }
}

/// 诊断身份：`source|message`（忽略具体行号）。
fn diagnostic_identity(d: &WriteDiagnostic) -> String {
    match &d.source {
        Some(s) => format!("{s}|{}", d.message),
        None => d.message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::DiagnosticSeverity;

    fn diag(message: &str, line: u32, source: Option<&str>) -> WriteDiagnostic {
        WriteDiagnostic {
            severity: DiagnosticSeverity::Error,
            line,
            character: 0,
            message: message.into(),
            source: source.map(String::from),
        }
    }

    #[test]
    fn first_report_returns_all() {
        let ledger = DiagnosticsLedger::new();
        let diags = vec![diag("missing ;", 1, Some("rust-analyzer"))];
        let fresh = ledger.reduce("/x.rs", &diags);
        assert_eq!(fresh.len(), 1);
    }

    #[test]
    fn second_report_dedups_seen_regardless_of_line() {
        let ledger = DiagnosticsLedger::new();
        let diags = vec![diag("missing ;", 1, Some("rust-analyzer"))];
        let _ = ledger.reduce("/x.rs", &diags);
        // 同身份但移行 → 应去重。
        let diags2 = vec![diag("missing ;", 5, Some("rust-analyzer"))];
        let fresh = ledger.reduce("/x.rs", &diags2);
        assert!(fresh.is_empty());
    }

    #[test]
    fn new_message_is_fresh() {
        let ledger = DiagnosticsLedger::new();
        let _ = ledger.reduce("/x.rs", &[diag("a", 1, None)]);
        let fresh = ledger.reduce("/x.rs", &[diag("a", 1, None), diag("b", 2, None)]);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].message, "b");
    }

    #[test]
    fn empty_clears_file_entry() {
        let ledger = DiagnosticsLedger::new();
        let _ = ledger.reduce("/x.rs", &[diag("a", 1, None)]);
        let fresh = ledger.reduce("/x.rs", &[]);
        assert!(fresh.is_empty());
        // 清空后再次报告应视为全新。
        let fresh2 = ledger.reduce("/x.rs", &[diag("a", 1, None)]);
        assert_eq!(fresh2.len(), 1);
    }

    #[test]
    fn per_file_independent() {
        let ledger = DiagnosticsLedger::new();
        let _ = ledger.reduce("/a.rs", &[diag("err", 1, None)]);
        // 不同文件，相同身份 → 应为全新。
        let fresh = ledger.reduce("/b.rs", &[diag("err", 1, None)]);
        assert_eq!(fresh.len(), 1);
    }
}
