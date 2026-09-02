//! Magic keywords（ultrathink / orchestrate / workflowz）：用户 prompt 中的独立小写词
//! 触发单轮行为契约（移植 oh-my-pi `modes/magic-keyword-boundary.ts` + `markdown-prose.ts`）。
//!
//! 匹配语义：
//! - 仅限**散文**（代码围栏 / 内联代码 / HTML 注释 / XML 标签内不触发）——长度保持掩码
//!   把非散文区域替换为空格后再匹配；
//! - 词边界：前后不得为字母/数字/下划线/`./`/`-`/`\`，后不得紧跟 `.` 或 `(`，
//!   前不得是 `::`（标识符与路径不触发）；
//! - 仅小写全词命中（`Ultrathink` / `ultrathink!` 不触发）。
//!
//! Rust `regex` crate 不支持 look-around，词边界经 `find_iter` + 前后字符检查实现。

/// 检测结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeywordDetect {
    /// 是否命中 `ultrathink`（思考预算拉满）。
    pub ultrathink: bool,
    /// 命中关键词对应的隐藏通知文本（按检测顺序，先于用户消息注入）。
    pub notices: Vec<String>,
}

const ULTRATHINK: &str = "ultrathink";
const ORCHESTRATE: &str = "orchestrate";
const WORKFLOWZ: &str = "workflowz";

/// ultrathink 通知（隐藏注入；思考预算在循环侧拉满）。
pub const ULTRATHINK_NOTICE: &str = "\
[magic-keyword:ultrathink]
<system-notice>
用户请求深度推理（ultrathink）：请进行多步、仔细的推理，使用当前模型支持的最高思考预算；不要急于给出结论。
</system-notice>";

/// orchestrate 通知（移植 omp `orchestrate-notice.md` 的 10 条编排契约）。
pub const ORCHESTRATE_NOTICE: &str = "\
[magic-keyword:orchestrate]
<system-notice>
用户请求多代理编排（orchestrate）：
1. 把任务拆成相互独立的子任务，优先并行委派给子代理（task 工具）；
2. 每个子代理收到完整、自包含的指令（目标/约束/验收），不依赖会话上下文；
3. 并行扇出宽度与任务真实分解一致，不虚增子代理；
4. 子代理间有共享契约时，在批量 context 中显式声明，不让子代理自行协商；
5. 每个阶段完成后先验证再进入下一阶段（验证门禁）；
6. 子代理失败时定向重试或重新委派，不降级为串行重做；
7. 不把「计划/设计」外包给空白的通用子代理——顶层拆分由你自己完成；
8. 汇总时引用各子代理的产出，标注来源；
9. 未完成全部子任务前不结束；
10. 任何子代理产出与预期不符时，如实报告差异而非粉饰。
</system-notice>";

/// 检测 prompt 中的 magic keywords。
///
/// `task_available`：task 工具是否在场（`workflowz` 需要；Gyre 无 eval 工具，
/// v1 不注入 workflowz 通知，避免承诺无法兑现的能力）。
#[must_use]
pub fn detect(text: &str, task_available: bool) -> KeywordDetect {
    let mut out = KeywordDetect::default();
    let masked = mask_non_prose(text);
    if word_boundary_contains(&masked, ULTRATHINK) {
        out.ultrathink = true;
        out.notices.push(ULTRATHINK_NOTICE.to_string());
    }
    if word_boundary_contains(&masked, ORCHESTRATE) {
        out.notices.push(ORCHESTRATE_NOTICE.to_string());
    }
    if task_available && word_boundary_contains(&masked, WORKFLOWZ) {
        out.notices.push(
            "[magic-keyword:workflowz]\n<system-notice>\n用户请求确定性多子代理工作流（workflowz）：\
             使用 task 工具构建多子代理流程，波间设验证门禁，按依赖顺序执行。\n</system-notice>"
                .to_string(),
        );
    }
    out
}

/// 词边界匹配（Rust regex 无 look-around：find_iter + 前后字符检查）。
///
/// 边界条件（移植 omp `magicKeywordRegex`）：
/// - 前一个字符不得为字母/数字/下划线/`.`/`/`/`-`/`\`；
/// - 前两个字符不得为 `::`；
/// - 后一个字符不得为字母/数字/下划线/`/`/`-`/`\`/`.`/`(`；
/// - 大小写敏感（仅小写全词）。
#[must_use]
pub fn word_boundary_contains(text: &str, word: &str) -> bool {
    if !text.contains(word) {
        return false;
    }
    let bytes = text.as_bytes();
    let w = word.as_bytes();
    let mut start = 0usize;
    while let Some(rel) = find_subslice(&bytes[start..], w) {
        let i = start + rel;
        let end = i + w.len();
        let prev_ok = i == 0
            || !matches!(
                bytes[i - 1],
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.' | b'/' | b'-' | b'\\'
            );
        let double_colon_ok = i < 2 || bytes[i - 2..i] != *b"::";
        let next_ok = end >= bytes.len()
            || !matches!(
                bytes[end],
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'/' | b'-' | b'\\' | b'.' | b'('
            );
        if prev_ok && double_colon_ok && next_ok {
            return true;
        }
        start = i + 1;
        if start >= bytes.len() {
            break;
        }
    }
    false
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 长度保持地掩码非散文区域：围栏代码块（≥3 反引号/波浪号）、内联代码、HTML 注释、
/// XML/HTML 标签（含嵌套同名标签内容）。掩码区每个字符替换为空格（单词不被拼接）。
#[must_use]
pub fn mask_non_prose(text: &str) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    while i < n {
        match chars[i] {
            '`' => {
                // 内联代码或围栏块。
                let run = count_run(&chars[i..], '`');
                if run >= 3 {
                    // 围栏代码块：找同长或更长的闭围栏行。
                    if let Some(end) = find_closing_fence(&chars, i + run, run) {
                        blank_range(&mut chars, i, end);
                        i = end;
                    } else {
                        i += run;
                    }
                } else if run == 1 || run == 2 {
                    // 内联代码：找配对反引号（同一行内）。
                    let line_end = chars[i..]
                        .iter()
                        .position(|c| *c == '\n')
                        .map_or(n, |p| i + p);
                    if let Some(pair) = chars[i + run..line_end].iter().position(|c| *c == '`') {
                        let end = i + run + pair + 1;
                        blank_range(&mut chars, i, end);
                        i = end;
                        continue;
                    }
                    i += run;
                } else {
                    i += 1;
                }
            }
            '<' => {
                // HTML 注释 `<!-- ... -->` 或标签（含自闭合与嵌套同名标签）。
                if chars[i..].starts_with(&['<', '!', '-', '-']) {
                    if let Some(end) = chars[i..].windows(3).position(|w| w == ['-', '-', '>']) {
                        let end = i + end + 3;
                        blank_range(&mut chars, i, end);
                        i = end;
                    } else {
                        i += 1;
                    }
                } else if let Some(end) = mask_tag(&chars, i) {
                    blank_range(&mut chars, i, end);
                    i = end;
                } else {
                    i += 1;
                }
            }
            '~' => {
                let run = count_run(&chars[i..], '~');
                if run >= 3 {
                    if let Some(end) = find_closing_fence(&chars, i + run, run) {
                        blank_range(&mut chars, i, end);
                        i = end;
                    } else {
                        i += run;
                    }
                } else {
                    i += run.max(1);
                }
            }
            _ => i += 1,
        }
    }
    chars.into_iter().collect()
}

/// 统计从 `chars[i]` 起连续相同字符数。
fn count_run(chars: &[char], c: char) -> usize {
    chars.iter().take_while(|x| **x == c).count()
}

/// 找围栏闭行：从 `from` 起找「行首 ≥run 个反引号/波浪号」的行。
fn find_closing_fence(chars: &[char], from: usize, run: usize) -> Option<usize> {
    let n = chars.len();
    let mut i = from;
    while i < n {
        // 行首（从 from 起第一个换行后）。
        if i > from {
            let cnt = count_run(&chars[i..], chars[from - 1]);
            if cnt >= run {
                // 闭围栏行：该行剩余部分只能是空白/属性（宽松：直接接受）。
                return Some((i + cnt).min(n));
            }
        }
        match chars[i] {
            '\n' => {
                i += 1;
                // 跳到行首非空白处。
                while i < n && chars[i].is_whitespace() && chars[i] != '\n' {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// 掩码一个 XML/HTML 标签（含内容）：`<name ...>` 到 `</name>`（嵌套同名深度计数），
/// 或自闭合 `/>`。返回闭区间终点（不含）。
fn mask_tag(chars: &[char], start: usize) -> Option<usize> {
    let n = chars.len();
    // 解析标签名。
    let mut j = start + 1;
    let name_start = j;
    while j < n && (chars[j].is_alphanumeric() || chars[j] == '-' || chars[j] == '_') {
        j += 1;
    }
    if j == name_start {
        return None; // 不是标签（如 `<=`）
    }
    let name: String = chars[name_start..j].iter().collect();
    // 找开标签结束 `>`。
    let mut depth = 1usize;
    let mut k = j;
    while k < n {
        if chars[k] == '/' && k + 1 < n && chars[k + 1] == '>' {
            return Some(k + 2); // 自闭合
        }
        if chars[k] == '>' {
            break;
        }
        k += 1;
    }
    if k >= n {
        return None;
    }
    // 内容区：找 `</name>`（嵌套同名标签深度计数）。
    let close_tag = format!("</{name}");
    let mut scan = k + 1;
    while scan < n {
        if chars[scan] == '<' {
            let rest: String = chars[scan..].iter().take(close_tag.len()).collect();
            if rest == close_tag {
                depth -= 1;
                if depth == 0 {
                    // 闭标签结束 `>`。
                    let after = scan + close_tag.len();
                    if after < n && chars[after] == '>' {
                        return Some(after + 1);
                    }
                    return Some(after);
                }
                scan += close_tag.len();
                continue;
            }
            // 嵌套同名开标签。
            let open: String = chars[scan..].iter().take(name.len() + 1).collect();
            if open == format!("<{name}") {
                depth += 1;
                scan += name.len();
                continue;
            }
        }
        scan += 1;
    }
    None
}

/// 把 `[start, end)` 区间替换为等长空格。
fn blank_range(chars: &mut [char], start: usize, end: usize) {
    let end = end.min(chars.len());
    for c in &mut chars[start..end] {
        *c = ' ';
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_prose_hits() {
        assert!(word_boundary_contains("orchestrate this", "orchestrate"));
        assert!(word_boundary_contains("请 ultrathink 分析", "ultrathink"));
        assert!(word_boundary_contains(
            "（orchestrate，继续）",
            "orchestrate"
        ));
    }

    #[test]
    fn boundary_exclusions() {
        assert!(!word_boundary_contains("orchestrates", "orchestrate"));
        assert!(!word_boundary_contains("ultrathink()", "ultrathink"));
        assert!(!word_boundary_contains("my_ultrathink", "ultrathink"));
        assert!(!word_boundary_contains("ultrathink.js", "ultrathink"));
        assert!(!word_boundary_contains("path/ultrathink", "ultrathink"));
        assert!(!word_boundary_contains("Foo::ultrathink", "ultrathink"));
        assert!(!word_boundary_contains("Ultrathink", "ultrathink"));
    }

    #[test]
    fn mask_removes_code_fences() {
        let text = "before\n```rust\nlet x = orchestrates();\n```\nafter";
        let masked = mask_non_prose(text);
        assert!(!word_boundary_contains(&masked, "orchestrates"));
        // 掩码长度保持。
        assert_eq!(masked.len(), text.len());
        assert!(word_boundary_contains(&masked, "before"));
        assert!(word_boundary_contains(&masked, "after"));
    }

    #[test]
    fn mask_removes_inline_code() {
        let text = "run `ultrathink --force` now ultrathink";
        let masked = mask_non_prose(text);
        assert_eq!(masked.len(), text.len());
        assert!(word_boundary_contains(&masked, "ultrathink"));
        // 只应命中末尾的散文词。
        let count = count_word(&masked, "ultrathink");
        assert_eq!(count, 1);
    }

    #[test]
    fn mask_removes_xml_tags_with_nested_content() {
        let text = "<tool_call>ultrathink</tool_call> then ultrathink";
        let masked = mask_non_prose(text);
        assert_eq!(masked.len(), text.len());
        assert!(word_boundary_contains(&masked, "then"));
        let count = count_word(&masked, "ultrathink");
        assert_eq!(count, 1);
    }

    #[test]
    fn mask_removes_html_comments() {
        let text = "a <!-- orchestrates here --> orchestrates";
        let masked = mask_non_prose(text);
        assert!(word_boundary_contains(&masked, "orchestrates"));
        assert_eq!(count_word(&masked, "orchestrates"), 1);
    }

    #[test]
    fn detect_injects_ultrathink_and_orchestrate() {
        let r = detect("请 ultrathink 分析，然后 orchestrates", false);
        assert!(r.ultrathink);
        assert_eq!(r.notices.len(), 1, "orchestrates 复数不命中");
        let r2 = detect("请 orchestrates 编排", true);
        assert!(!r2.ultrathink);
        assert!(r2.notices.is_empty());
    }

    #[test]
    fn detect_workflowz_requires_task() {
        let r = detect("workflowz 跑批量", true);
        assert!(r.notices.iter().any(|n| n.contains("workflowz")));
        let r2 = detect("workflowz 跑批量", false);
        assert!(r2.notices.is_empty());
    }

    #[test]
    fn detect_ignores_keywords_inside_code() {
        let text = "```\nultrathink\n```\norchestrate";
        let r = detect(text, false);
        assert!(!r.ultrathink);
        assert_eq!(r.notices.len(), 1);
        assert!(r.notices[0].contains("orchestrate"));
    }

    fn count_word(text: &str, word: &str) -> usize {
        let bytes = text.as_bytes();
        let w = word.as_bytes();
        let mut count = 0usize;
        let mut start = 0usize;
        while let Some(rel) = find_subslice(&bytes[start..], w) {
            let i = start + rel;
            count += 1;
            start = i + 1;
        }
        count
    }
}
