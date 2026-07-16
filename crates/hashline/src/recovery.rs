//! Hash 失配恢复：在先前快照版本上重放区段编辑，套用到当前正文。
//!
//! 移植自 [`oh-my-pi hashline/recovery.ts`](../../../third/oh-my-pi/packages/hashline/src/recovery.ts)。
//!
//! ## 恢复策略（按序尝试）
//! 1. **3-way 合并**（主路径）：在快照版 `previous` 上重放编辑得 `applied`，再用 `similar` 生成
//!    `previous→applied` 的行变更，按等价上下文在当前正文 `current` 上精确套用（fuzz=0）。
//!    处理行数变化与外部写。**任何对齐歧义即放弃**（保守：宁可不合并也不误改）。
//! 2. **会话链回放**（回退）：当 previous 与 current 行数相等、且每个锚行内容一致时，
//!    把区段直接套用到 current——先前的会话内编辑推进了 hash，但模型锚定的逻辑行未变。

use crate::apply::apply_section;
use crate::mismatch::anchor_lines_of;
use crate::types::FileSection;

/// 会话链回放成功时的告警。
pub const RECOVERY_SESSION_REPLAY_WARNING: &str =
    "Recovered via in-session replay (stale hash from a prior edit this session). \
     Verify the resulting diff — a coincidental insert+delete pair could land the edit on a duplicate row.";

/// 3-way 合并成功时的告警。
pub const RECOVERY_EXTERNAL_WARNING: &str =
    "Recovered via 3-way merge onto the live file (hash drifted from the version the edit was anchored to). \
     Verify the resulting diff.";

/// 恢复结果。
#[derive(Debug, Clone)]
pub struct RecoveryResult {
    /// 恢复后的正文。
    pub text: String,
    /// 相对当前正文的首个变更行（1-indexed），无净变化则 None。
    pub first_changed_line: Option<u32>,
    /// 恢复期收集的告警（含面向用户的恢复横幅）。
    pub warnings: Vec<String>,
}

/// 尝试恢复。
///
/// - `previous`：stale hash 对应的快照版本正文。
/// - `current`：当前落盘正文。
/// - `section`：待应用区段。
///
/// 返回 `None` 表示未找到安全恢复路径——调用方应回退到失配诊断（容忍模式下继续常规应用）。
#[must_use]
pub fn recover(previous: &str, current: &str, section: &FileSection) -> Option<RecoveryResult> {
    // 在先前版本上重放编辑
    let applied = apply_section(previous, section);
    let applied_text = applied.text?;
    if applied_text == previous {
        return None;
    }

    // 主路径：3-way 合并（处理行数变化 / 外部写）
    if let Some(merged) = apply_three_way(previous, &applied_text, current) {
        if merged != current {
            let first_changed =
                find_first_changed_line(current, &merged).or(applied.first_changed_line);
            let mut warnings = Vec::with_capacity(applied.warnings.len() + 1);
            if first_changed.is_some() {
                warnings.push(RECOVERY_EXTERNAL_WARNING.to_string());
            }
            warnings.extend(applied.warnings);
            return Some(RecoveryResult {
                text: merged,
                first_changed_line: first_changed,
                warnings,
            });
        }
    }

    // 回退：会话链回放（行数相等 + 锚行内容一致）
    if previous.split('\n').count() == current.split('\n').count() {
        let anchor_lines = anchor_lines_of(section);
        if anchor_lines.is_empty() || verify_anchor_content(previous, current, &anchor_lines) {
            let replay = apply_section(current, section);
            if let Some(new_text) = replay.text {
                if new_text != current {
                    let first_changed = find_first_changed_line(current, &new_text)
                        .or(replay.first_changed_line);
                    let mut warnings = Vec::with_capacity(replay.warnings.len() + 1);
                    if first_changed.is_some() {
                        warnings.push(RECOVERY_SESSION_REPLAY_WARNING.to_string());
                    }
                    warnings.extend(replay.warnings);
                    return Some(RecoveryResult {
                        text: new_text,
                        first_changed_line: first_changed,
                        warnings,
                    });
                }
            }
        }
    }

    None
}

/// 保守的 fuzz=0 三向合并：把 `previous→applied` 的行变更按等价上下文精确套用到 `current`。
///
/// - 用 [`similar`] 生成 previous→applied 的行变更（Equal/Delete/Insert/Replace）。
/// - Equal 区段在 `current` 中重新对齐（保留其间的外部插入行）；Delete/Replace 区段要求其旧行
///   在 current 对齐位置精确出现——外部若触碰了被编辑的行则放弃。
/// - 任何对齐失败 / 无净变化 → `None`（保守）。
fn apply_three_way(previous: &str, applied: &str, current: &str) -> Option<String> {
    if previous == applied {
        return None;
    }
    let prev: Vec<&str> = previous.split('\n').collect();
    let new: Vec<&str> = applied.split('\n').collect();
    let cur: Vec<&str> = current.split('\n').collect();

    let diff = similar::TextDiff::from_slices(&prev, &new);
    let ops = diff.ops();

    let mut out: Vec<String> = Vec::new();
    let mut pos = 0usize; // current 中的游标

    for op in ops {
        let old_range = op.old_range();
        let new_range = op.new_range();
        let old = &prev[old_range];
        let new_seg = &new[new_range];
        match op.tag() {
            similar::DiffTag::Equal => {
                // 在 current 剩余部分中定位等价行序列，重新对齐
                match find_subseq(&cur[pos..], old) {
                    Some(rel) => {
                        // 保留 current[pos..pos+rel]（外部可能在等价区前插入了行）
                        for l in &cur[pos..pos + rel] {
                            out.push((*l).to_string());
                        }
                        // 输出等价内容（与 current[pos+rel..] 一致）
                        for l in old {
                            out.push((*l).to_string());
                        }
                        pos += rel + old.len();
                    }
                    None => return None,
                }
            }
            similar::DiffTag::Delete | similar::DiffTag::Replace => {
                // 旧行须在 current 当前位置精确出现（外部未触碰被编辑区）
                if pos + old.len() > cur.len() || &cur[pos..pos + old.len()] != old {
                    return None;
                }
                // 输出新行（Delete 时 new_seg 为空）
                for l in new_seg {
                    out.push((*l).to_string());
                }
                pos += old.len();
            }
            similar::DiffTag::Insert => {
                // 无旧行：在当前位置插入，不消费 current
                for l in new_seg {
                    out.push((*l).to_string());
                }
            }
        }
    }
    // 末尾剩余 current 原样保留
    for l in &cur[pos..] {
        out.push((*l).to_string());
    }
    let result = out.join("\n");
    (result != current).then_some(result)
}

/// 在 `haystack` 中查找 `needle` 子序列的首次出现位置（行级精确匹配）。
fn find_subseq(haystack: &[&str], needle: &[&str]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| haystack[i..i + needle.len()] == *needle)
}

/// previous 与 current 在每个锚行（1-indexed）上的内容是否完全一致。
fn verify_anchor_content(previous: &str, current: &str, anchor_lines: &[u32]) -> bool {
    let prev: Vec<&str> = previous.split('\n').collect();
    let curr: Vec<&str> = current.split('\n').collect();
    for &line in anchor_lines {
        let idx = line as usize;
        if idx < 1 || idx > prev.len() || idx > curr.len() {
            return false;
        }
        if prev[idx - 1] != curr[idx - 1] {
            return false;
        }
    }
    true
}

/// `a` 与 `b` 首个分流行（1-indexed）；全等返回 None。
fn find_first_changed_line(a: &str, b: &str) -> Option<u32> {
    if a == b {
        return None;
    }
    let av: Vec<&str> = a.split('\n').collect();
    let bv: Vec<&str> = b.split('\n').collect();
    let max = av.len().max(bv.len());
    for i in 0..max {
        if av.get(i) != bv.get(i) {
            return Some(u32::try_from(i + 1).unwrap_or(u32::MAX));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_hashline;

    fn section_of(patch: &str) -> FileSection {
        parse_hashline(patch).unwrap().into_iter().next().unwrap()
    }

    #[test]
    fn recovers_external_write_via_three_way() {
        // 先前版本（模型读到）：函数体第 2 行
        let previous = "fn a() {\n    let x = 1;\n}\n";
        // 外部在顶部插入了一行注释（行数变化，锚行行号已漂移）
        let current = "// header comment\nfn a() {\n    let x = 1;\n}\n";
        // 模型带 stale hash，把（先前版本中的）第 2 行改为 2
        let section =
            section_of("[src/a.rs#AAAA]\nSWAP 2.=2:\n+    let x = 2;\n");
        let rec = recover(previous, current, &section).expect("应 3-way 合并成功");
        assert_eq!(rec.text, "// header comment\nfn a() {\n    let x = 2;\n}\n");
        // current 第 3 行（let x = 1）→ 结果第 3 行（let x = 2）
        assert_eq!(rec.first_changed_line, Some(3));
        assert!(rec.warnings.iter().any(|w| w.contains("3-way merge")));
    }

    #[test]
    fn three_way_refuses_when_external_touched_edit_region() {
        // 先前版本第 2 行 = b；外部把第 2 行改成了 B（触碰了被编辑行）
        let previous = "a\nb\nc\n";
        let current = "a\nB\nc\n";
        let section = section_of("[x]\nSWAP 2.=2:\n+B2\n");
        assert!(recover(previous, current, &section).is_none());
    }

    #[test]
    fn recovers_session_chain_same_line_count() {
        // 先前版本：第 1 行注释占位、第 2 行待改目标
        let previous = "// header\n    todo!()\n}\n";
        // 会话内编辑改了第 1 行（行数不变），但第 2 行（锚行）内容未漂移
        let current = "// HEADER-CHANGED\n    todo!()\n}\n";
        let section = section_of("[src/a.rs#AAAA]\nSWAP 2.=2:\n+    println!(\"hello\")\n");
        let rec = recover(previous, current, &section).expect("应会话链回放成功");
        assert_eq!(rec.text, "// HEADER-CHANGED\n    println!(\"hello\")\n}\n");
        assert_eq!(rec.first_changed_line, Some(2));
        assert!(rec.warnings.iter().any(|w| w.contains("in-session replay")));
    }

    #[test]
    fn refuses_when_line_count_differs_and_three_way_fails() {
        // 行数变化 + 3-way 对齐失败（被编辑行也被外部改）
        let previous = "a\nb\n";
        let current = "a\n";
        let section = section_of("[x]\nSWAP 1.=1:\n+X\n");
        assert!(recover(previous, current, &section).is_none());
    }

    #[test]
    fn refuses_when_anchor_content_drifted() {
        let previous = "alpha\nb\n";
        let current = "beta\nb\n";
        let section = section_of("[x]\nSWAP 1.=1:\n+ALPHA\n");
        assert!(recover(previous, current, &section).is_none());
    }

    #[test]
    fn none_when_edit_produces_no_change() {
        let previous = "a\nb\n";
        let current = "a\nb\n";
        let section = FileSection {
            path: "x".into(),
            hash: None,
            hunks: Vec::new(),
        };
        assert!(recover(previous, current, &section).is_none());
    }
}
