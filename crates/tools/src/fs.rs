//! `文件系统工具：read_file` / `write_file`。
//! （`str_replace/apply_diff` 已按工具收敛移除，编辑走 `apply_hashline`。）

use std::path::Path;
use std::time::Duration;

use agent_ast::{SegmentKind, SummaryOptions, SupportLang, summarize_code};
use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::compute_file_hash;
use crate::{ConflictBlock, PendingRewrite, Tool, ToolContext, write_with_effects};

/// 读取文件（带行号）。
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn description(&self) -> &'static str {
        "读取工作区内文件内容并附行号。仅限只读，不修改文件。路径可带行选择器（`:N`、`:N-M`、`:N+K`、`:N-`、逗号多区间、`:raw`）精确取段；普通文本读取首行输出 `[相对路径#哈希]` 段头（哈希为全文指纹），编辑（apply_hashline）必须引用最新段头与行号锚定。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径（相对工作区根或绝对路径），或内部协议：skill://<name>[/<rel>]（skill 内容）、memory://[summary|full]（跨会话记忆）、mcp://<server>/<uri>（MCP 资源）、local://<rel>（显式工作区相对）、artifact://<id>（shake 归档内容回读）、http(s)://（抓取网页）、pr://<owner>/<repo>[/<N>]（GitHub PR，文件缓存）、issue://<owner>/<repo>[/<N>]（GitHub issue）。本地路径可带行选择器后缀（1 基绝对行号）：`:N` / `:N-`（第 N 行到文件尾）、`:N-M`（闭区间）、`:N+K`（第 N 行起 K 行）、`:a-b,c-d`（逗号多区间，升序合并）、`:raw`（原样输出，无行号）、`:raw:N-M` 或 `:N-M:raw`（原文切片）。普通文本读取首行输出 `[相对路径#哈希]` 段头（哈希为全文指纹）；后续 apply_hashline 编辑必须引用最新段头与行号。raw / 协议 / 归档 / SQLite / notebook 读取无段头。" },
                "summary": { "type": "boolean", "description": "可选：对支持语言的代码文件做结构摘要——折叠大体块、保留签名，行号与原文一致。适合快速浏览大文件；需逐行编辑时仍用真实行号。与行选择器互斥（带选择器时忽略）。" }
            },
            "required": ["path"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        CapabilityTier::ReadOnly
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let path = input
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `path` 参数".into()))?;
        // 冲突协议：`conflict://<N>[/ours|theirs|base]` 读取已注册的冲突块。
        if let Some(out) = read_conflict_uri(path, ctx).await? {
            return Ok(ToolResult::text(out));
        }
        // `:conflicts` selector：扫描文件并注册全部冲突块，返回逐块摘要。
        if let Some(rest) = path.strip_suffix(":conflicts") {
            let text = resolve_path(rest.trim(), ctx).await?;
            let out = summarize_conflicts(rest.trim(), &text, ctx)?;
            return Ok(ToolResult::text(out));
        }
        // 可选摘要：对支持语言的代码文件做结构折叠，保留签名、行号保真。
        let want_summary = input
            .get("summary")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        // 内部协议路径：路由解析 + 摘要/逐行渲染（无段头、无行选择器，行为与既往一致）。
        // 裸路径 / `local://`：行选择器 + hashline 段头/快照管线。
        let out = if is_protocol_path(path) {
            let text = resolve_path(path, ctx).await?;
            finish_text(&text, want_summary, path)
        } else {
            let rel = path.strip_prefix("local://").unwrap_or(path);
            read_local_rendered(rel, want_summary, ctx).await?
        };
        Ok(ToolResult::text(out))
    }
}

/// 逐行带行号渲染（默认行为）。
fn render_numbered(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        out.push_str(&format!("{i:>5}\t{line}\n"));
    }
    out
}

/// 结构摘要渲染：折叠大体块、保留签名；行号与原文一致（编辑须基于真实行号）。
fn render_summary(text: &str, lang: SupportLang) -> String {
    let result = summarize_code(text, lang, &SummaryOptions::default());
    // 无可折叠内容（如小文件/解析失败）：回退逐行渲染，避免无意义的摘要头。
    if !result.elided {
        return render_numbered(text);
    }
    let mut out = String::new();
    out.push_str(&format!(
        "（结构摘要：原文 {} 行，已折叠体块；行号与原文一致，如需逐行编辑请基于这些行号）\n",
        result.total_lines
    ));
    for seg in &result.segments {
        match seg.kind {
            SegmentKind::Kept => {
                if let Some(body) = &seg.text {
                    for (offset, line) in body.lines().enumerate() {
                        let _ = std::fmt::Write::write_fmt(
                            &mut out,
                            format_args!("{:>5}\t{line}\n", seg.start_line as usize + offset),
                        );
                    }
                }
            }
            SegmentKind::Elided => {
                let _ = std::fmt::Write::write_fmt(
                    &mut out,
                    format_args!(
                        "     ⋯⋯ (折叠第 {}-{} 行，共 {} 行) ⋯⋯\n",
                        seg.start_line,
                        seg.end_line,
                        seg.end_line
                            .saturating_sub(seg.start_line)
                            .saturating_add(1),
                    ),
                );
            }
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// 行选择器与本地读取管线（移植 omp read-selector.ts / read.ts 的选择器语义）
// ─────────────────────────────────────────────────────────────────────────────

/// 1 基闭区间行区间；`end = None` 表示开区间（从 `start` 到文件尾）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineRange {
    start: usize,
    end: Option<usize>,
}

/// 行选择器解析结果（纯函数，便于单测）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum LineSelector {
    /// 无选择器：全量读取。
    None,
    /// `:raw` / `:raw:N-M` / `:N-M:raw`：原样输出（无行号、无段头、不记快照）；
    /// `ranges` 为伴随区间（可空）。
    Raw { ranges: Vec<LineRange> },
    /// 行区间选择器：1 基绝对行号，区间已升序合并。
    Ranges(Vec<LineRange>),
}

/// 非法选择器错误：列出全部合法形式（枚举风格对齐 omp `read.ts` 的选择器错误）。
fn invalid_selector(sel: &str) -> ToolError {
    ToolError::Execution(format!(
        "非法选择器 ':{sel}'。合法形式：:N（第 N 行到文件尾）、:N-M（闭区间）、:N+K（第 N 行起 K 行）、\
:N-（N 到文件尾）、逗号多区间如 :5-16,960-973、:raw（原样输出）、:raw:N-M 或 :N-M:raw（原文切片）。\
行号为 1 基绝对行号。`:img` 不支持——读图片请用 read_image 工具。"
    ))
}

/// 解析 `path` 的行选择器后缀（[`split_path_selector`] 拆出的 sel 串）。
///
/// 支持（1 基绝对行号，含端）：
/// - `:N` / `:N-`：从第 N 行到文件尾（裸 `N` 与 `N-` 同义，对齐 omp 语义）
/// - `:N-M`：闭区间；`:N+K`：第 N 行起 K 行（K ≥ 1）
/// - 逗号多区间 `:a-b,c-d`（升序合并重叠/相邻区间）
/// - `:raw`：原样输出；`:raw:N-M` / `:N-M:raw`：原文切片（两种顺序均接受）
///
/// 解析失败返回错误并列出全部合法形式——归档/SQLite/notebook 的自有冒号语义已在
/// [`read_binary_format`] 消费，`:conflicts` 已在 execute 前置分支消费，走到这里说明
/// 选择器确实是写给文本读取的，未识别即报错而非静默整读（`:img` SVG 栅格化明确不移植，
/// Gyre 用独立的 read_image 工具读图片）。
fn parse_line_selector(sel: Option<&str>) -> Result<LineSelector, ToolError> {
    let Some(sel) = sel.filter(|s| !s.is_empty()) else {
        return Ok(LineSelector::None);
    };
    if let Some((a, b)) = sel.split_once(':') {
        // 恰一侧为 `raw`（大小写不敏感），另一侧为区间列表。
        let (_, range_chunk) = if a.eq_ignore_ascii_case("raw") {
            (a, b)
        } else if b.eq_ignore_ascii_case("raw") {
            (b, a)
        } else {
            return Err(invalid_selector(sel));
        };
        return match parse_range_list(range_chunk)? {
            Some(ranges) => Ok(LineSelector::Raw { ranges }),
            None => Err(invalid_selector(sel)),
        };
    }
    if sel.eq_ignore_ascii_case("raw") {
        return Ok(LineSelector::Raw { ranges: Vec::new() });
    }
    match parse_range_list(sel)? {
        Some(ranges) => Ok(LineSelector::Ranges(ranges)),
        None => Err(invalid_selector(sel)),
    }
}

/// 解析逗号分隔的区间列表；任一段不是区间形态返回 `Ok(None)`（由调用方决定回退/报错）。
fn parse_range_list(chunk: &str) -> Result<Option<Vec<LineRange>>, ToolError> {
    let mut ranges = Vec::new();
    for c in chunk.split(',') {
        match parse_range_chunk(c)? {
            Some(r) => ranges.push(r),
            None => return Ok(None),
        }
    }
    if ranges.is_empty() {
        return Ok(None);
    }
    Ok(Some(merge_ranges(ranges)))
}

/// 解析单个区间段：`N` / `N-M` / `N-` / `N+K`。`..` 为 `-` 的宽容别名（`N..M` == `N-M`、
/// `N..` == `N-`），`L` 前缀宽容剥除（`L50` == `50`），语义对齐 omp `LINE_RANGE_CHUNK_RE`。
///
/// 非区间形态返回 `Ok(None)`；形态是区间但越界（行号 0、end < start、K < 1）返回具体错误。
fn parse_range_chunk(chunk: &str) -> Result<Option<LineRange>, ToolError> {
    let chunk = chunk.trim();
    let chunk = chunk.strip_prefix(['L', 'l']).unwrap_or(chunk);
    let (start_s, rest) = split_digits(chunk);
    // 空串 / 非数字开头 / 数字溢出：解析失败即非区间形态（最终统一报非法选择器）。
    let Some(start) = start_s.parse::<usize>().ok() else {
        return Ok(None);
    };
    if start == 0 {
        return Err(ToolError::Execution(
            "行号选择器 0 无效：行号从 1 开始（1 基）。用 :1 表示首行。".into(),
        ));
    }
    if rest.is_empty() {
        // 裸 `N`：从第 N 行到文件尾（与 `N-` 同义）。
        return Ok(Some(LineRange { start, end: None }));
    }
    if let Some(tail) = rest.strip_prefix('-').or_else(|| rest.strip_prefix("..")) {
        if tail.is_empty() {
            return Ok(Some(LineRange { start, end: None }));
        }
        let tail = tail.strip_prefix(['L', 'l']).unwrap_or(tail);
        let Some(end) = parse_line_no(tail) else {
            return Ok(None);
        };
        if end < start {
            return Err(ToolError::Execution(format!(
                "区间 {start}-{end} 无效：end 必须 >= start（行号为 1 基）。"
            )));
        }
        return Ok(Some(LineRange {
            start,
            end: Some(end),
        }));
    }
    if let Some(tail) = rest.strip_prefix('+') {
        let count = parse_line_no(tail).unwrap_or(0);
        if count == 0 {
            return Err(ToolError::Execution(format!(
                "区间 {start}+{tail} 无效：行数 K 必须 >= 1。"
            )));
        }
        return Ok(Some(LineRange {
            start,
            end: Some(start.saturating_add(count - 1)),
        }));
    }
    Ok(None)
}

/// 解析非负十进制行号（空串 / 溢出返回 `None`）。
fn parse_line_no(s: &str) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    s.parse().ok()
}

/// 拆出前导十进制数字串与其余部分。
fn split_digits(s: &str) -> (&str, &str) {
    let i = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s.split_at(i)
}

/// 区间升序排序，重叠/相邻（下一区间起点 ≤ 上一区间 end+1）合并；开区间吞并其后全部。
fn merge_ranges(mut ranges: Vec<LineRange>) -> Vec<LineRange> {
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<LineRange> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match merged.last_mut() {
            Some(last) if last.end.is_none_or(|e| r.start <= e.saturating_add(1)) => {
                if r.end.is_none() || last.end.is_some_and(|e| r.end.unwrap_or(e) > e) {
                    last.end = r.end;
                }
            }
            _ => merged.push(r),
        }
    }
    merged
}

/// 是否内部协议路径（skill/memory/mcp/artifact/pr/issue/conflict/xd/http(s)）。
/// 协议路径不经本地行选择器管线（无段头、无快照、选择器语法不适用）。
fn is_protocol_path(path: &str) -> bool {
    [
        "skill://",
        "memory://",
        "mcp://",
        "artifact://",
        "pr://",
        "issue://",
        "conflict://",
        "xd://",
        "http://",
        "https://",
    ]
    .iter()
    .any(|p| path.starts_with(p))
}

/// 协议路径文本收口：按需结构摘要，否则逐行带行号（与既往行为一致）。
fn finish_text(text: &str, want_summary: bool, path: &str) -> String {
    if want_summary {
        match SupportLang::from_path(Path::new(path)) {
            Some(lang) => render_summary(text, lang),
            None => render_numbered(text),
        }
    } else {
        render_numbered(text)
    }
}

/// 本地读取产物。
enum LocalRead {
    /// 归档/SQLite/notebook 等二进制格式的成品输出（自带格式；不加段头、不走行选择器）。
    Formatted(String),
    /// 普通文本正文 + 工作区相对显示路径（供段头/快照/行选择器渲染）。
    /// `literal` 为真表示走了字面路径命中——整读，不得再解析选择器。
    Text {
        text: String,
        display: String,
        literal: bool,
    },
}

/// 本地文件读取核心（裸路径与 `local://` 共用）：
///
/// 1. **字面优先**：路径含 `:` 时先探测完整路径（含冒号部分）是否真实存在为文件——
///    真实文件名长得像「路径:选择器」时字面获胜，整读、不解析选择器
///    （移植 omp `read.ts` 的 `splitPathAndSelPreferringLiteral` 字面优先语义）；
/// 2. 按扩展名分派二进制格式（归档/SQLite/notebook，selector 语义归各自读取器）；
/// 3. 其余按普通文本读取（字节上限 [`MAX_READ_BYTES`]）。
async fn read_local(rel: &str, ctx: &ToolContext<'_>) -> Result<LocalRead, ToolError> {
    if rel.contains(':') {
        let lit = ctx.workspace.resolve(Path::new(rel));
        if tokio::fs::metadata(&lit).await.is_ok_and(|m| m.is_file()) {
            return Ok(LocalRead::Text {
                text: read_bounded(&lit).await?,
                display: display_path(&lit, ctx),
                literal: true,
            });
        }
    }
    let (pure, sel) = split_path_selector(rel);
    let full = ctx.workspace.resolve(Path::new(pure));
    if let Some(out) = read_binary_format(&full, sel).await? {
        return Ok(LocalRead::Formatted(out));
    }
    Ok(LocalRead::Text {
        text: read_bounded(&full).await?,
        display: display_path(&full, ctx),
        literal: false,
    })
}

/// 本地路径完整读取管线（裸路径与 `local://` 共用）：核心读取 → 行选择器解析 →
/// hashline 段头/快照 → 渲染。
///
/// - `raw` 选择器：原样输出（无行号、无段头、不记快照）；
/// - 其余普通文本读取：首行输出 `[<相对路径>#<HASH>]` 段头（HASH 为全文指纹），
///   并把**全文**快照 record 进 `ctx.snapshots`（若有；传全文而非选中切片——
///   stale-hash 恢复需按指纹回放整版正文），供 apply_hashline 锚定与恢复；
/// - 归档/SQLite/notebook 读取保持格式化输出，不加段头；
/// - `summary` 与行选择器互斥，选择器优先。
async fn read_local_rendered(
    rel: &str,
    want_summary: bool,
    ctx: &ToolContext<'_>,
) -> Result<String, ToolError> {
    match read_local(rel, ctx).await? {
        LocalRead::Formatted(out) => Ok(out),
        LocalRead::Text {
            text,
            display,
            literal,
        } => {
            // 字面命中：整读，不解析选择器。
            let sel = if literal {
                None
            } else {
                split_path_selector(rel).1
            };
            match parse_line_selector(sel)? {
                LineSelector::Raw { ranges } => Ok(elide_middle(
                    render_raw_slice(&text, &ranges),
                    MAX_RANGE_OUTPUT,
                )),
                LineSelector::None => {
                    let head = file_header(&display, &text, ctx);
                    let body = if want_summary {
                        match SupportLang::from_path(Path::new(&display)) {
                            Some(lang) => render_summary(&text, lang),
                            None => render_numbered(&text),
                        }
                    } else {
                        render_numbered(&text)
                    };
                    Ok(format!("{head}\n{body}"))
                }
                LineSelector::Ranges(ranges) => {
                    let head = file_header(&display, &text, ctx);
                    let body = elide_middle(render_ranges(&text, &ranges), MAX_RANGE_OUTPUT);
                    Ok(format!("{head}\n{body}"))
                }
            }
        }
    }
}

/// hashline 段头 `[<相对路径>#<HASH>]`：HASH 为全文指纹（[`compute_file_hash`]）。
/// 有快照存储时经 `record` 取指纹（同一规范化），顺带记录全文版本供 stale-hash 恢复。
fn file_header(display: &str, full_text: &str, ctx: &ToolContext<'_>) -> String {
    let hash = match ctx.snapshots {
        Some(store) => store
            .write()
            .expect("snapshot 锁中毒")
            .record(display, full_text),
        None => compute_file_hash(full_text),
    };
    format!("[{display}#{hash}]")
}

/// 工作区相对显示路径（段头/快照键）：剥去工作区根前缀、分隔符统一为 `/`；
/// 不在根下（沙箱重映射后的怪路径）则原样返回。
fn display_path(full: &Path, ctx: &ToolContext<'_>) -> String {
    let rel = full.strip_prefix(ctx.workspace.root()).unwrap_or(full);
    rel.to_string_lossy().replace('\\', "/")
}

/// 按行区间渲染（1 基绝对行号，`{n:>5}\t` 风格）；多区间一次输出，区间间空行分隔。
/// 端点越界收敛到文件尾；整个区间落在文件外时输出范围提示（避免模型误读为空文件）。
fn render_ranges(text: &str, ranges: &[LineRange]) -> String {
    use std::fmt::Write as _;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let mut out = String::new();
    for (i, r) in ranges.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let start_idx = r.start.saturating_sub(1);
        let end_idx = r.end.map_or(total, |e| e.min(total));
        if start_idx >= total {
            let label = match r.end {
                Some(e) => format!("{}-{e}", r.start),
                None => format!("{}-", r.start),
            };
            let _ = writeln!(out, "（区间 {label} 超出文件范围：文件共 {total} 行）");
            continue;
        }
        for (idx, line) in lines.iter().enumerate().take(end_idx).skip(start_idx) {
            let _ = writeln!(out, "{:>5}\t{line}", idx + 1);
        }
    }
    out
}

/// raw 输出：原样正文（无行号、无段头）。带区间时按 1 基区间切片，多区间间空一行。
fn render_raw_slice(text: &str, ranges: &[LineRange]) -> String {
    if ranges.is_empty() {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let mut out = String::new();
    for (i, r) in ranges.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let start_idx = r.start.saturating_sub(1);
        let end_idx = r.end.map_or(total, |e| e.min(total));
        for line in lines.iter().take(end_idx).skip(start_idx) {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// 区间渲染输出上限：与全量读取的字节上限同源（选中行数超大时按 head+tail 折叠）。
const MAX_RANGE_OUTPUT: usize = MAX_READ_BYTES;

/// 超限输出 head+tail 中段省略（策略对齐 shell.rs `elide_middle`）：保留前 60% 与后 25%
/// 字节（各自回退 UTF-8 边界并对齐行首），中段以提示行替代——截断不再吃掉尾部。
fn elide_middle(out: String, max: usize) -> String {
    if out.len() <= max {
        return out;
    }
    // 回退 UTF-8 边界并向上对齐到行首（i 落在行首字符上）。
    let align = |i: usize| -> usize {
        let mut i = i.min(out.len());
        while i > 0 && !out.is_char_boundary(i) {
            i -= 1;
        }
        while i > 0 && out.as_bytes()[i - 1] != b'\n' {
            i -= 1;
        }
        i
    };
    let head_cut = align(out.len() * 3 / 5);
    let tail_start = align(out.len() - out.len() / 4);
    if tail_start <= head_cut {
        // 切点交叉（超长单行等极端情形）：退化为纯头部截断。
        let mut s = out[..head_cut].to_string();
        s.push_str("\n...(输出过长，已截断)");
        return s;
    }
    let omitted = out[head_cut..tail_start].len();
    let mut s = String::with_capacity(head_cut + (out.len() - tail_start) + 96);
    s.push_str(&out[..head_cut]);
    s.push_str(&format!("\n[... 已省略中间输出约 {omitted} 字节 ...]\n"));
    s.push_str(&out[tail_start..]);
    s
}

/// 解析 `read_file` 的 `path`：内部协议路由 + 裸本地路径，返回原始文本。
///
/// 支持协议（按前缀分流）：
/// - `skill://<name>[/<rel>]` → SkillResolver（skill 内容）
/// - `memory://[summary|full]` → MemoryStore（跨会话记忆；默认 summary）
/// - `mcp://<server>/<uri>` → ResourceResolver（MCP `resources/read`）
/// - `local://<rel>` → 工作区相对路径（显式本地协议，等价裸路径）
/// - `http(s)://` → HTTP 抓取
/// - 其他 → 工作区相对/绝对本地路径
async fn resolve_path(path: &str, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
    if path.starts_with("conflict://") {
        if let Some(out) = read_conflict_uri(path, ctx).await? {
            return Ok(out);
        }
    }
    // `pr://` / `issue://`：GitHub PR/issue 读取（文件缓存；`pr://owner/repo` 列最近，
    // `pr://owner/repo/<N>` 读详情；`?state=` 过滤）。
    if path.starts_with("pr://") || path.starts_with("issue://") {
        return resolve_gh_uri(path, ctx).await;
    }
    // `xd://pending`：列出暂存的 ast-rewrite 重写（供模型检查后 resolve/reject）。
    if path == "xd://pending" {
        let Some(queue) = ctx.pending_rewrites else {
            return Err(ToolError::Execution(
                "暂存队列未启用（当前运行环境不支持 xd://pending）".into(),
            ));
        };
        let out = {
            let q = queue.lock().expect("pending 队列锁");
            if q.is_empty() {
                return Ok("暂存队列为空".into());
            }
            q.iter()
                .map(|p| {
                    format!(
                        "{} (pattern: {}; {}) — {} replacements ({} → {} 字节)",
                        p.path,
                        p.pattern,
                        p.strictness.as_deref().unwrap_or("smart"),
                        p.count,
                        p.old_len,
                        p.new_len
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        return Ok(out);
    }
    if path.strip_prefix("skill://").is_some() {
        // 注意：`SkillResolver::resolve` 内部会再次剥离 `skill://` 前缀（见 skills/registry.rs），
        // 故此处传入完整 path，无需手动剥离。
        let Some(resolver) = ctx.skills else {
            return Err(ToolError::Execution(
                "skill:// 解析不可用（未注入 Skill 目录）".into(),
            ));
        };
        let file = resolver
            .resolve(path)
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        let bytes = tokio::fs::read(&file).await.map_err(ToolError::Io)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    } else if let Some(rest) = path.strip_prefix("memory://") {
        resolve_memory(rest, ctx).await
    } else if let Some(rest) = path.strip_prefix("mcp://") {
        resolve_mcp(rest, ctx).await
    } else if let Some(id) = path.strip_prefix("artifact://") {
        resolve_artifact(id, ctx).await
    } else if let Some(rel) = path.strip_prefix("local://") {
        // `local://` 等价裸路径：核心读取，返回原始正文（渲染由 execute 的本地管线负责，
        // `:conflicts` 等内部消费方需要未渲染文本）。
        match read_local(rel, ctx).await? {
            LocalRead::Formatted(out) => Ok(out),
            LocalRead::Text { text, .. } => Ok(text),
        }
    } else if path.starts_with("http://") || path.starts_with("https://") {
        fetch_http(path).await
    } else {
        match read_local(path, ctx).await? {
            LocalRead::Formatted(out) => Ok(out),
            LocalRead::Text { text, .. } => Ok(text),
        }
    }
}

/// `conflict://<N>[/ours|theirs|base]` 读取：完整块带原文行号，或单侧文本。
/// 非 conflict URI 返回 `Ok(None)`。
async fn read_conflict_uri(uri: &str, ctx: &ToolContext<'_>) -> Result<Option<String>, ToolError> {
    if !uri.starts_with("conflict://") {
        return Ok(None);
    }
    let Some(history) = ctx.conflicts else {
        return Err(ToolError::Execution(
            "conflict:// 未启用（无会话冲突注册表）".into(),
        ));
    };
    let (target, id, side) =
        crate::conflict::parse_conflict_uri(uri).map_err(ToolError::InvalidArgs)?;
    match target {
        crate::conflict::ConflictTarget::Wildcard => Err(ToolError::InvalidArgs(
            "conflict://* 仅支持写入（批量解决）".into(),
        )),
        crate::conflict::ConflictTarget::Single => {
            let block = {
                let h = history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                h.get(id).cloned().ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "冲突 #{id} 未注册（先用 read_file <file>:conflicts 扫描）"
                    ))
                })?
            };
            if side.is_empty() {
                // 完整块：带原文行号（与 hashline 锚点对齐）。
                let mut out = format!(
                    "冲突 #{}({}，L{}-L{}；@ours/@theirs{} 可解)：\n",
                    block.id,
                    block.path,
                    block.start_line,
                    block.end_line,
                    if block.is_two_way { "" } else { "/@base" },
                );
                for (i, line) in block.region_text().lines().enumerate() {
                    let _ = std::fmt::Write::write_fmt(
                        &mut out,
                        format_args!("{:>5}\t{line}\n", block.start_line + i),
                    );
                }
                Ok(Some(out))
            } else {
                let side_text = block.side(&side).map_err(ToolError::InvalidArgs)?;
                Ok(Some(format!(
                    "冲突 #{}({}) {side} 侧：\n{side_text}\n",
                    block.id, block.path
                )))
            }
        }
    }
}

/// `path:conflicts` 扫描：注册全部冲突块并返回逐块摘要（id 供 `conflict://` 使用）。
fn summarize_conflicts(path: &str, text: &str, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
    let Some(history) = ctx.conflicts else {
        return Err(ToolError::Execution(
            ":conflicts 未启用（无会话冲突注册表）".into(),
        ));
    };
    let mut h = history
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ids = crate::conflict::register_all(&mut h, path, text);
    if ids.is_empty() {
        return Ok(format!("{path}：无合并冲突"));
    }
    let has_base = ids
        .iter()
        .any(|id| h.get(*id).is_some_and(|b| !b.is_two_way));
    let mut out = format!(
        "{path}：⚠ {} 处合并冲突（write_file conflict://<N> 解决，content 用 @ours/@theirs{})\n",
        ids.len(),
        if has_base { "/@base" } else { "" },
    );
    for id in &ids {
        if let Some(b) = h.get(*id) {
            let ours_preview: String = b
                .ours
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            let theirs_preview: String = b
                .theirs
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            let _ = std::fmt::Write::write_fmt(
                &mut out,
                format_args!(
                    "  conflict://{} L{}-L{}  ours: {} | theirs: {}\n",
                    b.id, b.start_line, b.end_line, ours_preview, theirs_preview
                ),
            );
        }
    }
    Ok(out)
}

/// `conflict://<N>` / `conflict://*` 写入：按注册区域 splice 解决。
///
/// 单个：content 为 `@ours` / `@theirs` / `@base` / `@both` 或替换文本。
/// 批量：content 为 `N: @ours` 指令行（未列出的 id 跳过）或单一内容应用到全部已注册块。
#[allow(clippy::too_many_lines)] // 单/批量两条路径共享参数校验与注册表语义，拆分反损可读性
async fn resolve_conflict(
    uri: &str,
    content: &str,
    ctx: &ToolContext<'_>,
) -> Result<ToolResult, ToolError> {
    let Some(history) = ctx.conflicts else {
        return Err(ToolError::Execution(
            "conflict:// 未启用（无会话冲突注册表）".into(),
        ));
    };
    let (target, id, side) =
        crate::conflict::parse_conflict_uri(uri).map_err(ToolError::InvalidArgs)?;
    match target {
        crate::conflict::ConflictTarget::Single => {
            if !side.is_empty() {
                return Err(ToolError::InvalidArgs(format!(
                    "conflict://{id} 带侧仅支持读取（read_file）；写入请用 conflict://{id}"
                )));
            }
            // 锁内取出块 + 展开 token，尽早释放锁（不跨 await 持锁）。
            let (block, replacement) = {
                let h = history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let block = h.get(id).cloned().ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "冲突 #{id} 未注册（先用 read_file <file>:conflicts 扫描）"
                    ))
                })?;
                drop(h);
                let replacement = crate::conflict::expand_content_tokens(content, &block)
                    .map_err(ToolError::InvalidArgs)?;
                (block, replacement)
            };
            let full = ctx.workspace.resolve(Path::new(&block.path));
            let current = tokio::fs::read_to_string(&full)
                .await
                .map_err(ToolError::Io)?;
            let new_text = crate::conflict::splice_block(&current, &block, &replacement)
                .map_err(ToolError::Execution)?;
            write_with_effects(&full, &new_text, ctx).await?;
            history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id);
            Ok(ToolResult::text(format!(
                "已解决冲突 #{id}（{}）",
                block.path
            )))
        }
        crate::conflict::ConflictTarget::Wildcard => {
            // 锁内快照全部块 + 解析指令，尽早释放锁。
            let (blocks, directives) = {
                let h = history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let blocks = h.all().to_vec();
                let directives = parse_directives(content);
                drop(h);
                (blocks, directives)
            };
            if blocks.is_empty() {
                return Ok(ToolResult::text("无已注册冲突"));
            }
            // 按文件分组 + 自底向上（end_line 降序）保持锚点有效。
            let mut by_file: std::collections::BTreeMap<String, Vec<(ConflictBlock, String)>> =
                std::collections::BTreeMap::new();
            for b in &blocks {
                let repl = match &directives {
                    Some(d) => match d.get(&b.id) {
                        Some(r) => r.clone(),
                        None => continue,
                    },
                    None => content.to_string(),
                };
                let repl = crate::conflict::expand_content_tokens(&repl, b)
                    .map_err(ToolError::InvalidArgs)?;
                by_file
                    .entry(b.path.clone())
                    .or_default()
                    .push((b.clone(), repl));
            }
            if by_file.is_empty() {
                return Err(ToolError::InvalidArgs(
                    "指令未命中任何已注册冲突（可用 read_file <file>:conflicts 查看 id）".into(),
                ));
            }
            let mut solved: Vec<usize> = Vec::new();
            let mut failed: Vec<String> = Vec::new();
            for (path, mut items) in by_file {
                let full = ctx.workspace.resolve(Path::new(&path));
                let current = tokio::fs::read_to_string(&full)
                    .await
                    .map_err(ToolError::Io)?;
                items.sort_by_key(|(b, _)| std::cmp::Reverse(b.end_line));
                let mut text = current;
                let mut ok = true;
                for (b, repl) in &items {
                    match crate::conflict::splice_block(&text, b, repl) {
                        Ok(t) => text = t,
                        Err(e) => {
                            failed.push(format!("{}: {e}", b.path));
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    write_with_effects(&full, &text, ctx).await?;
                    solved.extend(items.iter().map(|(b, _)| b.id));
                }
            }
            // 移除已解决条目。
            if !solved.is_empty() {
                let mut h = history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for id in &solved {
                    h.remove(*id);
                }
            }
            let mut msg = format!("已解决 {} 处冲突", solved.len());
            if !failed.is_empty() {
                let _ = std::fmt::Write::write_fmt(
                    &mut msg,
                    format_args!("；失败 {} 处：{}", failed.len(), failed.join("; ")),
                );
            }
            Ok(ToolResult::text(msg))
        }
    }
}

/// 处理 `write_file` 的 `xd://resolve` / `xd://reject`：应用/丢弃暂存的 ast-rewrite 重写。
///
/// `content` 非空时按「路径前缀」过滤队列（匹配 `path` 或 `path:pattern` 的条目）；
/// 为空时作用于全部。应用时重读文件、重放重写，替换计数与暂存不一致（文件已漂移）
/// 则拒绝并提示重新 preview——要么全成要么全不。
#[allow(clippy::too_many_lines)] // 应用/丢弃 + 漂移复查 + 失败保留语义集中在一条路径，拆分反损可读性
async fn resolve_pending_rewrites(
    device: &str,
    content: &str,
    ctx: &ToolContext<'_>,
) -> Result<ToolResult, ToolError> {
    let Some(queue) = ctx.pending_rewrites else {
        return Err(ToolError::Execution(
            "暂存队列未启用（当前运行环境不支持 xd://resolve）".into(),
        ));
    };
    let filter = content.trim();
    let apply = device == "xd://resolve";

    // 先克隆待处理项（不跨 await 持锁）。
    let items: Vec<PendingRewrite> = {
        let q = queue.lock().expect("pending 队列锁");
        let iter = q.iter();
        let iter = if filter.is_empty() {
            iter.collect::<Vec<_>>()
        } else {
            iter.filter(|p| p.path == filter || format!("{}:{}", p.path, p.pattern) == filter)
                .collect::<Vec<_>>()
        };
        let items = iter.into_iter().cloned().collect::<Vec<_>>();
        drop(q);
        items
    };
    if items.is_empty() {
        let empty_msg = if apply {
            "暂存队列为空（没有待应用的 ast-rewrite）"
        } else {
            "暂存队列为空（没有可丢弃的 ast-rewrite）"
        };
        return Ok(ToolResult::text(empty_msg));
    }
    if !apply {
        // 丢弃：按 path 前缀移除。
        let mut q = queue.lock().expect("pending 队列锁");
        for p in &items {
            q.retain(|x| !(x.path == p.path && x.pattern == p.pattern));
        }
        drop(q);
        return Ok(ToolResult::text(format!(
            "已丢弃 {} 条暂存重写（{}）",
            items.len(),
            items[0].path
        )));
    }

    let mut applied = 0usize;
    let mut failures = Vec::new();
    for p in &items {
        let full = ctx.workspace.resolve(std::path::Path::new(&p.path));
        let text = match tokio::fs::read_to_string(&full).await {
            Ok(t) => t,
            Err(e) => {
                failures.push(format!("{}: 读取失败: {e}", p.path));
                continue;
            }
        };
        let Some(lang) = agent_ast::SupportLang::from_path(&full) else {
            failures.push(format!("{}: 无法推断语言", p.path));
            continue;
        };
        let strictness = agent_ast::AstMatchStrictness::parse(p.strictness.as_deref());
        // 漂移复查：当前匹配数必须与暂存一致（内容未变），且字节数一致。
        let matches = match agent_ast::search(&text, lang, &p.pattern, strictness) {
            Ok(m) => m,
            Err(e) => {
                failures.push(format!("{}: pattern 重编译失败: {e}", p.path));
                continue;
            }
        };
        if matches.len() != p.count || text.len() != p.old_len {
            failures.push(format!(
                "{}: 文件已变化（暂存 {} 处 / 当前 {} 处），拒绝写入；请重新 ast_rewrite preview",
                p.path,
                p.count,
                matches.len()
            ));
            continue;
        }
        let new_text = match agent_ast::rewrite(&text, lang, &p.pattern, &p.replacement, strictness)
        {
            Ok(t) => t,
            Err(e) => {
                failures.push(format!("{}: 重写失败: {e}", p.path));
                continue;
            }
        };
        if new_text.len() != p.new_len {
            failures.push(format!(
                "{}: 重写结果与预览不一致（{} vs {} 字节），拒绝写入；请重新 preview",
                p.path,
                new_text.len(),
                p.new_len
            ));
            continue;
        }
        if let Err(e) = write_with_effects(&full, &new_text, ctx).await {
            failures.push(format!("{}: 写入失败: {e}", p.path));
            continue;
        }
        applied += 1;
    }
    // 成功应用的条目从队列移除（未应用的在失败提示中保留，模型可重试）。
    if applied > 0 {
        let mut q = queue.lock().expect("pending 队列锁");
        for p in &items {
            q.retain(|x| !(x.path == p.path && x.pattern == p.pattern));
        }
        drop(q);
    }
    let mut msg = format!("已应用 {applied} 条暂存重写");
    if !failures.is_empty() {
        msg.push_str("；失败：");
        msg.push_str(&failures.join("；"));
    }
    Ok(ToolResult::text(msg))
}

/// 解析批量指令：`1: @ours` / `#1 = @theirs` 行；非指令格式返回 `None`（全文模式）。
fn parse_directives(content: &str) -> Option<std::collections::HashMap<usize, String>> {
    let mut map = std::collections::HashMap::new();
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let (id_s, val) = if let Some((a, b)) = t.split_once(':') {
            (a.trim(), b.trim().to_string())
        } else {
            let rest = t.strip_prefix('#')?;
            let (a, b) = rest.split_once('=')?;
            (a.trim(), b.trim().to_string())
        };
        let id: usize = id_s.parse().ok()?;
        map.insert(id, val);
    }
    (!map.is_empty()).then_some(map)
}

/// `pr://` / `issue://` URI 解析：`<kind>://<owner>/<repo>[/<N>][?state=…]`。
/// 缺省 state=open、limit=10；裸 `pr://` / `issue://`（无仓库）返回可读提示。
async fn resolve_gh_uri(path: &str, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
    let (kind, rest) = if let Some(r) = path.strip_prefix("pr://") {
        ("pr", r)
    } else if let Some(r) = path.strip_prefix("issue://") {
        ("issue", r)
    } else {
        return Err(ToolError::InvalidArgs(format!("未知 GitHub URI: {path}")));
    };
    let (loc, query) = match rest.split_once('?') {
        Some((l, q)) => (l, q),
        None => (rest, "state=open&per_page=10"),
    };
    let parts: Vec<&str> = loc.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Ok(format!(
            "{kind}:// 需要仓库：{kind}://<owner>/<repo>[/<编号>]（如 {kind}://rust-lang/rust/12345）；\n也可经 github 工具查询。"
        ));
    }
    if parts.len() < 2 {
        return Err(ToolError::InvalidArgs(format!(
            "{kind}:// 需形式 `{kind}://<owner>/<repo>[/<编号>]`，收到: {path}"
        )));
    }
    let owner = parts[0];
    let repo = parts[1];
    let number = parts.get(2).and_then(|s| s.parse::<u64>().ok());
    if parts.len() > 3 || (parts.len() == 3 && number.is_none()) {
        return Err(ToolError::InvalidArgs(format!(
            "{kind}:// 路径段过多或编号非法: {path}"
        )));
    }
    // state 过滤（列表时）：`?state=open|closed|all`。
    let query = if query.is_empty() {
        "state=open&per_page=10".to_string()
    } else {
        format!("{query}&per_page={}", 10)
    };
    crate::github::render_gh_uri(kind, owner, repo, number, &query, ctx).await
}

/// `artifact://<id>` 路由：读取 shake 归档落盘内容（`<workspace>/.gyre/artifacts/<id>`）。
///
/// id 仅允许十六进制字符（由 shake sink 的内容哈希产生），杜绝路径穿越。
async fn resolve_artifact(id: &str, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
    let id = id.trim();
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ToolError::Execution(
            "artifact:// id 非法（仅允许十六进制字符）".into(),
        ));
    }
    let rel = format!(".gyre/artifacts/{id}");
    let full = ctx.workspace.resolve(Path::new(&rel));
    read_bounded(&full).await
}

/// `memory://` 路由：`` / `summary` → 启动摘要；`full` → 完整 MEMORY.md。
async fn resolve_memory(rest: &str, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
    let Some(memory) = ctx.memory else {
        return Err(ToolError::Execution(
            "memory:// 解析不可用（未启用跨会话记忆）".into(),
        ));
    };
    match rest.trim_end_matches('/') {
        "" | "summary" => memory
            .summary()
            .await
            .map_err(ToolError::Io)?
            .ok_or_else(|| ToolError::Execution("该项目暂无记忆摘要（memory://summary）".into())),
        "full" => memory
            .read_full()
            .await
            .map_err(ToolError::Io)?
            .ok_or_else(|| ToolError::Execution("该项目暂无完整记忆文档（memory://full）".into())),
        other => Err(ToolError::Execution(format!(
            "未知的 memory:// 子路径 `{other}`（可用：summary / full）"
        ))),
    }
}

/// `mcp://<server>/<uri>` 路由：经 MCP `resources/read` 读取。
async fn resolve_mcp(rest: &str, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
    let Some(resolver) = ctx.resources else {
        return Err(ToolError::Execution(
            "mcp:// 解析不可用（未注入 MCP）".into(),
        ));
    };
    let (server, uri) = rest
        .split_once('/')
        .ok_or_else(|| ToolError::Execution("mcp:// 需形式 `mcp://<server>/<uri>`".into()))?;
    if server.is_empty() || uri.is_empty() {
        return Err(ToolError::Execution(
            "mcp:// 的 server 与 uri 均不可为空".into(),
        ));
    }
    resolver
        .read_resource(server, uri)
        .await
        .map_err(|e| ToolError::Execution(format!("mcp://{server}/{uri}: {e}")))
}

/// HTTP 抓取超时（连接 + 整体）。
const HTTP_FETCH_TIMEOUT: Duration = Duration::from_secs(15);
/// HTTP 抓取响应体大小上限（流式截断，防 OOM）。
const HTTP_FETCH_MAX_BYTES: usize = 1024 * 1024; // 1 MiB
/// HTTP 抓取最大跟随重定向次数。
const HTTP_FETCH_MAX_REDIRECTS: usize = 3;

/// `http(s)://` 抓取：返回响应体文本（带超时、大小上限与基础 SSRF 防护）。
///
/// SSRF 防护对**首层 URL 与每一跳重定向目标**均执行校验（拦截字面量内网 IP 与已知元数据
/// 主机名），杜绝「公网 URL 302 → 内网/元数据地址」绕过。**不**防御 DNS rebinding
/// （主机名解析到内网）。生产环境如需更强保证，应在此之上叠加 DNS 解析钉扎。
pub async fn fetch_http(url: &str) -> Result<String, ToolError> {
    ssrf_guard(url)?;
    let client = fetch_client();

    let mut current =
        url::Url::parse(url).map_err(|e| ToolError::Execution(format!("非法 URL: {e}")))?;
    let mut resp = client
        .get(current.as_str())
        .send()
        .await
        .map_err(|e| ToolError::Execution(format!("fetch {url} 失败: {e}")))?;

    // 手动跟随重定向，每一跳重新校验目标 host（含 IP / 内网 / 元数据主机）。
    let mut redirects = 0usize;
    while resp.status().is_redirection() {
        if redirects >= HTTP_FETCH_MAX_REDIRECTS {
            return Err(ToolError::Execution(format!(
                "重定向次数超过 {HTTP_FETCH_MAX_REDIRECTS} 上限"
            )));
        }
        let Some(loc) = resp.headers().get(reqwest::header::LOCATION) else {
            break;
        };
        let loc_str = loc
            .to_str()
            .map_err(|e| ToolError::Execution(format!("非法 Location 头: {e}")))?;
        current = current
            .join(loc_str)
            .map_err(|e| ToolError::Execution(format!("解析重定向 URL 失败: {e}")))?;
        ssrf_guard(current.as_str())?;
        redirects += 1;
        resp = client
            .get(current.as_str())
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("fetch {current} 失败: {e}")))?;
    }

    if !resp.status().is_success() {
        return Err(ToolError::Execution(format!("HTTP {}", resp.status())));
    }
    // 流式读取并在超限时立即中止，避免大响应体整段入内存。
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| ToolError::Execution(format!("读取响应失败: {e}")))?;
        let Some(chunk) = chunk else { break };
        buf.extend_from_slice(&chunk);
        if buf.len() > HTTP_FETCH_MAX_BYTES {
            return Err(ToolError::Execution(format!(
                "响应体超过 {HTTP_FETCH_MAX_BYTES} 字节上限"
            )));
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// 基础 SSRF 防护：仅允许 http/https，拒绝回环/私有/链路本地 IP 字面量与已知元数据主机名。
pub fn ssrf_guard(raw: &str) -> Result<(), ToolError> {
    let url = url::Url::parse(raw).map_err(|e| ToolError::Execution(format!("非法 URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolError::Execution(format!(
            "不允许的 scheme: {}",
            url.scheme()
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| ToolError::Execution("URL 缺少主机".into()))?;
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if ip.is_loopback() || ip.is_unspecified() || is_private_or_link_local(&ip) {
            return Err(ToolError::Execution(format!(
                "SSRF 防护：禁止访问内网地址 {host}"
            )));
        }
    }
    let h = host.to_ascii_lowercase();
    const BLOCKED_HOSTS: &[&str] = &[
        "localhost",
        "metadata.google.internal",
        "metadata",
        "metadata.azure.com",
    ];
    if BLOCKED_HOSTS.iter().any(|b| h == *b) {
        return Err(ToolError::Execution(format!("SSRF 防护：禁止访问 {host}")));
    }
    Ok(())
}

/// 判定 IPv4/IPv6 是否为私有/链路本地（含 IPv4-mapped IPv6 降级检查）。
fn is_private_or_link_local(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            // CGNAT 100.64.0.0/10：阿里云元数据 100.100.100.200 等落此段，必须拦截。
            let is_cgnat = o[0] == 100 && (64..=127).contains(&o[1]);
            v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || is_cgnat
        }
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            // Unique-local fc00::/7、链路本地 fe80::/10
            let is_ula = (s[0] & 0xfe00) == 0xfc00;
            let is_ll = (s[0] & 0xffc0) == 0xfe80;
            if is_ula || is_ll {
                return true;
            }
            // IPv4-mapped IPv6 (::ffff:a.b.c.d)：降级为 IPv4 重新检查，
            // 堵截 http://[::ffff:169.254.169.254]/ 等映射绕过。
            if let Some(v4) = v6.to_ipv4_mapped() {
                let o = v4.octets();
                let is_cgnat = o[0] == 100 && (64..=127).contains(&o[1]);
                return v4.is_loopback()
                    || v4.is_unspecified()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_broadcast()
                    || v4.is_documentation()
                    || is_cgnat;
            }
            false
        }
    }
}

/// 抓取用共享 HTTP client（禁用自动重定向 + 超时），复用连接池，避免每次 `read_file http(s)://`
/// 都新建 client（重复 TLS 握手）。
fn fetch_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(HTTP_FETCH_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            // 身份 UA：crates.io / npm / GitHub 等 API 对无 UA 请求返回 403。
            .user_agent(concat!(
                "gyre-agent/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/Gyre)"
            ))
            // 禁用自动重定向：手动跟随并对每一跳的目标重新做 SSRF 校验。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("构造 HTTP client 失败")
    })
}
// ─────────────────────────────────────────────────────────────────────────────
// 二进制格式面（移植 omp read 的归档/SQLite/notebook 支持）
// ─────────────────────────────────────────────────────────────────────────────

/// `read_file` 单条目成员数上限（防巨型归档撑爆上下文）。
const MAX_ARCHIVE_ENTRIES: usize = 200;
/// 单成员/单表渲染字节上限。
const MAX_MEMBER_BYTES: usize = 256 * 1024;
/// `SQLite` 表浏览行数上限。
const MAX_SQL_ROWS: usize = 50;

/// 拆 `路径:selector`（首个 `:` 分隔；无冒号 → selector None）。
///
/// `:conflicts` 后缀已在 [`ReadFileTool::execute`] 前置分支消费，此处不再处理。
fn split_path_selector(path: &str) -> (&str, Option<&str>) {
    match path.find(':') {
        Some(i) => (&path[..i], Some(&path[i + 1..])),
        None => (path, None),
    }
}

/// 按扩展名分派二进制格式读取：归档（zip/tar 族）成员浏览与读取、SQLite 表浏览与
/// 只读查询、Jupyter notebook 渲染。其它扩展名返回 `Ok(None)` 走普通文本路径。
///
/// - `archive.zip` → 成员清单；`archive.zip:member/path` → 成员内容
/// - `db.sqlite` → 表清单；`db.sqlite:表名` → 建表语句 + 前 [`MAX_SQL_ROWS`] 行；
///   `db.sqlite::SELECT …` → 只读 SQL（仅 SELECT/PRAGMA/EXPLAIN，多语句拒绝）
/// - `note.ipynb` → 单元格渲染（markdown/code/输出摘要）
async fn read_binary_format(full: &Path, sel: Option<&str>) -> Result<Option<String>, ToolError> {
    let ext = full
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let is_gz = ext == "gz" || ext == "tgz" || ext == "bz2" || ext == "xz" || ext == "zst";
    // tar 族：tar / tar.gz / tgz（bz2/xz/zst v1 不支持，明确报错而非误读为文本）。
    let base_ext = if is_gz {
        full.with_extension("")
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default()
    } else {
        ext.clone()
    };

    let kind = match (base_ext.as_str(), ext.as_str()) {
        ("zip" | "jar" | "war" | "ear" | "apk" | "whl" | "ipa", _) => FormatKind::Zip,
        ("tar", "tar") => FormatKind::Tar,
        ("tar", "gz") => FormatKind::TarGz,
        (_, "tgz") => FormatKind::TarGz,
        ("tar", other) if is_gz => {
            return Err(ToolError::Execution(format!(
                "暂不支持 .tar.{other}（支持 .tar / .tar.gz）"
            )));
        }
        ("sqlite" | "sqlite3" | "db" | "db3", _) => FormatKind::Sqlite,
        ("ipynb", _) => FormatKind::Notebook,
        _ => return Ok(None),
    };

    // 阻塞解压/SQL 在专用线程执行，路径按值 move。
    let path_buf = full.to_path_buf();
    let sel_owned = sel.map(str::to_string);
    let out = tokio::task::spawn_blocking(move || match kind {
        FormatKind::Zip => read_zip(&path_buf, sel_owned.as_deref()),
        FormatKind::Tar => read_tar(&path_buf, sel_owned.as_deref(), false),
        FormatKind::TarGz => read_tar(&path_buf, sel_owned.as_deref(), true),
        FormatKind::Sqlite => read_sqlite(&path_buf, sel_owned.as_deref()),
        FormatKind::Notebook => read_notebook(&path_buf),
    })
    .await
    .map_err(|e| ToolError::Execution(format!("格式读取线程失败: {e}")))??;
    Ok(Some(out))
}

/// 二进制格式族。
#[derive(Debug, Clone, Copy)]
enum FormatKind {
    /// zip 家族（zip/jar/war/ear/apk/whl/ipa）。
    Zip,
    /// 裸 tar。
    Tar,
    /// gzip 压缩 tar（tar.gz/tgz）。
    TarGz,
    /// `SQLite` 数据库。
    Sqlite,
    /// Jupyter notebook。
    Notebook,
}

/// zip 家族：成员清单（无 selector）或单成员内容。
fn read_zip(path: &Path, sel: Option<&str>) -> Result<String, ToolError> {
    let file = std::fs::File::open(path).map_err(ToolError::Io)?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| ToolError::Execution(format!("zip 打开失败: {e}")))?;
    let Some(member) = sel else {
        let mut out = format!("# {}（zip，{} 个条目）\n", path.display(), zip.len());
        let mut shown = 0;
        for i in 0..zip.len() {
            let entry = zip
                .by_index_raw(i)
                .map_err(|e| ToolError::Execution(format!("zip 条目读取失败: {e}")))?;
            let name = entry.name().to_string();
            let size = entry.size();
            if shown < MAX_ARCHIVE_ENTRIES {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("- {name} ({size} B)\n"));
                shown += 1;
            }
        }
        if zip.len() > shown {
            let _ = std::fmt::Write::write_fmt(
                &mut out,
                format_args!("…（共 {} 条目，仅列前 {shown}）\n", zip.len()),
            );
        }
        out.push_str(&format!(
            "\n读成员：read_file path = `{}`（路径:成员）",
            path.display()
        ));
        return Ok(out);
    };
    let mut entry = zip
        .by_name(member)
        .map_err(|e| ToolError::Execution(format!("zip 成员 {member:?} 不存在: {e}")))?;
    let mut buf = Vec::with_capacity(entry.size().min(MAX_MEMBER_BYTES as u64) as usize);
    std::io::Read::read_to_end(&mut entry, &mut buf)
        .map_err(|e| ToolError::Execution(format!("zip 成员读取失败: {e}")))?;
    bounded_lossy(&buf, member)
}

/// tar / tar.gz：成员清单或单成员内容。
fn read_tar(path: &Path, sel: Option<&str>, gz: bool) -> Result<String, ToolError> {
    let file = std::fs::File::open(path).map_err(ToolError::Io)?;
    let reader: Box<dyn std::io::Read> = if gz {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = tar::Archive::new(reader);
    let mut entries = archive
        .entries()
        .map_err(|e| ToolError::Execution(format!("tar 打开失败: {e}")))?;
    let mut listing = Vec::new();
    while let Some(mut entry) = entries
        .next()
        .transpose()
        .map_err(|e| ToolError::Execution(format!("tar 条目读取失败: {e}")))?
    {
        let name = entry
            .path()
            .map_err(|e| ToolError::Execution(format!("tar 路径解码失败: {e}")))?
            .to_string_lossy()
            .into_owned();
        let size = entry.size();
        if let Some(member) = sel {
            if name == member {
                let mut buf = Vec::with_capacity(size.min(MAX_MEMBER_BYTES as u64) as usize);
                std::io::Read::read_to_end(&mut entry, &mut buf).map_err(ToolError::Io)?;
                return bounded_lossy(&buf, member);
            }
            continue;
        }
        if listing.len() < MAX_ARCHIVE_ENTRIES {
            listing.push(format!("- {name} ({size} B)"));
        }
    }
    if let Some(member) = sel {
        return Err(ToolError::Execution(format!("tar 成员 {member:?} 不存在")));
    }
    let kind = if gz { "tar.gz" } else { "tar" };
    let mut out = format!("# {}（{kind}，{} 个条目）\n", path.display(), listing.len());
    for l in &listing {
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{l}\n"));
    }
    out.push_str(&format!(
        "\n读成员：read_file path = `{}`（路径:成员）",
        path.display()
    ));
    Ok(out)
}

/// SQLite：表清单 / 单表浏览（建表语句 + 前 N 行）/ `::` 前缀只读 SQL。
fn read_sqlite(path: &Path, sel: Option<&str>) -> Result<String, ToolError> {
    use rusqlite::OpenFlags;
    let conn = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| ToolError::Execution(format!("SQLite 打开失败: {e}")))?;

    // 只读 SQL：`db.sqlite::SELECT …`（双冒号与「表名」区分）。
    // `db.sqlite::SELECT …`：首个 `:` 已被 split_path_selector 消耗，此处再剥一个。
    if let Some(sql) = sel.and_then(|s| s.strip_prefix(':')) {
        let trimmed = sql.trim().trim_end_matches(';');
        let head = trimmed
            .split_whitespace()
            .next()
            .map(str::to_ascii_uppercase)
            .unwrap_or_default();
        if !matches!(head.as_str(), "SELECT" | "PRAGMA" | "EXPLAIN" | "WITH") {
            return Err(ToolError::Execution(format!(
                "只读 SQL 仅接受 SELECT/PRAGMA/EXPLAIN/WITH（收到 {head:?}）"
            )));
        }
        if trimmed.contains(';') {
            return Err(ToolError::Execution("拒绝多语句 SQL".into()));
        }
        return query_rows(&conn, trimmed, MAX_SQL_ROWS);
    }

    let tables: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT name, sql FROM sqlite_master WHERE type='table' ORDER BY name")
            .map_err(|e| ToolError::Execution(format!("sqlite_master 读取失败: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            })
            .map_err(|e| ToolError::Execution(format!("表枚举失败: {e}")))?;
        rows.filter_map(Result::ok).collect()
    };

    let Some(table) = sel else {
        let mut out = format!("# {}（SQLite，{} 张表）\n", path.display(), tables.len());
        for (name, sql) in &tables {
            let first_line = sql.lines().next().unwrap_or("");
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("- {name} — {first_line}\n"));
        }
        out.push_str(&format!(
            "\n浏览表：read_file path = `{}:表名`；只读查询：`{}::SELECT …`",
            path.display(),
            path.display()
        ));
        return Ok(out);
    };
    if !tables.iter().any(|(n, _)| n == table) {
        return Err(ToolError::Execution(format!("表 {table:?} 不存在")));
    }
    let schema = tables
        .iter()
        .find(|(n, _)| n == table)
        .map(|(_, s)| s.clone())
        .unwrap_or_default();
    let rows = query_rows(&conn, &format!("SELECT * FROM \"{table}\""), MAX_SQL_ROWS)?;
    Ok(format!("-- {table} 建表语句\n{schema}\n\n{rows}"))
}

/// 执行只读查询并渲染为管道分隔表（行数上限 `limit`）。
fn query_rows(conn: &rusqlite::Connection, sql: &str, limit: usize) -> Result<String, ToolError> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| ToolError::Execution(format!("SQL 预备失败: {e}")))?;
    let cols: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let ncols = cols.len();
    let mut rows = stmt
        .query([])
        .map_err(|e| ToolError::Execution(format!("SQL 执行失败: {e}")))?;
    let mut out = format!("| {} |\n|{}|\n", cols.join(" | "), "-|".repeat(ncols));
    let mut n = 0;
    while let Some(row) = rows
        .next()
        .map_err(|e| ToolError::Execution(format!("行读取失败: {e}")))?
    {
        if n >= limit {
            let _ =
                std::fmt::Write::write_fmt(&mut out, format_args!("…（仅显示前 {limit} 行）\n"));
            break;
        }
        let mut cells = Vec::with_capacity(ncols);
        for i in 0..ncols {
            let v = row
                .get_ref(i)
                .map_err(|e| ToolError::Execution(e.to_string()))?;
            cells.push(match v {
                rusqlite::types::ValueRef::Null => "NULL".to_string(),
                rusqlite::types::ValueRef::Integer(i) => i.to_string(),
                rusqlite::types::ValueRef::Real(f) => f.to_string(),
                rusqlite::types::ValueRef::Text(t) => {
                    String::from_utf8_lossy(t).truncate_chars_sql()
                }
                rusqlite::types::ValueRef::Blob(b) => format!("<blob {}B>", b.len()),
            });
        }
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("| {} |\n", cells.join(" | ")));
        n += 1;
    }
    Ok(out)
}

/// `SQLite` 文本单元格截断（80 字符）。
trait SqlCellTruncate {
    fn truncate_chars_sql(self) -> String;
}
impl SqlCellTruncate for std::borrow::Cow<'_, str> {
    fn truncate_chars_sql(self) -> String {
        let mut s: String = self.chars().take(80).collect();
        s = s.replace('\n', "\\n").replace('\r', "");
        s
    }
}

/// Jupyter notebook 渲染：markdown/code 单元格与输出摘要。
fn read_notebook(path: &Path) -> Result<String, ToolError> {
    let raw = std::fs::read_to_string(path).map_err(ToolError::Io)?;
    let nb: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| ToolError::Execution(format!("notebook JSON 解析失败: {e}")))?;
    let empty = Vec::new();
    let cells = nb
        .get("cells")
        .and_then(serde_json::Value::as_array)
        .unwrap_or(&empty);
    let mut out = format!(
        "# {}（notebook，{} 单元格；cells 可编辑，其它字段只读）\n",
        path.display(),
        cells.len()
    );
    for (i, cell) in cells.iter().enumerate() {
        let ty = cell
            .get("cell_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let source = join_nb_source(cell.get("source"));
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!("\n[{i}] {ty}:\n{}\n", number_lines(&source)),
        );
        if ty == "code" {
            if let Some(outputs) = cell.get("outputs").and_then(serde_json::Value::as_array) {
                for o in outputs {
                    // 注意：键名 "text/plain" 自带斜杠，JSON Pointer 会误当层级分隔，
                    // 必须用 get 链而非 pointer。
                    let text = o
                        .get("data")
                        .and_then(|d| d.get("text/plain"))
                        .and_then(|v| {
                            v.as_str().map(str::to_string).or_else(|| {
                                v.as_array().map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str())
                                        .collect::<Vec<_>>()
                                        .join("")
                                })
                            })
                        })
                        .or_else(|| {
                            o.get("text").and_then(|v| {
                                v.as_str().map(str::to_string).or_else(|| {
                                    v.as_array().map(|a| {
                                        a.iter()
                                            .filter_map(|x| x.as_str())
                                            .collect::<Vec<_>>()
                                            .join("")
                                    })
                                })
                            })
                        })
                        .unwrap_or_default();
                    if !text.trim().is_empty() {
                        let t: String = text.chars().take(2000).collect();
                        let _ = std::fmt::Write::write_fmt(
                            &mut out,
                            format_args!("  out: {}\n", t.trim_end()),
                        );
                    }
                }
            }
        }
    }
    if out.len() > MAX_MEMBER_BYTES {
        out.truncate(MAX_MEMBER_BYTES);
        out.push_str("\n…（notebook 渲染超长已截断）");
    }
    Ok(out)
}

/// notebook source 字段（字符串或字符串数组两种形态）拼接。
fn join_nb_source(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// 简单行号前缀（notebook 内嵌渲染用）。
fn number_lines(text: &str) -> String {
    text.lines()
        .enumerate()
        .map(|(i, l)| format!("  {i}\t{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn bounded_lossy(buf: &[u8], label: &str) -> Result<String, ToolError> {
    if buf.len() > MAX_MEMBER_BYTES {
        let mut end = MAX_MEMBER_BYTES;
        while end > 0 && std::str::from_utf8(&buf[..end]).is_err() {
            end -= 1;
        }
        let mut s = String::from_utf8_lossy(&buf[..end]).into_owned();
        s.push_str(&format!(
            "\n…（{label} 超过 {MAX_MEMBER_BYTES} 字节已截断）"
        ));
        Ok(s)
    } else {
        Ok(String::from_utf8_lossy(buf).into_owned())
    }
}
/// 本地文件读取大小上限（超出截断并标注，避免大文件整段入内存）。
const MAX_READ_BYTES: usize = 2 * 1024 * 1024; // 2 MiB

/// 读取本地文件为文本，超大文件仅读取前 [`MAX_READ_BYTES`] 字节并追加截断标记。
async fn read_bounded(full: &Path) -> Result<String, ToolError> {
    let meta = tokio::fs::metadata(full).await.map_err(ToolError::Io)?;
    if meta.len() > MAX_READ_BYTES as u64 {
        use tokio::io::AsyncReadExt;
        let mut f = tokio::fs::File::open(full).await.map_err(ToolError::Io)?;
        let mut buf = vec![0u8; MAX_READ_BYTES];
        let mut filled = 0usize;
        // 循环填满缓冲（单次 read 可能短读，否则只读到部分字节）。
        while filled < buf.len() {
            match f.read(&mut buf[filled..]).await {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(ToolError::Io(e)),
            }
        }
        buf.truncate(filled);
        // 在合法 UTF-8 边界截断，避免多字节字符被切断后 from_utf8_lossy 产生 U+FFFD。
        let safe_end = utf8_safe_boundary(&buf);
        let mut text = String::from_utf8_lossy(&buf[..safe_end]).into_owned();
        text.push_str("\n...(文件过大，已截断)");
        return Ok(text);
    }
    let bytes = tokio::fs::read(full).await.map_err(ToolError::Io)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// 返回不超过 `bytes.len()` 的最大合法 UTF-8 边界（末尾为残缺多字节序列时回退）。
fn utf8_safe_boundary(bytes: &[u8]) -> usize {
    if std::str::from_utf8(bytes).is_ok() {
        return bytes.len();
    }
    let mut end = bytes.len();
    while end > 0 {
        if std::str::from_utf8(&bytes[..end]).is_ok() {
            break;
        }
        end -= 1;
    }
    end
}

/// 写入文件（覆盖，自动创建父目录）。
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }
    fn description(&self) -> &'static str {
        "写入文件（覆盖）。自动创建父目录。属于写入类操作，通常需审批。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "文件路径" },
                "content": { "type": "string", "description": "完整文件内容" }
            },
            "required": ["path", "content"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        CapabilityTier::Write
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let path = input
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `path` 参数".into()))?;
        let content = input
            .get("content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `content` 参数".into()))?;
        // 冲突解决：`conflict://<N>`（@tokens / 替换文本）与 `conflict://*`（批量）。
        if path.starts_with("conflict://") {
            return resolve_conflict(path, content, ctx).await;
        }
        // ast-rewrite 暂存应用/丢弃：`xd://resolve`（应用全部或 content 过滤）/
        // `xd://reject`（丢弃指定）。应用时重放重写并复查替换计数（防文件漂移）。
        if path == "xd://resolve" || path == "xd://reject" {
            return resolve_pending_rewrites(path, content, ctx).await;
        }
        let full = ctx.workspace.resolve(Path::new(path));
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(ToolError::Io)?;
        }
        let report = write_with_effects(&full, content, ctx).await?;
        let mut msg = format!("已写入 {path}（{} 字节）", content.len());
        msg.push_str(&report.effect_suffix());
        Ok(ToolResult::text(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConflictHistory;
    use crate::InMemorySnapshotStore;
    use crate::Tool;
    use agent_core::{ApprovalMode, ApprovalRequest, CapabilityTier, Workspace};

    fn dummy_ctx(ws: &Workspace) -> ToolContext<'_> {
        dummy_ctx_q(ws, None)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn dummy_ctx_h<'a>(
        ws: &'a Workspace,
        history: Option<&'a std::sync::Arc<std::sync::Mutex<ConflictHistory>>>,
    ) -> ToolContext<'a> {
        dummy_ctx_full(ws, history, None, None)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn dummy_ctx_q<'a>(
        ws: &'a Workspace,
        queue: Option<&'a std::sync::Arc<std::sync::Mutex<Vec<PendingRewrite>>>>,
    ) -> ToolContext<'a> {
        dummy_ctx_full(ws, None, queue, None)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn dummy_ctx_full<'a>(
        ws: &'a Workspace,
        history: Option<&'a std::sync::Arc<std::sync::Mutex<ConflictHistory>>>,
        queue: Option<&'a std::sync::Arc<std::sync::Mutex<Vec<PendingRewrite>>>>,
        snapshots: Option<&'a std::sync::Arc<std::sync::RwLock<crate::InMemorySnapshotStore>>>,
    ) -> ToolContext<'a> {
        use agent_core::ApprovalDecision;
        struct AutoApprove;
        #[async_trait::async_trait]
        impl agent_core::ApprovalPolicy for AutoApprove {
            fn decide(&self, _r: &ApprovalRequest<'_>) -> ApprovalDecision {
                ApprovalDecision::Allow
            }
            async fn prompt(
                &self,
                _a: &agent_core::AskMessage,
            ) -> Result<agent_core::AskResponse, ToolError> {
                Ok(agent_core::AskResponse::Yes)
            }
        }
        static CANCEL: std::sync::OnceLock<tokio_util::sync::CancellationToken> =
            std::sync::OnceLock::new();
        let cancel = CANCEL.get_or_init(tokio_util::sync::CancellationToken::new);
        // 故意用 AlwaysAsk 但 decide 永远 Allow，避免误判工具能力
        let _ = ApprovalMode::AlwaysAsk;
        ToolContext {
            workspace: ws,
            approval: &AutoApprove,
            cancel,
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
            update_tx: None,
            conflicts: history,
            pending_rewrites: queue,
            context: None,
            snapshots,
            tool_call_id: None,
        }
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let tmp = tempfile_dir();
        let ws = Workspace::new(&tmp);
        let ctx = dummy_ctx(&ws);
        let write = WriteFileTool;
        let input = serde_json::json!({ "path": "a.txt", "content": "hello\nworld" });
        write.execute(input, &ctx).await.unwrap();

        let read = ReadFileTool;
        let out = read
            .execute(serde_json::json!({ "path": "a.txt" }), &ctx)
            .await
            .unwrap();
        match out {
            ToolResult::Text(t) => assert!(t.contains("hello") && t.contains("world")),
            _ => panic!("应为文本结果"),
        }
    }

    #[test]
    fn ssrf_blocks_cgnat_and_metadata() {
        // 回归：CGNAT 100.64.0.0/10（含阿里云元数据 100.100.100.200）必须被拦截。
        assert!(is_private_or_link_local(
            &"100.100.100.200".parse::<std::net::IpAddr>().unwrap()
        ));
        assert!(is_private_or_link_local(
            &"100.64.0.1".parse::<std::net::IpAddr>().unwrap()
        ));
        assert!(!is_private_or_link_local(
            &"8.8.8.8".parse::<std::net::IpAddr>().unwrap()
        ));
    }

    #[test]
    fn ssrf_blocks_ipv4_mapped_ipv6() {
        // 回归：IPv4-mapped IPv6 地址必须降级为 IPv4 检查。
        // ::ffff:169.254.169.254（AWS/GCP 元数据）必须被拦截。
        assert!(is_private_or_link_local(
            &"::ffff:169.254.169.254"
                .parse::<std::net::IpAddr>()
                .unwrap()
        ));
        // ::ffff:127.0.0.1（回环）必须被拦截。
        assert!(is_private_or_link_local(
            &"::ffff:127.0.0.1".parse::<std::net::IpAddr>().unwrap()
        ));
        // ::ffff:10.0.0.1（私有）必须被拦截。
        assert!(is_private_or_link_local(
            &"::ffff:10.0.0.1".parse::<std::net::IpAddr>().unwrap()
        ));
        // ::ffff:100.100.100.200（CGNAT）必须被拦截。
        assert!(is_private_or_link_local(
            &"::ffff:100.100.100.200"
                .parse::<std::net::IpAddr>()
                .unwrap()
        ));
        // 公网 IPv4-mapped 不应被拦截。
        assert!(!is_private_or_link_local(
            &"::ffff:8.8.8.8".parse::<std::net::IpAddr>().unwrap()
        ));
    }

    #[test]
    fn utf8_safe_boundary_keeps_valid_prefix() {
        // 「你」= E4 BD A0；切掉末字节后应在合法 UTF-8 边界截断，保留 "a"。
        let full = "a你".as_bytes();
        let safe = utf8_safe_boundary(&full[..full.len() - 1]);
        let s = std::str::from_utf8(&full[..safe]).unwrap();
        assert_eq!(s, "a");
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-test-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn uuid_v4() -> String {
        // 轻量伪随机，仅用于测试目录名，避免引入 uuid 依赖到测试
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{nanos:x}")
    }

    // 静默未使用告警
    #[allow(dead_code)]
    fn _force_capability_use() -> CapabilityTier {
        CapabilityTier::ReadOnly
    }
    // ── conflict:// 合并冲突解决（扫描 → 注册 → splice）────────────────────────

    const CONFLICT_FILE: &str = "cf.md";

    const CONFLICT_3WAY: &str = "line1\n<<<<<<< HEAD\nours-a\nours-b\n||||||| base\nbase-c\n=======\ntheirs-d\n>>>>>>> branch\nline9\n";

    #[tokio::test]
    async fn conflict_scan_then_resolve_single() {
        let dir = std::env::temp_dir().join(format!("agent-cf-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(CONFLICT_FILE);
        std::fs::write(&file, CONFLICT_3WAY).unwrap();
        let ws = Workspace::new(&dir);
        let history = std::sync::Arc::new(std::sync::Mutex::new(ConflictHistory::new()));
        let ctx = dummy_ctx_h(&ws, Some(&history));

        let res = ReadFileTool
            .execute(serde_json::json!({ "path": "cf.md:conflicts" }), &ctx)
            .await
            .unwrap();
        let msg = match res {
            ToolResult::Text(t) => t,
            _ => panic!("应为文本"),
        };
        assert!(
            msg.contains("1 处合并冲突") && msg.contains("L2-L9"),
            "{msg}"
        );
        assert_eq!(history.lock().unwrap().len(), 1);

        let res = ReadFileTool
            .execute(serde_json::json!({ "path": "conflict://1/ours" }), &ctx)
            .await
            .unwrap();
        assert!(matches!(res, ToolResult::Text(t) if t.contains("ours-a")));

        let res = WriteFileTool
            .execute(
                serde_json::json!({ "path": "conflict://1", "content": "@ours" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(res, ToolResult::Text(t) if t.contains("已解决冲突 #1")));
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert_eq!(on_disk, "line1\nours-a\nours-b\nline9\n");
        assert_eq!(history.lock().unwrap().len(), 0, "解决后注册表应清空");

        let err = WriteFileTool
            .execute(
                serde_json::json!({ "path": "conflict://1", "content": "@theirs" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("未注册"), "{err}");
    }

    #[tokio::test]
    async fn conflict_wildcard_batch_with_directives() {
        let dir = std::env::temp_dir().join(format!("agent-cf-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(CONFLICT_FILE);
        std::fs::write(
            &file,
            "a\n<<<<<<< x\n1\n=======\n2\n>>>>>>> y\nc\n<<<<<<< x\n3\n=======\n4\n>>>>>>> y\nz\n",
        )
        .unwrap();
        let ws = Workspace::new(&dir);
        let history = std::sync::Arc::new(std::sync::Mutex::new(ConflictHistory::new()));
        let ctx = dummy_ctx_h(&ws, Some(&history));

        ReadFileTool
            .execute(serde_json::json!({ "path": "cf.md:conflicts" }), &ctx)
            .await
            .unwrap();
        let res = WriteFileTool
            .execute(
                serde_json::json!({
                    "path": "conflict://*",
                    "content": "1: @ours\n2: @theirs"
                }),
                &ctx,
            )
            .await
            .unwrap();
        let msg = match res {
            ToolResult::Text(t) => t,
            _ => panic!("应为文本"),
        };
        assert!(msg.contains("已解决 2 处"), "批量应解决 2 处: {msg}");
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert_eq!(on_disk, "a\n1\nc\n4\nz\n");
    }

    #[tokio::test]
    async fn conflict_rejects_drifted_file() {
        let dir = std::env::temp_dir().join(format!("agent-cf-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(CONFLICT_FILE);
        std::fs::write(&file, CONFLICT_3WAY).unwrap();
        let ws = Workspace::new(&dir);
        let history = std::sync::Arc::new(std::sync::Mutex::new(ConflictHistory::new()));
        let ctx = dummy_ctx_h(&ws, Some(&history));

        ReadFileTool
            .execute(serde_json::json!({ "path": "cf.md:conflicts" }), &ctx)
            .await
            .unwrap();
        std::fs::write(&file, "line1\n<<<<<<< HEAD\nCHANGED\n||||||| base\nbase-c\n=======\ntheirs-d\n>>>>>>> branch\nline9\n").unwrap();
        let err = WriteFileTool
            .execute(
                serde_json::json!({ "path": "conflict://1", "content": "@ours" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("标记行不匹配"), "{err}");
    }

    #[tokio::test]
    async fn conflict_requires_registered_history() {
        let dir = std::env::temp_dir().join(format!("agent-cf-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CONFLICT_FILE), CONFLICT_3WAY).unwrap();
        let ws = Workspace::new(&dir);
        let ctx = dummy_ctx(&ws);

        let err = WriteFileTool
            .execute(
                serde_json::json!({ "path": "conflict://1", "content": "@ours" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("未启用"), "{err}");
    }

    #[tokio::test]
    async fn conflict_wildcard_read_rejected() {
        let dir = std::env::temp_dir().join(format!("agent-cf-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CONFLICT_FILE), CONFLICT_3WAY).unwrap();
        let ws = Workspace::new(&dir);
        let history = std::sync::Arc::new(std::sync::Mutex::new(ConflictHistory::new()));
        let ctx = dummy_ctx_h(&ws, Some(&history));

        let err = ReadFileTool
            .execute(serde_json::json!({ "path": "conflict://*" }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("仅支持写入"), "{err}");
    }

    // ── resolve 暂存（ast_rewrite preview → xd://resolve/reject）────────────────

    const REWRITE_FILE: &str = "rw.rs";

    async fn preview_rewrite(ctx: &ToolContext<'_>) -> String {
        let res = crate::AstRewriteTool
            .execute(
                serde_json::json!({
                    "path": REWRITE_FILE,
                    "pattern": "fn $A() {}",
                    "rewrite": "fn $A() { /* rewritten */ }",
                    "preview": true
                }),
                ctx,
            )
            .await
            .expect("preview 应成功");
        match res {
            ToolResult::Text(t) => t,
            _ => panic!("应为文本"),
        }
    }

    #[tokio::test]
    async fn resolve_staged_then_apply() {
        let dir = std::env::temp_dir().join(format!("agent-rw-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(REWRITE_FILE);
        std::fs::write(&file, "fn a() {}\nfn b() {}\n").unwrap();
        let ws = Workspace::new(&dir);
        let queue = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ctx = dummy_ctx_q(&ws, Some(&queue));

        let msg = preview_rewrite(&ctx).await;
        assert!(msg.contains("(proposed) 2 replacements"), "{msg}");
        // 未落盘。
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert!(on_disk.contains("fn a() {}"));
        assert!(!on_disk.contains("rewritten"));

        // 列出暂存。
        let res = ReadFileTool
            .execute(serde_json::json!({ "path": "xd://pending" }), &ctx)
            .await
            .unwrap();
        let listing = match res {
            ToolResult::Text(t) => t,
            _ => panic!("应为文本"),
        };
        assert!(
            listing.contains("rw.rs") && listing.contains("2 replacements"),
            "{listing}"
        );

        // 应用。
        let res = WriteFileTool
            .execute(
                serde_json::json!({ "path": "xd://resolve", "content": "" }),
                &ctx,
            )
            .await
            .unwrap();
        let applied = match res {
            ToolResult::Text(t) => t,
            _ => panic!("应为文本"),
        };
        assert!(applied.contains("已应用 1 条"), "{applied}");
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert!(on_disk.contains("fn a() { /* rewritten */ }"));
        assert!(on_disk.contains("fn b() { /* rewritten */ }"));
        assert!(queue.lock().unwrap().is_empty(), "应用后队列应清空");
        // 再应用 → 空队列提示。
        let res = WriteFileTool
            .execute(
                serde_json::json!({ "path": "xd://resolve", "content": "" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(res, ToolResult::Text(t) if t.contains("暂存队列为空")));
    }

    #[tokio::test]
    async fn resolve_rejects_drifted_file() {
        let dir = std::env::temp_dir().join(format!("agent-rw-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(REWRITE_FILE);
        std::fs::write(&file, "fn a() {}\n").unwrap();
        let ws = Workspace::new(&dir);
        let queue = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ctx = dummy_ctx_q(&ws, Some(&queue));

        preview_rewrite(&ctx).await;
        // 文件在预览后被外部修改（字节数变化）→ 应用必须拒绝。
        std::fs::write(&file, "fn a() {}\n// external edit\n").unwrap();
        let res = WriteFileTool
            .execute(
                serde_json::json!({ "path": "xd://resolve", "content": "" }),
                &ctx,
            )
            .await
            .unwrap();
        let msg = match res {
            ToolResult::Text(t) => t,
            _ => panic!("应为文本"),
        };
        assert!(
            msg.contains("文件已变化") && msg.contains("拒绝写入"),
            "{msg}"
        );
        assert!(!queue.lock().unwrap().is_empty(), "失败条目保留可重试");
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("external edit"),
            "文件不应被改写"
        );
    }

    #[tokio::test]
    async fn resolve_reject_discards_staged() {
        let dir = std::env::temp_dir().join(format!("agent-rw-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(REWRITE_FILE);
        std::fs::write(&file, "fn a() {}\n").unwrap();
        let ws = Workspace::new(&dir);
        let queue = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ctx = dummy_ctx_q(&ws, Some(&queue));

        preview_rewrite(&ctx).await;
        let res = WriteFileTool
            .execute(
                serde_json::json!({ "path": "xd://reject", "content": "" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(res, ToolResult::Text(t) if t.contains("已丢弃 1 条")));
        assert!(queue.lock().unwrap().is_empty());
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert!(!on_disk.contains("rewritten"), "丢弃后文件不应变化");
    }

    #[tokio::test]
    async fn resolve_repreview_replaces_staged() {
        let dir = std::env::temp_dir().join(format!("agent-rw-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(REWRITE_FILE);
        std::fs::write(&file, "fn a() {}\nfn b() {}\n").unwrap();
        let ws = Workspace::new(&dir);
        let queue = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ctx = dummy_ctx_q(&ws, Some(&queue));

        preview_rewrite(&ctx).await;
        // 同一文件再次 preview（不同 pattern）→ 只保留最新。
        let res = crate::AstRewriteTool
            .execute(
                serde_json::json!({
                    "path": REWRITE_FILE,
                    "pattern": "fn b() {}",
                    "rewrite": "fn b() { /* only-b */ }",
                    "preview": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(res, ToolResult::Text(t) if t.contains("(proposed) 1 replacements")));
        assert_eq!(queue.lock().unwrap().len(), 1, "同文件应只留最新一条");
        // 精确匹配过滤应用：只有 b 被重写。
        let res = WriteFileTool
            .execute(
                serde_json::json!({ "path": "xd://resolve", "content": format!("{REWRITE_FILE}:fn b() {{}}") }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(res, ToolResult::Text(t) if t.contains("已应用 1 条")));
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert!(on_disk.contains("only-b"));
        assert!(on_disk.contains("fn a() {}"), "a 不应被改写");
    }

    // ── pr:// / issue:// GitHub 路由（解析与错误路径）────

    #[tokio::test]
    async fn gh_uri_rejects_malformed() {
        let dir = std::env::temp_dir().join(format!("agent-gh-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::new(&dir);
        let ctx = dummy_ctx(&ws);

        let err = ReadFileTool
            .execute(serde_json::json!({ "path": "pr://rust-lang" }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("owner"), "{err}");
        let err = ReadFileTool
            .execute(
                serde_json::json!({ "path": "pr://rust-lang/rust/abc" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("编号"), "{err}");
        let res = ReadFileTool
            .execute(serde_json::json!({ "path": "pr://" }), &ctx)
            .await
            .unwrap();
        assert!(matches!(res, ToolResult::Text(t) if t.contains("需要仓库")));
    }

    #[tokio::test]
    async fn gh_uri_list_cache_roundtrip() {
        let dir = std::env::temp_dir().join(format!("agent-gh-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::new(&dir);
        let ctx = dummy_ctx(&ws);

        let cache_dir = dir.join(".gyre/cache/github");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let body = r#"[{"number": 42, "title": "fix cache", "state": "open", "user": {"login": "tester"}}]"#;
        std::fs::write(
            cache_dir.join("anon__octo__repo__pulls_state_open_per_page_10.json"),
            body,
        )
        .unwrap();

        let res = ReadFileTool
            .execute(
                serde_json::json!({ "path": "pr://octo/repo?state=open" }),
                &ctx,
            )
            .await
            .unwrap();
        let text = match res {
            ToolResult::Text(t) => t,
            _ => panic!("应为文本"),
        };
        assert!(text.contains("#42") && text.contains("fix cache"), "{text}");
    }

    // ── 二进制格式面（归档/SQLite/notebook）───────────────────────────────────

    /// 构造含两个文件的 zip。
    fn make_zip(dir: &std::path::Path) -> std::path::PathBuf {
        use std::io::Write;
        let path = dir.join("bundle.zip");
        let file = std::fs::File::create(&path).unwrap();
        let mut w = zip::ZipWriter::new(file);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
        w.start_file("a.txt", opts).unwrap();
        w.write_all(b"alpha content").unwrap();
        w.start_file("sub/b.txt", opts).unwrap();
        w.write_all(b"beta").unwrap();
        w.finish().unwrap();
        path
    }

    #[tokio::test]
    async fn zip_listing_and_member_read() {
        let dir = tempfile::tempdir().unwrap();
        let _zip_path = make_zip(dir.path());
        let ws = Workspace::new(dir.path().to_path_buf());
        let ctx = dummy_ctx(&ws);
        // 清单。
        let out = ReadFileTool
            .execute(serde_json::json!({"path": "bundle.zip"}), &ctx)
            .await
            .unwrap();
        let text = out.to_llm_text();
        assert!(text.contains("2 个条目"), "{text}");
        assert!(
            text.contains("a.txt") && text.contains("sub/b.txt"),
            "{text}"
        );
        // 成员读取。
        let out = ReadFileTool
            .execute(serde_json::json!({"path": "bundle.zip:a.txt"}), &ctx)
            .await
            .unwrap();
        assert!(out.to_llm_text().contains("alpha content"));
        // 不存在的成员。
        let err = ReadFileTool
            .execute(serde_json::json!({"path": "bundle.zip:nope.txt"}), &ctx)
            .await;
        assert!(err.is_err());
    }

    /// 构造 tar（可选 gzip）。
    fn make_tar(dir: &std::path::Path, gz: bool) -> std::path::PathBuf {
        let path = dir.join(if gz { "b.tgz" } else { "b.tar" });
        let file = std::fs::File::create(&path).unwrap();
        let mut builder = tar::Builder::new(file);
        let mut h = tar::Header::new_gnu();
        h.set_size(6);
        h.set_mode(0o644);
        h.set_cksum();
        builder
            .append_data(&mut h, "x.txt", std::io::Cursor::new(b"tar-ok".to_vec()))
            .unwrap();
        builder.finish().unwrap();
        if gz {
            let raw = std::fs::read(&path).unwrap();
            let mut enc = flate2::write::GzEncoder::new(
                std::fs::File::create(&path).unwrap(),
                flate2::Compression::default(),
            );
            std::io::Write::write_all(&mut enc, &raw).unwrap();
            enc.finish().unwrap();
        }
        path
    }

    #[tokio::test]
    async fn tar_and_tgz_member_read() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path().to_path_buf());
        let ctx = dummy_ctx(&ws);
        for (_path, expect) in [
            (make_tar(dir.path(), false), "b.tar"),
            (make_tar(dir.path(), true), "b.tgz"),
        ] {
            let out = ReadFileTool
                .execute(serde_json::json!({"path": format!("{expect}:x.txt")}), &ctx)
                .await
                .unwrap();
            assert!(
                out.to_llm_text().contains("tar-ok"),
                "{expect}: {}",
                out.to_llm_text()
            );
        }
        // 清单。
        let out = ReadFileTool
            .execute(serde_json::json!({"path": "b.tar"}), &ctx)
            .await
            .unwrap();
        assert!(out.to_llm_text().contains("x.txt"));
    }

    fn make_db(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("data.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO users(name) VALUES ('alice'), ('bob');",
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn sqlite_tables_rows_and_readonly_query() {
        let dir = tempfile::tempdir().unwrap();
        let _db = make_db(dir.path());
        let ws = Workspace::new(dir.path().to_path_buf());
        let ctx = dummy_ctx(&ws);
        // 表清单（相对工作区路径；绝对路径会被沙箱词法钳制）。
        let out = ReadFileTool
            .execute(serde_json::json!({"path": "data.db"}), &ctx)
            .await
            .unwrap();
        assert!(out.to_llm_text().contains("users"), "{}", out.to_llm_text());
        // 表浏览。
        let out = ReadFileTool
            .execute(serde_json::json!({"path": "data.db:users"}), &ctx)
            .await
            .unwrap();
        let text = out.to_llm_text();
        assert!(
            text.contains("CREATE TABLE") && text.contains("alice"),
            "{text}"
        );
        // 只读查询。
        let out = ReadFileTool
            .execute(
                serde_json::json!({"path": "data.db::SELECT COUNT(*) AS n FROM users"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.to_llm_text().contains("| 2 |"), "{}", out.to_llm_text());
        // 写语句拒绝。
        let err = ReadFileTool
            .execute(
                serde_json::json!({"path": "data.db::INSERT INTO users(name) VALUES('x')"}),
                &ctx,
            )
            .await;
        assert!(err.is_err());
        // 多语句拒绝。
        let err = ReadFileTool
            .execute(
                serde_json::json!({"path": "data.db::SELECT 1; SELECT 2"}),
                &ctx,
            )
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn ipynb_renders_cells_and_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nb.ipynb");
        std::fs::write(
            &path,
            serde_json::json!({
                "cells": [
                    {"cell_type": "markdown", "source": ["# Title\n", "desc"]},
                    {"cell_type": "code", "source": "print('hi')",
                     "outputs": [{"data": {"text/plain": ["hi\n"]}, "output_type": "stream"}]}
                ],
                "metadata": {}, "nbformat": 4
            })
            .to_string(),
        )
        .unwrap();
        let ws = Workspace::new(dir.path().to_path_buf());
        let ctx = dummy_ctx(&ws);
        let out = ReadFileTool
            .execute(serde_json::json!({"path": "nb.ipynb"}), &ctx)
            .await
            .unwrap();
        let text = out.to_llm_text();
        assert!(text.contains("2 单元格"), "{text}");
        assert!(
            text.contains("[0] markdown") && text.contains("# Title"),
            "{text}"
        );
        assert!(
            text.contains("[1] code") && text.contains("print('hi')"),
            "{text}"
        );
        assert!(text.contains("out: hi"), "{text}");
    }

    // ── 行选择器（解析 / 渲染 / raw / 段头 / 快照）──────────────────────────────

    /// 构造单个区间（测试辅助）。
    fn lr(start: usize, end: Option<usize>) -> LineRange {
        LineRange { start, end }
    }

    /// 构造区间列表（测试辅助）。
    fn lrs(v: &[(usize, Option<usize>)]) -> Vec<LineRange> {
        v.iter().map(|&(s, e)| lr(s, e)).collect()
    }

    #[test]
    fn selector_none_and_raw_forms() {
        assert_eq!(parse_line_selector(None).unwrap(), LineSelector::None);
        assert_eq!(parse_line_selector(Some("")).unwrap(), LineSelector::None);
        assert_eq!(
            parse_line_selector(Some("raw")).unwrap(),
            LineSelector::Raw { ranges: vec![] }
        );
        // 大小写不敏感
        assert_eq!(
            parse_line_selector(Some("RAW")).unwrap(),
            LineSelector::Raw { ranges: vec![] }
        );
    }

    #[test]
    fn selector_range_forms() {
        // 裸 N 与 N- 同义：从 N 到文件尾（omp 语义）
        assert_eq!(
            parse_line_selector(Some("50")).unwrap(),
            LineSelector::Ranges(lrs(&[(50, None)]))
        );
        assert_eq!(
            parse_line_selector(Some("50-")).unwrap(),
            LineSelector::Ranges(lrs(&[(50, None)]))
        );
        assert_eq!(
            parse_line_selector(Some("5-10")).unwrap(),
            LineSelector::Ranges(lrs(&[(5, Some(10))]))
        );
        assert_eq!(
            parse_line_selector(Some("20+5")).unwrap(),
            LineSelector::Ranges(lrs(&[(20, Some(24))]))
        );
        // `..` 别名与 `L` 前缀宽容
        assert_eq!(
            parse_line_selector(Some("3..5")).unwrap(),
            LineSelector::Ranges(lrs(&[(3, Some(5))]))
        );
        assert_eq!(
            parse_line_selector(Some("3..")).unwrap(),
            LineSelector::Ranges(lrs(&[(3, None)]))
        );
        assert_eq!(
            parse_line_selector(Some("L5-6")).unwrap(),
            LineSelector::Ranges(lrs(&[(5, Some(6))]))
        );
    }

    #[test]
    fn selector_multi_range_sorted_and_merged() {
        // 乱序输入 → 升序
        assert_eq!(
            parse_line_selector(Some("960-973,5-16")).unwrap(),
            LineSelector::Ranges(lrs(&[(5, Some(16)), (960, Some(973))]))
        );
        // 重叠合并
        assert_eq!(
            parse_line_selector(Some("10-20,5-12")).unwrap(),
            LineSelector::Ranges(lrs(&[(5, Some(20))]))
        );
        // 相邻（end+1）合并
        assert_eq!(
            parse_line_selector(Some("1-5,6-9")).unwrap(),
            LineSelector::Ranges(lrs(&[(1, Some(9))]))
        );
        // 开区间吞并其后全部（排序后开区间居首）
        assert_eq!(
            parse_line_selector(Some("1-,3-4,7-")).unwrap(),
            LineSelector::Ranges(lrs(&[(1, None)]))
        );
        // 有间隙的保留分立
        assert_eq!(
            parse_line_selector(Some("5-16,960-973")).unwrap(),
            LineSelector::Ranges(lrs(&[(5, Some(16)), (960, Some(973))]))
        );
    }

    #[test]
    fn selector_raw_compound_both_orders() {
        assert_eq!(
            parse_line_selector(Some("raw:50-100")).unwrap(),
            LineSelector::Raw {
                ranges: lrs(&[(50, Some(100))])
            }
        );
        assert_eq!(
            parse_line_selector(Some("50-100:raw")).unwrap(),
            LineSelector::Raw {
                ranges: lrs(&[(50, Some(100))])
            }
        );
        // raw + 多区间
        assert_eq!(
            parse_line_selector(Some("raw:5-10,20-30")).unwrap(),
            LineSelector::Raw {
                ranges: lrs(&[(5, Some(10)), (20, Some(30))])
            }
        );
    }

    #[test]
    fn selector_invalid_forms_error_with_syntax_list() {
        for bad in [
            "abc",
            "-5",
            "1-5,",
            ",1-5",
            "raw:raw",
            "raw:x",
            "1:2:3",
            "img",
            "99999999999999999999999",
        ] {
            let err = parse_line_selector(Some(bad)).unwrap_err().to_string();
            assert!(err.contains("合法形式"), "{bad} → {err}");
        }
    }

    #[test]
    fn selector_bound_errors_are_specific() {
        // 行号 0
        let err = parse_line_selector(Some("0")).unwrap_err().to_string();
        assert!(err.contains("1 基"), "{err}");
        // end < start
        let err = parse_line_selector(Some("5-2")).unwrap_err().to_string();
        assert!(err.contains("end 必须 >= start"), "{err}");
        // K < 1
        let err = parse_line_selector(Some("5+0")).unwrap_err().to_string();
        assert!(err.contains("K 必须 >= 1"), "{err}");
    }

    #[test]
    fn render_ranges_absolute_line_numbers() {
        let text = "a\nb\nc\nd\ne\n";
        assert_eq!(
            render_ranges(text, &[lr(2, Some(4))]),
            "    2\tb\n    3\tc\n    4\td\n"
        );
        // 多区间：区间间空行
        assert_eq!(
            render_ranges(text, &[lr(1, Some(2)), lr(4, Some(5))]),
            "    1\ta\n    2\tb\n\n    4\td\n    5\te\n"
        );
        // 开区间到文件尾
        assert_eq!(render_ranges(text, &[lr(4, None)]), "    4\td\n    5\te\n");
        // 端点越界收敛到文件尾
        assert_eq!(
            render_ranges(text, &[lr(4, Some(99))]),
            "    4\td\n    5\te\n"
        );
        // 整段落在文件外 → 范围提示
        let out = render_ranges(text, &[lr(9, Some(10))]);
        assert!(out.contains("超出文件范围"), "{out}");
    }

    #[test]
    fn render_raw_slice_verbatim_without_numbers() {
        let text = "a\nb\nc\n";
        // 无区间：整篇原样
        assert_eq!(render_raw_slice(text, &[]), text);
        // 切片：无行号、保留原文
        assert_eq!(render_raw_slice(text, &[lr(2, Some(3))]), "b\nc\n");
        // 多区间空行分隔
        assert_eq!(
            render_raw_slice(text, &[lr(1, Some(1)), lr(3, None)]),
            "a\n\nc\n"
        );
    }

    #[test]
    fn elide_middle_keeps_head_and_tail() {
        let small = String::from("short");
        assert_eq!(elide_middle(small.clone(), 100), small);
        let lines: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
        let out = elide_middle(lines, 2_000);
        assert!(out.contains("已省略中间输出"), "{out}");
        assert!(out.contains("line 1\n"), "头部保留: {out}");
        assert!(out.contains("line 1000\n"), "尾部保留: {out}");
        assert!(out.len() < 8_900, "折叠后应显著短于原文: {}", out.len());
    }

    #[test]
    fn protocol_paths_routed_away_from_local_pipeline() {
        for p in [
            "skill://x",
            "memory://summary",
            "mcp://s/u",
            "artifact://ab12",
            "pr://o/r",
            "issue://o/r/1",
            "conflict://1",
            "xd://pending",
            "http://e.com",
            "https://e.com/a",
        ] {
            assert!(is_protocol_path(p), "{p}");
        }
        assert!(!is_protocol_path("src/a.rs"));
        assert!(!is_protocol_path("local://src/a.rs"));
        assert!(!is_protocol_path("a.rs:1-5"));
    }

    #[tokio::test]
    async fn read_file_plain_emits_hashline_header_and_snapshot() {
        let tmp = tempfile_dir();
        let ws = Workspace::new(&tmp);
        let body = "fn a() {}\nfn b() {}\n";
        std::fs::write(tmp.join("a.rs"), body).unwrap();
        let store = std::sync::Arc::new(std::sync::RwLock::new(InMemorySnapshotStore::new()));
        let ctx = dummy_ctx_full(&ws, None, None, Some(&store));

        let out = ReadFileTool
            .execute(serde_json::json!({ "path": "a.rs" }), &ctx)
            .await
            .unwrap();
        let text = out.to_llm_text();
        let hash = compute_file_hash(body);
        // 段头为首行：[相对路径#HASH]
        assert!(
            text.starts_with(&format!("[a.rs#{hash}]\n")),
            "实际输出: {text}"
        );
        // 全文快照已记录（含正文与指纹）
        {
            let s = store.read().expect("snapshot 锁");
            assert!(s.recognizes("a.rs", &hash));
            assert_eq!(s.head("a.rs").unwrap().text, body);
        }
        // 再次读取：hash 稳定（同内容同指纹）
        let again = ReadFileTool
            .execute(serde_json::json!({ "path": "a.rs" }), &ctx)
            .await
            .unwrap()
            .to_llm_text();
        assert!(again.starts_with(&format!("[a.rs#{hash}]\n")));
    }

    #[tokio::test]
    async fn read_file_selector_slice_records_full_text_snapshot() {
        let tmp = tempfile_dir();
        let ws = Workspace::new(&tmp);
        let body = "l1\nl2\nl3\nl4\n";
        std::fs::write(tmp.join("m.txt"), body).unwrap();
        let store = std::sync::Arc::new(std::sync::RwLock::new(InMemorySnapshotStore::new()));
        let ctx = dummy_ctx_full(&ws, None, None, Some(&store));

        let out = ReadFileTool
            .execute(serde_json::json!({ "path": "m.txt:2-3" }), &ctx)
            .await
            .unwrap();
        let text = out.to_llm_text();
        // 段头 + 1 基绝对行号切片
        let hash = compute_file_hash(body);
        assert!(text.starts_with(&format!("[m.txt#{hash}]\n")), "{text}");
        assert!(text.contains("    2\tl2\n"), "{text}");
        assert!(text.contains("    3\tl3\n"), "{text}");
        assert!(!text.contains("    1\tl1"), "{text}");
        // 快照记录的是全文而非选中切片
        let s = store.read().expect("snapshot 锁");
        assert_eq!(s.head("m.txt").unwrap().text, body);
    }

    #[tokio::test]
    async fn read_file_raw_has_no_header_and_no_snapshot() {
        let tmp = tempfile_dir();
        let ws = Workspace::new(&tmp);
        let body = "l1\nl2\nl3\n";
        std::fs::write(tmp.join("r.txt"), body).unwrap();
        let store = std::sync::Arc::new(std::sync::RwLock::new(InMemorySnapshotStore::new()));
        let ctx = dummy_ctx_full(&ws, None, None, Some(&store));

        // 整篇 raw：原样输出
        let out = ReadFileTool
            .execute(serde_json::json!({ "path": "r.txt:raw" }), &ctx)
            .await
            .unwrap();
        assert_eq!(out.to_llm_text(), body);
        // raw 切片
        let out = ReadFileTool
            .execute(serde_json::json!({ "path": "r.txt:raw:2-3" }), &ctx)
            .await
            .unwrap();
        assert_eq!(out.to_llm_text(), "l2\nl3\n");
        // 无段头无快照
        assert!(store.read().expect("snapshot 锁").head("r.txt").is_none());
    }

    #[tokio::test]
    async fn read_file_literal_path_wins_over_selector() {
        let tmp = tempfile_dir();
        let ws = Workspace::new(&tmp);
        // 字面文件名长得像「路径:选择器」
        std::fs::write(tmp.join("w:1-2.txt"), "literal\n").unwrap();
        let ctx = dummy_ctx(&ws);
        let out = ReadFileTool
            .execute(serde_json::json!({ "path": "w:1-2.txt" }), &ctx)
            .await
            .unwrap();
        let text = out.to_llm_text();
        // 整读字面文件（含段头），未把 "1-2" 当选择器解析
        assert!(text.starts_with('['), "{text}");
        assert!(text.contains("literal"), "{text}");
        // 对照：无字面文件时选择器生效
        std::fs::write(tmp.join("s.txt"), "a\nb\nc\nd\ne\n").unwrap();
        let out = ReadFileTool
            .execute(serde_json::json!({ "path": "s.txt:2-3" }), &ctx)
            .await
            .unwrap();
        let text = out.to_llm_text();
        assert!(
            text.contains("    2\tb") && text.contains("    3\tc"),
            "{text}"
        );
        assert!(!text.contains("    4\td"), "{text}");
    }

    #[tokio::test]
    async fn read_file_invalid_selector_errors() {
        let tmp = tempfile_dir();
        let ws = Workspace::new(&tmp);
        std::fs::write(tmp.join("x.txt"), "a\nb\n").unwrap();
        let ctx = dummy_ctx(&ws);
        let err = ReadFileTool
            .execute(serde_json::json!({ "path": "x.txt:abc" }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("合法形式"), "{err}");
    }

    #[tokio::test]
    async fn archive_read_has_no_hashline_header() {
        let tmp = tempfile_dir();
        let ws = Workspace::new(&tmp);
        let f = std::fs::File::create(tmp.join("x.zip")).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        zw.start_file("m.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut zw, b"hello zip").unwrap();
        zw.finish().unwrap();
        let ctx = dummy_ctx(&ws);
        let out = ReadFileTool
            .execute(serde_json::json!({ "path": "x.zip" }), &ctx)
            .await
            .unwrap();
        let text = out.to_llm_text();
        assert!(!text.starts_with('['), "归档读取不加段头: {text}");
        assert!(text.contains("m.txt"), "{text}");
    }
}
