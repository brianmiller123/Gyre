//! `WorkspaceEdit` 应用器：把 LSP 编辑（rename / code actions / formatting）应用到文件文本。
//!
//! 移植 oh-my-pi `lsp/edits.ts` 的核心语义：
//! - 同文件编辑**自底向上**应用（按行/列降序），后应用的位置不因前一个编辑失效；
//! - **重叠校验**：同一文件内两个编辑区间重叠 → 报错（不静默吞掉）；
//! - 位置为 0-based UTF-16 code units（LSP 规范），按行先切分再换算列；
//! - 空区间（start == end）视为插入点。

use crate::client::{LspError, LspRenameEdit};

/// 把一组编辑应用到文件文本。
///
/// `edits` 须全部属于同一文件（调用方按 uri 分组后再调）。
///
/// # Errors
/// 编辑区间重叠、位置越界或 UTF-16 换算失败时返回 [`LspError`]。
pub fn apply_text_edits(text: &str, edits: &[LspRenameEdit]) -> Result<String, LspError> {
    if edits.is_empty() {
        return Ok(text.to_string());
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let line_starts = line_starts(text);

    // 校验 + 构造 (byte_start, byte_end, new_text)，自底向上排序。
    let mut resolved: Vec<(usize, usize, String)> = edits
        .iter()
        .map(|e| {
            let start = utf16_to_byte(text, &lines, &line_starts, e.line, e.character)?;
            let end = utf16_to_byte(text, &lines, &line_starts, e.end_line, e.end_character)?;
            if start > end {
                return Err(LspError::InvalidEdit(format!(
                    "编辑区间反向（{}:{} → {}:{}）",
                    e.line, e.character, e.end_line, e.end_character
                )));
            }
            Ok((start, end, e.new_text.clone()))
        })
        .collect::<Result<Vec<_>, LspError>>()?;

    // 自底向上：按起点降序（同起点按终点降序）。
    resolved.sort_unstable_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
    });

    // 重叠校验：相邻项（排序后）区间交叉即冲突。
    for w in resolved.windows(2) {
        let (s1, e1, _) = &w[0];
        let (s2, e2, _) = &w[1];
        // 降序排列下，w[0] 起点 ≥ w[1] 起点；若 w[0] 起点 < w[1] 终点 → 重叠。
        if *s1 < *e2 && *e1 > *s2 {
            return Err(LspError::InvalidEdit(format!(
                "编辑区间重叠（[{s1},{e1}) 与 [{s2},{e2})）"
            )));
        }
    }

    let mut out = String::with_capacity(text.len() + 64);
    // 降序应用：从尾部往前拼片段（后应用大偏移，先取 text[end..cursor] 未处理区）。
    let mut cursor = text.len();
    let mut parts: Vec<&str> = Vec::with_capacity(resolved.len() * 2 + 1);
    for (start, end, new_text) in &resolved {
        parts.push(&text[*end..cursor]);
        parts.push(new_text);
        cursor = *start;
    }
    parts.push(&text[0..cursor]);
    parts.reverse();
    for p in parts {
        out.push_str(p);
    }
    Ok(out)
}

/// 每行起始字节偏移（含行尾 `\n` 属于前一行）。
fn line_starts(text: &str) -> Vec<usize> {
    let mut out = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            out.push(i + 1);
        }
    }
    out
}

/// 0-based 行/UTF-16 列 → 字节偏移。
///
/// # Errors
/// 行越界或列不在合法 UTF-16 边界（代理对中间）时返回 [`LspError`]。
fn utf16_to_byte(
    _text: &str,
    lines: &[&str],
    line_starts: &[usize],
    line: u32,
    character: u32,
) -> Result<usize, LspError> {
    #[allow(clippy::cast_possible_truncation)]
    let line = line as usize;
    let Some(&base) = line_starts.get(line) else {
        return Err(LspError::InvalidEdit(format!(
            "行 {line} 越界（文件共 {} 行）",
            lines.len()
        )));
    };
    let line_text = lines.get(line).copied().unwrap_or("");
    let mut units = 0u32;
    for (off, ch) in line_text.char_indices() {
        let w = u32::try_from(ch.len_utf16()).expect("UTF-16 单元数 ≤ 2");
        if character >= units && character < units + w {
            // 落在本字符的 UTF-16 区间内：恰为起点 → 合法；代理对中间 → 非法。
            if character == units {
                return Ok(base + off);
            }
            return Err(LspError::InvalidEdit(format!(
                "列 {character} 落在多单元字符（UTF-16 代理对）中间"
            )));
        }
        units += w;
    }
    // 列落在行尾（含行尾插入场景）或恰好为行长度。
    if units == character {
        return Ok(base + line_text.len());
    }
    Err(LspError::InvalidEdit(format!(
        "列 {character} 越界（行 {line} 共 {units} UTF-16 单元）"
    )))
}

/// 多文件编辑分组应用：`(uri → 编辑列表)` → `(uri → 应用后文本)`。
///
/// # Errors
/// 任一组编辑应用失败（重叠/越界）时返回 [`LspError`]（整批不落盘，调用方决定）。
pub fn apply_grouped(
    groups: &[(String, Vec<LspRenameEdit>)],
) -> Result<Vec<(String, String)>, LspError> {
    let mut out = Vec::with_capacity(groups.len());
    for (uri, edits) in groups {
        let text = std::fs::read_to_string(uri_to_path(uri))
            .map_err(|e| LspError::InvalidEdit(format!("读取 {uri} 失败: {e}")))?;
        let new_text = apply_text_edits(&text, edits)?;
        out.push((uri.clone(), new_text));
    }
    Ok(out)
}

/// `file://` URI → 本地路径（不做解码以外的处理）。
#[must_use]
pub fn uri_to_path(uri: &str) -> String {
    uri.strip_prefix("file://")
        .map_or_else(|| uri.to_string(), |s| s.replace("%20", " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(uri: &str, line: u32, ch: u32, end_line: u32, end_ch: u32, new_text: &str) -> LspRenameEdit {
        LspRenameEdit {
            uri: uri.into(),
            line,
            character: ch,
            end_line,
            end_character: end_ch,
            new_text: new_text.into(),
            old_text: String::new(),
        }
    }

    #[test]
    fn applies_bottom_up() {
        let text = "aaa\nbbb\nccc\n";
        // 两处替换：L1 与 L2 同时改。
        let edits = vec![
            edit("u", 1, 0, 1, 3, "BBB"),
            edit("u", 2, 0, 2, 3, "CCC"),
        ];
        assert_eq!(apply_text_edits(text, &edits).unwrap(), "aaa\nBBB\nCCC\n");
    }

    #[test]
    fn overlapping_edits_rejected() {
        let text = "abcdef";
        let edits = vec![
            edit("u", 0, 1, 0, 4, "X"),
            edit("u", 0, 2, 0, 5, "Y"),
        ];
        let err = apply_text_edits(text, &edits).unwrap_err();
        assert!(err.to_string().contains("重叠"), "{err}");
    }

    #[test]
    fn insertion_at_empty_range() {
        let text = "ab";
        // 在 0 行 1 列插入（空区间）。
        let edits = vec![edit("u", 0, 1, 0, 1, "X")];
        assert_eq!(apply_text_edits(text, &edits).unwrap(), "aXb");
    }

    #[test]
    fn utf16_surrogate_pair_column() {
        // 「你」= U+4F60 单 BMP 字符；😀 = U+1F600 代理对（2 UTF-16 单元）。
        let text = "a😀b";
        // 列 3 = 'b' 前（a=1 单元 + 😀=2 单元）。
        let edits = vec![edit("u", 0, 3, 0, 3, "X")];
        assert_eq!(apply_text_edits(text, &edits).unwrap(), "a😀Xb");
        // 列 2 = 代理对中间 → 报错。
        let err = apply_text_edits(text, &[edit("u", 0, 2, 0, 2, "X")]).unwrap_err();
        assert!(err.to_string().contains("代理对"), "{err}");
    }

    #[test]
    fn multi_line_region() {
        let text = "line1\nline2\nline3\n";
        let edits = vec![edit("u", 0, 0, 2, 5, "REPLACED")];
        assert_eq!(apply_text_edits(text, &edits).unwrap(), "REPLACED\n");
    }

    #[test]
    fn out_of_bounds_line_rejected() {
        let text = "one line";
        let err = apply_text_edits(text, &[edit("u", 5, 0, 5, 0, "X")]).unwrap_err();
        assert!(err.to_string().contains("越界"), "{err}");
    }
}
