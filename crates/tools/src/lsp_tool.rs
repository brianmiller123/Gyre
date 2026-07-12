//! LSP 工具包装器：将 LSP 客户端能力暴露为可供智能体调用的 [`Tool`]。
//!
//! 通过 `action` 参数路由到具体操作：
//! - `diagnostics` — 获取文件诊断
//! - `goto_definition` — 跳转到定义
//! - `find_references` — 查找引用
//! - `hover` — 获取悬停类型信息
//! - `document_symbols` — 获取文件符号列表
//! - `workspace_symbols` — 工作区全局符号搜索
//! - `rename` — 语义重命名
//! - `code_actions` — 获取代码操作/快速修复

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{CapabilityTier, ToolResult};
use agent_lsp::LspManager;
use agent_lsp::client::DiagnosticSeverity;
use agent_lsp::detect::detect_servers;
use tokio::sync::Mutex;
use tracing::warn;

use super::{Tool, ToolContext};

/// 共享 LSP 管理器池类型（`workspace_root → LspManager`），供 `LspTool` 与 `LspWriteEffect` 复用同一套语言服务器实例。
pub type LspPool = Arc<Mutex<HashMap<PathBuf, LspManager>>>;

/// LSP 工具：统一的 LSP 能力入口。
pub struct LspTool {
    managers: Arc<Mutex<HashMap<PathBuf, LspManager>>>,
}

impl LspTool {
    #[must_use]
    pub fn new() -> Self {
        Self {
            managers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 共享 LSP 管理器池（供 `LspWriteEffect` 等复用同一套语言服务器实例）。
    #[must_use]
    pub fn pool(&self) -> LspPool {
        Arc::clone(&self.managers)
    }
}

impl Default for LspTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        "Language Server Protocol client: get diagnostics, go to definition, find references, \
         hover for type info, list document symbols, search workspace symbols, rename symbols, \
         and get code actions/quick fixes."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "diagnostics", "goto_definition", "find_references", "hover",
                        "document_symbols", "workspace_symbols", "rename", "code_actions",
                        "format", "open_document", "close_document"
                    ],
                    "description": "The LSP operation to perform."
                },
                "uri": {
                    "type": "string", "format": "uri",
                    "description": "File URI (file:///path/to/file). Required for all actions except workspace_symbols."
                },
                "text": {
                    "type": "string",
                    "description": "Full file content (required for open_document)."
                },
                "line": {
                    "type": "integer",
                    "description": "0-based line number."
                },
                "character": {
                    "type": "integer",
                    "description": "0-based character offset."
                },
                "new_name": {
                    "type": "string",
                    "description": "New symbol name (required for rename)."
                },
                "query": {
                    "type": "string",
                    "description": "Search query (required for workspace_symbols)."
                }
            }
        })
    }

    fn capability(&self) -> CapabilityTier {
        CapabilityTier::ReadOnly
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, agent_core::ToolError> {
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| agent_core::ToolError::InvalidArgs("缺少 'action' 参数".into()))?;

        let workspace_root = ctx.workspace.root().to_path_buf();
        let mut managers = self.managers.lock().await;

        if !managers.contains_key(&workspace_root) {
            let servers = detect_servers(&workspace_root);
            if servers.is_empty() {
                return Ok(ToolResult::text(
                    "未检测到支持的语言服务器。请确保项目根目录包含 \
                     Cargo.toml、package.json、go.mod 或 pyproject.toml 等配置文件。",
                ));
            }
            match LspManager::start(&workspace_root, &servers).await {
                Ok(mgr) => {
                    managers.insert(workspace_root.clone(), mgr);
                }
                Err(e) => {
                    warn!(error = %e, "启动 LSP 管理器失败");
                    return Ok(ToolResult::text(format!("启动语言服务器失败: {e}")));
                }
            }
        }

        let manager = managers.get_mut(&workspace_root).unwrap();

        match action {
            "open_document" => {
                let uri = parse_uri(&input)?;
                let text = input
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| agent_core::ToolError::InvalidArgs("缺少 'text' 参数".into()))?;
                manager
                    .open_document(&uri, text)
                    .await
                    .map_err(|e| agent_core::ToolError::Execution(format!("打开文档失败: {e}")))?;
                Ok(ToolResult::text(format!("文档已打开: {uri}")))
            }
            "close_document" => {
                let uri = parse_uri(&input)?;
                manager
                    .close_document(&uri)
                    .await
                    .map_err(|e| agent_core::ToolError::Execution(format!("关闭文档失败: {e}")))?;
                Ok(ToolResult::text(format!("文档已关闭: {uri}")))
            }
            "diagnostics" => {
                let uri = parse_uri(&input)?;
                let diags = manager.diagnostics(&uri).await;
                if diags.is_empty() {
                    return Ok(ToolResult::text("无诊断信息。"));
                }
                let mut result = String::from("诊断结果:\n\n");
                for d in &diags {
                    let severity = match d.severity {
                        Some(DiagnosticSeverity::Error) => "❌ ERROR",
                        Some(DiagnosticSeverity::Warning) => "⚠️  WARNING",
                        Some(DiagnosticSeverity::Information) => "ℹ️  INFO",
                        Some(DiagnosticSeverity::Hint) => "💡 HINT",
                        _ => "❓ UNKNOWN",
                    };
                    result.push_str(&format!(
                        "{severity} [L{}:C{}]: {}\n",
                        d.line + 1,
                        d.character + 1,
                        d.message
                    ));
                    if let Some(source) = &d.source {
                        result.push_str(&format!("  source: {source}\n"));
                    }
                    if let Some(code) = &d.code {
                        result.push_str(&format!("  code: {code}\n"));
                    }
                    result.push('\n');
                }
                Ok(ToolResult::text(result))
            }
            "goto_definition" => {
                let uri = parse_uri(&input)?;
                let (line, character) = parse_position(&input)?;
                let locations = manager
                    .goto_definition(&uri, line, character)
                    .await
                    .map_err(|e| agent_core::ToolError::Execution(format!("跳转定义失败: {e}")))?;
                if locations.is_empty() {
                    return Ok(ToolResult::text("未找到定义。"));
                }
                let mut result = String::from("定义位置:\n");
                for loc in &locations {
                    result.push_str(&format!(
                        "  {} [L{}:C{}]\n",
                        loc.uri,
                        loc.line + 1,
                        loc.character + 1
                    ));
                }
                Ok(ToolResult::text(result))
            }
            "find_references" => {
                let uri = parse_uri(&input)?;
                let (line, character) = parse_position(&input)?;
                let locations = manager
                    .find_references(&uri, line, character)
                    .await
                    .map_err(|e| agent_core::ToolError::Execution(format!("查找引用失败: {e}")))?;
                if locations.is_empty() {
                    return Ok(ToolResult::text("未找到引用。"));
                }
                let mut result = format!("找到 {} 处引用:\n", locations.len());
                for loc in &locations {
                    result.push_str(&format!(
                        "  {} [L{}:C{}]\n",
                        loc.uri,
                        loc.line + 1,
                        loc.character + 1
                    ));
                }
                Ok(ToolResult::text(result))
            }
            "hover" => {
                let uri = parse_uri(&input)?;
                let (line, character) = parse_position(&input)?;
                let hover = manager.hover(&uri, line, character).await.map_err(|e| {
                    agent_core::ToolError::Execution(format!("获取悬停信息失败: {e}"))
                })?;
                match hover {
                    Some(h) => {
                        let content = h.contents.join("\n---\n");
                        let loc = h.line.map_or(String::new(), |l| {
                            format!(" [L{}:C{}]", l + 1, h.character.unwrap_or(0) + 1)
                        });
                        Ok(ToolResult::text(format!("悬停信息{loc}:\n{content}")))
                    }
                    None => Ok(ToolResult::text("无悬停信息。")),
                }
            }
            "document_symbols" => {
                let uri = parse_uri(&input)?;
                let symbols = manager.document_symbols(&uri).await.map_err(|e| {
                    agent_core::ToolError::Execution(format!("获取文档符号失败: {e}"))
                })?;
                if symbols.is_empty() {
                    return Ok(ToolResult::text("未找到文档符号。"));
                }
                let mut result = format!("文档符号 ({} 个):\n", symbols.len());
                for sym in &symbols {
                    let container = sym
                        .container
                        .as_deref()
                        .map_or(String::new(), |c| format!(" (in {c})"));
                    result.push_str(&format!(
                        "  [{}] {} @ L{}:C{}{container}\n",
                        sym.kind,
                        sym.name,
                        sym.line + 1,
                        sym.character + 1
                    ));
                }
                Ok(ToolResult::text(result))
            }
            "workspace_symbols" => {
                let query = input.get("query").and_then(|v| v.as_str()).ok_or_else(|| {
                    agent_core::ToolError::InvalidArgs("缺少 'query' 参数".into())
                })?;
                let symbols = manager.workspace_symbols(query).await.map_err(|e| {
                    agent_core::ToolError::Execution(format!("工作区符号搜索失败: {e}"))
                })?;
                if symbols.is_empty() {
                    return Ok(ToolResult::text(format!("未找到匹配 '{query}' 的符号。")));
                }
                let mut result = format!("工作区符号 ({} 个匹配 '{query}'):\n", symbols.len());
                for sym in &symbols {
                    result.push_str(&format!(
                        "  [{}] {} — {} [L{}:C{}]\n",
                        sym.kind,
                        sym.name,
                        sym.uri,
                        sym.line + 1,
                        sym.character + 1
                    ));
                }
                Ok(ToolResult::text(result))
            }
            "rename" => {
                let uri = parse_uri(&input)?;
                let (line, character) = parse_position(&input)?;
                let new_name = input
                    .get("new_name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        agent_core::ToolError::InvalidArgs("缺少 'new_name' 参数".into())
                    })?;
                let edits = manager
                    .rename(&uri, line, character, new_name)
                    .await
                    .map_err(|e| agent_core::ToolError::Execution(format!("重命名失败: {e}")))?;
                if edits.is_empty() {
                    return Ok(ToolResult::text("未找到需要修改的位置。"));
                }
                let mut result = format!("重命名为 '{new_name}' 的变更 ({} 处):\n", edits.len());
                for edit in &edits {
                    result.push_str(&format!(
                        "  {} [L{}:C{}]: '{}'\n",
                        edit.uri,
                        edit.line + 1,
                        edit.character + 1,
                        edit.new_text
                    ));
                }
                Ok(ToolResult::text(result))
            }
            "code_actions" => {
                let uri = parse_uri(&input)?;
                let (line, character) = parse_position(&input)?;
                let actions = manager
                    .code_actions(&uri, line, character)
                    .await
                    .map_err(|e| {
                        agent_core::ToolError::Execution(format!("获取代码操作失败: {e}"))
                    })?;
                if actions.is_empty() {
                    return Ok(ToolResult::text("无可用的代码操作。"));
                }
                let mut result = format!("代码操作 ({} 个):\n", actions.len());
                for action in &actions {
                    let kind = action.kind.as_deref().unwrap_or("");
                    let pref = if action.is_preferred { " ⭐" } else { "" };
                    result.push_str(&format!("  [{kind}] {}{pref}\n", action.title));
                    for edit in &action.edits {
                        result.push_str(&format!(
                            "    {} [L{}:C{}]: '{}'\n",
                            edit.uri,
                            edit.line + 1,
                            edit.character + 1,
                            edit.new_text
                        ));
                    }
                }
                Ok(ToolResult::text(result))
            }
            "format" => {
                let uri = parse_uri(&input)?;
                let formatted = manager
                    .format(&uri)
                    .await
                    .map_err(|e| agent_core::ToolError::Execution(format!("格式化失败: {e}")))?;
                match formatted {
                    Some(text) => Ok(ToolResult::text(format!(
                        "已格式化（{} 字节）:\n{text}",
                        text.len()
                    ))),
                    None => Ok(ToolResult::text(
                        "服务器不支持 formatting 或文件无需格式化。",
                    )),
                }
            }
            _ => Err(agent_core::ToolError::InvalidArgs(format!(
                "未知 action: '{action}'"
            ))),
        }
    }
}

// ── 辅助函数 ───────────────────────────────────────────────────────────

fn parse_uri(input: &serde_json::Value) -> Result<url::Url, agent_core::ToolError> {
    let s = input
        .get("uri")
        .and_then(|v| v.as_str())
        .ok_or_else(|| agent_core::ToolError::InvalidArgs("缺少 'uri' 参数".into()))?;
    s.parse()
        .map_err(|e| agent_core::ToolError::InvalidArgs(format!("无效 URI '{s}': {e}")))
}

fn parse_position(input: &serde_json::Value) -> Result<(u32, u32), agent_core::ToolError> {
    let line = input
        .get("line")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| agent_core::ToolError::InvalidArgs("缺少 'line' 参数".into()))?
        as u32;
    let character = input
        .get("character")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| agent_core::ToolError::InvalidArgs("缺少 'character' 参数".into()))?
        as u32;
    Ok((line, character))
}
