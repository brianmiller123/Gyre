//! LSP 编辑副作用：编辑后 format（可选）+ diagnostics（可选）+ 去重。
//!
//! 实现 [`agent_core::WriteEffect`]，由装配层注入 [`crate::ToolContext`]。与
//! [`crate::LspTool`] 共享同一套语言服务器实例（经 `pool`），避免重复启动。
//!
//! ## 降级语义
//!
//! LSP 不可用（无服务器 / 启动失败 / format 失败）时返回空 outcome，不阻断编辑。
//!
//! ## deferred 简化
//!
//! 当前同步收集诊断（`ensure_document_open` 内部已含短暂等待 + drain notifications），
//! [`agent_core::WriteOutcome::deferred`] 暂为 `None`。跨工具调用的异步诊断合并留作后续增强。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_core::{DiagnosticSeverity, WriteDiagnostic, WriteEffect, WriteOutcome};
use agent_lsp::client::DiagnosticSeverity as LspDiagnosticSeverity;
use agent_lsp::client::LspDiagnostic;
use agent_lsp::{detect_servers, DiagnosticsLedger, LspManager};
use tokio::sync::Mutex;
use url::Url;

/// LSP 编辑副作用实现：编辑后触发 LSP format / diagnostics，并做诊断去重。
///
/// 与 [`crate::LspTool`] 共享同一 `pool`，复用已启动的语言服务器。
pub struct LspWriteEffect {
    /// 工作区根（用于 detect_servers 与 manager 池 key）。
    workspace_root: PathBuf,
    /// 共享 LSP 管理器池（通常取自 [`crate::LspTool::pool`]）。
    pool: Arc<Mutex<HashMap<PathBuf, LspManager>>>,
    /// 诊断去重账本。
    ledger: Arc<DiagnosticsLedger>,
    /// 是否启用编辑后 format。
    enable_format: bool,
    /// 是否启用编辑后 diagnostics。
    enable_diagnostics: bool,
    /// 是否对诊断去重（`source|message` 身份）。
    deduplicate: bool,
}

impl LspWriteEffect {
    /// 构造：`pool` 通常取自 [`crate::LspTool::pool`]，确保共享同一套语言服务器。
    #[must_use]
    pub fn new(
        workspace_root: PathBuf,
        pool: Arc<Mutex<HashMap<PathBuf, LspManager>>>,
        enable_format: bool,
        enable_diagnostics: bool,
        deduplicate: bool,
    ) -> Self {
        Self {
            workspace_root,
            pool,
            ledger: Arc::new(DiagnosticsLedger::new()),
            enable_format,
            enable_diagnostics,
            deduplicate,
        }
    }

    /// 获取或启动 `workspace_root` 对应的 [`LspManager`]（持有 pool 锁时调用）。
    async fn ensure_manager<'a>(
        &self,
        managers: &'a mut HashMap<PathBuf, LspManager>,
    ) -> Option<&'a mut LspManager> {
        if !managers.contains_key(&self.workspace_root) {
            let servers = detect_servers(&self.workspace_root);
            if servers.is_empty() {
                return None;
            }
            match LspManager::start(&self.workspace_root, &servers).await {
                Ok(m) => {
                    managers.insert(self.workspace_root.clone(), m);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "LspWriteEffect: 启动 LSP 失败，降级 noop");
                    return None;
                }
            }
        }
        managers.get_mut(&self.workspace_root)
    }
}

#[async_trait::async_trait]
impl WriteEffect for LspWriteEffect {
    async fn after_write(&self, path: &Path, new_text: &str) -> Result<WriteOutcome, String> {
        let uri = match Url::from_file_path(path) {
            Ok(u) => u,
            Err(_) => return Ok(WriteOutcome::empty()),
        };

        let mut managers = self.pool.lock().await;
        let Some(manager) = self.ensure_manager(&mut managers).await else {
            return Ok(WriteOutcome::empty());
        };

        let mut formatted_text: Option<String> = None;
        let mut text_for_diag = new_text.to_string();

        // format（可选）。
        if self.enable_format {
            match manager.format(&uri).await {
                Ok(Some(fmt)) => {
                    text_for_diag = fmt.clone();
                    formatted_text = Some(fmt);
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "LspWriteEffect: format 失败"),
            }
        }

        // diagnostics（可选）：同步文档（用 format 后内容）→ 收集 → 去重。
        let mut diagnostics = Vec::new();
        if self.enable_diagnostics {
            if let Err(e) = manager.ensure_document_open(&uri, &text_for_diag).await {
                tracing::warn!(error = %e, "LspWriteEffect: 同步文档失败");
            }
            let diags = manager.diagnostics(&uri).await;
            let mapped: Vec<WriteDiagnostic> =
                diags.into_iter().map(lsp_to_write_diagnostic).collect();
            let abs = path.to_string_lossy();
            diagnostics = if self.deduplicate {
                self.ledger.reduce(&abs, &mapped)
            } else {
                mapped
            };
        }

        Ok(WriteOutcome {
            formatted_text,
            diagnostics,
            deferred: None,
        })
    }
}

/// 把 agent-lsp 诊断转换为 agent-core 中性诊断（行/列 0-based → 1-based）。
fn lsp_to_write_diagnostic(d: LspDiagnostic) -> WriteDiagnostic {
    WriteDiagnostic {
        severity: match d.severity {
            Some(LspDiagnosticSeverity::Error) => DiagnosticSeverity::Error,
            Some(LspDiagnosticSeverity::Warning) => DiagnosticSeverity::Warning,
            Some(LspDiagnosticSeverity::Information) => DiagnosticSeverity::Information,
            Some(LspDiagnosticSeverity::Hint) => DiagnosticSeverity::Hint,
            None => DiagnosticSeverity::Information,
        },
        line: d.line + 1,
        character: d.character + 1,
        message: d.message,
        source: d.source,
    }
}
