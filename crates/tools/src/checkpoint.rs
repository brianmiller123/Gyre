//! `checkpoint` / `rewind` 会话回卷工具（移植 oh-my-pi checkpoint/rewind 语义）。
//!
//! - `checkpoint`：在**当前活跃分支位置**打一个检查点（记录活跃叶子节点 id + 节点数 +
//!   说明），单活跃不变量（新检查点覆盖旧检查点，omp 同语义）。
//! - `rewind`：回卷到检查点——经 [`ContextManager::switch_branch_with_handoff`] 把
//!   检查点之后的分支后缀折叠为 handoff 摘要并继续（等价 omp「替换上下文 + 分支摘要」）；
//!   回卷消耗检查点。无检查点时返回可恢复错误。
//!
//! 状态经 [`CheckpointState`] 在装配层构造一次（`checkpoint` 与 `rewind` 共享），
//! 跨 Agent 重建存活（/model、/mode 切换不丢）。
//!
//! 与 omp 的差异（有意简化）：omp 把回卷副作用延迟到 `turn_end；Gyre` 在工具内同步
//! 回卷（下一次 `build_provider_context` 即反映新分支），中间流式增量仍属旧分支——
//! 对模型无副作用（回卷后不再引用）。

use std::sync::{Arc, Mutex};

use agent_core::{CapabilityTier, NodeId, ToolError, ToolResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Concurrency, Tool, ToolContext};

/// 单活跃检查点（omp 语义：新检查点覆盖旧的）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// 检查点处的活跃叶子节点 id。
    pub leaf: NodeId,
    /// 检查点处的会话节点总数（供状态展示）。
    pub node_count: usize,
    /// 检查点说明（模型自注，如「编辑前的干净基线」）。
    pub note: String,
}

/// 检查点共享状态（checkpoint / rewind 两工具 + REPL 共用）。
pub struct CheckpointState {
    inner: Mutex<Option<Checkpoint>>,
}

impl CheckpointState {
    /// 空状态（无检查点）。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// 共享句柄。
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// 当前检查点（无则 `None`）。
    #[must_use]
    pub fn current(&self) -> Option<Checkpoint> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 覆盖式写入（单活跃）。
    fn set(&self, cp: Checkpoint) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = Some(cp);
    }

    /// 取出并清除（rewind 消耗）。
    fn take(&self) -> Option<Checkpoint> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

impl Default for CheckpointState {
    fn default() -> Self {
        Self::new()
    }
}

/// `checkpoint`：在当前位置打检查点（供探索后回卷）。
pub struct CheckpointTool {
    state: Arc<CheckpointState>,
}

impl CheckpointTool {
    /// 绑定共享状态。
    #[must_use]
    pub const fn new(state: Arc<CheckpointState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for CheckpointTool {
    fn name(&self) -> &'static str {
        "checkpoint"
    }
    fn description(&self) -> &'static str {
        "在当前对话位置打一个检查点（单活跃：新的覆盖旧的）。适合在开始一段探索性改动前\
建立基线；走偏后可用 rewind 回卷到检查点（被回卷的消息折叠为摘要）。note 说明该位置的意义。"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "note": { "type": "string", "description": "检查点说明（如「重构前基线」）" },
                "status": { "type": "boolean", "description": "true 时仅查看当前检查点，不创建" }
            }
        })
    }
    fn capability(&self) -> CapabilityTier {
        // 仅记录会话位置，不触工作区——read 级审批。
        CapabilityTier::ReadOnly
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        if input
            .get("status")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(ToolResult::text(match self.state.current() {
                Some(cp) => format!(
                    "当前检查点 @{}（{} 节点）：{}",
                    cp.leaf, cp.node_count, cp.note
                ),
                None => "当前无检查点".to_string(),
            }));
        }
        let Some(context) = ctx.context else {
            return Ok(ToolResult::Error {
                recoverable: true,
                message: "会话上下文不可用（当前宿主未注入 ContextManager）".into(),
            });
        };
        let Some(leaf) = context.active_leaf().await else {
            return Ok(ToolResult::Error {
                recoverable: true,
                message: "当前会话不支持分支（线性上下文），checkpoint 不可用".into(),
            });
        };
        let nodes = context.snapshot_nodes().await.len();
        let note = input
            .get("note")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("（未说明）")
            .to_string();
        let cp = Checkpoint {
            leaf,
            node_count: nodes,
            note: note.clone(),
        };
        self.state.set(cp.clone());
        Ok(ToolResult::text(format!(
            "检查点已创建 @{}（{} 节点，{note}）。走偏后可用 rewind 回卷。",
            cp.leaf, cp.node_count
        )))
    }
}

/// `rewind`：回卷到检查点（消耗检查点）。
pub struct RewindTool {
    state: Arc<CheckpointState>,
}

impl RewindTool {
    /// 绑定共享状态（与 checkpoint 同一实例）。
    #[must_use]
    pub const fn new(state: Arc<CheckpointState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for RewindTool {
    fn name(&self) -> &'static str {
        "rewind"
    }
    fn description(&self) -> &'static str {
        "回卷到最近一次 checkpoint 的位置：检查点之后的消息折叠为 handoff 摘要并继续。\
没有检查点时返回错误。回卷消耗检查点（需重新 checkpoint）。适合探索走偏后的整体回退。"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "confirm": { "type": "boolean",
                             "description": "确认回卷（防误触；true 才执行）" }
            }
        })
    }
    fn capability(&self) -> CapabilityTier {
        // 会话状态管理（不触工作区），read 级审批；但会改变对话分支——Exclusive 屏障。
        CapabilityTier::ReadOnly
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let confirm = input
            .get("confirm")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !confirm {
            return Ok(ToolResult::Error {
                recoverable: true,
                message: "rewind 需要 confirm:true 确认（回卷会折叠检查点后的消息）".into(),
            });
        }
        let Some(cp) = self.state.take() else {
            return Ok(ToolResult::Error {
                recoverable: true,
                message: "当前无检查点——先调用 checkpoint 再 rewind".into(),
            });
        };
        let Some(context) = ctx.context else {
            // 检查点已被消耗：恢复原状（失败路径不留半状态）。
            self.state.set(cp);
            return Ok(ToolResult::Error {
                recoverable: true,
                message: "会话上下文不可用（当前宿主未注入 ContextManager）".into(),
            });
        };
        match context.switch_branch_with_handoff(&cp.leaf).await {
            Ok(true) => Ok(ToolResult::text(format!(
                "已回卷到检查点 @{}（{}）：被回卷的消息已折叠为摘要。",
                cp.leaf, cp.note
            ))),
            Ok(false) => {
                self.state.set(cp.clone());
                Ok(ToolResult::Error {
                    recoverable: true,
                    message: format!("回卷目标 @{} 不存在或会话不支持分支", cp.leaf),
                })
            }
            Err(e) => {
                self.state.set(cp.clone());
                Ok(ToolResult::Error {
                    recoverable: true,
                    message: format!("回卷失败: {e}"),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{
        ApprovalDecision, ApprovalPolicy, ApprovalRequest, AskMessage, AskResponse, ContextError,
        ContextManager, SessionNode, TokenUsage,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 脚本化 ContextManager：记录 `active_leaf` 结果与 switch 调用。
    struct ScriptContext {
        leaf: String,
        switched: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ContextManager for ScriptContext {
        async fn append(&self, _m: agent_core::AgentMessage) {}
        async fn set_system(&self, _s: Vec<String>, _t: &[agent_core::ToolSpec]) {}
        async fn build_provider_context(
            &self,
            _m: &agent_core::Model,
            _t: &[agent_core::ToolSpec],
        ) -> Result<agent_core::ProviderContext, ContextError> {
            Err(ContextError::Compaction("n/a".into()))
        }
        async fn compact(&self, _s: agent_core::CompactionStrategy) -> Result<(), ContextError> {
            Ok(())
        }
        fn token_usage(&self) -> TokenUsage {
            TokenUsage::default()
        }
        fn prefix_fingerprint(&self) -> String {
            "x".into()
        }
        async fn active_leaf(&self) -> Option<NodeId> {
            Some(self.leaf.clone())
        }
        async fn snapshot_nodes(&self) -> Vec<SessionNode> {
            vec![
                SessionNode {
                    id: "n1".into(),
                    parent_id: None,
                    message: agent_core::AgentMessage::User(agent_core::UserMessage::from_text(
                        "a",
                    )),
                },
                SessionNode {
                    id: "n2".into(),
                    parent_id: Some("n1".into()),
                    message: agent_core::AgentMessage::User(agent_core::UserMessage::from_text(
                        "b",
                    )),
                },
            ]
        }
        async fn switch_branch_with_handoff(
            &self,
            new_leaf: &NodeId,
        ) -> Result<bool, ContextError> {
            self.switched.fetch_add(1, Ordering::SeqCst);
            Ok(new_leaf == "L1")
        }
        async fn list_leaves(&self) -> Vec<NodeId> {
            vec![]
        }
        async fn children_of(&self, _id: &NodeId) -> Vec<NodeId> {
            vec![]
        }
    }

    struct NoopApproval;
    #[async_trait::async_trait]
    impl ApprovalPolicy for NoopApproval {
        fn decide(&self, _r: &ApprovalRequest<'_>) -> ApprovalDecision {
            ApprovalDecision::Allow
        }
        async fn prompt(&self, _a: &AskMessage) -> Result<AskResponse, ToolError> {
            Ok(AskResponse::Yes)
        }
    }

    fn ctx<'a>(
        ws: &'a agent_core::Workspace,
        cancel: &'a tokio_util::sync::CancellationToken,
        context: Option<&'a dyn ContextManager>,
    ) -> ToolContext<'a> {
        ToolContext {
            workspace: ws,
            approval: &NoopApproval,
            cancel,
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
            update_tx: None,
            conflicts: None,
            pending_rewrites: None,
            context,
        }
    }

    #[tokio::test]
    async fn checkpoint_records_position_and_overwrites() {
        let state = CheckpointState::new().shared();
        let tool = CheckpointTool::new(Arc::clone(&state));
        let sc = ScriptContext {
            leaf: "L1".into(),
            switched: AtomicUsize::new(0),
        };
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let out = tool
            .execute(
                serde_json::json!({"note": "基线"}),
                &ctx(&ws, &cancel, Some(&sc)),
            )
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("检查点已创建 @L1"),
            "{}",
            out.to_llm_text()
        );
        let cp = state.current().unwrap();
        assert_eq!(cp.leaf, "L1");
        assert_eq!(cp.node_count, 2);
        assert_eq!(cp.note, "基线");
        // 新检查点覆盖旧的。
        tool.execute(
            serde_json::json!({"note": "v2"}),
            &ctx(&ws, &cancel, Some(&sc)),
        )
        .await
        .unwrap();
        assert_eq!(state.current().unwrap().note, "v2");
        // status 只读。
        let out = tool
            .execute(
                serde_json::json!({"status": true}),
                &ctx(&ws, &cancel, Some(&sc)),
            )
            .await
            .unwrap();
        assert!(out.to_llm_text().contains("当前检查点 @L1"));
        assert_eq!(state.current().unwrap().note, "v2", "status 不应改动状态");
    }

    #[tokio::test]
    async fn checkpoint_requires_context() {
        let state = CheckpointState::new().shared();
        let tool = CheckpointTool::new(state);
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let out = tool
            .execute(serde_json::json!({}), &ctx(&ws, &cancel, None))
            .await
            .unwrap();
        assert!(matches!(out, ToolResult::Error { .. }));
    }

    #[tokio::test]
    async fn rewind_requires_confirm_and_consumes() {
        let state = CheckpointState::new().shared();
        let rw = RewindTool::new(Arc::clone(&state));
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        // 无检查点。
        let out = rw
            .execute(
                serde_json::json!({"confirm": true}),
                &ctx(&ws, &cancel, None),
            )
            .await
            .unwrap();
        assert!(matches!(out, ToolResult::Error { .. }));
        // 无 confirm。
        let sc = ScriptContext {
            leaf: "L1".into(),
            switched: AtomicUsize::new(0),
        };
        let cp_tool = CheckpointTool::new(Arc::clone(&state));
        cp_tool
            .execute(
                serde_json::json!({"note": "x"}),
                &ctx(&ws, &cancel, Some(&sc)),
            )
            .await
            .unwrap();
        let out = rw
            .execute(serde_json::json!({}), &ctx(&ws, &cancel, Some(&sc)))
            .await
            .unwrap();
        assert!(matches!(out, ToolResult::Error { .. }));
        assert!(state.current().is_some(), "未确认时检查点应保留");
        // 确认回卷。
        let out = rw
            .execute(
                serde_json::json!({"confirm": true}),
                &ctx(&ws, &cancel, Some(&sc)),
            )
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("已回卷到检查点 @L1"),
            "{}",
            out.to_llm_text()
        );
        assert_eq!(sc.switched.load(Ordering::SeqCst), 1);
        assert!(state.current().is_none(), "回卷应消耗检查点");
    }

    #[tokio::test]
    async fn rewind_failure_restores_checkpoint() {
        let state = CheckpointState::new().shared();
        let rw = RewindTool::new(Arc::clone(&state));
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let sc = ScriptContext {
            leaf: "L9".into(), // 切换目标不存在（switch 返回 false）
            switched: AtomicUsize::new(0),
        };
        let cp_tool = CheckpointTool::new(Arc::clone(&state));
        cp_tool
            .execute(
                serde_json::json!({"note": "x"}),
                &ctx(&ws, &cancel, Some(&sc)),
            )
            .await
            .unwrap();
        let out = rw
            .execute(
                serde_json::json!({"confirm": true}),
                &ctx(&ws, &cancel, Some(&sc)),
            )
            .await
            .unwrap();
        assert!(matches!(out, ToolResult::Error { .. }));
        assert!(state.current().is_some(), "失败路径应恢复检查点");
    }
}
