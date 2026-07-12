//! list_files 工具：列目录条目（尊重 .gitignore）。

use std::path::Path;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::{Tool, ToolContext};

/// 列出目录条目（可选递归，尊重 .gitignore）。
pub struct ListFilesTool;

#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }
    fn description(&self) -> &str {
        "列出目录条目。recursive=false（默认）仅直接子项；true 递归所有文件（尊重 .gitignore）。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "目录路径（相对工作区，默认 .）" },
                "recursive": { "type": "boolean", "description": "是否递归（默认 false）" }
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
    ) -> Result<ToolResult, ToolError> {
        let rel = input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let recursive = input
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let root = ctx.workspace.resolve(Path::new(&rel));
        let items =
            tokio::task::spawn_blocking(move || agent_search::list_files(&root, recursive, 200))
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;
        if items.is_empty() {
            return Ok(ToolResult::text("（空目录或不存在）"));
        }
        let mut out = String::new();
        for p in items {
            out.push_str(&format!("{}\n", p.display()));
        }
        Ok(ToolResult::text(out))
    }
}
