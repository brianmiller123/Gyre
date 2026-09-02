//! `hub` 工具：进程内消息总线（移植 oh-my-pi hub 工具的进程内子集）。
//!
//! 经 [`agent_core::hub::Hub`] 与在册代理互发消息：`send`（定向投递）、`recv`
//! （拉取自己的收件箱）、`list`（在册代理）。装配层把本工具 id 注册为 `"main"`，
//! 子 Agent（task 委派）经 [`Hub::register`] 以 `"task-<n>"` 入册后可被父模型定向
//! 消息（其消费逻辑在子 Agent 循环内——v1 由子任务描述自携带，跨进程 IPC 与回执
//! 协议留待后续，见 hub 模块文档）。

use std::sync::Arc;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::{Concurrency, Tool, ToolContext};

/// 当前代理在总线上的注册 id（装配层注入）。
#[derive(Clone)]
pub struct HubIdentity {
    id: String,
}

impl HubIdentity {
    /// 构造。
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }

    /// 当前代理 id。
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// `hub`：进程内消息总线工具。
pub struct HubTool {
    hub: Arc<agent_core::hub::Hub>,
    identity: HubIdentity,
    // execute 签名为 &self（Tool trait），收件箱须可共享——tokio Mutex 包裹。
    inbox: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<agent_core::hub::HubMessage>>,
}

impl HubTool {
    /// 构造并注册当前代理（收件箱随工具持有）。
    #[must_use]
    pub fn register(hub: Arc<agent_core::hub::Hub>, id: impl Into<String>) -> Self {
        let identity = HubIdentity::new(id);
        let inbox = hub.register(identity.id.clone());
        Self {
            hub,
            identity,
            inbox: tokio::sync::Mutex::new(inbox),
        }
    }

    /// 从既有构造（测试用：已注册的收件箱）。
    #[must_use]
    pub fn from_parts(
        hub: Arc<agent_core::hub::Hub>,
        identity: HubIdentity,
        inbox: tokio::sync::mpsc::UnboundedReceiver<agent_core::hub::HubMessage>,
    ) -> Self {
        Self {
            hub,
            identity,
            inbox: tokio::sync::Mutex::new(inbox),
        }
    }
}

impl Drop for HubTool {
    fn drop(&mut self) {
        self.hub.unregister(&self.identity.id);
    }
}

#[async_trait]
impl Tool for HubTool {
    fn name(&self) -> &'static str {
        "hub"
    }
    fn description(&self) -> &'static str {
        "进程内消息总线：向在册代理（如子 Agent）投递消息（send）、拉取自己的收件箱\
（recv）、列出在册代理（list）。适合多代理编排中的定向通信；消息不持久化。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "op": { "type": "string", "enum": ["send", "recv", "list"], "default": "list",
                        "description": "send 投递 / recv 拉取收件箱（drain）/ list 在册代理" },
                "to": { "type": "string", "description": "send：目标代理 id（list 可查）" },
                "message": { "type": "string", "description": "send：消息体（纯文本）" }
            },
            "required": ["op"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        // 仅进程内消息，不触工作区——read 级审批。
        CapabilityTier::ReadOnly
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let op = input
            .get("op")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("list");
        match op {
            "send" => {
                let to = input
                    .get("to")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| ToolError::InvalidArgs("send 需要 `to` 参数".into()))?;
                let message = input
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .ok_or_else(|| ToolError::InvalidArgs("send 需要 `message` 参数".into()))?;
                self.hub
                    .send(to, &self.identity.id, message)
                    .map_err(|e| ToolError::Execution(format!("hub send 失败: {e}")))?;
                Ok(ToolResult::text(format!(
                    "已投递给 {to}（来自 {}）",
                    self.identity.id
                )))
            }
            "recv" => {
                let mut msgs = Vec::new();
                let mut inbox = self.inbox.lock().await;
                while let Ok(m) = inbox.try_recv() {
                    msgs.push(format!("<{}> {}", m.from, m.body));
                }
                if msgs.is_empty() {
                    Ok(ToolResult::text("（收件箱为空）"))
                } else {
                    Ok(ToolResult::text(msgs.join("\n")))
                }
            }
            "list" => {
                let peers = self.hub.peers();
                if peers.is_empty() {
                    Ok(ToolResult::text("（总线上无在册代理）"))
                } else {
                    Ok(ToolResult::text(format!(
                        "在册代理（{}）：{}",
                        peers.len(),
                        peers.join(", ")
                    )))
                }
            }
            other => Err(ToolError::InvalidArgs(format!("未知 op：{other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ApprovalDecision, ApprovalPolicy, ApprovalRequest, AskMessage, AskResponse};
    use std::sync::Arc;

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
            context: None,
        }
    }

    #[tokio::test]
    async fn send_recv_list_roundtrip() {
        let hub = agent_core::hub::Hub::new().shared();
        // 子代理入册（持有收件箱但不在本测试消费——用 from_parts 保留其生命周期）。
        let sub_inbox = hub.register("task-1".into());
        let _keep = sub_inbox;

        let tool = HubTool::register(Arc::clone(&hub), "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = &ctx(&ws, &cancel);

        // list 看到两个在册代理。
        let out = tool
            .execute(serde_json::json!({"op": "list"}), c)
            .await
            .unwrap();
        let t = out.to_llm_text();
        assert!(t.contains("task-1") && t.contains("main"), "{t}");

        // send：main → task-1。
        let out = tool
            .execute(
                serde_json::json!({"op": "send", "to": "task-1", "message": "开始任务"}),
                c,
            )
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("已投递"),
            "{}",
            out.to_llm_text()
        );

        // 未注册目标报错。
        let err = tool
            .execute(
                serde_json::json!({"op": "send", "to": "nobody", "message": "x"}),
                c,
            )
            .await;
        assert!(err.is_err());

        // main 的收件箱：task-1 反投。
        hub.send("main", "task-1", "收到").unwrap();
        let out = tool
            .execute(serde_json::json!({"op": "recv"}), c)
            .await
            .unwrap();
        assert!(
            out.to_llm_text().contains("<task-1> 收到"),
            "{}",
            out.to_llm_text()
        );
        // 空箱。
        let out = tool
            .execute(serde_json::json!({"op": "recv"}), c)
            .await
            .unwrap();
        assert!(out.to_llm_text().contains("收件箱为空"));

        // drop 注销。
        drop(tool);
        assert_eq!(hub.peers(), vec!["task-1".to_string()]);
    }

    #[tokio::test]
    async fn missing_args_rejected() {
        let hub = agent_core::hub::Hub::new().shared();
        let tool = HubTool::register(hub, "main");
        let ws = agent_core::Workspace::new(std::env::temp_dir());
        let cancel = tokio_util::sync::CancellationToken::new();
        let err = tool
            .execute(serde_json::json!({"op": "send"}), &ctx(&ws, &cancel))
            .await;
        assert!(err.is_err());
        let err = tool
            .execute(serde_json::json!({"op": "unknown"}), &ctx(&ws, &cancel))
            .await;
        assert!(err.is_err());
    }
}
