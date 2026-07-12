//! `apply_hashline` 工具：把 hashline patch 应用到工作区文件。
//!
//! 与 `apply_diff`（块级 SEARCH/REPLACE）互补：hashline 是**行号锚定**的批量编辑格式，
//! 适合一次性描述多文件、多区间、插入/删除/替换/重命名。

use std::path::Path;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use agent_tools::{render_diagnostics, write_with_effects, Tool, ToolContext, WriteReport};
use async_trait::async_trait;
use serde_json::json;

use crate::apply::apply_section;
use crate::parser::parse_hashline;
use crate::preview::build_compact_diff;
use crate::types::FileOp;

/// 应用 hashline patch 到工作区文件。
pub struct HashlineTool;

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

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext<'_>) -> Result<ToolResult, ToolError> {
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
                    tokio::fs::remove_file(&target).await.map_err(ToolError::Io)?;
                }
                summary.push(format!("{}: REM（已删除）", section.path));
                continue;
            }

            let original = tokio::fs::read_to_string(&target)
                .await
                .map_err(ToolError::Io)?;
            let result = apply_section(&original, section);

            // MV：写到目标，删原文件
            if let Some(dest) = &result.moved_to {
                let dest_path = ctx.workspace.resolve(Path::new(dest));
                if let Some(new_text) = &result.text {
                    let report = write_ensure_parent(&dest_path, new_text, ctx).await?;
                    all_diagnostics.extend(report.diagnostics);
                }
                if target.exists() && target != dest_path {
                    tokio::fs::remove_file(&target).await.map_err(ToolError::Io)?;
                }
                let diff = build_compact_diff(&original, result.text.as_deref().unwrap_or(&original));
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
                    summary.push(format!(
                        "{}: 应用 {} 操作（+{} / -{}）{}",
                        section.path,
                        section.hunks.len(),
                        diff.added_lines,
                        diff.removed_lines,
                        if result.warnings.is_empty() {
                            String::new()
                        } else {
                            format!("；告警 {}", result.warnings.len())
                        }
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
            tokio::fs::create_dir_all(parent).await.map_err(ToolError::Io)?;
        }
    }
    write_with_effects(path, content, ctx).await
}
