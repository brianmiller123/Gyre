//! 统一写入辅助：写盘 + 触发 [`WriteEffect`]（LSP format / diagnostics 等）。
//!
//! 所有写工具（`write_file` / `apply_hashline` /
//! `replace_block` / `ast_rewrite`）经 [`write_with_effects`] 写盘，避免各自直接
//! `tokio::fs::write` 并重复接 LSP 胶水。装配层在 [`ToolContext`](crate::ToolContext)
//! 注入具体 [`WriteEffect`]（如 `LspWriteEffect`）即可让全部写工具自动获得
//! 「写后 format + diagnostics」能力。
//!
//! ## 降级语义
//!
//! 编辑主流程以「落盘成功」为准：[`WriteEffect`] 副作用（format / diagnostics）失败时
//! 仅记录告警并按 noop 处理，**不**传播为工具错误——文件已成功写入。

use std::path::Path;

use agent_core::{
    DeferredDiagnosticsHandle, DiagnosticSeverity, ToolError, WriteDiagnostic, WriteEffect,
    WriteOutcome,
};

use crate::ToolContext;

/// 写入报告：实际落盘文本 + 副作用产物。
pub struct WriteReport {
    /// 实际写入磁盘的最终文本（可能经 format 重写）。
    pub final_text: String,
    /// 是否触发了格式化重写。
    pub formatted: bool,
    /// 副作用收集的诊断（去重等处理后的最终诊断）。
    pub diagnostics: Vec<WriteDiagnostic>,
    /// 延迟诊断句柄（异步诊断未就绪时；执行循环可后续 flush）。
    pub deferred: Option<Box<dyn DeferredDiagnosticsHandle>>,
}

impl std::fmt::Debug for WriteReport {
    // 手动实现：`deferred` 含 `dyn` 无法 derive Debug。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let deferred_desc = if self.deferred.is_some() {
            "<deferred handle>"
        } else {
            "<none>"
        };
        f.debug_struct("WriteReport")
            .field("final_text", &self.final_text)
            .field("formatted", &self.formatted)
            .field("diagnostics", &self.diagnostics)
            .field("deferred", &deferred_desc)
            .finish()
    }
}

impl WriteReport {
    /// 返回用于附加到工具成功消息的后缀：格式化标记 + 诊断渲染（无内容则空串）。
    #[must_use]
    pub fn effect_suffix(&self) -> String {
        let mut s = String::new();
        if self.formatted {
            s.push_str("；已应用 LSP 格式化");
        }
        s.push_str(&render_diagnostics(&self.diagnostics));
        s
    }
}

/// 空的 [`WriteEffect`]（noop）：写盘后无任何副作用。
///
/// 未注入 `WriteEffect` 的场景（如单测）等价于此实现。
#[derive(Debug, Default)]
pub struct NoopWriteEffect;

#[async_trait::async_trait]
impl WriteEffect for NoopWriteEffect {
    async fn after_write(&self, _path: &Path, _new_text: &str) -> Result<WriteOutcome, String> {
        Ok(WriteOutcome::empty())
    }
}

/// 统一写入：落盘 → 触发 [`WriteEffect`]（若有）→ 按结果可能重写 → 返回 [`WriteReport`]。
///
/// 调用方需先 resolve 出绝对路径并创建父目录。
///
/// # Errors
/// 仅磁盘 IO 失败时返回 [`ToolError::Io`]；format / 诊断副作用失败不报错（降级为 noop）。
pub async fn write_with_effects(
    full_path: &Path,
    text: &str,
    ctx: &ToolContext<'_>,
) -> Result<WriteReport, ToolError> {
    // 1. 落盘。
    tokio::fs::write(full_path, text)
        .await
        .map_err(ToolError::Io)?;

    // 2. 无 WriteEffect → noop 报告。
    let Some(effect) = ctx.write_effect else {
        return Ok(WriteReport {
            final_text: text.to_string(),
            formatted: false,
            diagnostics: Vec::new(),
            deferred: None,
        });
    };

    // 3. 触发副作用（降级：失败按 noop，不阻断编辑）。
    let outcome = match effect.after_write(full_path, text).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, path = ?full_path, "WriteEffect 副作用失败，降级为 noop");
            return Ok(WriteReport {
                final_text: text.to_string(),
                formatted: false,
                diagnostics: Vec::new(),
                deferred: None,
            });
        }
    };

    // 4. 格式化重写（仅当返回不同文本时才重写，避免无意义 IO）。
    let (final_text, formatted) = match outcome.formatted_text.as_deref() {
        Some(fmt) if fmt != text => {
            tokio::fs::write(full_path, fmt)
                .await
                .map_err(ToolError::Io)?;
            (fmt.to_string(), true)
        }
        _ => (text.to_string(), false),
    };

    Ok(WriteReport {
        final_text,
        formatted,
        diagnostics: outcome.diagnostics,
        deferred: outcome.deferred,
    })
}

/// 把诊断渲染为人类/模型可读文本；无诊断返回空串。
#[must_use]
pub fn render_diagnostics(diagnostics: &[WriteDiagnostic]) -> String {
    if diagnostics.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n诊断:\n");
    for d in diagnostics {
        let sev = match d.severity {
            DiagnosticSeverity::Error => "❌ ERROR",
            DiagnosticSeverity::Warning => "⚠️  WARNING",
            DiagnosticSeverity::Information => "ℹ️  INFO",
            DiagnosticSeverity::Hint => "💡 HINT",
        };
        out.push_str(&format!(
            "  {sev} [L{}:C{}]: {}\n",
            d.line, d.character, d.message
        ));
        if let Some(source) = &d.source {
            out.push_str(&format!("    source: {source}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Allow;
    #[async_trait::async_trait]
    impl agent_core::ApprovalPolicy for Allow {
        fn decide(&self, _r: &agent_core::ApprovalRequest<'_>) -> agent_core::ApprovalDecision {
            agent_core::ApprovalDecision::Allow
        }
        async fn prompt(
            &self,
            _a: &agent_core::AskMessage,
        ) -> Result<agent_core::AskResponse, ToolError> {
            Ok(agent_core::AskResponse::Yes)
        }
    }

    fn cancel() -> &'static tokio_util::sync::CancellationToken {
        static CANCEL: std::sync::OnceLock<tokio_util::sync::CancellationToken> =
            std::sync::OnceLock::new();
        CANCEL.get_or_init(tokio_util::sync::CancellationToken::new)
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let nano = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("we-{tag}-{nano:x}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn noop_when_no_effect() {
        // 无 write_effect 注入 → 只落盘，无副作用。
        let dir = tmp_dir("noop");
        let ws = agent_core::Workspace::new(&dir);
        let ctx = ToolContext {
            workspace: &ws,
            approval: &Allow,
            cancel: cancel(),
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
            update_tx: None,
        };
        let p = dir.join("a.txt");
        let report = write_with_effects(&p, "hi\n", &ctx).await.unwrap();
        assert!(!report.formatted);
        assert!(report.diagnostics.is_empty());
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "hi\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn effect_failure_degrades_to_noop() {
        // after_write 报错时降级 noop，不传播错误（落盘已成功）。
        struct FailEffect;
        #[async_trait::async_trait]
        impl WriteEffect for FailEffect {
            async fn after_write(&self, _p: &Path, _t: &str) -> Result<WriteOutcome, String> {
                Err("boom".into())
            }
        }
        let dir = tmp_dir("fail");
        let ws = agent_core::Workspace::new(&dir);
        let effect = FailEffect;
        let ctx = ToolContext {
            workspace: &ws,
            approval: &Allow,
            cancel: cancel(),
            skills: None,
            memory: None,
            resources: None,
            write_effect: Some(&effect),
            update_tx: None,
        };
        let p = dir.join("b.txt");
        let report = write_with_effects(&p, "ok\n", &ctx).await.unwrap();
        assert!(!report.formatted);
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "ok\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn formatted_text_rewrites_disk() {
        // formatted_text 与原文不同 → 重写盘并标记 formatted。
        struct FmtEffect;
        #[async_trait::async_trait]
        impl WriteEffect for FmtEffect {
            async fn after_write(&self, _p: &Path, _t: &str) -> Result<WriteOutcome, String> {
                let mut o = WriteOutcome::empty();
                o.formatted_text = Some("FORMATTED\n".into());
                Ok(o)
            }
        }
        let dir = tmp_dir("fmt");
        let ws = agent_core::Workspace::new(&dir);
        let effect = FmtEffect;
        let ctx = ToolContext {
            workspace: &ws,
            approval: &Allow,
            cancel: cancel(),
            skills: None,
            memory: None,
            resources: None,
            write_effect: Some(&effect),
            update_tx: None,
        };
        let p = dir.join("c.txt");
        let report = write_with_effects(&p, "raw\n", &ctx).await.unwrap();
        assert!(report.formatted);
        assert_eq!(report.final_text, "FORMATTED\n");
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "FORMATTED\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn identical_formatted_text_skips_rewrite() {
        // formatted_text 与原文相同 → 不重写，formatted=false。
        struct SameEffect;
        #[async_trait::async_trait]
        impl WriteEffect for SameEffect {
            async fn after_write(&self, _p: &Path, t: &str) -> Result<WriteOutcome, String> {
                let mut o = WriteOutcome::empty();
                o.formatted_text = Some(t.to_string());
                Ok(o)
            }
        }
        let dir = tmp_dir("same");
        let ws = agent_core::Workspace::new(&dir);
        let effect = SameEffect;
        let ctx = ToolContext {
            workspace: &ws,
            approval: &Allow,
            cancel: cancel(),
            skills: None,
            memory: None,
            resources: None,
            write_effect: Some(&effect),
            update_tx: None,
        };
        let p = dir.join("d.txt");
        let report = write_with_effects(&p, "stable\n", &ctx).await.unwrap();
        assert!(!report.formatted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_diagnostics_empty_and_filled() {
        assert_eq!(render_diagnostics(&[]), "");
        let diags = vec![WriteDiagnostic {
            severity: DiagnosticSeverity::Error,
            line: 3,
            character: 5,
            message: "missing semicolon".into(),
            source: Some("rust-analyzer".into()),
        }];
        let out = render_diagnostics(&diags);
        assert!(out.contains("❌ ERROR"));
        assert!(out.contains("L3:C5"));
        assert!(out.contains("missing semicolon"));
        assert!(out.contains("rust-analyzer"));
    }
}
