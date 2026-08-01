//! 合并冲突检测与解决（`conflict://` 协议）。
//!
//! 移植 oh-my-pi `tools/conflict-detect.ts` 的 Gyre 子集：
//! - **检测**：read 侧纯状态机扫描 `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>` 块
//!   （严格列 0 标记），注册进会话级 [`ConflictHistory`]（id 稳定：同 path+start 复用）；
//! - **解决**：write 侧 `conflict://<N>` 按注册区域 splice 替换（内容校验防漂移），
//!   支持 `@ours` / `@theirs` / `@base` / `@both` 整行 token 与批量 `conflict://*`。

use std::collections::HashMap;

/// 单块冲突（注册表条目）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictBlock {
    /// 会话级 id（1-based；read 注册时分配，跨 read/write 稳定）。
    pub id: usize,
    /// 文件路径（工作区相对）。
    pub path: String,
    /// 块起始行（1-based，`<<<<<<<` 所在行）。
    pub start_line: usize,
    /// 块结束行（1-based，`>>>>>>>` 所在行，含）。
    pub end_line: usize,
    /// ours 侧文本（不含标记行）。
    pub ours: String,
    /// base 侧文本（`|||||||` 与 `=======` 之间；2-way 冲突为 `None`）。
    pub base: Option<String>,
    /// theirs 侧文本（不含标记行）。
    pub theirs: String,
    /// 是否 2-way（无 `|||||||` 行）。
    pub is_two_way: bool,
    /// 标记行原文（防漂移校验）：(ours 行, base 行或空, 分隔行, theirs 行)。
    pub markers: (String, String, String, String),
}

impl ConflictBlock {
    /// 完整区域文本（含标记行，末尾换行保留风格）。
    #[must_use]
    pub fn region_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.markers.0);
        out.push('\n');
        out.push_str(&self.ours);
        if !self.ours.is_empty() && !self.ours.ends_with('\n') {
            out.push('\n');
        }
        if let Some(base) = &self.base {
            out.push_str(&self.markers.1);
            out.push('\n');
            out.push_str(base);
            if !base.is_empty() && !base.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push_str(&self.markers.2);
        out.push('\n');
        out.push_str(&self.theirs);
        if !self.theirs.is_empty() && !self.theirs.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&self.markers.3);
        out
    }

    /// 取单侧文本。
    ///
    /// # Errors
    /// 未知侧名或 2-way 冲突请求 base 侧时返回错误。
    pub fn side(&self, side: &str) -> Result<&str, String> {
        match side {
            "ours" => Ok(&self.ours),
            "theirs" => Ok(&self.theirs),
            "base" => self
                .base
                .as_deref()
                .ok_or_else(|| "该冲突为 2-way（无 base 侧）".to_string()),
            _ => Err(format!("未知侧: {side}")),
        }
    }
}

/// 会话级冲突注册表（`read_file :conflicts` 注册，`write_file conflict://N` 解决）。
#[derive(Debug, Default)]
pub struct ConflictHistory {
    blocks: Vec<ConflictBlock>,
    /// `path:start_line` → id（复用：同文件同位置重扫不新建 id）。
    by_key: HashMap<String, usize>,
    next_id: usize,
}

impl ConflictHistory {
    /// 空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个冲突块（同 `path`+`start_line` 复用 id，覆盖区域与侧文本）。
    pub fn register(&mut self, block: ConflictBlock) -> usize {
        let key = format!("{}:{}", block.path, block.start_line);
        if let Some(id) = self.by_key.get(&key) {
            let id = *id;
            if let Some(existing) = self.blocks.iter_mut().find(|b| b.id == id) {
                *existing = ConflictBlock { id, ..block };
            }
            return id;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.by_key.insert(key, id);
        self.blocks.push(ConflictBlock { id, ..block });
        id
    }

    /// 按 id 取块。
    #[must_use]
    pub fn get(&self, id: usize) -> Option<&ConflictBlock> {
        self.blocks.iter().find(|b| b.id == id)
    }

    /// 取全部块（注册顺序）。
    #[must_use]
    pub fn all(&self) -> &[ConflictBlock] {
        &self.blocks
    }

    /// 解决后移除条目。
    pub fn remove(&mut self, id: usize) {
        if let Some(pos) = self.blocks.iter().position(|b| b.id == id) {
            let block = self.blocks.remove(pos);
            self.by_key.remove(&format!("{}:{}", block.path, block.start_line));
        }
    }

    /// 未解决块数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// 扫描原始块（纯状态机，行级）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawBlock {
    /// 块起始行（0-based）。
    pub start: usize,
    /// 块结束行（0-based，含 `>>>>>>>` 行）。
    pub end: usize,
    /// ours 侧行（不含标记行）。
    pub ours: Vec<String>,
    /// base 侧行（`|||||||` 与 `=======` 之间；2-way 为空）。
    pub base: Vec<String>,
    /// theirs 侧行。
    pub theirs: Vec<String>,
    /// 标记行原文。
    pub markers: (String, String, String, String),
}

/// 扫描冲突块：严格列 0 标记（`<<<<<<<` / `|||||||` / `=======` / `>>>>>>>`，
/// 前缀或 前缀+空格+标签；CRLF 容忍）；仅闭合块返回；2-way（无 `|||||||`）base 为空。
#[must_use]
pub fn scan_conflict_blocks(lines: &[&str]) -> Vec<RawBlock> {
    #[derive(PartialEq)]
    enum Phase {
        Idle,
        Ours,
        Base,
        Theirs,
    }
    let mut blocks = Vec::new();
    let mut phase = Phase::Idle;
    let mut start = 0usize;
    let mut ours: Vec<String> = Vec::new();
    let mut base: Vec<String> = Vec::new();
    let mut theirs: Vec<String> = Vec::new();
    let mut markers = (String::new(), String::new(), String::new(), String::new());

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_end();
        let is_marker = |prefix: &str| {
            trimmed == prefix || trimmed.starts_with(&format!("{prefix} "))
        };
        match phase {
            Phase::Idle => {
                if is_marker("<<<<<<<") {
                    phase = Phase::Ours;
                    start = i;
                    ours.clear();
                    base.clear();
                    theirs.clear();
                    markers.0 = trimmed.to_string();
                }
            }
            Phase::Ours => {
                if is_marker("|||||||") {
                    phase = Phase::Base;
                    markers.1 = trimmed.to_string();
                } else if is_marker("=======") {
                    phase = Phase::Theirs;
                    markers.2 = trimmed.to_string();
                } else if is_marker(">>>>>>>") {
                    // 空 ours + 直接结束：异常块，忽略。
                    phase = Phase::Idle;
                } else {
                    ours.push((*line).to_string());
                }
            }
            Phase::Base => {
                if is_marker("=======") {
                    phase = Phase::Theirs;
                    markers.2 = trimmed.to_string();
                } else if is_marker(">>>>>>>") {
                    phase = Phase::Idle;
                } else {
                    base.push((*line).to_string());
                }
            }
            Phase::Theirs => {
                if is_marker(">>>>>>>") {
                    markers.3 = trimmed.to_string();
                    blocks.push(RawBlock {
                        start,
                        end: i,
                        ours: std::mem::take(&mut ours),
                        base: std::mem::take(&mut base),
                        theirs: std::mem::take(&mut theirs),
                        markers: markers.clone(),
                    });
                    phase = Phase::Idle;
                } else {
                    theirs.push((*line).to_string());
                }
            }
        }
    }
    blocks
}

/// 把 `RawBlock` 转成注册表条目（path 为工作区相对路径；行号转 1-based）。
#[must_use]
pub fn block_to_entry(raw: &RawBlock, path: &str) -> ConflictBlock {
    ConflictBlock {
        id: 0, // register 分配
        path: path.to_string(),
        start_line: raw.start + 1,
        end_line: raw.end + 1,
        ours: raw.ours.join("\n"),
        base: if raw.base.is_empty() {
            None
        } else {
            Some(raw.base.join("\n"))
        },
        theirs: raw.theirs.join("\n"),
        is_two_way: raw.base.is_empty(),
        markers: raw.markers.clone(),
    }
}

/// 从文件全文扫描并注册所有冲突块。返回注册的 id 列表（注册顺序）。
pub fn register_all(history: &mut ConflictHistory, path: &str, text: &str) -> Vec<usize> {
    let lines: Vec<&str> = text.lines().collect();
    let raw = scan_conflict_blocks(&lines);
    raw.iter().map(|r| history.register(block_to_entry(r, path))).collect()
}

/// 解析 `conflict://` 目标：`conflict://<N>`、`conflict://<N>/<side>`、`conflict://*`。
///
/// 返回 `(模式, id, side)`：`*` → `(Wildcard, 0, "")`；无侧 → `(Single, id, "")`；
/// 带侧 → `(Single, id, side)`。
///
/// # Errors
/// URI 前缀非 `conflict://` 或 id 非数字时返回错误。
pub fn parse_conflict_uri(uri: &str) -> Result<(ConflictTarget, usize, String), String> {
    let rest = uri
        .strip_prefix("conflict://")
        .ok_or_else(|| format!("非法 conflict URI: {uri}"))?;
    if rest == "*" {
        return Ok((ConflictTarget::Wildcard, 0, String::new()));
    }
    let (id_part, side) = match rest.split_once('/') {
        Some((id, side)) => (id, side.to_string()),
        None => (rest, String::new()),
    };
    let id: usize = id_part
        .parse()
        .map_err(|_| format!("冲突 id 非法: {id_part}"))?;
    Ok((ConflictTarget::Single, id, side))
}

/// 冲突目标模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictTarget {
    /// `conflict://<N>`（或带侧）。
    Single,
    /// `conflict://*`（批量）。
    Wildcard,
}

/// 展开整行 token：`@ours` / `@theirs` / `@base` / `@both`（`@base` 2-way 报错；
/// `@both` = ours + theirs，仅纯追加语义）。
///
/// # Errors
/// 2-way 冲突请求 `@base` 时返回错误。
pub fn expand_content_tokens(content: &str, block: &ConflictBlock) -> Result<String, String> {
    let t = content.trim();
    match t {
        "@ours" => Ok(block.ours.clone()),
        "@theirs" => Ok(block.theirs.clone()),
        "@base" => block
            .base
            .clone()
            .ok_or_else(|| format!("冲突 #{}({}) 为 2-way，无 base 侧", block.id, block.path)),
        "@both" => {
            let mut out = block.ours.clone();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&block.theirs);
            Ok(out)
        }
        _ => Ok(content.to_string()),
    }
}

/// 按注册区域把冲突块替换为 `replacement`（内容校验防漂移：标记行必须与注册时一致）。
///
/// 行尾风格跟随文件（CRLF 保留）。
///
/// # Errors
/// 行号越界或标记行不匹配（文件已变化）时返回错误。
pub fn splice_block(current_text: &str, block: &ConflictBlock, replacement: &str) -> Result<String, String> {
    // 按 1-based 行号定位字节区间。
    let mut line_starts: Vec<usize> = Vec::new();
    let mut offset = 0usize;
    for line in current_text.split_inclusive('\n') {
        line_starts.push(offset);
        offset += line.len();
    }
    let last_len = current_text.len();
    let start_off = block
        .start_line
        .checked_sub(1)
        .and_then(|i| line_starts.get(i).copied())
        .ok_or_else(|| format!("冲突 #{}({}) 行号越界，文件已变化？请重新 :conflicts 扫描", block.id, block.path))?;
    let end_off = block
        .end_line
        .checked_sub(1)
        .and_then(|i| line_starts.get(i + 1).copied())
        .unwrap_or(last_len);

    // 标记行内容校验（防漂移：行号对上但内容已变 → 拒绝，避免错切）。
    // 首行/末行必须与注册的 ours/theirs 标记一致；sep（与 base，3-way）标记必须按序出现
    //（ours 侧行数不固定，故不能按固定索引取）。
    let region = &current_text[start_off..end_off];
    let region_lines: Vec<&str> = region.lines().map(str::trim_end).collect();
    let first = region_lines.first().copied().unwrap_or("");
    let last = region_lines.last().copied().unwrap_or("");
    let inner = &region_lines[1..region_lines.len().saturating_sub(1)];
    let sep_pos = inner.iter().position(|l| *l == block.markers.2);
    let base_ok = if block.is_two_way {
        true
    } else {
        inner
            .iter()
            .position(|l| *l == block.markers.1)
            .is_some_and(|p| sep_pos.is_some_and(|s| p < s))
    };
    if first != block.markers.0
        || last != block.markers.3
        || sep_pos.is_none()
        || !base_ok
    {
        return Err(format!(
            "冲突 #{}({}) 区域内容已变化（标记行不匹配），拒绝写入；请重新 :conflicts 扫描",
            block.id, block.path
        ));
    }

    // 行尾风格：区域首行若 CRLF 则 replacement 也 CRLF；区域以换行结尾而 replacement
    // 未以换行结尾时补一个（保持行结构，避免与下行粘连）。
    let crlf = region.contains("\r\n");
    let repl = if region.ends_with('\n') && !replacement.ends_with('\n') {
        format!("{replacement}\n")
    } else {
        replacement.to_string()
    };
    let mut out = String::with_capacity(current_text.len() + repl.len());
    out.push_str(&current_text[..start_off]);
    if crlf {
        out.push_str(&repl.replace('\n', "\r\n"));
    } else {
        out.push_str(&repl);
    }
    if end_off < current_text.len() {
        out.push_str(&current_text[end_off..]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_2way() -> String {
        "line1\n<<<<<<< HEAD\nours-a\nours-b\n=======\ntheirs-a\n>>>>>>> branch\nline9\n".to_string()
    }

    fn sample_3way() -> String {
        "line1\n<<<<<<< HEAD\nours\n||||||| base\nbase\n=======\ntheirs\n>>>>>>> branch\nline9\n".to_string()
    }

    #[test]
    fn scans_2way_block() {
        let text = sample_2way();
        let lines: Vec<&str> = text.lines().collect();
        let blocks = scan_conflict_blocks(&lines);
        assert_eq!(blocks.len(), 1);
        let b = &blocks[0];
        assert_eq!(b.start, 1);
        assert_eq!(b.end, 6);
        assert_eq!(b.ours, vec!["ours-a", "ours-b"]);
        assert!(b.base.is_empty());
        assert_eq!(b.theirs, vec!["theirs-a"]);
        assert_eq!(b.markers.0, "<<<<<<< HEAD");
        assert_eq!(b.markers.2, "=======");
        assert_eq!(b.markers.3, ">>>>>>> branch");
    }

    #[test]
    fn scans_3way_block() {
        let text = sample_3way();
        let lines: Vec<&str> = text.lines().collect();
        let blocks = scan_conflict_blocks(&lines);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].base, vec!["base"]);
        assert!(blocks[0].markers.1.starts_with("|||||||"));
    }

    #[test]
    fn ignores_marker_without_column_zero() {
        let text = "  <<<<<<< HEAD\nx\n=======\ny\n>>>>>>> b\n";
        let lines: Vec<&str> = text.lines().collect();
        assert!(scan_conflict_blocks(&lines).is_empty());
    }

    #[test]
    fn ignores_unclosed_block() {
        let text = "<<<<<<< HEAD\nx\n=======\ny\n";
        let lines: Vec<&str> = text.lines().collect();
        assert!(scan_conflict_blocks(&lines).is_empty());
    }

    #[test]
    fn scans_multiple_blocks() {
        let text = "<<<<<<< a\n1\n=======\n2\n>>>>>>> b\nok\n<<<<<<< a\n3\n=======\n4\n>>>>>>> b\n";
        let lines: Vec<&str> = text.lines().collect();
        let blocks = scan_conflict_blocks(&lines);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].ours, vec!["1"]);
        assert_eq!(blocks[1].theirs, vec!["4"]);
    }

    #[test]
    fn register_dedupes_by_path_and_start() {
        let mut h = ConflictHistory::new();
        let text = sample_2way();
        let lines: Vec<&str> = text.lines().collect();
        let raw = &scan_conflict_blocks(&lines)[0];
        let id1 = h.register(block_to_entry(raw, "a.txt"));
        let id2 = h.register(block_to_entry(raw, "a.txt"));
        assert_eq!(id1, id2, "同 path+start 应复用 id");
        assert_eq!(h.len(), 1);
        let id3 = h.register(block_to_entry(raw, "b.txt"));
        assert_ne!(id1, id3);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn splice_replaces_with_ours() {
        let text = sample_2way();
        let lines: Vec<&str> = text.lines().collect();
        let raw = &scan_conflict_blocks(&lines)[0];
        let block = block_to_entry(raw, "a.txt");
        let out = splice_block(&text, &block, "@ours-not-token").unwrap();
        assert_eq!(out, "line1\n@ours-not-token\nline9\n");
    }

    #[test]
    fn splice_keeps_crlf_style() {
        let text = "line1\r\n<<<<<<< HEAD\r\nours\r\n=======\r\ntheirs\r\n>>>>>>> b\r\nline9\r\n";
        let lines: Vec<&str> = text.lines().collect();
        let raw = &scan_conflict_blocks(&lines)[0];
        let block = block_to_entry(raw, "a.txt");
        let out = splice_block(&text, &block, "resolved").unwrap();
        assert_eq!(out, "line1\r\nresolved\r\nline9\r\n");
    }

    #[test]
    fn splice_rejects_drifted_content() {
        let text = sample_2way();
        let lines: Vec<&str> = text.lines().collect();
        let raw = &scan_conflict_blocks(&lines)[0];
        let block = block_to_entry(raw, "a.txt");
        // 文件在注册后发生变化（标记行被改动）。
        let drifted = text.replace("<<<<<<< HEAD", "<<<<<<< CHANGED");
        assert!(splice_block(&drifted, &block, "x").is_err());
    }

    #[test]
    fn expand_tokens() {
        let text = sample_2way();
        let lines: Vec<&str> = text.lines().collect();
        let raw = &scan_conflict_blocks(&lines)[0];
        let block = block_to_entry(raw, "a.txt");
        assert_eq!(expand_content_tokens("@ours", &block).unwrap(), "ours-a\nours-b");
        assert_eq!(expand_content_tokens("@theirs", &block).unwrap(), "theirs-a");
        assert!(expand_content_tokens("@base", &block).is_err()); // 2-way 无 base
        assert_eq!(
            expand_content_tokens("@both", &block).unwrap(),
            "ours-a\nours-b\ntheirs-a"
        );
        assert_eq!(expand_content_tokens("plain", &block).unwrap(), "plain");
    }

    #[test]
    fn register_all_and_render_region() {
        let mut h = ConflictHistory::new();
        let ids = register_all(&mut h, "a.txt", &sample_2way());
        assert_eq!(ids, vec![1]);
        let b = h.get(1).unwrap();
        let region = b.region_text();
        assert!(region.starts_with("<<<<<<< HEAD"));
        assert!(region.ends_with(">>>>>>> branch"));
        assert!(region.contains("ours-b"));
        assert!(region.contains("theirs-a"));
    }
}
