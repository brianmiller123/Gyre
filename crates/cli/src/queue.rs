//! H4：运行期消息队列（`/queue`）。
//!
//! 移植 oh-my-pi `modes/queue-input.ts` 的核心语义：
//!
//! - **多消息输入**：一行输入可以是**顺序枚举列表**（`1.` / `a.` / `i.` 连续编号，
//!   同级缩进与同一收尾标点）→ 拆成多条排队消息；否则整段算一条。
//! - **简写**：`-> 消息` / `=> 消息` 前缀表示「只入队、不立即执行」。
//! - **队列操作**：入队（尾部）/ 出队（头部 = 下一条执行）/ 取回（尾部 = 撤销最近一次）/
//!   删除指定序号 / 列表 / 清空。
//!
//! 队列由宿主（REPL）持有：运行中键入入队，空闲时 `/queue pop` 或 `Ctrl-Y` 取出执行。

use std::collections::VecDeque;

/// 顺序消息队列（FIFO；`pop_front` 即「下一条执行」）。
#[derive(Debug, Default)]
pub struct MessageQueue {
    items: VecDeque<String>,
}

impl MessageQueue {
    /// 空队列。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 入队一条（空白串忽略）；返回入队后的长度。
    pub fn enqueue(&mut self, text: impl Into<String>) -> usize {
        let text = text.into();
        if !text.trim().is_empty() {
            self.items.push_back(text);
        }
        self.items.len()
    }

    /// 批量入队；返回入队后的长度。
    pub fn enqueue_many<I, S>(&mut self, texts: I) -> usize
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for t in texts {
            self.enqueue(t);
        }
        self.items.len()
    }

    /// 出队头部（= 下一条执行）。
    pub fn pop_front(&mut self) -> Option<String> {
        self.items.pop_front()
    }

    /// 取回尾部（撤销最近一次入队）。
    pub fn pop_back(&mut self) -> Option<String> {
        self.items.pop_back()
    }

    /// 按 1 基序号删除。
    pub fn remove(&mut self, index: usize) -> Option<String> {
        index.checked_sub(1).and_then(|i| self.items.remove(i))
    }

    /// 当前队列（序号从 1 起）。
    #[must_use]
    pub fn list(&self) -> Vec<(usize, &str)> {
        self.items
            .iter()
            .enumerate()
            .map(|(i, s)| (i + 1, s.as_str()))
            .collect()
    }

    /// 清空；返回清掉的条数。
    pub fn clear(&mut self) -> usize {
        let n = self.items.len();
        self.items.clear();
        n
    }

    /// 条数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// `-> 消息` / `=> 消息` 简写 → 消息体（无前缀返回 `None`）。
#[must_use]
pub fn parse_queue_shorthand(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    for prefix in ["->", "=>"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let body = rest.trim();
            return (!body.is_empty()).then(|| body.to_string());
        }
    }
    None
}

/// 一条被识别出的枚举项。
struct EnumeratedItem {
    /// 原始行号。
    line: usize,
    /// 缩进（必须同级）。
    indent: String,
    /// 编号原文。
    marker: String,
    /// 收尾标点（`.` 或 `)`，必须一致）。
    punctuation: char,
    /// 去掉编号后的内容。
    content: String,
}

/// 解析一行枚举项（`<缩进><编号><. | )><空白>内容`）。
fn parse_item(line: &str) -> Option<EnumeratedItem> {
    let indent_len = line.len() - line.trim_start().len();
    let indent = line[..indent_len].to_string();
    let rest = &line[indent_len..];
    let marker_end = rest
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(rest.len());
    if marker_end == 0 {
        return None;
    }
    let marker = &rest[..marker_end];
    let punct = rest[marker_end..].chars().next()?;
    if punct != '.' && punct != ')' {
        return None;
    }
    let after = &rest[marker_end + punct.len_utf8()..];
    // 编号后必须是空白或行尾（`1.5` 这类数字不是列表）。
    if !after.is_empty() && !after.starts_with([' ', '\t']) {
        return None;
    }
    Some(EnumeratedItem {
        line: 0,
        indent,
        marker: marker.to_string(),
        punctuation: punct,
        content: after.trim_start().to_string(),
    })
}

/// 十进制编号。
fn decode_decimal(marker: &str) -> Option<u64> {
    marker.parse::<u64>().ok()
}

/// 字母编号（a=1, b=2, …, z=26, aa=27）。
fn decode_alpha(marker: &str) -> Option<u64> {
    if marker.is_empty() || !marker.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut value: u64 = 0;
    for c in marker.chars() {
        value = value
            .checked_mul(26)?
            .checked_add(u64::from(c.to_ascii_uppercase() as u8 - b'A' + 1))?;
    }
    Some(value)
}

/// 罗马数字编号（仅接受规范写法：解析后回写一致）。
fn decode_roman(marker: &str) -> Option<u64> {
    if marker.is_empty()
        || !marker
            .chars()
            .all(|c| "MDCLXVI".contains(c.to_ascii_uppercase()))
    {
        return None;
    }
    let upper = marker.to_ascii_uppercase();
    let value = |c: char| match c {
        'I' => 1,
        'V' => 5,
        'X' => 10,
        'L' => 50,
        'C' => 100,
        'D' => 500,
        'M' => 1000,
        _ => 0,
    };
    let mut total: u64 = 0;
    let chars: Vec<char> = upper.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let current = value(*c);
        let next = chars.get(i + 1).map_or(0, |n| value(*n));
        if current < next {
            total = total.checked_sub(current)?;
        } else {
            total = total.checked_add(current)?;
        }
    }
    // 规范写法回写（拒绝 `IIII` / `VX` 这类非规范串）。
    (to_roman(total) == upper).then_some(total)
}

/// 数字 → 规范罗马数字（≤ 3999；超出返回空串以拒绝）。
fn to_roman(mut n: u64) -> String {
    if n == 0 || n > 3999 {
        return String::new();
    }
    const TABLE: [(u64, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for (value, sym) in TABLE {
        while n >= value {
            out.push_str(sym);
            n -= value;
        }
    }
    out
}

/// 编号是否严格递增（按给定解码器）。
fn sequential(markers: &[String], decode: fn(&str) -> Option<u64>) -> bool {
    let mut previous = match decode(&markers[0]) {
        Some(v) => v,
        None => return false,
    };
    for m in &markers[1..] {
        match decode(m) {
            Some(v) if v == previous + 1 => previous = v,
            _ => return false,
        }
    }
    true
}

/// 整段文本 → 多条排队消息（非枚举列表时返回单条；空文本返回空 vec）。
#[must_use]
pub fn split_queued_messages(text: &str) -> Vec<String> {
    let source = text.trim();
    if source.is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = source.lines().collect();
    let Some(first) = parse_item(lines[0]) else {
        return vec![source.to_string()];
    };
    let mut items = vec![EnumeratedItem {
        line: 0,
        ..parse_item(lines[0]).expect("首行已解析")
    }];
    for (idx, line) in lines.iter().enumerate().skip(1) {
        if let Some(mut item) = parse_item(line) {
            if item.indent == first.indent {
                item.line = idx;
                items.push(item);
            }
        }
    }
    let markers: Vec<String> = items.iter().map(|i| i.marker.clone()).collect();
    let same_punct = items.iter().all(|i| i.punctuation == first.punctuation);
    let numeric = markers.iter().all(|m| decode_decimal(m).is_some());
    let alpha = markers.iter().all(|m| decode_alpha(m).is_some());
    let roman = markers.iter().all(|m| decode_roman(m).is_some());
    let ordered = (numeric && sequential(&markers, decode_decimal))
        || (alpha && sequential(&markers, decode_alpha))
        || (roman && sequential(&markers, decode_roman));
    if items.len() < 2 || !same_punct || !ordered {
        return vec![source.to_string()];
    }
    // 每条 = 本项内容 + 到下一项之前的续行。
    let mut messages = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let next_line = items.get(i + 1).map_or(lines.len(), |n| n.line);
        let mut body = vec![item.content.clone()];
        body.extend(
            lines[item.line + 1..next_line]
                .iter()
                .map(|l| (*l).to_string()),
        );
        let joined = body.join("\n").trim().to_string();
        if !joined.is_empty() {
            messages.push(joined);
        }
    }
    if messages.is_empty() {
        vec![source.to_string()]
    } else {
        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_is_fifo_with_lifo_undo_and_indexed_remove() {
        let mut q = MessageQueue::new();
        assert!(q.is_empty());
        q.enqueue("a");
        q.enqueue("  "); // 空白忽略
        q.enqueue("b");
        assert_eq!(q.len(), 2);
        assert_eq!(q.list(), vec![(1, "a"), (2, "b")]);
        assert_eq!(q.pop_front().as_deref(), Some("a"), "FIFO：先入先执行");
        assert_eq!(q.pop_back().as_deref(), Some("b"), "撤销最近一次入队");
        q.enqueue_many(["x", "y", "z"]);
        assert_eq!(q.remove(2).as_deref(), Some("y"), "按 1 基序号删除");
        assert_eq!(q.remove(0), None, "序号 0 非法");
        assert_eq!(q.list(), vec![(1, "x"), (2, "z")]);
        assert_eq!(q.clear(), 2);
        assert!(q.is_empty());
    }

    #[test]
    fn shorthand_prefixes_enqueue_body_only() {
        assert_eq!(
            parse_queue_shorthand("-> 先跑测试").as_deref(),
            Some("先跑测试")
        );
        assert_eq!(
            parse_queue_shorthand("=> 再看文档").as_deref(),
            Some("再看文档")
        );
        assert_eq!(parse_queue_shorthand("->"), None, "空体不入队");
        assert_eq!(parse_queue_shorthand("普通输入"), None);
    }

    #[test]
    fn splits_sequential_lists_into_messages() {
        // 十进制 + 同级缩进 + 同标点 → 多条。
        let msgs = split_queued_messages("1. 第一件事\n2. 第二件事\n3. 第三件事");
        assert_eq!(msgs, vec!["第一件事", "第二件事", "第三件事"]);
        // 字母编号。
        let msgs = split_queued_messages("a) alpha\nb) beta");
        assert_eq!(msgs, vec!["alpha", "beta"]);
        // 罗马数字（规范写法）。
        let msgs = split_queued_messages("i. one\nii. two\niii. three");
        assert_eq!(msgs, vec!["one", "two", "three"]);
        // 续行归属当前项。
        let msgs = split_queued_messages("1. 第一段\n   续行\n2. 第二段");
        assert_eq!(msgs, vec!["第一段\n   续行", "第二段"]);
    }

    #[test]
    fn non_sequential_or_mixed_input_stays_single_message() {
        // 编号不连续。
        assert_eq!(split_queued_messages("1. 一\n3. 三"), vec!["1. 一\n3. 三"]);
        // 单项不算队列。
        assert_eq!(split_queued_messages("1. 只有一项"), vec!["1. 只有一项"]);
        // 标点混用。
        assert_eq!(split_queued_messages("1. 一\n2) 二"), vec!["1. 一\n2) 二"]);
        // 普通散文。
        assert_eq!(split_queued_messages("就这样吧"), vec!["就这样吧"]);
        // 空输入。
        assert!(split_queued_messages("   \n  ").is_empty());
        // 马甲数字（`1.5`）不是列表项。
        assert_eq!(split_queued_messages("1.5 版本"), vec!["1.5 版本"]);
    }
}
