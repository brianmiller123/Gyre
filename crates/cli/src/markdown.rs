//! 终端 Markdown 流式渲染器。
//!
//! 把 LLM 增量文本美化（标题加粗着色 / 行内代码 / 围栏代码块语法高亮 /
//! 列表 / 引用 / 分隔线）后输出，提升 CLI 可读性。
//!
//! 流式友好：增量喂入文本，内部按行边界缓冲处理；围栏代码块跨行累积，
//! 闭合时用 [`agent_search::highlight_to_ansi`]（syntect）一次性高亮输出。
//! 非 TTY（管道 / 重定向）自动关闭着色与高亮，透传原文，避免 ANSI 转义码污染。

use std::fmt::Write as _;

/// 终端 Markdown 流式渲染器。
pub struct MarkdownRenderer {
    /// 行缓冲（尚未遇到换行的增量尾部）。
    line_buf: String,
    /// 是否处于围栏代码块内。
    in_fenced: bool,
    /// 围栏字符（`` ` `` 或 `~`），用于闭合匹配。
    fence_char: char,
    /// 围栏语言标记（如 `rust`）。
    lang: String,
    /// 代码块累积原文。
    code_buf: String,
    /// 是否启用 ANSI 着色与语法高亮（仅 TTY 启用）。
    color: bool,
}

impl MarkdownRenderer {
    /// 构造；`color` 控制是否输出 ANSI 颜色与语法高亮。
    #[must_use]
    pub fn new(color: bool) -> Self {
        Self {
            line_buf: String::new(),
            in_fenced: false,
            fence_char: '`',
            lang: String::new(),
            code_buf: String::new(),
            color,
        }
    }

    /// 喂入增量文本，返回应立即打印的美化输出（可能为空——如代码块正在累积）。
    pub fn push(&mut self, delta: &str) -> String {
        self.line_buf.push_str(delta);
        let mut out = String::new();
        while let Some(idx) = self.line_buf.find('\n') {
            let line = self.line_buf[..idx].to_string();
            self.line_buf = self.line_buf[idx + 1..].to_string();
            self.render_line(&line, &mut out);
        }
        out
    }

    /// 流结束时 flush 残留：末尾未换行行，以及未闭合代码块（原样透传，不高亮）。
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if self.in_fenced {
            let mut combined = std::mem::take(&mut self.code_buf);
            combined.push_str(&self.line_buf);
            self.line_buf.clear();
            self.in_fenced = false;
            let lang = std::mem::take(&mut self.lang);
            out.push_str(&fenced_plain(&combined, &lang));
            out.push('\n');
        } else if !self.line_buf.is_empty() {
            let last = std::mem::take(&mut self.line_buf);
            out.push_str(&render_inline(&last, self.color));
            out.push('\n');
        }
        out
    }

    /// 渲染一行（输出含末尾换行）。
    fn render_line(&mut self, line: &str, out: &mut String) {
        if self.in_fenced {
            if is_closing_fence(line, self.fence_char) {
                let code = std::mem::take(&mut self.code_buf);
                let lang = std::mem::take(&mut self.lang);
                self.in_fenced = false;
                out.push_str(&self.render_code_block(&code, &lang));
                out.push('\n');
            } else {
                self.code_buf.push_str(line);
                self.code_buf.push('\n');
            }
            return;
        }
        // 开围栏：进入代码块，围栏行本身不输出（闭合时整体高亮输出）。
        if let Some((ch, lang)) = open_fence(line) {
            self.in_fenced = true;
            self.fence_char = ch;
            self.lang = lang;
            self.code_buf.clear();
            return;
        }
        out.push_str(&render_inline(line, self.color));
        out.push('\n');
    }

    /// 渲染闭合的代码块：着色时用 syntect 高亮 + 边框；否则纯围栏透传。
    fn render_code_block(&self, code: &str, lang: &str) -> String {
        if !self.color {
            return fenced_plain(code, lang);
        }
        let ext = lang_to_ext(lang);
        match agent_search::highlight_to_ansi(code.trim_end_matches('\n'), ext) {
            Ok(highlit) => {
                let mut s = String::new();
                let label = if lang.is_empty() { "code" } else { lang };
                let _ = writeln!(s, "\x1b[2m┌─ {label} ─────────────────\x1b[0m");
                for l in highlit.split('\n') {
                    let _ = writeln!(s, "\x1b[2m│\x1b[0m {l}");
                }
                s.push_str("\x1b[2m└──────────────────────────\x1b[0m");
                s
            }
            Err(_) => fenced_plain(code, lang),
        }
    }
}

/// 渲染普通行（块级标题 / 引用 / 列表 / 分隔线 + 行内 `code`/`**bold**`）。
fn render_inline(line: &str, color: bool) -> String {
    if !color {
        return line.to_string();
    }
    let trimmed = line.trim();
    // 分隔线（--- / ___ / ***）
    if (trimmed.starts_with("---") && trimmed.chars().all(|c| c == '-'))
        || (trimmed.starts_with("___") && trimmed.chars().all(|c| c == '_'))
        || (trimmed.starts_with("***") && trimmed.chars().all(|c| c == '*'))
    {
        return "\x1b[2m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m".to_string();
    }
    // 标题 # ~ ######
    if let Some(rest) = strip_heading(line) {
        return format!("\x1b[1;36m{}\x1b[0m", inline_marks(&rest));
    }
    // 引用 >
    if let Some(rest) = line.strip_prefix("> ") {
        return format!("\x1b[2m│ {}\x1b[0m", inline_marks(rest));
    }
    // 无序列表 - * +
    for pfx in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(pfx) {
            return format!("\x1b[1;33m•\x1b[0m {}", inline_marks(rest));
        }
    }
    // 有序列表 1.
    if let Some(rest) = strip_ordered(line) {
        return format!("\x1b[1;33m▶\x1b[0m {}", inline_marks(&rest));
    }
    inline_marks(line)
}

/// 处理行内 `` `code` `` 与 `**bold**`，返回带 ANSI 的字符串。
fn inline_marks(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        // 行内代码 `...`
        if chars[i] == '`' {
            if let Some(j) = chars[i + 1..].iter().position(|&c| c == '`') {
                let code: String = chars[i + 1..=i + j].iter().collect();
                out.push_str(&format!("\x1b[36m{code}\x1b[0m"));
                i += j + 2;
                continue;
            }
        }
        // 粗体 **...**
        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(j) = find_double(&chars[i + 2..], '*') {
                let bold: String = chars[i + 2..i + 2 + j].iter().collect();
                out.push_str(&format!("\x1b[1m{bold}\x1b[0m"));
                i += j + 4;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 在切片中查找连续两个 `c` 的起始下标。
fn find_double(slice: &[char], c: char) -> Option<usize> {
    let mut i = 0;
    while i + 1 < slice.len() {
        if slice[i] == c && slice[i + 1] == c {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `# ` ~ `###### ` 标题前缀剥离；返回标题正文（要求 `#` 后紧跟空格）。
fn strip_heading(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut n = 0;
    while n < bytes.len() && bytes[n] == b'#' {
        n += 1;
    }
    if (1..=6).contains(&n) && bytes.get(n) == Some(&b' ') {
        Some(line[n + 1..].to_string())
    } else {
        None
    }
}

/// `1. ` 有序列表前缀剥离；返回列表项正文。
fn strip_ordered(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut n = 0;
    while n < bytes.len() && bytes[n].is_ascii_digit() {
        n += 1;
    }
    if n > 0 && bytes.get(n) == Some(&b'.') && bytes.get(n + 1) == Some(&b' ') {
        Some(line[n + 2..].to_string())
    } else {
        None
    }
}

/// 开围栏检测：行首 ≥3 个 `` ` `` 或 `~`，返回（围栏字符, 语言标记）。
fn open_fence(line: &str) -> Option<(char, String)> {
    let t = line.trim_start();
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let run: String = t.chars().take_while(|&ch| ch == c).collect();
    if run.len() >= 3 {
        let lang = t[run.len()..].trim().to_string();
        Some((c, lang))
    } else {
        None
    }
}

/// 闭合围栏检测：整行（去空白）为 ≥3 个同字符围栏。
fn is_closing_fence(line: &str, fence_char: char) -> bool {
    let t = line.trim();
    !t.is_empty() && t.len() >= 3 && t.chars().all(|c| c == fence_char)
}

/// 纯围栏代码块透传（无高亮）。
fn fenced_plain(code: &str, lang: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "```{lang}");
    s.push_str(code);
    if !code.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("```");
    s
}

/// Markdown 围栏语言名 → 文件扩展名（syntect 按 extension 查找语法）。
fn lang_to_ext(lang: &str) -> &str {
    match lang {
        "rust" | "rs" => "rs",
        "python" | "py" => "py",
        "javascript" | "js" => "js",
        "typescript" | "ts" => "ts",
        "go" | "golang" => "go",
        "c" => "c",
        "cpp" | "c++" => "cpp",
        "java" => "java",
        "sh" | "bash" | "shell" | "zsh" => "sh",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "html" => "html",
        "css" => "css",
        "sql" => "sql",
        "xml" => "xml",
        "php" => "php",
        "ruby" | "rb" => "rb",
        "swift" => "swift",
        "kotlin" | "kt" => "kt",
        "scala" => "scala",
        _ => lang,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_heading_and_inline_code() {
        let out = render_inline("# Title with `code` here", true);
        assert!(out.contains("\x1b[1;36m"));
        assert!(out.contains("\x1b[36mcode\x1b[0m"));
    }

    #[test]
    fn passthrough_when_color_disabled() {
        assert_eq!(render_inline("# Title `code`", false), "# Title `code`");
    }

    #[test]
    fn fenced_block_accumulates_then_highlights() {
        let mut md = MarkdownRenderer::new(false); // 关闭高亮，断言纯围栏
        let out = md.push("```rust\nfn main() {}\n```\n");
        // 闭合后才输出代码块；push 期间代码块内容不外泄
        assert!(out.contains("```rust"));
        assert!(out.contains("fn main() {}"));
    }

    #[test]
    fn finishes_unclosed_block_as_plain() {
        let mut md = MarkdownRenderer::new(false);
        md.push("```py\nprint(1)\n");
        let tail = md.finish();
        assert!(tail.contains("```py"));
        assert!(tail.contains("print(1)"));
    }
}
