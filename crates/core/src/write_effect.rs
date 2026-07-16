//! 写入效果端口：编辑工具写盘后触发的副作用（LSP format / diagnostics 等）。
//!
//! 与 [`crate::hook`] 互补：[`Hook`](crate::Hook) 仅观察事件（无返回值、不阻止执行），
//! 而 [`WriteEffect`] 可返回格式化文本与诊断，影响写入结果与工具返回值。
//!
//! ## 动机
//!
//! 编辑工具（`write_file` / `apply_hashline` /
//! `replace_block` / `ast_rewrite`）原先各自直接 `tokio::fs::write`，若要让每个工具
//! 在写盘后统一接 LSP format / diagnostics，会出现大量重复胶水。本端口提供一个
//! 「写盘后副作用」的统一注入点：装配层注入具体实现（如 LSP format + diagnostics
//! 组合的 `LspWriteEffect`），编辑工具经 `write_with_effects` 辅助统一调用。
//!
//! ## 依赖方向
//!
//! 定义于 `agent-core`（零业务依赖），`agent-lsp`（实现 `LspWriteEffect`）与
//! `agent-tools`（`ToolContext` 持有 `&dyn WriteEffect`）均可引用，不产生循环依赖。

use std::path::Path;

/// 诊断严重级别（语言服务器报告）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    /// 错误（编译失败 / 类型错误）。
    Error,
    /// 警告。
    Warning,
    /// 信息。
    Information,
    /// 提示。
    Hint,
}

/// 一条诊断信息（跨 crate 共享的中性类型；`LspWriteEffect` 负责把
/// `LspDiagnostic` 转换为此类型）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteDiagnostic {
    /// 严重级别。
    pub severity: DiagnosticSeverity,
    /// 行号（1-based，便于直接呈现给用户/模型）。
    pub line: u32,
    /// 列号（1-based）。
    pub character: u32,
    /// 诊断消息。
    pub message: String,
    /// 来源（如 `rust-analyzer`）。
    pub source: Option<String>,
}

/// 延迟诊断句柄。
///
/// LSP 诊断是异步推送的（`didChange` 后服务器需时间计算），编辑工具返回时
/// 诊断可能尚未就绪。此句柄允许执行循环在后续时机（下一轮工具调用或显式 flush）
/// 合并补发的诊断，避免阻塞编辑主流程等待服务器。
#[async_trait::async_trait]
pub trait DeferredDiagnosticsHandle: Send + Sync {
    /// 非阻塞地收集当前已就绪的延迟诊断。
    ///
    /// 返回空向量表示尚未就绪；调用方可择机重试或放弃。
    ///
    /// # Errors
    /// 收集失败时返回错误信息（不应阻断主流程）。
    async fn poll(&self) -> Result<Vec<WriteDiagnostic>, String>;
}

/// [`WriteEffect::after_write`] 的返回值。
pub struct WriteOutcome {
    /// 格式化后的完整文件内容。
    ///
    /// `Some` 表示需用该文本重写磁盘；`None` 表示不格式化（保持写入原文）。
    pub formatted_text: Option<String>,
    /// 本次写入后的诊断（可能为空，或经 [`Self::deferred`] 延后补发）。
    pub diagnostics: Vec<WriteDiagnostic>,
    /// 延迟诊断句柄（异步诊断未就绪时提供；`None` 表示无延迟诊断）。
    pub deferred: Option<Box<dyn DeferredDiagnosticsHandle>>,
}

impl WriteOutcome {
    /// 构造空结果（无格式化、无诊断、无延迟）。
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }
}

impl Default for WriteOutcome {
    fn default() -> Self {
        Self {
            formatted_text: None,
            diagnostics: Vec::new(),
            deferred: None,
        }
    }
}

impl std::fmt::Debug for WriteOutcome {
    // 手动实现：`deferred` 含 `dyn` 无法 derive Debug，故用占位描述。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let deferred_desc = if self.deferred.is_some() {
            "<deferred handle>"
        } else {
            "<none>"
        };
        f.debug_struct("WriteOutcome")
            .field("formatted_text", &self.formatted_text)
            .field("diagnostics", &self.diagnostics)
            .field("deferred", &deferred_desc)
            .finish()
    }
}

/// 写入效果端口：编辑工具写盘后触发。
///
/// 装配层注入具体实现（如 LSP format + diagnostics 组合的 `LspWriteEffect`）。
/// 未注入时，编辑工具按 noop 处理（仅写盘，无副作用）。
///
/// ## 语义约定
///
/// 实现应「尽力而为」：format / diagnostics 失败时应返回 [`WriteOutcome::empty`]
/// 或仅记录告警，**不应**因副作用失败而让编辑主流程报错——编辑本身已成功落盘。
#[async_trait::async_trait]
pub trait WriteEffect: Send + Sync {
    /// 文件写入磁盘后触发：可返回格式化文本、诊断、延迟句柄。
    ///
    /// # Errors
    /// 效果执行失败时返回错误信息；调用方据此降级（忽略副作用），不传播为工具错误。
    async fn after_write(&self, path: &Path, new_text: &str) -> Result<WriteOutcome, String>;
}
