//! 搜索工具：grep / glob（包装 agent-search，移植 pi-natives）。

use std::path::Path;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::{Tool, ToolContext};

/// 正则搜索文件内容（尊重 .gitignore）。
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "在工作区正则搜索文件内容（尊重 .gitignore）。返回命中文件、行号与行文本。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern":   { "type": "string", "description": "正则表达式" },
                "path":      { "type": "string", "description": "搜索根（相对工作区，默认 .）" },
                "highlight": { "type": "boolean", "default": false, "description": "是否用 ANSI 高亮命中子串" }
            },
            "required": ["pattern"]
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
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `pattern`".into()))?
            .to_string();
        let highlight = input
            .get("highlight")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // pattern 会被 move 进 spawn_blocking 闭包；另留一份用于命中高亮。
        let pattern_for_hl = pattern.clone();
        let rel = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let root = ctx.workspace.resolve(Path::new(rel));
        // 阻塞式遍历走 spawn_blocking，避免占用异步运行时
        let hits = tokio::task::spawn_blocking(move || agent_search::grep(&root, &pattern, 50))
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?
            .map_err(ToolError::Execution)?;
        if hits.is_empty() {
            return Ok(ToolResult::text("无匹配".to_string()));
        }
        let mut out = String::new();
        for h in hits {
            // 可选高亮：把命中子串用 ANSI 红色加粗包裹，便于终端阅读
            let text = if highlight {
                agent_search::highlight_match(&h.text, &pattern_for_hl)
                    .unwrap_or_else(|_| h.text.clone())
            } else {
                h.text.clone()
            };
            out.push_str(&format!("{}:{}: {}\n", h.path.display(), h.line, text));
        }
        Ok(ToolResult::text(out))
    }
}

/// 按 glob 模式发现文件。
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "按 glob 模式（如 **/*.rs）发现工作区内文件路径。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "glob 模式" }
            },
            "required": ["pattern"]
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
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `pattern`".into()))?
            .to_string();
        let root = ctx.workspace.root().to_path_buf();
        let files =
            tokio::task::spawn_blocking(move || agent_search::glob_match(&root, &pattern, 100))
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?
                .map_err(ToolError::Execution)?;
        if files.is_empty() {
            return Ok(ToolResult::text("无匹配文件".to_string()));
        }
        Ok(ToolResult::text(
            files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}
