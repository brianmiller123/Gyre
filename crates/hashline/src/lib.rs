//! # agent-hashline
//!
//! Hashline 行锚定差分：`[path#hash]` 段头 + `SWAP/DEL/INS/REM/MV` 行操作。
//!
//! 移植自 [`oh-my-pi hashline`](../../../third/oh-my-pi/packages/hashline)（核心语法 + 应用器 + 预览）。
//!
//! hashline 用**行号锚定**描述批量编辑，适合一次性改动多文件、多区间；
//! 每次编辑后行号重编号，须基于最新读取结果。工具收敛后为唯一编辑工具。
//!
//! 模块：
//! - [`format`] —— sigil / 段头 / 内容指纹（FNV-1a 低 16 位 4 hex）
//! - [`parser`] —— patch 文本 → [`FileSection`]
//! - [`apply`] —— 区段 → 应用到文本（区间表 + 插入桶 + 边界回声修复）
//! - [`preview`] —— 编辑前后紧凑差异
//! - [`tool`] —— `apply_hashline` 工具

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

pub mod apply;
pub mod format;
pub mod mismatch;
pub mod normalize;
pub mod parser;
pub mod preview;
pub mod recovery;
mod repair;
pub mod snapshots;
pub mod tool;
pub mod types;

pub use apply::apply_section;
pub use format::{compute_file_hash, format_numbered_lines};
pub use mismatch::{
    MISMATCH_CONTEXT, MismatchDetails, anchor_lines_of, format_anchored_context,
    format_display_message, format_full_anchor_requirement, parse_tag, rejection_header,
    validate_line_ref,
};
pub use normalize::{
    BomResult, LineEnding, detect_line_ending, normalize_to_lf, restore_line_endings, strip_bom,
};
pub use parser::parse_hashline;
pub use preview::{CompactDiffPreview, build_compact_diff};
pub use recovery::{RECOVERY_SESSION_REPLAY_WARNING, RecoveryResult, recover};
pub use snapshots::{InMemorySnapshotStore, Snapshot};
pub use tool::HashlineTool;
pub use types::{Anchor, ApplyResult, Cursor, FileOp, FileSection, Hunk, PatchReport};

/// 注入 system prompt 的 hashline 工具使用指引（启用时由装配层追加）。
///
/// 工具收敛后：apply_hashline 为唯一编辑工具（write_file 仅整文件创建/覆写）。
pub const PROMPT_SECTION: &str = "<hashline>\n\
编辑文件用 `apply_hashline`：行锚定、一次调用可改多文件/多区间。\n\
每段以 [path#hash] 开头（hash 取自最近一次 read 的段头标签，勿编造、勿跨会话复用）；段内用 SWAP/DEL/INS/REM/MV 描述行操作。\n\
每次编辑后行号重编号，须基于最新 read 的行号。\n\
创建/整体覆写新文件用 `write_file`；对已有内容的任何改动一律用 apply_hashline（项目已移除 str_replace / apply_diff）。\n\
hash 失配时按返回诊断修正（含期望/实际标签 + 锚行上下文，据此重读刷新标签后再试）。\n\
</hashline>";

/// 纯函数便捷入口：把单区段 patch 应用到 `original` 文本。
///
/// # Errors
/// 解析失败时返回错误信息。
pub fn apply_hashline_to_text(patch: &str, original: &str) -> Result<ApplyResult, String> {
    let sections = parse_hashline(patch)?;
    let section = sections.into_iter().next().ok_or("patch 不含任何区段")?;
    Ok(apply_section(original, &section))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_to_end_text_apply() {
        let original = "fn main() {\n    println!(\"hi\");\n}\n";
        let patch = "[src/main.rs]\nSWAP 2.=2:\n+    println!(\"hello\");\n";
        let result = apply_hashline_to_text(patch, original).unwrap();
        assert_eq!(
            result.text.as_deref(),
            Some("fn main() {\n    println!(\"hello\");\n}\n")
        );
        assert_eq!(result.first_changed_line, Some(2));
    }

    #[test]
    fn numbered_lines_for_anchoring() {
        let text = "a\nb\nc\n";
        let numbered = format_numbered_lines(text, 1);
        assert!(numbered.contains("2:b"));
    }
}
