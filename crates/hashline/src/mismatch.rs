//! Hash 失配诊断与装饰行号锚解析。
//!
//! 移植自 [`oh-my-pi hashline/mismatch.ts`](../../../third/oh-my-pi/packages/hashline/src/mismatch.ts)。
//!
//! 当段头 hash 与落盘文件实际指纹不一致时，构造可操作诊断（而非裸错误）：
//! 区分「hash 非本会话产生」与「文件在 read 与 edit 之间被改动」两种情形，
//! 并附锚行附近 ±2 行的编号上下文，提示模型重新 read 刷新标签。

use crate::format::{HL_FILE_HASH_SEP, HL_FILE_PREFIX, HL_FILE_SUFFIX};
use crate::types::{Cursor, FileOp, FileSection, Hunk};

/// 失配上下文：锚行两侧各显示的行数。
pub const MISMATCH_CONTEXT: u32 = 2;

/// 解析装饰裸行号锚（如 `42` / `*42:foo` / `> 7`），返回 1-indexed 行号。
///
/// 等价上游正则 `^\s*[>+\-*]*\s*(\d+)(?::.*)?\s*$`：允许前导空白、`>`/`+`/`-`/`*` 标记前缀，
/// 后接数字；数字后可有 `:任意` 后缀，或仅尾随空白。
///
/// # Errors
/// 形态非法或行号 < 1 时返回带「期望形态」提示的错误串。
pub fn parse_tag(tag: &str) -> Result<u32, String> {
    let mut chars = tag.chars().peekable();
    // 前导空白
    while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
        chars.next();
    }
    // 标记前缀 '>' '+' '-' '*'
    while matches!(chars.peek(), Some('>') | Some('+') | Some('-') | Some('*')) {
        chars.next();
    }
    // 标记与数字间空白
    while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
        chars.next();
    }
    // 数字
    let mut num = String::new();
    while let Some(d) = chars.peek() {
        if d.is_ascii_digit() {
            num.push(*d);
            chars.next();
        } else {
            break;
        }
    }
    if num.is_empty() {
        return Err(format!(
            "Invalid line reference. Expected {}",
            format_full_anchor_requirement(Some(tag))
        ));
    }
    // 数字后：`:任意`（吞到尾）或仅尾随空白
    if chars.peek() == Some(&':') {
        while chars.next().is_some() {}
    } else {
        while let Some(c) = chars.peek() {
            if c.is_whitespace() {
                chars.next();
            } else {
                return Err(format!(
                    "Invalid line reference. Expected {}",
                    format_full_anchor_requirement(Some(tag))
                ));
            }
        }
    }
    let line: u32 = num.parse().map_err(|_| {
        format!(
            "Invalid line reference. Expected {}",
            format_full_anchor_requirement(Some(tag))
        )
    })?;
    if line < 1 {
        return Err(format!("Line number must be >= 1, got {line} in \"{tag}\"."));
    }
    Ok(line)
}

/// 模型期望的锚形态描述（收到非法锚时给出可操作示例）。
#[must_use]
pub fn format_full_anchor_requirement(raw: Option<&str>) -> String {
    let received = match raw {
        Some(r) => format!(" Received {r:?}."),
        None => String::new(),
    };
    format!(
        "a bare line number from read/search output plus the section header content-hash tag \
         (for example {p}src/foo.ts{s}1A2B{e} and line \"160\"){received}",
        p = HL_FILE_PREFIX,
        s = HL_FILE_HASH_SEP,
        e = HL_FILE_SUFFIX,
    )
}

/// 校验行号是否落在文件行数范围内。
///
/// # Errors
/// 越界时返回错误串。
pub fn validate_line_ref(line: u32, file_len: usize) -> Result<(), String> {
    if line < 1 || (line as usize) > file_len {
        return Err(format!(
            "Line {line} does not exist (file has {file_len} lines)"
        ));
    }
    Ok(())
}

/// 失配诊断细节。
#[derive(Debug, Clone, Default)]
pub struct MismatchDetails {
    /// 文件路径（可选）。
    pub path: Option<String>,
    /// 段头期望的 hash。
    pub expected_file_hash: String,
    /// 落盘文件实际 hash。
    pub actual_file_hash: String,
    /// 文件正文行（用于上下文）。
    pub file_lines: Vec<String>,
    /// 失配区段涉及的锚行（1-indexed）。
    pub anchor_lines: Vec<u32>,
    /// `true`=该 hash 曾在本会话记录过（文件被改动）；`false`=从未记录（疑似编造/复用）。
    pub hash_recognized: bool,
}

/// 拒绝头：两种模式的可操作文案。
#[must_use]
pub fn rejection_header(details: &MismatchDetails) -> Vec<String> {
    let path_text = match &details.path {
        Some(p) => format!(" for {p}"),
        None => String::new(),
    };
    if !details.hash_recognized {
        return vec![
            format!(
                "Edit rejected{path_text}: hash {s}{expected} is not from this session.",
                s = HL_FILE_HASH_SEP,
                expected = details.expected_file_hash,
            ),
            format!(
                "The current file hashes to {s}{actual}. Re-read the file with `read` to copy a \
                 current {p}path{s}tag{e} header — never invent the tag and never reuse one from \
                 a prior session.",
                s = HL_FILE_HASH_SEP,
                actual = details.actual_file_hash,
                p = HL_FILE_PREFIX,
                e = HL_FILE_SUFFIX,
            ),
        ];
    }
    vec![
        format!("Edit rejected{path_text}: file changed between read and edit."),
        format!(
            "Section is bound to {s}{expected}, but the current file hashes to {s}{actual}. \
             If a prior edit in this session modified this file, copy the {p}path{s}newhash{e} \
             header from that edit's response; otherwise re-read the file with `read` to refresh \
             the tag before retrying.",
            s = HL_FILE_HASH_SEP,
            expected = details.expected_file_hash,
            actual = details.actual_file_hash,
            p = HL_FILE_PREFIX,
            e = HL_FILE_SUFFIX,
        ),
    ]
}

/// 锚行 ±[`MISMATCH_CONTEXT`] 的编号上下文：`*` 标锚行、空格标普通行，非相邻段以 `...` 分隔。
#[must_use]
pub fn format_anchored_context(anchor_lines: &[u32], file_lines: &[&str]) -> Vec<String> {
    use std::collections::BTreeSet;

    let mut display: BTreeSet<u32> = BTreeSet::new();
    for &line in anchor_lines {
        if line < 1 || (line as usize) > file_lines.len() {
            continue;
        }
        let lo = line.saturating_sub(MISMATCH_CONTEXT).max(1);
        let hi = (line + MISMATCH_CONTEXT).min(file_lines.len() as u32);
        for n in lo..=hi {
            display.insert(n);
        }
    }

    let anchor_set: BTreeSet<u32> = anchor_lines.iter().copied().collect();
    let mut rows = Vec::new();
    let mut previous: i64 = -1;
    for &line_num in &display {
        if previous != -1 && i64::from(line_num) > previous + 1 {
            rows.push("...".to_string());
        }
        previous = i64::from(line_num);
        let marker = if anchor_set.contains(&line_num) {
            "*"
        } else {
            " "
        };
        let text = file_lines
            .get(line_num as usize - 1)
            .copied()
            .unwrap_or("");
        rows.push(format!("{marker}{line_num}:{text}"));
    }
    rows
}

/// 完整失配诊断：拒绝头 + 锚行上下文（多行 join）。
#[must_use]
pub fn format_display_message(details: &MismatchDetails) -> String {
    let mut lines = rejection_header(details);
    let file_lines_refs: Vec<&str> = details.file_lines.iter().map(String::as_str).collect();
    let context = format_anchored_context(&details.anchor_lines, &file_lines_refs);
    if context.is_empty() {
        lines.join("\n")
    } else {
        lines.push(String::new());
        lines.extend(context);
        lines.join("\n")
    }
}

/// 收集区段中各 hunk 的锚行（Replace/Delete 的 start，Insert 的 cursor 锚）。
#[must_use]
pub fn anchor_lines_of(section: &FileSection) -> Vec<u32> {
    let mut out = Vec::new();
    for hunk in &section.hunks {
        match hunk {
            Hunk::Replace { start, .. } | Hunk::Delete { start, .. } => out.push(*start),
            Hunk::Insert { cursor, .. } => match cursor {
                Cursor::BeforeAnchor(a) | Cursor::AfterAnchor(a) => out.push(*a),
                Cursor::Bof | Cursor::Eof => {}
            },
            Hunk::File(FileOp::Remove | FileOp::Move { .. }) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tag_accepts_decorations() {
        assert_eq!(parse_tag("42").unwrap(), 42);
        assert_eq!(parse_tag("*42:foo").unwrap(), 42);
        assert_eq!(parse_tag("> 7").unwrap(), 7);
        assert_eq!(parse_tag("  +160 ").unwrap(), 160);
        assert_eq!(parse_tag("-1:rest of line").unwrap(), 1);
    }

    #[test]
    fn parse_tag_rejects_non_digits() {
        assert!(parse_tag("foo").is_err());
        assert!(parse_tag("").is_err());
        assert!(parse_tag("0").is_err()); // < 1
        assert!(parse_tag("42 foo").is_err()); // 无冒号时禁止尾随非空白
    }

    #[test]
    fn validate_line_ref_bounds() {
        assert!(validate_line_ref(1, 10).is_ok());
        assert!(validate_line_ref(10, 10).is_ok());
        assert!(validate_line_ref(0, 10).is_err());
        assert!(validate_line_ref(11, 10).is_err());
    }

    #[test]
    fn anchored_context_marks_and_gaps() {
        let file = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        // 锚行 5 → 上下文 3..7，无间隔
        let rows = format_anchored_context(&[5], &file);
        assert_eq!(rows[0], " 3:c");
        assert!(rows.iter().any(|r| r == "*5:e"));
        assert_eq!(rows.last().unwrap(), " 7:g");
        assert!(!rows.iter().any(|r| r == "..."));
    }

    #[test]
    fn anchored_context_inserts_ellipsis_between_runs() {
        let file: Vec<&str> = (0..20).map(|_| "x").collect();
        // 锚 2 与锚 18 → 两个不相邻窗口，中间应出现 ...
        let rows = format_anchored_context(&[2, 18], &file);
        assert!(rows.iter().any(|r| r == "..."));
    }

    #[test]
    fn anchored_context_skips_out_of_range_anchors() {
        let file = ["a", "b"];
        let rows = format_anchored_context(&[99], &file);
        assert!(rows.is_empty());
    }

    #[test]
    fn display_message_includes_hashes_and_context() {
        let details = MismatchDetails {
            path: Some("src/a.rs".into()),
            expected_file_hash: "1A2B".into(),
            actual_file_hash: "3C4D".into(),
            file_lines: vec!["fn a() {}".into(), "fn b() {}".into(), "fn c() {}".into()],
            anchor_lines: vec![2],
            hash_recognized: true,
        };
        let msg = format_display_message(&details);
        assert!(msg.contains("file changed between read and edit"));
        assert!(msg.contains("#1A2B"));
        assert!(msg.contains("#3C4D"));
        assert!(msg.contains("*2:fn b()"));
    }

    #[test]
    fn rejection_header_unrecognized_hash() {
        let details = MismatchDetails {
            path: None,
            expected_file_hash: "DEAD".into(),
            actual_file_hash: "BEEF".into(),
            hash_recognized: false,
            ..Default::default()
        };
        let header = rejection_header(&details);
        assert_eq!(header.len(), 2);
        assert!(header[0].contains("is not from this session"));
        assert!(header[1].contains("never invent the tag"));
    }
}
