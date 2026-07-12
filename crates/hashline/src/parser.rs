//! Hashline 文本解析器：把 patch 文本解析为 [`FileSection`] 列表。
//!
//! 移植自 [`oh-my-pi hashline/parser.ts`](../../../third/oh-my-pi/packages/hashline/src/parser.ts)（精简核心语法）。
//!
//! 支持语法：
//! - 段头 `[path]` / `[path#HASH]`
//! - `SWAP start.=end:` / `SWAP N:`（替换，后跟 `+TEXT` 正文）
//! - `DEL start.=end` / `DEL N`（删除）
//! - `INS.PRE N:` / `INS.POST N:` / `INS.HEAD:` / `INS.TAIL:`（插入，后跟 `+TEXT` 正文）
//! - `REM`（删文件） / `MV dest`（重命名）
//!
//! 正文行以 `+` 起首；不以 `+` 起首的行结束当前 hunk 的正文收集。

use crate::format::{HL_FILE_HASH_SEP, HL_FILE_PREFIX, HL_FILE_SUFFIX, HL_HEADER_COLON, HL_PAYLOAD_REPLACE, HL_RANGE_SEP, kw};
use crate::types::{Anchor, Cursor, FileOp, FileSection, Hunk};

/// 解析整个 patch 文本为区段列表。
///
/// # Errors
/// 遇到无法识别的 hunk 头或非法行号时返回错误信息。
pub fn parse_hashline(text: &str) -> Result<Vec<FileSection>, String> {
    let mut sections: Vec<FileSection> = Vec::new();
    let mut current: Option<FileSection> = None;
    let mut open_hunk: Option<Hunk> = None;

    for (lineno, raw) in text.lines().enumerate() {
        let line_no = lineno + 1;
        let trimmed = raw.trim_start();

        // 空行/注释：结束当前正文收集
        if trimmed.is_empty() || trimmed.starts_with('#') {
            close_hunk(&mut current, &mut open_hunk);
            continue;
        }

        // 段头 [path#hash]
        if let Some(body) = strip_brackets(trimmed) {
            close_hunk(&mut current, &mut open_hunk);
            if let Some(prev) = current.take() {
                sections.push(prev);
            }
            let (path, hash) = split_path_hash(body);
            current = Some(FileSection { path, hash, hunks: Vec::new() });
            continue;
        }

        // 正文行 +TEXT
        if let Some(rest) = trimmed.strip_prefix(HL_PAYLOAD_REPLACE) {
            if let Some(hunk) = open_hunk.as_mut() {
                push_body(hunk, rest);
            } else if current.is_some() {
                // 有段但无打开的 hunk：正文悬空，忽略并告警
                return Err(format!("line {line_no}: 正文行 `+...` 出现在任何 hunk 头之前"));
            }
            continue;
        }

        // 否则视作 hunk 头：先关闭上一个 hunk
        close_hunk(&mut current, &mut open_hunk);
        let hunk = parse_hunk_header(trimmed)
            .map_err(|e| format!("line {line_no}: {e}"))?;
        // 文件级操作（REM/MV）立即落地，不收集正文
        if let Hunk::File(_) = hunk {
            if let Some(section) = current.as_mut() {
                section.hunks.push(hunk);
            }
            open_hunk = None;
        } else {
            open_hunk = Some(hunk);
        }
    }

    close_hunk(&mut current, &mut open_hunk);
    if let Some(prev) = current.take() {
        sections.push(prev);
    }
    Ok(sections)
}

fn strip_brackets(s: &str) -> Option<&str> {
    s.strip_prefix(HL_FILE_PREFIX)?.strip_suffix(HL_FILE_SUFFIX)
}

fn split_path_hash(body: &str) -> (String, Option<String>) {
    match body.split_once(HL_FILE_HASH_SEP) {
        Some((p, h)) if !h.is_empty() && is_hex_tag(h) => (p.to_string(), Some(h.to_string())),
        _ => (body.to_string(), None),
    }
}

fn is_hex_tag(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn close_hunk(current: &mut Option<FileSection>, open: &mut Option<Hunk>) {
    if let Some(h) = open.take() {
        if let Some(section) = current.as_mut() {
            section.hunks.push(h);
        }
    }
}

fn push_body(hunk: &mut Hunk, text: &str) {
    match hunk {
        Hunk::Replace { body, .. } | Hunk::Insert { body, .. } => body.push(text.to_string()),
        Hunk::Delete { .. } | Hunk::File(_) => {}
    }
}

fn parse_hunk_header(s: &str) -> Result<Hunk, String> {
    // 关键字 = 首个空白前 token；`INS.HEAD:`/`INS.TAIL:` 无空白，冒号附在关键字上，故去尾冒号。
    let (head_tok, rest_with_colon) = s.split_once(char::is_whitespace).unwrap_or((s, ""));
    let head = head_tok.trim_end_matches(HL_HEADER_COLON);
    let rest = rest_with_colon.trim_end_matches(HL_HEADER_COLON).trim();

    match head {
        kw::SWAP => {
            let (start, end) = parse_range(rest)?;
            Ok(Hunk::Replace { start, end, body: Vec::new() })
        }
        kw::DEL => {
            let (start, end) = parse_range(rest)?;
            Ok(Hunk::Delete { start, end })
        }
        "INS.HEAD" => {
            require_empty(rest)?;
            Ok(Hunk::Insert { cursor: Cursor::Bof, body: Vec::new() })
        }
        "INS.TAIL" => {
            require_empty(rest)?;
            Ok(Hunk::Insert { cursor: Cursor::Eof, body: Vec::new() })
        }
        "INS.PRE" => {
            Ok(Hunk::Insert { cursor: Cursor::BeforeAnchor(parse_anchor(rest)?), body: Vec::new() })
        }
        "INS.POST" => {
            Ok(Hunk::Insert { cursor: Cursor::AfterAnchor(parse_anchor(rest)?), body: Vec::new() })
        }
        kw::REM => {
            require_empty(rest)?;
            Ok(Hunk::File(FileOp::Remove))
        }
        kw::MV => {
            if rest.is_empty() {
                return Err("MV 缺少目标路径".to_string());
            }
            Ok(Hunk::File(FileOp::Move { dest: rest.to_string() }))
        }
        other => Err(format!("无法识别的 hunk 头 `{other}`")),
    }
}

fn require_empty(s: &str) -> Result<(), String> {
    if s.is_empty() {
        Ok(())
    } else {
        Err(format!("此处不应有参数 `{s}`"))
    }
}

fn parse_anchor(s: &str) -> Result<Anchor, String> {
    s.parse::<Anchor>()
        .map_err(|_| format!("非法行号 `{s}`"))
}

/// 解析 `N` 或 `N.=M`；返回 `(start, end)`，单值时 `start==end`。
fn parse_range(s: &str) -> Result<(Anchor, Anchor), String> {
    if let Some((a, b)) = s.split_once(HL_RANGE_SEP) {
        let start = a.parse::<Anchor>().map_err(|_| format!("非法起始行 `{a}`"))?;
        let end = b.parse::<Anchor>().map_err(|_| format!("非法结束行 `{b}`"))?;
        if end < start {
            return Err(format!("范围结束 {end} 小于起始 {start}"));
        }
        Ok((start, end))
    } else {
        let n = s.parse::<Anchor>().map_err(|_| format!("非法行号 `{s}`"))?;
        Ok((n, n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_swap_del_ins() {
        let patch = "[a.txt#1A2B]\nSWAP 1.=1:\n+ALPHA\nDEL 3\nINS.POST 2:\n+middle\nINS.HEAD:\n+top\n";
        let sections = parse_hashline(patch).unwrap();
        assert_eq!(sections.len(), 1);
        let s = &sections[0];
        assert_eq!(s.path, "a.txt");
        assert_eq!(s.hash.as_deref(), Some("1A2B"));
        assert_eq!(s.hunks.len(), 4);
        assert!(matches!(s.hunks[1], Hunk::Delete { start: 3, end: 3 }));
    }

    #[test]
    fn parses_file_ops() {
        let patch = "[a.txt]\nREM\n";
        let s = parse_hashline(patch).unwrap();
        assert!(matches!(&s[0].hunks[0], Hunk::File(FileOp::Remove)));
        let patch = "[a.txt]\nMV b.txt\n";
        let s = parse_hashline(patch).unwrap();
        assert!(matches!(&s[0].hunks[0], Hunk::File(FileOp::Move { dest }) if dest == "b.txt"));
    }

    #[test]
    fn rejects_bad_range() {
        assert!(parse_range("5.=2").is_err());
        assert!(parse_range("5.=2").unwrap_err().contains("小于"));
        assert_eq!(parse_range("7").unwrap(), (7, 7));
        assert_eq!(parse_range("3.=5").unwrap(), (3, 5));
    }
}
