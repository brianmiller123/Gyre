//! 文件系统工具：read_file / write_file。
//! （str_replace/apply_diff 已按工具收敛移除，编辑走 apply_hashline。）

use std::path::Path;
use std::time::Duration;

use agent_ast::{SegmentKind, SummaryOptions, SupportLang, summarize_code};
use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::{ConflictBlock, PendingRewrite, Tool, ToolContext, write_with_effects};

/// 读取文件（带行号）。
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "读取工作区内文件内容并附行号。仅限只读，不修改文件。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径（相对工作区根或绝对路径），或内部协议：skill://<name>[/<rel>]（skill 内容）、memory://[summary|full]（跨会话记忆）、mcp://<server>/<uri>（MCP 资源）、local://<rel>（显式工作区相对）、artifact://<id>（shake 归档内容回读）、http(s)://（抓取网页）、pr://<owner>/<repo>[/<N>]（GitHub PR，文件缓存）、issue://<owner>/<repo>[/<N>]（GitHub issue）" },
                "summary": { "type": "boolean", "description": "可选：对支持语言的代码文件做结构摘要——折叠大体块、保留签名，行号与原文一致。适合快速浏览大文件；需逐行编辑时仍用真实行号。" }
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
        // 内部协议路由：skill:// memory:// mcp:// local:// http(s):// 或裸本地路径。
        let text = resolve_path(path, ctx).await?;
        // 可选摘要：对支持语言的代码文件做结构折叠，保留签名、行号保真。
        let want_summary = input
            .get("summary")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let out = if want_summary {
            match SupportLang::from_path(Path::new(path)) {
                Some(lang) => render_summary(&text, lang),
                None => render_numbered(&text),
            }
        } else {
            render_numbered(&text)
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
                        let _ = std::fmt::Write::write_fmt(&mut out, format_args!(
                            "{:>5}\t{line}\n",
                            seg.start_line as usize + offset
                        ));
                    }
                }
            }
            SegmentKind::Elided => {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!(
                    "     ⋯⋯ (折叠第 {}-{} 行，共 {} 行) ⋯⋯\n",
                    seg.start_line,
                    seg.end_line,
                    seg.end_line
                        .saturating_sub(seg.start_line)
                        .saturating_add(1),
                ));
            }
        }
    }
    out
}

/// 解析 read_file 的 `path`：内部协议路由 + 裸本地路径，返回原始文本。
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
                        p.path, p.pattern, p.strictness.as_deref().unwrap_or("smart"), p.count,
                        p.old_len, p.new_len
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
        let full = ctx.workspace.resolve(Path::new(rel));
        Ok(read_bounded(&full).await?)
    } else if path.starts_with("http://") || path.starts_with("https://") {
        fetch_http(path).await
    } else {
        let full = ctx.workspace.resolve(Path::new(path));
        Ok(read_bounded(&full).await?)
    }
}

/// `conflict://<N>[/ours|theirs|base]` 读取：完整块带原文行号，或单侧文本。
/// 非 conflict URI 返回 `Ok(None)`。
async fn read_conflict_uri(
    uri: &str,
    ctx: &ToolContext<'_>,
) -> Result<Option<String>, ToolError> {
    if !uri.starts_with("conflict://") {
        return Ok(None);
    }
    let Some(history) = ctx.conflicts else {
        return Err(ToolError::Execution(
            "conflict:// 未启用（无会话冲突注册表）".into(),
        ));
    };
    let (target, id, side) = crate::conflict::parse_conflict_uri(uri)
        .map_err(ToolError::InvalidArgs)?;
    match target {
        crate::conflict::ConflictTarget::Wildcard => Err(ToolError::InvalidArgs(
            "conflict://* 仅支持写入（批量解决）".into(),
        )),
        crate::conflict::ConflictTarget::Single => {
            let block = {
                let h = history.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
                    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{:>5}\t{line}\n", block.start_line + i));
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
fn summarize_conflicts(
    path: &str,
    text: &str,
    ctx: &ToolContext<'_>,
) -> Result<String, ToolError> {
    let Some(history) = ctx.conflicts else {
        return Err(ToolError::Execution(
            ":conflicts 未启用（无会话冲突注册表）".into(),
        ));
    };
    let mut h = history.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!(
                "  conflict://{} L{}-L{}  ours: {} | theirs: {}\n",
                b.id, b.start_line, b.end_line, ours_preview, theirs_preview
            ));
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
                let h = history.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let block = h.get(id).cloned().ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "冲突 #{id} 未注册（先用 read_file <file>:conflicts 扫描）"
                    ))
                })?;
                drop(h);
                let replacement =
                    crate::conflict::expand_content_tokens(content, &block)
                        .map_err(ToolError::InvalidArgs)?;
                (block, replacement)
            };
            let full = ctx.workspace.resolve(Path::new(&block.path));
            let current = tokio::fs::read_to_string(&full).await.map_err(ToolError::Io)?;
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
                let h = history.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
                let repl =
                    crate::conflict::expand_content_tokens(&repl, b).map_err(ToolError::InvalidArgs)?;
                by_file.entry(b.path.clone()).or_default().push((b.clone(), repl));
            }
            if by_file.is_empty() {
                return Err(ToolError::InvalidArgs(
                    "指令未命中任何已注册冲突（可用 read_file <file>:conflicts 查看 id）"
                        .into(),
                ));
            }
            let mut solved: Vec<usize> = Vec::new();
            let mut failed: Vec<String> = Vec::new();
            for (path, mut items) in by_file {
                let full = ctx.workspace.resolve(Path::new(&path));
                let current = tokio::fs::read_to_string(&full).await.map_err(ToolError::Io)?;
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
                let mut h = history.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
                p.path, p.count, matches.len()
            ));
            continue;
        }
        let new_text =
            match agent_ast::rewrite(&text, lang, &p.pattern, &p.replacement, strictness) {
                Ok(t) => t,
                Err(e) => {
                    failures.push(format!("{}: 重写失败: {e}", p.path));
                    continue;
                }
            };
        if new_text.len() != p.new_len {
            failures.push(format!(
                "{}: 重写结果与预览不一致（{} vs {} 字节），拒绝写入；请重新 preview",
                p.path, new_text.len(), p.new_len
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
        } else if let Some(rest) = t.strip_prefix('#') {
            let (a, b) = rest.split_once('=')?;
            (a.trim(), b.trim().to_string())
        } else {
            return None;
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
    Ok(read_bounded(&full).await?)
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
pub(crate) async fn fetch_http(url: &str) -> Result<String, ToolError> {
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
            .map_err(|e| ToolError::Execution(format!("fetch {} 失败: {e}", current)))?;
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
pub(crate) fn ssrf_guard(raw: &str) -> Result<(), ToolError> {
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
            .user_agent(concat!("gyre-agent/", env!("CARGO_PKG_VERSION"), " (+https://github.com/Gyre)"))
            // 禁用自动重定向：手动跟随并对每一跳的目标重新做 SSRF 校验。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("构造 HTTP client 失败")
    })
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
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
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
    use crate::Tool;
    use agent_core::{ApprovalMode, ApprovalRequest, CapabilityTier, Workspace};

    fn dummy_ctx<'a>(ws: &'a Workspace) -> ToolContext<'a> {
        dummy_ctx_q(ws, None)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn dummy_ctx_h<'a>(
        ws: &'a Workspace,
        history: Option<&'a std::sync::Arc<std::sync::Mutex<ConflictHistory>>>,
    ) -> ToolContext<'a> {
        dummy_ctx_hq(ws, history, None)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn dummy_ctx_q<'a>(
        ws: &'a Workspace,
        queue: Option<&'a std::sync::Arc<std::sync::Mutex<Vec<PendingRewrite>>>>,
    ) -> ToolContext<'a> {
        dummy_ctx_hq(ws, None, queue)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn dummy_ctx_hq<'a>(
        ws: &'a Workspace,
        history: Option<&'a std::sync::Arc<std::sync::Mutex<ConflictHistory>>>,
        queue: Option<&'a std::sync::Arc<std::sync::Mutex<Vec<PendingRewrite>>>>,
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
        assert!(msg.contains("1 处合并冲突") && msg.contains("L2-L9"), "{msg}");
        assert_eq!(history.lock().unwrap().len(), 1);

        let res = ReadFileTool
            .execute(serde_json::json!({ "path": "conflict://1/ours" }), &ctx)
            .await
            .unwrap();
        assert!(matches!(res, ToolResult::Text(t) if t.contains("ours-a")));

        let res = WriteFileTool
            .execute(serde_json::json!({ "path": "conflict://1", "content": "@ours" }), &ctx)
            .await
            .unwrap();
        assert!(matches!(res, ToolResult::Text(t) if t.contains("已解决冲突 #1")));
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert_eq!(on_disk, "line1\nours-a\nours-b\nline9\n");
        assert_eq!(history.lock().unwrap().len(), 0, "解决后注册表应清空");

        let err = WriteFileTool
            .execute(serde_json::json!({ "path": "conflict://1", "content": "@theirs" }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("未注册"), "{err}");
    }

    #[tokio::test]
    async fn conflict_wildcard_batch_with_directives() {
        let dir = std::env::temp_dir().join(format!("agent-cf-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(CONFLICT_FILE);
        std::fs::write(&file, "a\n<<<<<<< x\n1\n=======\n2\n>>>>>>> y\nc\n<<<<<<< x\n3\n=======\n4\n>>>>>>> y\nz\n").unwrap();
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
            .execute(serde_json::json!({ "path": "conflict://1", "content": "@ours" }), &ctx)
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
            .execute(serde_json::json!({ "path": "conflict://1", "content": "@ours" }), &ctx)
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
        assert!(listing.contains("rw.rs") && listing.contains("2 replacements"), "{listing}");

        // 应用。
        let res = WriteFileTool
            .execute(serde_json::json!({ "path": "xd://resolve", "content": "" }), &ctx)
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
            .execute(serde_json::json!({ "path": "xd://resolve", "content": "" }), &ctx)
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
            .execute(serde_json::json!({ "path": "xd://resolve", "content": "" }), &ctx)
            .await
            .unwrap();
        let msg = match res {
            ToolResult::Text(t) => t,
            _ => panic!("应为文本"),
        };
        assert!(msg.contains("文件已变化") && msg.contains("拒绝写入"), "{msg}");
        assert!(!queue.lock().unwrap().is_empty(), "失败条目保留可重试");
        assert!(
            std::fs::read_to_string(&file).unwrap().contains("external edit"),
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
            .execute(serde_json::json!({ "path": "xd://reject", "content": "" }), &ctx)
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
            .execute(serde_json::json!({ "path": "pr://rust-lang/rust/abc" }), &ctx)
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
        std::fs::write(cache_dir.join("anon__octo__repo__pulls_state_open_per_page_10.json"), body).unwrap();

        let res = ReadFileTool
            .execute(serde_json::json!({ "path": "pr://octo/repo?state=open" }), &ctx)
            .await
            .unwrap();
        let text = match res {
            ToolResult::Text(t) => t,
            _ => panic!("应为文本"),
        };
        assert!(text.contains("#42") && text.contains("fix cache"), "{text}");
    }
}
