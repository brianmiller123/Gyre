//! `lsp_apply` 工具：把 LSP 编辑（rename / code actions / formatting 返回的编辑列表）
//! 应用到磁盘文件。与只读的 `lsp` 工具互补，属写入类操作（需审批）。
//!
//! 模型工作流：`lsp {action: rename|code_actions, ...}` 预览 → `lsp_apply {edits: [...]}`
//! 应用（或 `{code_action_index}` 直接应用服务器 code action 的 edits/command）。
//!
//! 应用经 `agent_lsp::apply_text_edits`（重叠校验 + 自底向上 + UTF-16 位置换算），
//! 文件按 uri 分组后逐文件读改写（写盘前校验旧文本与 `old_text` 一致，防漂移）。

use std::collections::BTreeMap;
use std::path::PathBuf;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use agent_lsp::client::LspRenameEdit;
use agent_lsp::edits::apply_text_edits;
use async_trait::async_trait;
use serde_json::json;

use super::{LspPool, Tool, ToolContext, write_with_effects};

/// `lsp_apply`：应用 LSP 编辑到文件。
pub struct LspApplyTool {
    managers: LspPool,
}

impl LspApplyTool {
    /// 共享 `lsp` 工具的同一套语言服务器池。
    #[must_use]
    pub const fn new(pool: LspPool) -> Self {
        Self { managers: pool }
    }
}

#[async_trait]
impl Tool for LspApplyTool {
    fn name(&self) -> &str {
        "lsp_apply"
    }
    fn description(&self) -> &str {
        "应用 LSP 编辑（write_file 之外的结构化编辑）：\
`edits` 直接传入 lsp rename/code_actions 返回的编辑列表；或 `code_action_index` + `uri`/`line`/`character`\
取对应 code action 的 edits 应用、有 command 则执行。多文件自动分组，自底向上应用，区间重叠报错。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "edits": { "type": "array",
                           "description": "LSP 编辑列表（lsp rename/code_actions 输出原样回传）",
                           "items": { "type": "object" } },
                "code_action_index": { "type": "integer", "minimum": 0,
                                       "description": "按 code_actions 返回列表的下标应用（与 uri/line/character 配合）" },
                "uri":      { "type": "string", "description": "文件 URI（code_action_index 模式）" },
                "line":     { "type": "integer", "description": "行（code_action_index 模式，0-based）" },
                "character": { "type": "integer", "description": "列（code_action_index 模式，0-based）" }
            }
        })
    }
    fn capability(&self) -> CapabilityTier {
        CapabilityTier::Write
    }

    #[allow(clippy::too_many_lines, clippy::significant_drop_tightening)] // edits 直传 / code action 两条路径共享分组应用逻辑；tokio MutexGuard 跨 await 持锁是设计意图
    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        // 模式 1：显式 edits。
        #[allow(clippy::single_match_else)] // 两条模式分支差异大，match 结构清晰
        let edits: Vec<LspRenameEdit> = match input.get("edits") {
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| ToolError::InvalidArgs(format!("edits 解析失败: {e}")))?,
            None => {
                // 模式 2：按 code action 下标应用（重取 code actions → edits 或 command）。
                let idx = input
                    .get("code_action_index")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| {
                        ToolError::InvalidArgs("需要 `edits` 或 `code_action_index`".into())
                    })?;
                let idx = usize::try_from(idx).map_err(|_| {
                    ToolError::InvalidArgs("code_action_index 超出平台范围".into())
                })?;
                let uri = input
                    .get("uri")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| ToolError::InvalidArgs("缺少 `uri` 参数".into()))?
                    .parse::<url::Url>()
                    .map_err(|e| ToolError::InvalidArgs(format!("无效 URI: {e}")))?;
                let line = u32::try_from(
                    input
                        .get("line")
                        .and_then(serde_json::Value::as_u64)
                        .ok_or_else(|| ToolError::InvalidArgs("缺少 `line` 参数".into()))?,
                )
                .map_err(|_| ToolError::InvalidArgs("line 超出范围".into()))?;
                let character = u32::try_from(
                    input
                        .get("character")
                        .and_then(serde_json::Value::as_u64)
                        .ok_or_else(|| ToolError::InvalidArgs("缺少 `character` 参数".into()))?,
                )
                .map_err(|_| ToolError::InvalidArgs("character 超出范围".into()))?;
                // 锁内获取管理器并调用（tokio::Mutex 可跨 await 持锁）。
                let mut managers = self.managers.lock().await;
                let root = find_root(&managers, &uri_path(&uri));
                let Some(root) = root else {
                    return Err(ToolError::Execution(
                        "未找到匹配工作区的 LSP 管理器（先确认 lsp 工具已初始化）".into(),
                    ));
                };
                let manager = managers.get_mut(&root).ok_or_else(|| {
                    ToolError::Execution("LSP 管理器缺失（内部错误）".into())
                })?;
                let actions = manager
                    .code_actions(&uri, line, character)
                    .await
                    .map_err(|e| ToolError::Execution(format!("获取代码操作失败: {e}")))?;
                let action = actions.get(idx).ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "code_action_index {idx} 越界（共 {} 个）",
                        actions.len()
                    ))
                })?;
                if let Some(cmd) = &action.command {
                    let resp = manager
                        .execute_command(&uri, &cmd.command, &cmd.arguments)
                        .await
                        .map_err(|e| ToolError::Execution(format!("执行命令失败: {e}")))?;
                    return Ok(ToolResult::text(format!(
                        "已执行 code action「{}」的命令 `{}`（响应：{}）",
                        action.title,
                        cmd.command,
                        serde_json::to_string(&resp).unwrap_or_else(|_| "null".into())
                    )));
                }
                action.edits.clone()
            }
        };

        if edits.is_empty() {
            return Ok(ToolResult::text("无编辑可应用"));
        }

        // 按 uri 分组，逐文件读改写（写盘前校验 old_text 一致性，防漂移）。
        let mut by_uri: BTreeMap<String, Vec<LspRenameEdit>> = BTreeMap::new();
        for e in edits {
            by_uri.entry(e.uri.clone()).or_default().push(e);
        }
        let mut applied = 0usize;
        let mut failures = Vec::new();
        for (uri, group) in &by_uri {
            let path = agent_lsp::edits::uri_to_path(uri);
            let full = ctx.workspace.resolve(std::path::Path::new(&path));
            let text = match tokio::fs::read_to_string(&full).await {
                Ok(t) => t,
                Err(e) => {
                    failures.push(format!("{path}: 读取失败: {e}"));
                    continue;
                }
            };
            // old_text 漂移校验（old_text 非空时）。
            let mut drift = None;
            for e in group {
                if !e.old_text.is_empty() {
                    if let Some(pos) = text.find(&e.old_text) {
                        if pos != utf16_pos(&text, e.line, e.character) {
                            drift = Some(format!(
                                "{path}: 旧文本与磁盘内容不一致（编辑基于过期快照）"
                            ));
                            break;
                        }
                    }
                }
            }
            if let Some(d) = drift {
                failures.push(d);
                continue;
            }
            let new_text = match apply_text_edits(&text, group) {
                Ok(t) => t,
                Err(e) => {
                    failures.push(format!("{path}: {e}"));
                    continue;
                }
            };
            if new_text == text {
                continue;
            }
            if let Err(e) = write_with_effects(&full, &new_text, ctx).await {
                failures.push(format!("{path}: 写入失败: {e}"));
                continue;
            }
            applied += 1;
        }
        let mut msg = format!("已应用 {applied} 个文件的 LSP 编辑");
        if !failures.is_empty() {
            msg.push_str("；失败：");
            msg.push_str(&failures.join("；"));
        }
        Ok(ToolResult::text(msg))
    }
}

/// file:// URI → 本地路径（浅解码）。
fn uri_path(uri: &url::Url) -> std::path::PathBuf {
    uri.to_file_path().unwrap_or_else(|_| {
        std::path::PathBuf::from(uri.as_str().trim_start_matches("file://"))
    })
}

/// 为路径找最深的已注册工作区根。
fn utf16_pos(text: &str, line: u32, character: u32) -> usize {
    let mut offset = 0usize;
    for _ in 0..line {
        match text[offset..].find('\n') {
            Some(p) => offset += p + 1,
            None => return text.len(),
        }
    }
    let line_text = &text[offset..text[offset..].find('\n').map_or(text.len(), |p| offset + p)];
    let mut units = 0u32;
    for (off, ch) in line_text.char_indices() {
        if units >= character {
            return offset + off;
        }
        units += ch.len_utf16() as u32;
    }
    offset + line_text.len()
}

/// 为路径找最深的已注册工作区根。
fn find_root<K>(
    managers: &std::collections::HashMap<PathBuf, K>,
    path: &std::path::Path,
) -> Option<PathBuf> {
    managers
        .keys()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_pos_basic() {
        let text = "ab\ncd";
        assert_eq!(utf16_pos(text, 0, 0), 0);
        assert_eq!(utf16_pos(text, 0, 1), 1);
        assert_eq!(utf16_pos(text, 1, 0), 3);
        assert_eq!(utf16_pos(text, 1, 2), 5);
    }

    #[test]
    fn find_root_deepest() {
        let mut m: std::collections::HashMap<PathBuf, ()> = std::collections::HashMap::new();
        let r1 = PathBuf::from("/ws");
        let r2 = PathBuf::from("/ws/sub");
        m.insert(r1.clone(), ());
        m.insert(r2.clone(), ());
        let p = std::path::Path::new("/ws/sub/file.rs");
        assert_eq!(find_root(&m, p), Some(r2));
    }
}
