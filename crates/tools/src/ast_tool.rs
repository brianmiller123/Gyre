//! AST 工具：replace_block（tree-sitter 块替换，移植 oh-my-pi hashline `replace block N:`）。

use std::path::Path;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::{Tool, ToolContext, write_with_effects};

/// 用 tree-sitter 解析文件，将某行起始的句法块整体替换为新内容。
pub struct ReplaceBlockTool;

#[async_trait]
impl Tool for ReplaceBlockTool {
    fn name(&self) -> &'static str {
        "replace_block"
    }
    fn description(&self) -> &'static str {
        "用 tree-sitter 解析文件，将「某行起始的句法块」（如函数/结构体/方法）整体替换为新内容。当前支持 Rust。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "文件路径" },
                "line":    { "type": "integer", "description": "块起始的 1-indexed 行号" },
                "content": { "type": "string", "description": "替换为的新块内容" }
            },
            "required": ["path", "line", "content"]
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
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `path`".into()))?;
        let line = input
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `line`".into()))? as u32;
        let content = input
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `content`".into()))?;

        let full = ctx.workspace.resolve(Path::new(path));
        let text = tokio::fs::read_to_string(&full)
            .await
            .map_err(ToolError::Io)?;
        let lang = agent_ast::SupportLang::from_path(&full).unwrap_or(agent_ast::SupportLang::Rust);
        let range = agent_ast::block_range_at(&text, line, lang)
            .map_err(ToolError::Execution)?
            .ok_or_else(|| ToolError::Execution(format!("行 {line} 处无起始句法块")))?;

        let start = usize::try_from(range.start_line)
            .map_err(|_| ToolError::Execution("行号溢出".into()))?
            - 1;
        let end = usize::try_from(range.end_line)
            .map_err(|_| ToolError::Execution("行号溢出".into()))?
            - 1;

        let lines: Vec<&str> = text.lines().collect();
        if end >= lines.len() {
            return Err(ToolError::Execution("块范围越界".into()));
        }
        let mut rebuilt = String::new();
        for (i, l) in lines.iter().enumerate() {
            if i == start {
                rebuilt.push_str(content);
                if !content.ends_with('\n') {
                    rebuilt.push('\n');
                }
            } else if i < start || i > end {
                rebuilt.push_str(l);
                rebuilt.push('\n');
            }
            // start < i <= end 的旧行被丢弃（由 content 取代）
        }
        let report = write_with_effects(&full, &rebuilt, ctx).await?;
        let mut msg = format!(
            "已在 {path} 替换第 {}-{} 行的句法块",
            range.start_line, range.end_line
        );
        msg.push_str(&report.effect_suffix());
        Ok(ToolResult::text(msg))
    }
}

// ── ast-grep 结构化搜索/重写（多语言）──────────────────────────────────────────

/// 用 ast-grep 在源码中按结构化 pattern 搜索（支持 `$X` / `$$$Y` meta 变量）。
///
/// 语言按文件扩展名自动推断，亦可经 `lang` 显式指定。
pub struct AstSearchTool;

#[async_trait]
impl Tool for AstSearchTool {
    fn name(&self) -> &'static str {
        "ast_search"
    }
    fn description(&self) -> &'static str {
        "用 ast-grep 按结构化 pattern 在源码文件中搜索（支持 $X / $$$Y meta 变量），\
返回每处匹配的行号与片段。多语言：rust/python/javascript/typescript/go。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path":       { "type": "string", "description": "源码文件路径" },
                "pattern":    { "type": "string", "description": "ast-grep 模式，如 `fn $NAME($$$ARGS) { $$$BODY }`" },
                "lang":       { "type": "string", "enum": ["rust", "python", "javascript", "typescript", "go"],
                                "description": "语言（可选；省略则按扩展名推断）" },
                "strictness": { "type": "string", "enum": ["cst", "smart", "ast", "relaxed", "signature", "template"],
                                "description": "匹配严格度（可选，默认 smart）" }
            },
            "required": ["path", "pattern"]
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
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `path`".into()))?;
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `pattern`".into()))?;

        let full = ctx.workspace.resolve(Path::new(path));
        let text = tokio::fs::read_to_string(&full)
            .await
            .map_err(ToolError::Io)?;
        let lang = resolve_lang(&input, &full)?;
        let strictness =
            agent_ast::AstMatchStrictness::parse(input.get("strictness").and_then(|v| v.as_str()));

        let matches =
            agent_ast::search(&text, lang, pattern, strictness).map_err(ToolError::Execution)?;
        if matches.is_empty() {
            return Ok(ToolResult::text(format!("未在 {path} 中找到匹配")));
        }
        let mut out = format!("在 {path} 找到 {} 处匹配:\n", matches.len());
        for (i, m) in matches.iter().enumerate().take(50) {
            let start_line = line_of(&text, m.byte_start);
            let end_line = line_of(&text, m.byte_end);
            let preview: String = m
                .text
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(120)
                .collect();
            out.push_str(&format!(
                "  {}. L{start_line}-L{end_line}: {preview}\n",
                i + 1
            ));
        }
        if matches.len() > 50 {
            out.push_str(&format!("  …（另有 {} 处未显示）\n", matches.len() - 50));
        }
        Ok(ToolResult::text(out))
    }
}

/// 用 ast-grep 按结构化 pattern + rewrite 重写源码文件（原地改写）。
///
/// `preview: true`（暂存模式）：不落盘，把重写暂存到会话级 pending 队列并返回
/// `(proposed) N replacements` 预览；随后用 `write_file {path: "xd://resolve"}`
/// 应用（应用时重放重写并复查替换计数，文件已漂移则拒绝），或 `xd://reject` 丢弃。
/// 移植 oh-my-pi `ast_edit` 的 `(proposed) → 应用/拒绝` 暂存语义。
pub struct AstRewriteTool;

/// 暂存的 ast-grep 重写（预览 → 应用/拒绝）。
#[derive(Debug, Clone)]
pub struct PendingRewrite {
    /// 目标文件路径（相对工作区根，与调用时一致）。
    pub path: String,
    /// ast-grep 匹配模式。
    pub pattern: String,
    /// 重写模板。
    pub replacement: String,
    /// 匹配严格度（可选）。
    pub strictness: Option<String>,
    /// 预览时的替换计数（应用时复查）。
    pub count: usize,
    /// 预览时文件字节数。
    pub old_len: usize,
    /// 重写后字节数。
    pub new_len: usize,
}

#[async_trait]
impl Tool for AstRewriteTool {
    fn name(&self) -> &'static str {
        "ast_rewrite"
    }
    fn description(&self) -> &'static str {
        "用 ast-grep 按结构化 pattern 重写源码文件（支持 $X / $$$Y meta 变量，原地改写）。\
多语言：rust/python/javascript/typescript/go。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path":       { "type": "string", "description": "源码文件路径" },
                "pattern":    { "type": "string", "description": "ast-grep 匹配模式" },
                "rewrite":    { "type": "string", "description": "重写模板（可引用 pattern 中的 meta 变量）" },
                "lang":       { "type": "string", "enum": ["rust", "python", "javascript", "typescript", "go"],
                                "description": "语言（可选；省略则按扩展名推断）" },
                "strictness": { "type": "string", "enum": ["cst", "smart", "ast", "relaxed", "signature", "template"],
                                "description": "匹配严格度（可选，默认 smart）" },
                "preview":    { "type": "boolean", "description": "暂存模式：不落盘，返回 (proposed) 预览；用 write_file xd://resolve 应用" }
            },
            "required": ["path", "pattern", "rewrite"]
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
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `path`".into()))?;
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `pattern`".into()))?;
        let replacement = input
            .get("rewrite")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `rewrite`".into()))?;

        let full = ctx.workspace.resolve(Path::new(path));
        let text = tokio::fs::read_to_string(&full)
            .await
            .map_err(ToolError::Io)?;
        let lang = resolve_lang(&input, &full)?;
        let strictness =
            agent_ast::AstMatchStrictness::parse(input.get("strictness").and_then(|v| v.as_str()));
        let strictness_str = input
            .get("strictness")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        // 暂存模式：预览替换计数 + 新文本，不落盘；应用经 `write_file xd://resolve`。
        let preview = input
            .get("preview")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if preview {
            let matches = agent_ast::search(&text, lang, pattern, strictness)
                .map_err(ToolError::Execution)?;
            if matches.is_empty() {
                return Ok(ToolResult::text(format!(
                    "模式未匹配任何节点，{path} 未改动（未暂存）"
                )));
            }
            let new_text = agent_ast::rewrite(&text, lang, pattern, replacement, strictness)
                .map_err(ToolError::Execution)?;
            let pending = PendingRewrite {
                path: path.to_string(),
                pattern: pattern.to_string(),
                replacement: replacement.to_string(),
                strictness: strictness_str,
                count: matches.len(),
                old_len: text.len(),
                new_len: new_text.len(),
            };
            let Some(queue) = ctx.pending_rewrites else {
                return Err(ToolError::Execution(
                    "暂存队列未启用（当前运行环境不支持 preview）".into(),
                ));
            };
            let mut q = queue.lock().expect("pending 队列锁");
            // 同一 path 已有暂存：先移除旧项（最新预览为准）。
            q.retain(|p| p.path != pending.path);
            q.push(pending);
            drop(q);
            return Ok(ToolResult::text(format!(
                "(proposed) {} replacements — {path}（{} → {} 字节）已暂存，未写入。\
用 write_file {{path: \"xd://resolve\"}} 应用，或 xd://reject 丢弃；\
首处变化预览：\n{}",
                matches.len(),
                text.len(),
                new_text.len(),
                first_diff_line(&text, &new_text)
            )));
        }

        let new_text = agent_ast::rewrite(&text, lang, pattern, replacement, strictness)
            .map_err(ToolError::Execution)?;
        if new_text == text {
            return Ok(ToolResult::text(format!(
                "模式未匹配任何节点，{path} 未改动"
            )));
        }
        let report = write_with_effects(&full, &new_text, ctx).await?;
        let mut msg = format!(
            "已在 {path} 应用结构重写（{} → {} 字节）",
            text.len(),
            new_text.len()
        );
        msg.push_str(&report.effect_suffix());
        Ok(ToolResult::text(msg))
    }
}

/// 解析语言：优先 `lang` 参数，否则按路径扩展名推断；二者皆失败则报错。
fn resolve_lang(
    input: &serde_json::Value,
    path: &Path,
) -> Result<agent_ast::SupportLang, ToolError> {
    input
        .get("lang")
        .and_then(|v| v.as_str())
        .and_then(agent_ast::SupportLang::parse)
        .or_else(|| agent_ast::SupportLang::from_path(path))
        .ok_or_else(|| {
            ToolError::InvalidArgs(
                "无法推断语言，请用 `lang` 指定 (rust/python/javascript/typescript/go)".into(),
            )
        })
}

/// 字节偏移 → 1-indexed 行号（统计其前的换行数）。
fn line_of(src: &str, byte_off: usize) -> usize {
    let off = byte_off.min(src.len());
    src[..off].matches('\n').count() + 1
}

/// 首个差异行的前后各一行预览（≤3 行，`-`/`+` 前缀，限 200 字符/行）。
fn first_diff_line(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let common = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = String::new();
    if common > 0 {
        let show = (common - 1).max(common.saturating_sub(1));
        if show > 0 {
            let line = old_lines[show.min(old_lines.len() - 1)];
            let line = &line[..line.len().min(200)];
            out.push_str(&format!("  {line}\n"));
        }
    }
    if let Some(l) = old_lines.get(common) {
        let l = &l[..l.len().min(200)];
        out.push_str(&format!("- {l}\n"));
    }
    if let Some(l) = new_lines.get(common) {
        let l = &l[..l.len().min(200)];
        out.push_str(&format!("+ {l}\n"));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;
    use agent_core::{ApprovalDecision, ApprovalRequest, Workspace};

    fn auto_ctx(ws: &Workspace) -> ToolContext<'_> {
        struct Auto;
        #[async_trait::async_trait]
        impl agent_core::ApprovalPolicy for Auto {
            fn decide(&self, _: &ApprovalRequest<'_>) -> ApprovalDecision {
                ApprovalDecision::Allow
            }
            async fn prompt(
                &self,
                _: &agent_core::AskMessage,
            ) -> Result<agent_core::AskResponse, ToolError> {
                Ok(agent_core::AskResponse::Yes)
            }
        }
        static CANCEL: std::sync::OnceLock<tokio_util::sync::CancellationToken> =
            std::sync::OnceLock::new();
        let cancel = CANCEL.get_or_init(tokio_util::sync::CancellationToken::new);
        ToolContext {
            workspace: ws,
            approval: &Auto,
            cancel,
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
            update_tx: None,
            conflicts: None,
            pending_rewrites: None,
            context: None,
            snapshots: None,
            tool_call_id: None,
        }
    }

    #[tokio::test]
    async fn replaces_function_block() {
        let dir = std::env::temp_dir().join(format!("agent-ast-{}", unique()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("m.rs");
        std::fs::write(&file, "fn old() {\n    let x = 1;\n}\nfn keep() {}\n").unwrap();
        let ws = Workspace::new(&dir);
        let ctx = auto_ctx(&ws);
        let res = ReplaceBlockTool
            .execute(
                serde_json::json!({ "path": "m.rs", "line": 1, "content": "fn new() {\n    let y = 2;\n}" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(res, ToolResult::Text(_)));
        let after = std::fs::read_to_string(&file).unwrap();
        assert!(after.contains("fn new()"));
        assert!(!after.contains("fn old()"));
        assert!(after.contains("fn keep()"));
    }

    fn unique() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
