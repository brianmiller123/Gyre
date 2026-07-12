//! Hashline 应用器：把一个 [`FileSection`] 的 hunk 列表应用到文本正文。
//!
//! 移植自 [`oh-my-pi hashline/apply.ts`](../../../third/oh-my-pi/packages/hashline/src/apply.ts)（核心算法）。
//!
//! 策略：把 Replace/Delete 归并为「区间表」，Insert 归入 before/after/bof/eof 桶，
//! 单遍扫描原始行产出新正文。区间表与插入锚假定不重叠（常见情形）；重叠时以告警丢弃后到者。
//! 含结构闭包边界回声修复（body 误重述区间外不变的行时自动去除）。

use std::collections::BTreeMap;

use crate::format::compute_file_hash;
use crate::types::{Anchor, ApplyResult, Cursor, FileOp, FileSection, Hunk};

/// 把一个区段应用到 `text`，返回结果（含告警）。
#[must_use]
pub fn apply_section(text: &str, section: &FileSection) -> ApplyResult {
    let mut warnings: Vec<String> = Vec::new();

    // 指纹校验（宽容：不匹配仅告警，继续应用）
    if let Some(expected) = &section.hash {
        let actual = compute_file_hash(text);
        if !actual.eq_ignore_ascii_case(expected) {
            warnings.push(format!(
                "hash 不匹配：段头 {expected}，实际 {actual}（宽容模式继续应用）"
            ));
        }
    }

    // 文件级操作优先：REM 删文件
    for hunk in &section.hunks {
        if matches!(hunk, Hunk::File(FileOp::Remove)) {
            return ApplyResult {
                text: None,
                warnings,
                first_changed_line: Some(1),
                moved_to: None,
            };
        }
    }
    // MV：记录目标，正文不变
    let moved_to = section.hunks.iter().find_map(|h| match h {
        Hunk::File(FileOp::Move { dest }) => Some(dest.clone()),
        _ => None,
    });

    // 拆行（剥除尾部换行产生的幻影空行）
    let had_trailing_newline = text.ends_with('\n');
    let mut lines: Vec<&str> = text.split('\n').collect();
    if had_trailing_newline {
        lines.pop();
    }
    let n = lines.len();

    let mut ranges: BTreeMap<Anchor, (Anchor, Option<Vec<String>>)> = BTreeMap::new();
    let mut before: BTreeMap<Anchor, Vec<Vec<String>>> = BTreeMap::new();
    let mut after: BTreeMap<Anchor, Vec<Vec<String>>> = BTreeMap::new();
    let mut bof: Vec<Vec<String>> = Vec::new();
    let mut eof: Vec<Vec<String>> = Vec::new();
    let mut first_changed: Option<Anchor> = None;

    let note = |fc: &mut Option<Anchor>, v: Anchor| {
        if fc.is_none() || Some(v) < *fc {
            *fc = Some(v);
        }
    };

    for hunk in &section.hunks {
        match hunk {
            Hunk::Replace { start, end, body } => {
                validate_bounds(*start, *end, n, &mut warnings);
                if ranges.contains_key(start) {
                    warnings.push(format!("行 {start} 已被前一个区间覆盖，后到 Replace 被丢弃"));
                    continue;
                }
                let repaired = repair_boundaries(&lines, *start, *end, body);
                note(&mut first_changed, *start);
                ranges.insert(*start, (*end, Some(repaired)));
            }
            Hunk::Delete { start, end } => {
                validate_bounds(*start, *end, n, &mut warnings);
                if ranges.contains_key(start) {
                    warnings.push(format!("行 {start} 已被前一个区间覆盖，后到 Delete 被丢弃"));
                    continue;
                }
                note(&mut first_changed, *start);
                ranges.insert(*start, (*end, None));
            }
            Hunk::Insert { cursor, body } => match cursor {
                Cursor::Bof => {
                    bof.push(body.clone());
                    note(&mut first_changed, 1);
                }
                Cursor::Eof => {
                    eof.push(body.clone());
                    note(&mut first_changed, n.saturating_add(1) as Anchor);
                }
                Cursor::BeforeAnchor(a) => {
                    validate_anchor(*a, n, &mut warnings);
                    before.entry(*a).or_default().push(body.clone());
                    note(&mut first_changed, *a);
                }
                Cursor::AfterAnchor(a) => {
                    validate_anchor(*a, n, &mut warnings);
                    after.entry(*a).or_default().push(body.clone());
                    note(&mut first_changed, a.saturating_add(1));
                }
            },
            Hunk::File(FileOp::Remove) | Hunk::File(FileOp::Move { .. }) => {}
        }
    }

    // 单遍扫描产出
    let mut out: Vec<String> = Vec::new();
    for body in &bof {
        out.extend(body.iter().cloned());
    }
    flush_bucket(&before, 1, &mut out);

    let mut i = 1usize;
    while i <= n {
        if let Some((end, body)) = ranges.get(&(i as Anchor)) {
            if let Some(body) = body {
                out.extend(body.iter().cloned());
            }
            i = end.saturating_add(1) as usize;
            flush_bucket(&before, i as Anchor, &mut out);
            continue;
        }
        out.push(lines[i - 1].to_string());
        flush_bucket(&after, i as Anchor, &mut out);
        i += 1;
        flush_bucket(&before, i as Anchor, &mut out);
    }
    for body in &eof {
        out.extend(body.iter().cloned());
    }

    let mut result = out.join("\n");
    if had_trailing_newline && !result.ends_with('\n') {
        result.push('\n');
    }

    ApplyResult {
        text: Some(result),
        warnings,
        first_changed_line: first_changed,
        moved_to,
    }
}

fn flush_bucket(map: &BTreeMap<Anchor, Vec<Vec<String>>>, line: Anchor, out: &mut Vec<String>) {
    if let Some(bodies) = map.get(&line) {
        for body in bodies {
            out.extend(body.iter().cloned());
        }
    }
}

fn validate_bounds(start: Anchor, end: Anchor, n: usize, warnings: &mut Vec<String>) {
    if start == 0 || (end as usize) > n {
        warnings.push(format!(
            "区间 {start}.={end} 越界（文件共 {n} 行）"
        ));
    }
}

fn validate_anchor(a: Anchor, n: usize, warnings: &mut Vec<String>) {
    if a == 0 || (a as usize) > n {
        warnings.push(format!("锚点行 {a} 越界（文件共 {n} 行）"));
    }
}

/// 结构闭包边界回声修复：去掉 body 误重述的区间外不变行。
///
/// - body 首行等于区间前一行（`start-1`）时去掉
/// - body 末行等于区间后一行（`end+1`）时去掉
fn repair_boundaries(lines: &[&str], start: Anchor, end: Anchor, body: &[String]) -> Vec<String> {
    if body.is_empty() {
        return Vec::new();
    }
    let mut repaired: Vec<String> = body.to_vec();

    // 前回声：body 首行 == 区间前一行
    if start > 1 {
        let prev = lines.get(start as usize - 2);
        if let Some(prev) = prev {
            if repaired.first().is_some_and(|f| f == prev) {
                repaired.remove(0);
            }
        }
    }
    // 后回声：body 末行 == 区间后一行
    let after_idx = end as usize; // 0-indexed: lines[end]
    if let Some(next) = lines.get(after_idx) {
        if repaired.last().is_some_and(|l| l == next) {
            repaired.pop();
        }
    }
    repaired
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_hashline;

    fn apply(patch: &str, original: &str) -> ApplyResult {
        let sections = parse_hashline(patch).unwrap();
        apply_section(original, &sections[0])
    }

    #[test]
    fn swap_single_line() {
        let r = apply("[a]\nSWAP 1.=1:\n+ALPHA\n", "alpha\nbeta\n");
        assert_eq!(r.text.as_deref(), Some("ALPHA\nbeta\n"));
        assert_eq!(r.first_changed_line, Some(1));
    }

    #[test]
    fn del_range_and_insert() {
        let r = apply("[a]\nDEL 2.=3\nINS.POST 1:\n+x\n", "a\nb\nc\nd\n");
        assert_eq!(r.text.as_deref(), Some("a\nx\nd\n"));
    }

    #[test]
    fn head_and_tail_insert() {
        let r = apply("[a]\nINS.HEAD:\n+top\nINS.TAIL:\n+end\n", "mid\n");
        assert_eq!(r.text.as_deref(), Some("top\nmid\nend\n"));
    }

    #[test]
    fn boundary_echo_repair() {
        // body 末行 "+c" 误重述了区间后一行（原 line 3 = "c"），应被自动去除
        let r = apply("[a]\nSWAP 2.=2:\n+B\n+c\n", "a\nb\nc\nd\n");
        assert_eq!(r.text.as_deref(), Some("a\nB\nc\nd\n"));
    }

    #[test]
    fn rem_deletes_file() {
        let r = apply("[a]\nREM\n", "anything\n");
        assert!(r.text.is_none());
    }

    #[test]
    fn mv_records_dest() {
        let r = apply("[a]\nMV b.txt\n", "content\n");
        assert_eq!(r.moved_to.as_deref(), Some("b.txt"));
        assert_eq!(r.text.as_deref(), Some("content\n"));
    }

    #[test]
    fn trailing_newline_preserved() {
        let r = apply("[a]\nSWAP 1.=1:\n+X\n", "a\n");
        assert_eq!(r.text.as_deref(), Some("X\n"));
        let r2 = apply("[a]\nSWAP 1.=1:\n+X\n", "a");
        assert_eq!(r2.text.as_deref(), Some("X"));
    }
}
