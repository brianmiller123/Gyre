//! 文本形状规范化的最小集：行尾检测/往返 + UTF-8 BOM 剥离。
//!
//! 移植自 [`oh-my-pi hashline/normalize.ts`](../../../third/oh-my-pi/packages/hashline/src/normalize.ts)。
//!
//! 应用器在落盘前先把正文规范到 LF、应用编辑、再按原始行尾回写，
//! 以保证 CRLF 文件编辑后行尾不漂移（BOM 同理还原）。

/// UTF-8 BOM 字符（U+FEFF）。
pub const BOM: &str = "\u{FEFF}";

/// 行尾风格。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    /// Unix `\n`（默认）。
    #[default]
    Lf,
    /// Windows `\r\n`。
    Crlf,
}

impl LineEnding {
    /// 作为字符串。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

/// 检测 `content` 中最先出现的行尾风格。
///
/// 两者皆无时回退 [`LineEnding::Lf`]；CRLF 先于 LF 出现时返回 [`LineEnding::Crlf`]，否则 [`LineEnding::Lf`]。
#[must_use]
pub fn detect_line_ending(content: &str) -> LineEnding {
    let Some(lf) = content.find('\n') else {
        return LineEnding::Lf;
    };
    let Some(crlf) = content.find("\r\n") else {
        return LineEnding::Lf;
    };
    if crlf < lf {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

/// 把所有行尾规范为 LF：`\r\n` 与孤立 `\r`（旧 Mac 风格）统一转为 `\n`。
#[must_use]
pub fn normalize_to_lf(text: &str) -> String {
    // \r 与 \n 在 UTF-8 中均为单字节 ASCII，绝不出现在多字节续接字节位，
    // 故按 char 逐字符扫描并吞掉可选的紧跟 \n 是安全的。
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            out.push('\n');
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 用指定行尾重新编码（输入应为 LF 文本）。
#[must_use]
pub fn restore_line_endings(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::Crlf => text.replace('\n', "\r\n"),
    }
}

/// BOM 剥离结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BomResult {
    /// 被剥离的 BOM（空串或 [`BOM`]）。
    pub bom: String,
    /// 剥离 BOM 后的文本。
    pub text: String,
}

/// 若 `content` 开头为 UTF-8 BOM 则剥离，返回 BOM 与剩余文本。
#[must_use]
pub fn strip_bom(content: &str) -> BomResult {
    if let Some(rest) = content.strip_prefix(BOM) {
        BomResult {
            bom: BOM.to_string(),
            text: rest.to_string(),
        }
    } else {
        BomResult {
            bom: String::new(),
            text: content.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_line_endings() {
        assert_eq!(detect_line_ending("a\nb"), LineEnding::Lf);
        assert_eq!(detect_line_ending("a\r\nb"), LineEnding::Crlf);
        // CRLF 先出现
        assert_eq!(detect_line_ending("a\r\nb\nc"), LineEnding::Crlf);
        // LF 先出现
        assert_eq!(detect_line_ending("a\nb\r\nc"), LineEnding::Lf);
        // 无换行
        assert_eq!(detect_line_ending("no newlines"), LineEnding::Lf);
    }

    #[test]
    fn crlf_round_trip_preserved() {
        let original = "line1\r\nline2\r\nline3";
        let lf = normalize_to_lf(original);
        assert_eq!(lf, "line1\nline2\nline3");
        let restored = restore_line_endings(&lf, LineEnding::Crlf);
        assert_eq!(restored, original);
    }

    #[test]
    fn lone_cr_becomes_lf() {
        assert_eq!(normalize_to_lf("a\rb"), "a\nb");
        assert_eq!(normalize_to_lf("a\r\nb\rc"), "a\nb\nc");
    }

    #[test]
    fn lf_passthrough_unchanged() {
        assert_eq!(normalize_to_lf("a\nb\n"), "a\nb\n");
        assert_eq!(restore_line_endings("a\nb\n", LineEnding::Lf), "a\nb\n");
    }

    #[test]
    fn strips_utf8_bom() {
        let r = strip_bom("\u{FEFF}hello");
        assert_eq!(r.bom, "\u{FEFF}");
        assert_eq!(r.text, "hello");
        let r2 = strip_bom("hello");
        assert!(r2.bom.is_empty());
        assert_eq!(r2.text, "hello");
    }

    #[test]
    fn bom_plus_crlf_round_trip() {
        // 模拟 Windows + BOM 文件：编辑后 BOM 与 CRLF 均应还原
        let raw = "\u{FEFF}fn main() {\r\n    todo!()\r\n}\r\n";
        let bom = strip_bom(raw);
        let ending = detect_line_ending(&bom.text);
        assert_eq!(ending, LineEnding::Crlf);
        let lf = normalize_to_lf(&bom.text);
        assert_eq!(lf, "fn main() {\n    todo!()\n}\n");
        // 模拟一次原地无改动回写
        let mut out = restore_line_endings(&lf, ending);
        if !bom.bom.is_empty() {
            out.insert_str(0, &bom.bom);
        }
        assert_eq!(out, raw);
    }
}
