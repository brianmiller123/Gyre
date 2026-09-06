//! Hashline 格式原语：sigil、分隔符、段头/行格式化、内容指纹。
//!
//! 移植自 [`oh-my-pi hashline/format.ts`](../../../third/oh-my-pi/packages/hashline/src/format.ts)。
//! 文件指纹用 FNV-1a 32-bit 的低 16 位（4 hex 大写），语义与原版 xxHash32 低 16 位一致：
//! 任意字节级一致的内容产生相同标签；编辑后重新 `read` 即可重新锚定。

use crate::types::{Anchor, Cursor};

/// 段头前/后缀：`[path#hash]`。
pub const HL_FILE_PREFIX: char = '[';
pub const HL_FILE_SUFFIX: char = ']';
/// 正文行 sigil：`+TEXT`。
pub const HL_PAYLOAD_REPLACE: char = '+';
/// 段头 path 与 hash 分隔符：`path#hash`。
pub const HL_FILE_HASH_SEP: char = '#';
/// 范围分隔符：`start.=end`。
pub const HL_RANGE_SEP: &str = ".=";
/// hunk 头尾冒号。
pub const HL_HEADER_COLON: char = ':';

/// hunk 头关键字。
pub mod kw {
    pub const SWAP: &str = "SWAP";
    pub const DEL: &str = "DEL";
    pub const INS: &str = "INS";
    pub const REM: &str = "REM";
    pub const MV: &str = "MV";
    pub const INS_PRE: &str = "PRE";
    pub const INS_POST: &str = "POST";
    pub const INS_HEAD: &str = "HEAD";
    pub const INS_TAIL: &str = "TAIL";
}

/// 指纹 hex 长度。
pub use agent_tools::snapshot::HASH_LEN;

/// 格式化段头：`[path#hash]` 或 `[path]`（无 hash）。
#[must_use]
pub fn format_header(path: &str, hash: Option<&str>) -> String {
    match hash {
        Some(h) => format!("{HL_FILE_PREFIX}{path}{HL_FILE_HASH_SEP}{h}{HL_FILE_SUFFIX}"),
        None => format!("{HL_FILE_PREFIX}{path}{HL_FILE_SUFFIX}"),
    }
}

/// 格式化替换头：`SWAP start.=end:`。
#[must_use]
pub fn format_replace_header(start: Anchor, end: Anchor) -> String {
    format!("{} {start}{}{end}{HL_HEADER_COLON}", kw::SWAP, HL_RANGE_SEP)
}

/// 格式化删除头：`DEL start.=end` 或 `DEL N`。
#[must_use]
pub fn format_delete_header(start: Anchor, end: Anchor) -> String {
    if start == end {
        format!("{} {start}", kw::DEL)
    } else {
        format!("{} {start}{}{end}", kw::DEL, HL_RANGE_SEP)
    }
}

/// 格式化插入头。
#[must_use]
pub fn format_insert_header(cursor: &Cursor) -> String {
    match cursor {
        Cursor::BeforeAnchor(n) => format!("{}.{} {n}{HL_HEADER_COLON}", kw::INS, kw::INS_PRE),
        Cursor::AfterAnchor(n) => format!("{}.{} {n}{HL_HEADER_COLON}", kw::INS, kw::INS_POST),
        Cursor::Bof => format!("{}.{}{HL_HEADER_COLON}", kw::INS, kw::INS_HEAD),
        Cursor::Eof => format!("{}.{}{HL_HEADER_COLON}", kw::INS, kw::INS_TAIL),
    }
}

/// 把文本格式化为带行号前缀的展示（`N:TEXT`），供模型锚定。
#[must_use]
pub fn format_numbered_lines(text: &str, start_line: Anchor) -> String {
    text.split('\n')
        .enumerate()
        .map(|(i, line)| format!("{}:{line}", start_line + i as u32))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 内容指纹：4 hex 大写。规范化先去除每行尾随空白与 `\r`。
///
/// 实体在 `agent-tools::snapshot`（`ToolContext` 字段所在 crate；本 crate 再导出，
/// `agent_hashline::format::compute_file_hash` 对外路径不变）。
pub use agent_tools::snapshot::compute_file_hash;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_headers() {
        assert_eq!(format_header("a.txt", Some("1A2B")), "[a.txt#1A2B]");
        assert_eq!(format_header("a.txt", None), "[a.txt]");
        assert_eq!(format_replace_header(3, 5), "SWAP 3.=5:");
        assert_eq!(format_delete_header(4, 4), "DEL 4");
        assert_eq!(format_delete_header(4, 6), "DEL 4.=6");
        assert_eq!(format_insert_header(&Cursor::AfterAnchor(2)), "INS.POST 2:");
        assert_eq!(format_insert_header(&Cursor::Bof), "INS.HEAD:");
    }

    #[test]
    fn numbered_lines_start_offset() {
        assert_eq!(format_numbered_lines("a\nb", 10), "10:a\n11:b");
    }
}
