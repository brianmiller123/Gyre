//! LSP 客户端：管理语言服务器生命周期与所有协议功能。
//!
//! 使用原始 `serde_json::Value` 与语言服务器通信，避免 lsp-types 版本差异。
//! 输出类型定义精简子集，确保与智能体工具的互操作性。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use url::Url;

use crate::detect::LspServerInfo;
use crate::transport::{LspTransport, TransportError};

/// 默认请求超时。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

// ── 精简 LSP 输出类型（用于智能体工具返回） ────────────────────────────

/// 诊断严重级别。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

/// 单个诊断。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspDiagnostic {
    pub severity: Option<DiagnosticSeverity>,
    pub message: String,
    pub line: u32,
    pub character: u32,
    pub source: Option<String>,
    pub code: Option<String>,
}

/// 文件位置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspLocation {
    pub uri: String,
    pub line: u32,
    pub character: u32,
}

/// 悬停内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspHover {
    pub contents: Vec<String>,
    pub line: Option<u32>,
    pub character: Option<u32>,
}

/// 符号信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspSymbol {
    pub name: String,
    pub kind: String,
    pub uri: String,
    pub line: u32,
    pub character: u32,
    pub container: Option<String>,
}

/// 重命名编辑。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspRenameEdit {
    pub uri: String,
    pub line: u32,
    pub character: u32,
    pub new_text: String,
    pub old_text: String,
}

/// 代码操作。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspCodeAction {
    pub title: String,
    pub kind: Option<String>,
    pub is_preferred: bool,
    pub edits: Vec<LspRenameEdit>,
}

/// 文本编辑（textDocument/formatting 等返回）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspTextEdit {
    /// 起始行（0-based）。
    pub start_line: u32,
    /// 起始列（0-based，UTF-16 code units）。
    pub start_character: u32,
    /// 结束行（0-based）。
    pub end_line: u32,
    /// 结束列（0-based，UTF-16 code units）。
    pub end_character: u32,
    /// 替换文本。
    pub new_text: String,
}

// ── LSP 客户端 ─────────────────────────────────────────────────────────

/// LSP 客户端：封装完整的语言服务器协议通信。
pub struct LspClient {
    transport: LspTransport,
    server_info: serde_json::Value,
    root: PathBuf,
    next_id: AtomicU64,
    diagnostics: Arc<Mutex<HashMap<String, Vec<LspDiagnostic>>>>,
    open_docs: HashMap<String, i32>,
}

impl LspClient {
    /// 启动语言服务器并完成 LSP initialize 握手。
    pub async fn initialize(root: &Path, server: &LspServerInfo) -> Result<Self, LspError> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

        info!(server = %server.name, root = %root.display(), "正在启动 LSP 服务器");

        let args: Vec<&str> = server.args.iter().map(String::as_str).collect();
        let transport = LspTransport::spawn(&server.command, &args, Some(&root))
            .map_err(LspError::Transport)?;

        let mut client = Self {
            transport,
            server_info: serde_json::Value::Null,
            root: root.clone(),
            next_id: AtomicU64::new(1),
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
            open_docs: HashMap::new(),
        };

        // ── initialize 握手 ──────────────────────────────────────────
        let root_uri = Url::from_directory_path(&root)
            .map_err(|_| LspError::InvalidUri(root.display().to_string()))?;

        let init_params = serde_json::json!({
            "processId": std::process::id(),
            "rootPath": root.to_string_lossy(),
            "rootUri": root_uri.to_string(),
            "capabilities": {
                "textDocument": {
                    "definition": { "dynamicRegistration": false, "linkSupport": true },
                    "formatting": { "dynamicRegistration": false },
                    "references": { "dynamicRegistration": false },
                    "hover": { "dynamicRegistration": false, "contentFormat": ["markdown", "plaintext"] },
                    "documentSymbol": { "dynamicRegistration": false, "hierarchicalDocumentSymbolSupport": true },
                    "rename": { "dynamicRegistration": false, "prepareSupport": false },
                    "codeAction": {
                        "dynamicRegistration": false,
                        "codeActionLiteralSupport": {
                            "codeActionKind": {
                                "valueSet": ["", "quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite", "source", "source.organizeImports"]
                            }
                        }
                    }
                },
                "workspace": {
                    "symbol": { "dynamicRegistration": false }
                }
            },
            "trace": "off",
            "clientInfo": { "name": "agent-lsp", "version": "0.1.0" }
        });

        let id = client.next_id();
        let response = client
            .transport
            .request(id, "initialize", init_params, DEFAULT_TIMEOUT)
            .await
            .map_err(LspError::Transport)?;

        client.server_info = response;

        client
            .transport
            .notify("initialized", serde_json::json!({}))
            .await
            .map_err(LspError::Transport)?;

        info!(server = %server.name, "LSP 服务器已初始化");
        Ok(client)
    }

    // ── 文档生命周期 ─────────────────────────────────────────────────

    pub async fn open_document(
        &mut self,
        uri: &Url,
        text: &str,
        language_id: &str,
    ) -> Result<(), LspError> {
        let params = serde_json::json!({
            "textDocument": {
                "uri": uri.to_string(),
                "languageId": language_id,
                "version": 1,
                "text": text
            }
        });
        self.notify("textDocument/didOpen", params).await?;
        self.open_docs.insert(uri.to_string(), 1);
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.collect_notifications().await;
        Ok(())
    }

    pub async fn change_document(&mut self, uri: &Url, text: &str) -> Result<(), LspError> {
        let key = uri.to_string();
        let version = self.open_docs.get(&key).copied().unwrap_or(1) + 1;
        let params = serde_json::json!({
            "textDocument": { "uri": uri.to_string(), "version": version },
            "contentChanges": [{ "text": text }]
        });
        self.notify("textDocument/didChange", params).await?;
        self.open_docs.insert(key, version);
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.collect_notifications().await;
        Ok(())
    }

    pub async fn close_document(&mut self, uri: &Url) -> Result<(), LspError> {
        let params = serde_json::json!({ "textDocument": { "uri": uri.to_string() } });
        self.notify("textDocument/didClose", params).await?;
        self.open_docs.remove(&uri.to_string());
        Ok(())
    }

    // ── 诊断 ──────────────────────────────────────────────────────────

    pub async fn diagnostics(&mut self, uri: &Url) -> Vec<LspDiagnostic> {
        self.collect_notifications().await;
        self.diagnostics
            .lock()
            .await
            .get(&uri.to_string())
            .cloned()
            .unwrap_or_default()
    }

    pub async fn all_diagnostics(&self) -> HashMap<String, Vec<LspDiagnostic>> {
        self.diagnostics.lock().await.clone()
    }

    // ── 跳转定义 ─────────────────────────────────────────────────────

    pub async fn goto_definition(
        &self,
        uri: &Url,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, LspError> {
        let params = serde_json::json!({
            "textDocument": { "uri": uri.to_string() },
            "position": { "line": line, "character": character }
        });
        let response = self.request("textDocument/definition", params).await?;
        if response.is_null() {
            return Ok(vec![]);
        }
        Ok(parse_locations(&response))
    }

    // ── 查找引用 ─────────────────────────────────────────────────────

    pub async fn find_references(
        &self,
        uri: &Url,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, LspError> {
        let params = serde_json::json!({
            "textDocument": { "uri": uri.to_string() },
            "position": { "line": line, "character": character },
            "context": { "includeDeclaration": true }
        });
        let response = self.request("textDocument/references", params).await?;
        if response.is_null() {
            return Ok(vec![]);
        }
        Ok(parse_locations(&response))
    }

    // ── 悬停信息 ─────────────────────────────────────────────────────

    pub async fn hover(
        &self,
        uri: &Url,
        line: u32,
        character: u32,
    ) -> Result<Option<LspHover>, LspError> {
        let params = serde_json::json!({
            "textDocument": { "uri": uri.to_string() },
            "position": { "line": line, "character": character }
        });
        let response = self.request("textDocument/hover", params).await?;
        if response.is_null() {
            return Ok(None);
        }
        Ok(Some(parse_hover(&response)))
    }

    // ── 文档符号 ─────────────────────────────────────────────────────

    pub async fn document_symbols(&self, uri: &Url) -> Result<Vec<LspSymbol>, LspError> {
        let params = serde_json::json!({ "textDocument": { "uri": uri.to_string() } });
        let response = self.request("textDocument/documentSymbol", params).await?;
        if response.is_null() {
            return Ok(vec![]);
        }
        Ok(parse_symbols(&response, uri))
    }

    // ── 工作区符号 ────────────────────────────────────────────────────

    pub async fn workspace_symbols(&self, query: &str) -> Result<Vec<LspSymbol>, LspError> {
        let params = serde_json::json!({ "query": query });
        let response = self.request("workspace/symbol", params).await?;
        if response.is_null() {
            return Ok(vec![]);
        }
        Ok(parse_symbols(&response, &Url::parse("file:///").unwrap()))
    }

    // ── 重命名 ────────────────────────────────────────────────────────

    pub async fn rename(
        &self,
        uri: &Url,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<Vec<LspRenameEdit>, LspError> {
        let params = serde_json::json!({
            "textDocument": { "uri": uri.to_string() },
            "position": { "line": line, "character": character },
            "newName": new_name
        });
        let response = self.request("textDocument/rename", params).await?;
        Ok(parse_workspace_edit(&response))
    }

    // ── 代码操作 ─────────────────────────────────────────────────────

    pub async fn code_actions(
        &self,
        uri: &Url,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspCodeAction>, LspError> {
        let params = serde_json::json!({
            "textDocument": { "uri": uri.to_string() },
            "range": {
                "start": { "line": line, "character": character },
                "end": { "line": line, "character": character + 1 }
            },
            "context": { "diagnostics": [] }
        });
        let response = self.request("textDocument/codeAction", params).await?;
        if response.is_null() {
            return Ok(vec![]);
        }
        Ok(parse_code_actions(&response))
    }

    // ── 格式化 ───────────────────────────────────────────────────────

    /// 服务器是否声明支持 `textDocument/formatting`（documentFormattingProvider）。
    #[must_use]
    pub fn supports_formatting(&self) -> bool {
        self.server_info
            .get("capabilities")
            .and_then(|c| c.get("documentFormattingProvider"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// 请求整文档格式化（`textDocument/formatting`），返回 [`LspTextEdit`] 列表。
    ///
    /// 服务器不支持 formatting 时返回空向量。调用前须已 open/change document
    /// （服务器基于已同步内容格式化）。
    pub async fn formatting(&self, uri: &Url) -> Result<Vec<LspTextEdit>, LspError> {
        if !self.supports_formatting() {
            return Ok(vec![]);
        }
        let params = serde_json::json!({
            "textDocument": { "uri": uri.to_string() },
            "options": {}
        });
        let response = self.request("textDocument/formatting", params).await?;
        if response.is_null() {
            return Ok(vec![]);
        }
        Ok(parse_text_edits(&response))
    }

    /// 确保文档已 open 且内容同步：已 open 则 didChange，否则 didOpen。
    pub async fn ensure_document_open(
        &mut self,
        uri: &Url,
        text: &str,
        language_id: &str,
    ) -> Result<(), LspError> {
        if self.open_docs.contains_key(&uri.to_string()) {
            self.change_document(uri, text).await
        } else {
            self.open_document(uri, text, language_id).await
        }
    }

    // ── 公共访问器 ────────────────────────────────────────────────────

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn shutdown(self) -> Result<(), LspError> {
        self.transport
            .shutdown(Duration::from_secs(10))
            .await
            .map_err(LspError::Transport)
    }

    // ── 内部方法 ──────────────────────────────────────────────────────

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, LspError> {
        let id = self.next_id();
        self.transport
            .request(id, method, params, DEFAULT_TIMEOUT)
            .await
            .map_err(LspError::Transport)
    }

    async fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), LspError> {
        self.transport
            .notify(method, params)
            .await
            .map_err(LspError::Transport)
    }

    async fn collect_notifications(&mut self) {
        let notifications = self.transport.drain_notifications();
        let diags = Arc::clone(&self.diagnostics);
        for msg in notifications {
            if let Some(diag) = try_parse_diagnostics(&msg.payload) {
                diags.lock().await.insert(diag.0, diag.1);
            }
        }
    }
}

// ── 解析辅助函数 ───────────────────────────────────────────────────────

fn try_parse_diagnostics(payload: &str) -> Option<(String, Vec<LspDiagnostic>)> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    if v.get("method")?.as_str()? != "textDocument/publishDiagnostics" {
        return None;
    }
    let params = v.get("params")?;
    let uri = params.get("uri")?.as_str()?.to_string();
    let diags: Vec<LspDiagnostic> = params
        .get("diagnostics")?
        .as_array()?
        .iter()
        .map(|d| LspDiagnostic {
            severity: d.get("severity").and_then(|s| s.as_u64()).map(|n| match n {
                1 => DiagnosticSeverity::Error,
                2 => DiagnosticSeverity::Warning,
                3 => DiagnosticSeverity::Information,
                4 => DiagnosticSeverity::Hint,
                _ => DiagnosticSeverity::Information,
            }),
            message: d
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
            line: d
                .get("range")
                .and_then(|r| r.get("start"))
                .and_then(|s| s.get("line"))
                .and_then(|l| l.as_u64())
                .unwrap_or(0) as u32,
            character: d
                .get("range")
                .and_then(|r| r.get("start"))
                .and_then(|s| s.get("character"))
                .and_then(|c| c.as_u64())
                .unwrap_or(0) as u32,
            source: d.get("source").and_then(|s| s.as_str()).map(String::from),
            code: d.get("code").and_then(|c| {
                if let Some(s) = c.as_str() {
                    Some(s.to_string())
                } else if let Some(n) = c.as_i64() {
                    Some(n.to_string())
                } else {
                    None
                }
            }),
        })
        .collect();
    Some((uri, diags))
}

fn parse_locations(v: &serde_json::Value) -> Vec<LspLocation> {
    if let Some(arr) = v.as_array() {
        arr.iter().filter_map(parse_single_location).collect()
    } else {
        parse_single_location(v).into_iter().collect()
    }
}

fn parse_single_location(v: &serde_json::Value) -> Option<LspLocation> {
    // 可能是 Location 或 LocationLink
    let uri = v
        .get("uri")
        .or_else(|| v.get("targetUri"))
        .and_then(|u| u.as_str())?;
    let range = v.get("range").or_else(|| v.get("targetSelectionRange"))?;
    let start = range.get("start")?;
    Some(LspLocation {
        uri: uri.to_string(),
        line: start.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32,
        character: start.get("character").and_then(|c| c.as_u64()).unwrap_or(0) as u32,
    })
}

/// 解析 `textDocument/formatting` 响应为 [`LspTextEdit`] 列表。
fn parse_text_edits(v: &serde_json::Value) -> Vec<LspTextEdit> {
    let arr = match v.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    arr.iter()
        .filter_map(|e| {
            let range = e.get("range")?;
            let start = range.get("start")?;
            let end = range.get("end")?;
            Some(LspTextEdit {
                start_line: start.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32,
                start_character: start.get("character").and_then(|c| c.as_u64()).unwrap_or(0)
                    as u32,
                end_line: end.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32,
                end_character: end.get("character").and_then(|c| c.as_u64()).unwrap_or(0) as u32,
                new_text: e
                    .get("newText")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// 把 LSP 位置（行 + UTF-16 列）转换为字节偏移。
fn position_to_byte_offset(text: &str, line: u32, character: u32) -> usize {
    let mut offset = 0usize;
    let mut current_line = 0u32;
    // 跳到目标行首。
    for ch in text.chars() {
        if current_line == line {
            break;
        }
        offset += ch.len_utf8();
        if ch == '\n' {
            current_line += 1;
        }
    }
    // 在目标行内按 UTF-16 code units 推进。
    let mut utf16_seen = 0u32;
    for ch in text[offset..].chars() {
        if ch == '\n' || utf16_seen >= character {
            break;
        }
        let u16_len = ch.len_utf16() as u32;
        if utf16_seen + u16_len > character {
            break;
        }
        utf16_seen += u16_len;
        offset += ch.len_utf8();
    }
    offset
}

/// 把一组 [`LspTextEdit`] 应用到原文，返回完整结果文本。
///
/// edits 按 start 倒序应用，避免后续 edit 的偏移失效。
#[must_use]
pub fn apply_text_edits(text: &str, edits: &[LspTextEdit]) -> String {
    let mut spans: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            (
                position_to_byte_offset(text, e.start_line, e.start_character),
                position_to_byte_offset(text, e.end_line, e.end_character),
                e.new_text.as_str(),
            )
        })
        .collect();
    spans.sort_by(|a, b| b.0.cmp(&a.0));
    let mut result = text.to_string();
    for (start, end, new) in spans {
        if start <= end && end <= result.len() {
            result.replace_range(start..end, new);
        }
    }
    result
}

fn parse_hover(v: &serde_json::Value) -> LspHover {
    let contents = v
        .get("contents")
        .map(|c| match c {
            serde_json::Value::String(s) => vec![s.clone()],
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|e| {
                    if let Some(s) = e.as_str() {
                        Some(s.to_string())
                    } else {
                        e.get("value").and_then(|v| v.as_str()).map(String::from)
                    }
                })
                .collect(),
            serde_json::Value::Object(obj) => {
                vec![
                    obj.get("value")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                ]
            }
            _ => vec![],
        })
        .unwrap_or_default();

    let range = v.get("range");
    LspHover {
        contents,
        line: range
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("line"))
            .and_then(|l| l.as_u64())
            .map(|n| n as u32),
        character: range
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("character"))
            .and_then(|c| c.as_u64())
            .map(|n| n as u32),
    }
}

fn parse_symbols(v: &serde_json::Value, file_uri: &Url) -> Vec<LspSymbol> {
    let mut result = Vec::new();
    if let Some(arr) = v.as_array() {
        for elem in arr {
            if elem.get("range").is_some() && elem.get("selectionRange").is_some() {
                // DocumentSymbol (层次化)
                flatten_symbol(elem, file_uri, None, &mut result);
            } else if elem.get("location").is_some() {
                // SymbolInformation (扁平)
                if let Some(loc) = elem.get("location") {
                    let uri = loc.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                    let start = loc.get("range").and_then(|r| r.get("start"));
                    result.push(LspSymbol {
                        name: elem
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string(),
                        kind: format!(
                            "{:?}",
                            elem.get("kind").and_then(|k| k.as_u64()).unwrap_or(0)
                        ),
                        uri: uri.to_string(),
                        line: start
                            .and_then(|s| s.get("line"))
                            .and_then(|l| l.as_u64())
                            .unwrap_or(0) as u32,
                        character: start
                            .and_then(|s| s.get("character"))
                            .and_then(|c| c.as_u64())
                            .unwrap_or(0) as u32,
                        container: elem
                            .get("containerName")
                            .and_then(|c| c.as_str())
                            .map(String::from),
                    });
                }
            }
        }
    }
    result
}

fn flatten_symbol(
    v: &serde_json::Value,
    file_uri: &Url,
    container: Option<String>,
    result: &mut Vec<LspSymbol>,
) {
    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let kind = format!("{:?}", v.get("kind").and_then(|k| k.as_u64()).unwrap_or(0));
    let start = v.get("selectionRange").and_then(|r| r.get("start"));
    result.push(LspSymbol {
        name: name.to_string(),
        kind,
        uri: file_uri.to_string(),
        line: start
            .and_then(|s| s.get("line"))
            .and_then(|l| l.as_u64())
            .unwrap_or(0) as u32,
        character: start
            .and_then(|s| s.get("character"))
            .and_then(|c| c.as_u64())
            .unwrap_or(0) as u32,
        container: container.clone(),
    });
    if let Some(children) = v.get("children").and_then(|c| c.as_array()) {
        for child in children {
            flatten_symbol(child, file_uri, Some(name.to_string()), result);
        }
    }
}

fn parse_workspace_edit(v: &serde_json::Value) -> Vec<LspRenameEdit> {
    let mut edits = Vec::new();
    if let Some(changes) = v.get("changes").and_then(|c| c.as_object()) {
        for (uri, text_edits) in changes {
            if let Some(arr) = text_edits.as_array() {
                for edit in arr {
                    let start = edit.get("range").and_then(|r| r.get("start"));
                    edits.push(LspRenameEdit {
                        uri: uri.clone(),
                        line: start
                            .and_then(|s| s.get("line"))
                            .and_then(|l| l.as_u64())
                            .unwrap_or(0) as u32,
                        character: start
                            .and_then(|s| s.get("character"))
                            .and_then(|c| c.as_u64())
                            .unwrap_or(0) as u32,
                        new_text: edit
                            .get("newText")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                        old_text: String::new(),
                    });
                }
            }
        }
    }
    edits
}

fn parse_code_actions(v: &serde_json::Value) -> Vec<LspCodeAction> {
    let arr = match v.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    arr.iter()
        .filter_map(|action| {
            let title = action.get("title").and_then(|t| t.as_str())?.to_string();
            let kind = action
                .get("kind")
                .and_then(|k| k.as_str())
                .map(String::from);
            let is_preferred = action
                .get("isPreferred")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            let mut edits = Vec::new();
            if let Some(edit) = action.get("edit") {
                if let Some(changes) = edit.get("changes").and_then(|c| c.as_object()) {
                    for (uri, text_edits) in changes {
                        if let Some(arr) = text_edits.as_array() {
                            for e in arr {
                                let start = e.get("range").and_then(|r| r.get("start"));
                                edits.push(LspRenameEdit {
                                    uri: uri.clone(),
                                    line: start
                                        .and_then(|s| s.get("line"))
                                        .and_then(|l| l.as_u64())
                                        .unwrap_or(0)
                                        as u32,
                                    character: start
                                        .and_then(|s| s.get("character"))
                                        .and_then(|c| c.as_u64())
                                        .unwrap_or(0)
                                        as u32,
                                    new_text: e
                                        .get("newText")
                                        .and_then(|t| t.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    old_text: String::new(),
                                });
                            }
                        }
                    }
                }
            }
            Some(LspCodeAction {
                title,
                kind,
                is_preferred,
                edits,
            })
        })
        .collect()
}

// ── 错误类型 ───────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("传输错误: {0}")]
    Transport(#[from] TransportError),
    #[error("序列化失败: {0}")]
    Serialize(String),
    #[error("反序列化失败: {0}")]
    Deserialize(String),
    #[error("无效 URI: {0}")]
    InvalidUri(String),
    #[error("文档未打开: {0}")]
    DocumentNotOpen(String),
    #[error("服务器不支持: {0}")]
    Unsupported(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_single_edit_replaces_range() {
        let text = "fn main() {}\n";
        // 替换 "main" → "run"（第 1 行 0-based，列 3..7）。
        let edit = LspTextEdit {
            start_line: 0,
            start_character: 3,
            end_line: 0,
            end_character: 7,
            new_text: "run".into(),
        };
        assert_eq!(apply_text_edits(text, &[edit]), "fn run() {}\n");
    }

    #[test]
    fn apply_multiple_edits_in_reverse_order() {
        // 两个独立 edit，验证倒序应用使前面偏移不失效。
        let text = "aaa\nbbb\n";
        let e1 = LspTextEdit {
            start_line: 0,
            start_character: 0,
            end_line: 0,
            end_character: 3,
            new_text: "XXX".into(),
        };
        let e2 = LspTextEdit {
            start_line: 1,
            start_character: 0,
            end_line: 1,
            end_character: 3,
            new_text: "YYY".into(),
        };
        assert_eq!(apply_text_edits(text, &[e1, e2]), "XXX\nYYY\n");
    }

    #[test]
    fn apply_edit_with_multibyte_utf16() {
        // 「你」= U+4F60，UTF-16 占 1 unit、UTF-8 占 3 bytes。
        // "a你b"：列 0='a'，列 1='你'，列 2='b'。替换「你」→「好」（列 1..2）。
        let text = "a你b";
        let edit = LspTextEdit {
            start_line: 0,
            start_character: 1,
            end_line: 0,
            end_character: 2,
            new_text: "好".into(),
        };
        assert_eq!(apply_text_edits(text, &[edit]), "a好b");
    }

    #[test]
    fn apply_full_document_replace() {
        // formatting 常返回单 edit 覆盖全文（end 指向 EOF 行首）。
        let text = "a\nb\nc\n";
        let edit = LspTextEdit {
            start_line: 0,
            start_character: 0,
            end_line: 3,
            end_character: 0,
            new_text: "X\nY\n".into(),
        };
        assert_eq!(apply_text_edits(text, &[edit]), "X\nY\n");
    }

    #[test]
    fn position_offset_handles_multibyte_line() {
        // 行 0 "x你y"：列 2（'y'）字节偏移 = 1(x) + 3(你) = 4。
        let text = "x你y\nz\n";
        assert_eq!(position_to_byte_offset(text, 0, 2), 4);
        // 行 1 列 0 = "x你y\n" 后 = 'x'(1)+'你'(3)+'y'(1)+'\n'(1)=6。
        assert_eq!(position_to_byte_offset(text, 1, 0), 6);
    }

    /// 端到端：真实 rust-analyzer 测 `LspManager::format`。默认 ignored；CI 用 `--ignored` 跑。
    #[ignore = "需 rust-analyzer: cargo test -p agent-lsp -- --ignored format_end_to_end"]
    #[tokio::test]
    async fn format_end_to_end() {
        use crate::{LspManager, detect_servers};
        let root = std::env::current_dir().expect("cwd");
        let servers = detect_servers(&root);
        if servers.is_empty() {
            eprintln!("跳过：未检测到语言服务器（需 rust-analyzer）");
            return;
        }
        let mut mgr = LspManager::start(&root, &servers).await.expect("启动 LSP");
        let uri = url::Url::from_file_path(root.join("src/lib.rs")).expect("uri");
        let result = mgr.format(&uri).await;
        let _ = mgr.shutdown_all().await;
        assert!(result.is_ok(), "format 应 Ok(Some) 或 Ok(None)");
    }
}
