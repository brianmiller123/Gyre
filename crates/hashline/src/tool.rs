//! `apply_hashline` 工具：把 hashline patch 应用到工作区文件。
//!
//! hashline 是**行号锚定**的批量编辑编辑格式，适合一次性描述多文件、多区间、插入/删除/替换/重命名。
//! 工具收敛后为项目唯一编辑工具。

use std::path::Path;
use std::sync::{Arc, RwLock};

use agent_core::{CapabilityTier, ToolError, ToolResult};
use agent_tools::{Tool, ToolContext, WriteReport, render_diagnostics, write_with_effects};
use async_trait::async_trait;
use serde_json::json;

use crate::apply::apply_section;
use crate::format::compute_file_hash;
use crate::normalize::{detect_line_ending, normalize_to_lf, restore_line_endings, strip_bom};
use crate::parser::parse_hashline;
use crate::preview::build_compact_diff;
use crate::types::{ApplyResult, FileOp};

/// 应用 hashline patch 到工作区文件。
pub struct HashlineTool {
    /// 会话级快照存储：写前记录版本，stale hash 失配时供恢复使用。
    /// `&self` 在注册表内全会话复用，故用内部可变状态而无需改 ToolContext。
    snapshots: Arc<RwLock<crate::snapshots::InMemorySnapshotStore>>,
}

impl HashlineTool {
    /// 构造带新会话快照存储的实例。
    #[must_use]
    pub fn new() -> Self {
        Self {
            snapshots: Arc::new(RwLock::new(crate::snapshots::InMemorySnapshotStore::new())),
        }
    }
}

impl Default for HashlineTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for HashlineTool {
    fn name(&self) -> &str {
        "apply_hashline"
    }
    fn description(&self) -> &str {
        "按 hashline 行锚定格式批量编辑文件：每段以 [path#hash] 开头，含 SWAP/DEL/INS/REM/MV 操作。\
         每次编辑后行号重新编号，须基于最新 read 的行号。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "hashline patch 文本，可含多个 [path#hash] 段；段内为 SWAP/DEL/INS/REM/MV 操作"
                },
                "path": {
                    "type": "string",
                    "description": "可选：当 patch 不含段头时的回退目标文件路径"
                }
            },
            "required": ["patch"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        CapabilityTier::Write
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let patch = input
            .get("patch")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `patch`".into()))?;
        let fallback_path = input.get("path").and_then(serde_json::Value::as_str);

        let mut sections = parse_hashline(patch).map_err(ToolError::InvalidArgs)?;
        // 无段头时用 fallback_path 兜底
        if sections.is_empty() {
            if let Some(p) = fallback_path {
                sections.push(crate::types::FileSection {
                    path: p.to_string(),
                    hash: None,
                    hunks: Vec::new(),
                });
            } else {
                return Err(ToolError::InvalidArgs(
                    "patch 不含任何段头，且未提供 `path` 回退".into(),
                ));
            }
        }

        let mut summary: Vec<String> = Vec::new();
        let mut all_diagnostics: Vec<agent_core::WriteDiagnostic> = Vec::new();
        for section in &sections {
            let target = ctx.workspace.resolve(Path::new(&section.path));
            // REM：删除文件
            let is_rem = section
                .hunks
                .iter()
                .any(|h| matches!(h, crate::types::Hunk::File(FileOp::Remove)));
            if is_rem {
                if target.exists() {
                    tokio::fs::remove_file(&target)
                        .await
                        .map_err(ToolError::Io)?;
                }
                summary.push(format!("{}: REM（已删除）", section.path));
                continue;
            }

            let raw = tokio::fs::read_to_string(&target)
                .await
                .map_err(ToolError::Io)?;
            // 规范化：剥 BOM + 检测行尾 + 归一为 LF（应用器在 LF 正文上操作，
            // 哈希校验与编辑均基于 LF，避免 CRLF/BOM 干扰）。
            let bom = strip_bom(&raw);
            let ending = detect_line_ending(&bom.text);
            let original = normalize_to_lf(&bom.text);

            // stale-hash 恢复：段头 hash 与实际失配时，尝试在先前快照版上重放编辑（会话链回放）。
            let actual_hash = compute_file_hash(&original);
            let mismatch = section
                .hash
                .as_deref()
                .is_some_and(|h| !h.eq_ignore_ascii_case(&actual_hash));
            let mut extra_warnings: Vec<String> = Vec::new();
            let mut result = if mismatch {
                let recovered = {
                    let store = self.snapshots.read().expect("snapshot lock poisoned");
                    store
                        .by_hash(&section.path, section.hash.as_deref().unwrap_or(""))
                        .map(|s| s.text.clone())
                        .and_then(|prev| crate::recovery::recover(&prev, &original, section))
                };
                match recovered {
                    Some(rec) => {
                        extra_warnings.extend(rec.warnings);
                        ApplyResult {
                            text: Some(rec.text),
                            warnings: Vec::new(),
                            first_changed_line: rec.first_changed_line,
                            moved_to: None,
                        }
                    }
                    // 恢复失败：容忍地常规应用（apply 会推富失配诊断到 warnings）。
                    None => apply_section(&original, section),
                }
            } else {
                apply_section(&original, section)
            };
            // 写前记录快照（original 即模型读到/即将覆盖的 LF 正文），供后续 stale-hash 恢复。
            self.snapshots
                .write()
                .expect("snapshot lock poisoned")
                .record(&section.path, &original);

            // 回写保真：按原始行尾重新编码 + 还原 BOM（CRLF/BOM 文件编辑后不漂移）。
            if let Some(text) = result.text.as_mut() {
                *text = restore_line_endings(text, ending);
                if !bom.bom.is_empty() {
                    text.insert_str(0, &bom.bom);
                }
            }

            // MV：写到目标，删原文件
            if let Some(dest) = &result.moved_to {
                let dest_path = ctx.workspace.resolve(Path::new(dest));
                if let Some(new_text) = &result.text {
                    let report = write_ensure_parent(&dest_path, new_text, ctx).await?;
                    all_diagnostics.extend(report.diagnostics);
                }
                if target.exists() && target != dest_path {
                    tokio::fs::remove_file(&target)
                        .await
                        .map_err(ToolError::Io)?;
                }
                let diff =
                    build_compact_diff(&original, result.text.as_deref().unwrap_or(&original));
                summary.push(format!(
                    "{} → {dest}: MV（+{} / -{}）",
                    section.path, diff.added_lines, diff.removed_lines
                ));
                continue;
            }

            match &result.text {
                Some(new_text) => {
                    let diff = build_compact_diff(&original, new_text);
                    let report = write_ensure_parent(&target, new_text, ctx).await?;
                    all_diagnostics.extend(report.diagnostics);
                    // 告警以文本形式回灌：失配富诊断 + 恢复横幅，便于模型据诊断修正。
                    let warn_suffix = warning_suffix(&extra_warnings, &result.warnings);
                    summary.push(format!(
                        "{}: 应用 {} 操作（+{} / -{}）{warn_suffix}",
                        section.path,
                        section.hunks.len(),
                        diff.added_lines,
                        diff.removed_lines,
                    ));
                }
                None => {
                    summary.push(format!("{}: 无文本输出（异常）", section.path));
                }
            }
        }

        let diag_text = render_diagnostics(&all_diagnostics);
        if !diag_text.is_empty() {
            summary.push(diag_text.trim().to_string());
        }
        Ok(ToolResult::text(summary.join("\n")))
    }
}

async fn write_ensure_parent(
    path: &Path,
    content: &str,
    ctx: &ToolContext<'_>,
) -> Result<WriteReport, ToolError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(ToolError::Io)?;
        }
    }
    write_with_effects(path, content, ctx).await
}

/// 合并恢复期告警（extra）与 apply 告警为后缀文本；空则空串。
fn warning_suffix(extra: &[String], result_warnings: &[String]) -> String {
    let mut all: Vec<&str> = Vec::new();
    all.extend(extra.iter().map(String::as_str));
    all.extend(result_warnings.iter().map(String::as_str));
    if all.is_empty() {
        String::new()
    } else {
        format!("\n{}", all.join("\n"))
    }
}
