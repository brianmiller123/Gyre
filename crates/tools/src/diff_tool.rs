//! apply_diff 工具：多块精准 SEARCH/REPLACE 编辑。

use std::path::Path;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::{find_unique_match, write_with_effects, MatchError, MatchMethod, Tool, ToolContext};

/// 多块精准 SEARCH/REPLACE 编辑（apply_diff）。
///
/// 每个 SEARCH 块须在文件中唯一匹配，替换为对应 REPLACE；一次可提交多个块。
pub struct ApplyDiffTool;

#[async_trait]
impl Tool for ApplyDiffTool {
    fn name(&self) -> &str {
        "apply_diff"
    }
    fn description(&self) -> &str {
        "对文件做多块精准编辑：每个 SEARCH 块须在文件中唯一匹配，替换为对应 REPLACE。一次可提交多个块。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "目标文件路径" },
                "diff": { "type": "string", "description": "差分文本：多个 SEARCH/REPLACE 块，每块以 7 个小于号起、7 个等号分隔、7 个大于号止；可选 :start_line: 与 ------- 元信息行" }
            },
            "required": ["path", "diff"]
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
        let diff = input
            .get("diff")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `diff`".into()))?;
        let blocks = parse_diff_blocks(diff);
        if blocks.is_empty() {
            return Err(ToolError::InvalidArgs(
                "diff 不含任何 SEARCH/REPLACE 块".into(),
            ));
        }
        let full = ctx.workspace.resolve(Path::new(path));
        let mut text = tokio::fs::read_to_string(&full)
            .await
            .map_err(ToolError::Io)?;
        let opts = crate::fuzzy_match::resolve_opts();
        let mut fuzzy_used = false;
        let mut min_sim = 1.0_f64;
        for (i, (search, replace)) in blocks.iter().enumerate() {
            let outcome = find_unique_match(&text, search, &opts).map_err(|e| match e {
                MatchError::NotFound => {
                    ToolError::Execution(format!("块 {} 的 SEARCH 未在 {} 匹配", i + 1, path))
                }
                MatchError::Ambiguous(c) => ToolError::Execution(format!(
                    "块 {} 的 SEARCH 在 {} 匹配 {c} 处，需唯一",
                    i + 1,
                    path
                )),
            })?;
            if outcome.method != MatchMethod::Exact {
                fuzzy_used = true;
                min_sim = min_sim.min(outcome.similarity);
                // fuzzy 命中：按实际匹配位置的缩进自适应调整 replace。
                let actual = &text[outcome.byte_start..outcome.byte_end];
                let adjusted = crate::fuzzy_match::adjust_indentation(search, actual, replace);
                text.replace_range(outcome.byte_start..outcome.byte_end, &adjusted);
            } else {
                text.replace_range(outcome.byte_start..outcome.byte_end, replace);
            }
        }
        let report = write_with_effects(&full, &text, ctx).await?;
        let mut msg = format!("已在 {path} 应用 {} 个编辑块", blocks.len());
        if fuzzy_used {
            msg.push_str(&format!("；含模糊匹配（最低相似度 {min_sim:.2}）"));
        }
        msg.push_str(&report.effect_suffix());
        Ok(ToolResult::text(msg))
    }
}

/// 解析 diff 文本为 `(search, replace)` 块列表。
///
/// 状态机：遇 `<<<<<<<` 进入 search，遇 `=======` 切到 replace，遇 `>>>>>>>` 收块。
/// 跳过 `:start_line:` 与 `-------` 元信息行。search/replace 内容保留原始缩进。
fn parse_diff_blocks(diff: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut mode = 0u8; // 0=外, 1=search, 2=replace
    let mut search = String::new();
    let mut replace = String::new();
    for line in diff.lines() {
        let t = line.trim();
        if t.starts_with("<<<<<<<") {
            mode = 1;
            search.clear();
            replace.clear();
        } else if t.starts_with("=======") && mode == 1 {
            mode = 2;
        } else if t.starts_with(">>>>>>>") && mode == 2 {
            blocks.push((std::mem::take(&mut search), std::mem::take(&mut replace)));
            mode = 0;
        } else if mode == 1 {
            if t.starts_with(":start_line:") || t == "-------" {
                continue;
            }
            if !search.is_empty() {
                search.push('\n');
            }
            search.push_str(line);
        } else if mode == 2 {
            if !replace.is_empty() {
                replace.push('\n');
            }
            replace.push_str(line);
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriteFileTool;
    use agent_core::Workspace;

    fn dummy_ctx<'a>(ws: &'a Workspace) -> ToolContext<'a> {
        use agent_core::{ApprovalDecision, ApprovalRequest};
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
        ToolContext {
            workspace: ws,
            approval: &AutoApprove,
            cancel,
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
        }
    }

    fn tmp() -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("agent-diff-{}-{:#x}", std::process::id(), nano()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn nano() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[tokio::test]
    async fn multi_block_replace() {
        let dir = tmp();
        let ws = Workspace::new(&dir);
        let ctx = dummy_ctx(&ws);
        WriteFileTool
            .execute(
                serde_json::json!({ "path": "d.txt", "content": "alpha\nbeta\ngamma\n" }),
                &ctx,
            )
            .await
            .unwrap();
        let diff = "<<<<<<< SEARCH\nalpha\n=======\nALPHA\n>>>>>>> REPLACE\n<<<<<<< SEARCH\ngamma\n=======\nGAMMA\n>>>>>>> REPLACE";
        let res = ApplyDiffTool
            .execute(serde_json::json!({ "path": "d.txt", "diff": diff }), &ctx)
            .await
            .unwrap();
        assert!(matches!(res, ToolResult::Text(_)));
        let after = tokio::fs::read_to_string(dir.join("d.txt")).await.unwrap();
        assert_eq!(after, "ALPHA\nbeta\nGAMMA\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ambiguous_errors() {
        let dir = tmp();
        let ws = Workspace::new(&dir);
        let ctx = dummy_ctx(&ws);
        WriteFileTool
            .execute(
                serde_json::json!({ "path": "e.txt", "content": "dup\ndup\n" }),
                &ctx,
            )
            .await
            .unwrap();
        let diff = "<<<<<<< SEARCH\ndup\n=======\nX\n>>>>>>> REPLACE";
        let res = ApplyDiffTool
            .execute(serde_json::json!({ "path": "e.txt", "diff": diff }), &ctx)
            .await;
        assert!(res.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_skips_metadata_and_preserves_indent() {
        let diff =
            "<<<<<<< SEARCH\n:start_line:1\n-------\n    foo\n=======\n    bar\n>>>>>>> REPLACE";
        let blocks = parse_diff_blocks(diff);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0, "    foo");
        assert_eq!(blocks[0].1, "    bar");
    }

    #[test]
    fn parse_multiple_blocks() {
        let diff = "<<<<<<< SEARCH\na\n=======\nA\n>>>>>>> REPLACE\n<<<<<<< SEARCH\nb\n=======\nB\n>>>>>>> REPLACE";
        let blocks = parse_diff_blocks(diff);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], ("a".to_string(), "A".to_string()));
        assert_eq!(blocks[1], ("b".to_string(), "B".to_string()));
    }
}
