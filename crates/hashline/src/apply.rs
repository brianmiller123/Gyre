//! Hashline 应用器：把一个 [`FileSection`] 的 hunk 列表应用到文本正文。
//!
//! 移植自 [`oh-my-pi hashline/apply.ts`](../../../third/oh-my-pi/packages/hashline/src/apply.ts)（核心算法）。
//!
//! 策略：把 Replace/Delete 归并为「区间表」，Insert 归入 before/after/bof/eof 桶，
//! 单遍扫描原始行产出新正文。区间表与插入锚假定不重叠（常见情形）；重叠时以告警丢弃后到者。
//! 含结构闭包边界回声修复（body 误重述区间外不变的行时自动去除）。

use std::collections::{BTreeMap, HashSet};

use crate::format::compute_file_hash;
use crate::mismatch::MismatchDetails;
use crate::repair::{repair_replacement_boundaries, ReplaceGroup};
use crate::types::{Anchor, ApplyResult, Cursor, FileOp, FileSection, Hunk};

/// 把一个区段应用到 `text`，返回结果（含告警）。
#[must_use]
pub fn apply_section(text: &str, section: &FileSection) -> ApplyResult {
    let mut warnings: Vec<String> = Vec::new();

    // 指纹校验（宽容：不匹配仅告警，继续应用）—— 失配时给出可操作富诊断。
    if let Some(expected) = &section.hash {
        let actual = compute_file_hash(text);
        if !actual.eq_ignore_ascii_case(expected) {
            warnings.push(crate::mismatch::format_display_message(&MismatchDetails {
                path: Some(section.path.clone()),
                expected_file_hash: expected.clone(),
                actual_file_hash: actual,
                file_lines: text.lines().map(String::from).collect(),
                anchor_lines: crate::mismatch::anchor_lines_of(section),
                // apply 层无快照存储，无法判定「是否本会话产生」；按「文件被改」给文案。
                hash_recognized: true,
            }));
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

    // 收集替换组 / 删除区间 / 插入桶：先收集再修复，因边界修复需访问整 patch 投影
    //（分隔符平衡、区间上下方幸存行）。
    let mut groups: Vec<ReplaceGroup> = Vec::new();
    let mut deletes: Vec<(Anchor, Anchor)> = Vec::new();
    let mut before: BTreeMap<Anchor, Vec<Vec<String>>> = BTreeMap::new();
    let mut after: BTreeMap<Anchor, Vec<Vec<String>>> = BTreeMap::new();
    let mut bof: Vec<Vec<String>> = Vec::new();
    let mut eof: Vec<Vec<String>> = Vec::new();
    let mut first_changed: Option<Anchor> = None;
    // 已占用 start：同 start 后到 Replace/Delete 丢弃（与既有区间表去重语义一致）。
    let mut occupied: HashSet<Anchor> = HashSet::new();

    let note = |fc: &mut Option<Anchor>, v: Anchor| {
        if fc.is_none() || Some(v) < *fc {
            *fc = Some(v);
        }
    };

    for hunk in &section.hunks {
        match hunk {
            Hunk::Replace { start, end, body } => {
                validate_bounds(*start, *end, n, &mut warnings);
                if !occupied.insert(*start) {
                    warnings.push(format!("行 {start} 已被前一个区间覆盖，后到 Replace 被丢弃"));
                    continue;
                }
                note(&mut first_changed, *start);
                groups.push(ReplaceGroup {
                    start: *start,
                    end: *end,
                    payload: body.clone(),
                });
            }
            Hunk::Delete { start, end } => {
                validate_bounds(*start, *end, n, &mut warnings);
                if !occupied.insert(*start) {
                    warnings.push(format!("行 {start} 已被前一个区间覆盖，后到 Delete 被丢弃"));
                    continue;
                }
                note(&mut first_changed, *start);
                deletes.push((*start, *end));
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

    // 边界修复（分隔符平衡 + 缺闭合符保留）：返回与 groups 同序的修复结果。
    let repaired = repair_replacement_boundaries(&groups, &deletes, &before, &after, &lines);
    for r in &repaired {
        if let Some(w) = &r.warning {
            warnings.push(w.clone());
        }
    }

    // 转区间表：修复后的替换（含保留段后额外删除）+ 纯删除。保留段含区间头时
    //（`end < start`）区间替换部分为空，body 退化为 `before[start]` 纯插入，
    // 避免空区间在单遍扫描里死循环。
    let mut ranges: BTreeMap<Anchor, (Anchor, Option<Vec<String>>)> = BTreeMap::new();
    for r in &repaired {
        if r.end >= r.start {
            ranges.insert(r.start, (r.end, Some(r.body.clone())));
        } else {
            before.entry(r.start).or_default().push(r.body.clone());
        }
        if let Some((ds, de)) = r.trailing_delete {
            ranges.insert(ds, (de, None));
        }
    }
    for (s, e) in &deletes {
        ranges.insert(*s, (*e, None));
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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_hashline;

    fn apply(patch: &str, original: &str) -> ApplyResult {
        let sections = parse_hashline(patch).unwrap();
        apply_section(original, &sections[0])
    }

    /// 多行正文拼接（无尾换行，对齐上游 `join("\n")`）。
    fn j(parts: &[&str]) -> String {
        parts.join("\n")
    }
    /// patch 正文：自动加 `[t]` 段头 + 每行换行。
    fn p(body: &[&str]) -> String {
        let mut s = String::from("[t]\n");
        for l in body {
            s.push_str(l);
            s.push('\n');
        }
        s
    }
    fn warn_any(r: &ApplyResult, needle: &str) -> bool {
        r.warnings.iter().any(|w| w.contains(needle))
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
    fn single_line_content_echo_not_repaired() {
        // 单行区间重述下方普通内容：上游保守语义不修（避免误删故意内容；仅当重复边
        // 为结构闭合符时单行才修）。c 保留 → 出现两次，且无告警。
        let r = apply("[a]\nSWAP 2.=2:\n+B\n+c\n", "a\nb\nc\nd\n");
        assert_eq!(r.text.as_deref(), Some("a\nB\nc\nc\nd\n"));
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn back_echo_drops_single_duplicated_closer() {
        // 经典「单闭合符重复」：SWAP 替换两行体，但 body 末尾重述了区间下方仍存的 `});`
        let file = "it('a', () => {\n\tsetup();\n\trun();\n});\nafter();\n";
        let patch = "[t.ts]\nSWAP 2.=3:\n+\tsetup2();\n+\trun2();\n+});\n";
        let out = apply(patch, file).text.unwrap();
        assert_eq!(
            out,
            "it('a', () => {\n\tsetup2();\n\trun2();\n});\nafter();\n"
        );
        assert_eq!(out.matches("});").count(), 1);
    }

    #[test]
    fn back_echo_drops_multi_line_duplicated_block() {
        // 多行闭合块重复：body 后缀 [D, E] 与区间下方幸存行逐字一致，应整体去除
        let file = "A\nB\nC\nD\nE\n";
        let patch = "[t]\nSWAP 2.=3:\n+X\n+D\n+E\n";
        let out = apply(patch, file).text.unwrap();
        assert_eq!(out, "A\nX\nD\nE\n");
        assert_eq!(out.matches('D').count(), 1, "不应出现重复的 D");
        assert_eq!(out.matches('E').count(), 1, "不应出现重复的 E");
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

    // ── 上游 boundary-repair.test.ts 移植：验证分隔符平衡 + 缺闭合符保留语义对齐 ──

    #[test]
    fn drops_duplicated_multiline_closing_block() {
        // Root.tsx 事件：区间替换重述了下方幸存的 </> + );，应去重
        let file = j(&[
            "import type React from \"react\";",
            "import { Composition } from \"remotion\";",
            "import { Sizzle, type SizzleProps } from \"./compositions/Sizzle\";",
            "import { FPS, totalDurationInFrames } from \"./lib/scenes\";",
            "",
            "export const RemotionRoot: React.FC = () => {",
            "\tconst durationInFrames = totalDurationInFrames();",
            "\treturn (",
            "\t\t<>",
            "\t\t\t<Composition",
            "\t\t\t\tid=\"Sizzle\"",
            "\t\t\t\tcomponent={Sizzle}",
            "\t\t\t\tdurationInFrames={durationInFrames}",
            "\t\t\t\twidth={1920}",
            "\t\t\t\tdefaultProps={{ layout: \"landscape\" }}",
            "\t\t\t/>",
            "\t\t</>",
            "\t);",
            "};",
        ]);
        let patch = p(&[
            "SWAP 7.=16:",
            "+\treturn (",
            "+\t\t<>",
            "+\t\t\t<Composition",
            "+\t\t\t\tid=\"Sizzle\"",
            "+\t\t\t\tcomponent={Sizzle}",
            "+\t\t\t\tdurationInFrames={durationInFrames}",
            "+\t\t\t\twidth={1920}",
            "+\t\t\t\tdefaultProps={{ layout: \"landscape\" } satisfies SizzleProps}",
            "+\t\t\t/>",
            "+\t\t</>",
            "+\t);",
        ]);
        let out = apply(&patch, &file).text.unwrap();
        assert_eq!(out.lines().filter(|l| l.trim() == "</>").count(), 1);
        assert_eq!(out.lines().filter(|l| l.trim() == ");").count(), 1);
        assert!(out.ends_with("\t\t</>\n\t);\n};"));
        assert!(warn_any(&apply(&patch, &file), "delimiter-balance"));
    }

    #[test]
    fn drops_single_duplicated_closer() {
        let file = j(&["it('a', () => {", "\tsetup();", "\trun();", "});", "after();"]);
        let patch = p(&["SWAP 2.=3:", "+\tsetup2();", "+\trun2();", "+});"]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(j(&["it('a', () => {", "\tsetup2();", "\trun2();", "});", "after();"]).as_str())
        );
        assert!(warn_any(&r, "delimiter-balance"));
    }

    #[test]
    fn drops_single_duplicated_opener() {
        let file = j(&[
            "class Foo {",
            "\t/** doc */",
            "\tplanRender(",
            "\t\ta: string[],",
            "\t\tb: boolean,",
            "\t): Intent {",
            "\t\treturn x;",
            "\t}",
            "}",
        ]);
        let patch = p(&[
            "SWAP 4.=6:",
            "+\tplanRender(",
            "+\t\ta: string[],",
            "+\t\tb: boolean,",
            "+\t\tc: number,",
            "+\t): Intent {",
        ]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(j(&[
                "class Foo {",
                "\t/** doc */",
                "\tplanRender(",
                "\t\ta: string[],",
                "\t\tb: boolean,",
                "\t\tc: number,",
                "\t): Intent {",
                "\t\treturn x;",
                "\t}",
                "}",
            ]).as_str())
        );
        assert_eq!(r.text.as_ref().unwrap().matches("\tplanRender(").count(), 1);
        assert!(warn_any(&r, "delimiter-balance"));
    }

    #[test]
    fn preserves_opener_when_doesnt_account() {
        let file = j(&["if (a) {", "\tfoo();", "}", "bar();"]);
        let patch = p(&["SWAP 2.=2:", "+if (a) {", "+\tif (b) {", "+\t\tfoo();"]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(
                j(&[
                    "if (a) {",
                    "if (a) {",
                    "\tif (b) {",
                    "\t\tfoo();",
                    "}",
                    "bar();",
                ])
                .as_str()
            )
        );
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn spares_omitted_closer() {
        let file = j(&["const handlers = {", "\ta() {", "\t\treturn 1;", "\t},", "};"]);
        let patch = p(&["SWAP 5.=5:", "+\tb() {", "+\t\treturn 2;", "+\t},"]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(j(&[
                "const handlers = {",
                "\ta() {",
                "\t\treturn 1;",
                "\t},",
                "\tb() {",
                "\t\treturn 2;",
                "\t},",
                "};",
            ]).as_str())
        );
        assert!(warn_any(&r, "delimiter-balance"));
    }

    #[test]
    fn does_not_spare_restated_closer() {
        let file = j(&["class Foo {", "\tok();", "\t}", "}"]);
        let patch = p(&["SWAP 1.=4:", "+class Foo {", "+\tok();", "+}"]);
        let r = apply(&patch, &file);
        assert_eq!(r.text.as_deref(), Some(j(&["class Foo {", "\tok();", "}"]).as_str()));
        assert_eq!(r.text.as_ref().unwrap().matches('}').count(), 1);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn drops_leading_trailing_boundary_echo() {
        let file = j(&[
            "func _cmd_travel_homeworld():",
            "\tvar destination = get_homeworld()",
            "\ttravel_to(destination)",
            "\tprint_status()",
        ]);
        let patch = p(&[
            "SWAP 2.=3:",
            "+func _cmd_travel_homeworld():",
            "+\tvar destination = find_homeworld()",
            "+\ttravel_to(destination)",
            "+\tprint_status()",
        ]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(j(&[
                "func _cmd_travel_homeworld():",
                "\tvar destination = find_homeworld()",
                "\ttravel_to(destination)",
                "\tprint_status()",
            ]).as_str())
        );
        assert_eq!(r.text.as_ref().unwrap().matches("func _cmd_travel_homeworld():").count(), 1);
        assert!(warn_any(&r, "boundary echo"));
    }

    #[test]
    fn preserves_payload_all_echoes() {
        let file = j(&["A", "B", "old", "C", "D"]);
        let patch = p(&["SWAP 3.=3:", "+A", "+B", "+C", "+D"]);
        let r = apply(&patch, &file);
        assert_eq!(r.text.as_deref(), Some(j(&["A", "B", "A", "B", "C", "D", "C", "D"]).as_str()));
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn preserves_payload_both_neighbors() {
        let file = j(&["a", "old", "c"]);
        let patch = p(&["SWAP 2.=2:", "+a", "+c"]);
        let r = apply(&patch, &file);
        assert_eq!(r.text.as_deref(), Some(j(&["a", "a", "c", "c"]).as_str()));
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn preserves_balance_shifting_echo() {
        let file = j(&["}", "old();", "}"]);
        let patch = p(&["SWAP 2.=2:", "+}", "+if (a) {", "+if (b) {", "+x();", "+}"]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(j(&["}", "}", "if (a) {", "if (b) {", "x();", "}", "}"]).as_str())
        );
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn still_drops_balance_neutral_wrapper_echo() {
        let file = j(&["function f() {", "old();", "}"]);
        let patch = p(&["SWAP 2.=2:", "+function f() {", "+fresh();", "+}"]);
        let r = apply(&patch, &file);
        assert_eq!(r.text.as_deref(), Some(j(&["function f() {", "fresh();", "}"]).as_str()));
        assert!(warn_any(&r, "boundary echo"));
    }

    #[test]
    fn leaves_balance_preserving_alone() {
        let file = j(&["foo();", "bar();", "bar();", "baz();"]);
        let patch = p(&["SWAP 2.=2:", "+qux();", "+bar();"]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(j(&["foo();", "qux();", "bar();", "bar();", "baz();"]).as_str())
        );
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn does_not_drop_balance_neutral_dup_statement() {
        let file = j(&["a = 1;", "b = 2;", "c = 3;"]);
        let patch = p(&["SWAP 1.=1:", "+a = 1;", "+b = 2;"]);
        let r = apply(&patch, &file);
        assert_eq!(r.text.as_deref(), Some(j(&["a = 1;", "b = 2;", "b = 2;", "c = 3;"]).as_str()));
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn ignores_brackets_in_strings() {
        let file = j(&["const a = \"}\";", "const b = \"x\";", "const c = \"y\";"]);
        let patch = p(&["SWAP 2.=2:", "+const b = \"}}}\";"]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(j(&["const a = \"}\";", "const b = \"}}}\";", "const c = \"y\";"]).as_str())
        );
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn drops_one_sided_trailing_keeper_echo() {
        let file = j(&[
            "function f() {",
            "  a();",
            "  b();",
            "  const out = [];",
            "  return out;",
            "}",
        ]);
        let patch = p(&["SWAP 2.=3:", "+  a2();", "+  b2();", "+  const out = [];"]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(j(&[
                "function f() {",
                "  a2();",
                "  b2();",
                "  const out = [];",
                "  return out;",
                "}",
            ]).as_str())
        );
        assert!(warn_any(&r, "boundary echo"));
    }

    #[test]
    fn drops_one_sided_jsx_closer_echo() {
        let file = j(&["const view = (", "  <section>", "    <Old />", "  </section>", ");"]);
        let patch = p(&["SWAP 3.=3:", "+    <New />", "+  </section>"]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(
                j(&["const view = (", "  <section>", "    <New />", "  </section>", ");"]).as_str()
            )
        );
        assert_eq!(r.text.as_ref().unwrap().matches("  </section>").count(), 1);
        assert!(warn_any(&r, "boundary echo"));
    }

    #[test]
    fn preserves_nested_jsx_closer() {
        let file = j(&[
            "const view = (",
            "<section className=\"outer\">",
            "old text",
            "</section>",
            ");",
        ]);
        let patch = p(&["SWAP 3.=3:", "+<section>", "+new text", "+</section>"]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(j(&[
                "const view = (",
                "<section className=\"outer\">",
                "<section>",
                "new text",
                "</section>",
                "</section>",
                ");",
            ]).as_str())
        );
        assert_eq!(
            r.text.as_ref().unwrap().lines().filter(|l| l.trim() == "</section>").count(),
            2
        );
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn drops_one_sided_leading_keeper_echo() {
        let file = j(&["setup();", "a();", "b();", "c();"]);
        let patch = p(&["SWAP 3.=4:", "+a();", "+B();", "+C();"]);
        let r = apply(&patch, &file);
        assert_eq!(r.text.as_deref(), Some(j(&["setup();", "a();", "B();", "C();"]).as_str()));
        assert!(warn_any(&r, "boundary echo"));
    }

    #[test]
    fn does_not_keep_closer_when_opener_removed_elsewhere() {
        // #3142：另一 hunk 删了匹配的开符 → 整 patch 平衡 → 闭合符保持删除
        let file = j(&["if enabled {", "\tText(\"Old\")", "}", "\tText(\"Tail\")"]);
        let patch = p(&["DEL 1", "SWAP 2.=3:", "+Text(\"New\")"]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(j(&["Text(\"New\")", "\tText(\"Tail\")"]).as_str())
        );
        assert!(!warn_any(&r, "structural closing line"));
    }

    #[test]
    fn keeps_closer_when_opener_replaced() {
        let file = j(&["if (a) {", "\told();", "}"]);
        let patch = p(&["SWAP 1.=1:", "+if (b) {", "SWAP 2.=3:", "+\tnew();"]);
        let r = apply(&patch, &file);
        assert_eq!(r.text.as_deref(), Some(j(&["if (b) {", "\tnew();", "}"]).as_str()));
        assert_eq!(
            r.warnings.iter().filter(|w| w.contains("structural closing line")).count(),
            1
        );
    }

    #[test]
    fn keeps_only_non_restated_outer_closer() {
        let file = j(&["class C {", "\told();", "\t}", "}"]);
        let patch = p(&["SWAP 2.=4:", "+\tnewMethod() {", "+\t\treturn 1;", "+\t}"]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(j(&["class C {", "\tnewMethod() {", "\t\treturn 1;", "\t}", "}"]).as_str())
        );
        assert_eq!(
            r.warnings.iter().filter(|w| w.contains("structural closing line")).count(),
            1
        );
    }

    #[test]
    fn still_keeps_missing_closer_when_dup_suffix_masks_delta() {
        let file = j(&[
            "addEventListener(\"click\", () => {",
            "\tfoo();",
            "\tbar();",
            "});",
            "",
            "const config = {",
            "\ta: 1,",
            "};",
        ]);
        let patch = p(&[
            "SWAP 2.=3:",
            "+\tsetup();",
            "+\tfoo();",
            "+\tbar();",
            "+});",
            "SWAP 8.=8:",
            "+\tb: 2,",
        ]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(j(&[
                "addEventListener(\"click\", () => {",
                "\tsetup();",
                "\tfoo();",
                "\tbar();",
                "});",
                "",
                "const config = {",
                "\ta: 1,",
                "\tb: 2,",
                "};",
            ]).as_str())
        );
        assert!(warn_any(&r, "trailing payload line"));
        assert!(warn_any(&r, "structural closing line"));
    }

    #[test]
    fn does_not_let_unterminated_template_mask() {
        let file = j(&["const log = makeLog(`", "prefix", "`);", "const obj = {", "\ta: 1", "};"]);
        let patch = p(&["SWAP 1.=1:", "+const log = createLog(`", "SWAP 5.=6:", "+\ta: 2"]);
        let r = apply(&patch, &file);
        assert_eq!(
            r.text.as_deref(),
            Some(
                j(&[
                    "const log = createLog(`",
                    "prefix",
                    "`);",
                    "const obj = {",
                    "\ta: 2",
                    "};",
                ])
                .as_str()
            )
        );
        assert_eq!(
            r.warnings.iter().filter(|w| w.contains("structural closing line")).count(),
            1
        );
    }
}
