//! 文本归一化与分页：ANSI 剥离、空白折叠、按帧容量分页。
//!
//! 对齐 oh-my-pi snapcompact.ts 的 `normalize` / `paginateCells` 意图，但实现
//! 简化为逐行语义：归一化后按行切分，每页最多 `max_lines` 行，行内超宽截断
//! （v1 不做词级 wrap 与双栏 doc 布局）。

/// 归一化选项。
#[derive(Debug, Clone, Copy)]
pub struct SerializeOptions {
    /// 行内最大字符数（超出截断；对应帧列数）。
    pub max_cols: usize,
    /// 单条工具结果的最大字符数（超长折叠为截断标记）。
    pub tool_result_max_chars: usize,
    /// 单条工具调用参数的最大字符数。
    pub tool_arg_max_chars: usize,
}

impl Default for SerializeOptions {
    fn default() -> Self {
        Self {
            max_cols: 196,
            tool_result_max_chars: 2000,
            tool_arg_max_chars: 500,
        }
    }
}

/// 剥离 ANSI 转义序列（ESC [ ... 字母 与 OSC 等），OMP 在 normalize 前预处理。
/// 覆盖：CSI（ESC [ params 最终字节 0x40-0x7E）、两字符序列（ESC 后单字节）、
/// OSC（ESC ] ... BEL/ST）。
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // ESC 之后的序列：CSI 解析到最终字节；OSC 解析到 BEL/ST；否则跳 1 字节。
        match chars.peek() {
            Some('[') => {
                chars.next();
                for n in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                for n in chars.by_ref() {
                    if n == '\u{07}' || n == '\u{1b}' {
                        if n == '\u{1b}' {
                            let _ = chars.next(); // ST 的 '\'
                        }
                        break;
                    }
                }
            }
            Some(_) => {
                let _ = chars.next();
            }
            None => {}
        }
    }
    out
}

/// 归一化：ANSI 剥离、`\r\n`→`\n`、tab→4 空格、控制字符剔除、超长行截断。
/// 行结构保留（换行是帧内最有价值的结构信息）。
#[must_use]
pub fn normalize(input: &str, opts: &SerializeOptions) -> String {
    let stripped = strip_ansi(input);
    let mut out = String::with_capacity(stripped.len());
    let mut chars = stripped.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                // \r\n 折叠为单个 \n；孤立 \r 也按换行处理。
                if chars.peek() == Some(&'\n') {
                    let _ = chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    // 行内截断 + 尾部空白去除。
    out.split('\n')
        .map(|line| {
            let line = line.trim_end();
            if line.chars().count() > opts.max_cols {
                let cut: String = line.chars().take(opts.max_cols.saturating_sub(3)).collect();
                format!("{cut}…")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 分页：把归一化文本按行切成 `Vec<String>`（每页 `\n` 连接，最多 `max_lines`
/// 行）。空页不产生。单行超长已在 [`normalize`] 截断。
#[must_use]
pub fn paginate(normalized: &str, max_lines: usize) -> Vec<String> {
    let lines: Vec<&str> = normalized.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let mut pages: Vec<String> = Vec::new();
    for chunk in lines.chunks(max_lines) {
        let page = chunk.join("\n");
        // 纯空行页不产生（避免全空白帧）。
        if !page.trim().is_empty() {
            pages.push(page);
        }
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_and_os_sequences() {
        let s = "\u{1b}[31mred\u{1b}[0m \u{1b}]0;title\u{07}ok";
        assert_eq!(normalize(s, &SerializeOptions::default()), "red ok");
    }

    #[test]
    fn folds_newlines_and_tabs() {
        let s = "a\r\nb\rc\td";
        assert_eq!(normalize(s, &SerializeOptions::default()), "a\nb\nc    d");
    }

    #[test]
    fn drops_other_control_chars() {
        let s = "a\u{07}b\u{00}c";
        assert_eq!(normalize(s, &SerializeOptions::default()), "abc");
    }

    #[test]
    fn truncates_overlong_lines_with_ellipsis() {
        let opts = SerializeOptions {
            max_cols: 10,
            ..SerializeOptions::default()
        };
        let out = normalize("0123456789ABCDEF", &opts);
        assert_eq!(out, "0123456…");
    }

    #[test]
    fn trims_line_trailing_space() {
        assert_eq!(normalize("a  \nb  ", &SerializeOptions::default()), "a\nb");
    }

    #[test]
    fn paginates_by_line_count() {
        let text = "1\n2\n3\n4\n5";
        let pages = paginate(text, 2);
        assert_eq!(pages, vec!["1\n2", "3\n4", "5"]);
    }

    #[test]
    fn empty_input_yields_no_pages() {
        assert!(paginate("", 10).is_empty());
        assert!(paginate("\n\n", 10).is_empty());
    }

    #[test]
    fn single_page_when_fits() {
        let pages = paginate("only", 98);
        assert_eq!(pages, vec!["only"]);
    }
}
