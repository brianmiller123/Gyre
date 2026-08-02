//! `eval` 工具：向 LLM 暴露持久 Python 内核。

use std::sync::Arc;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use agent_tools::{Concurrency, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{BridgeServer, EvalError, EvalManager, EvalOutput};

/// `eval` 工具：在持久 Python 内核中执行代码，内核内可经 `tool.<name>` 回调宿主工具。
///
/// 执行经 [`EvalManager::execute`] 落盘；`ctx.cancel` 触发时中止（kill 内核并注销会话）。
/// 环回桥**懒加载**：首次 execute 时按构造时注入的 registry/workspace/approval 在
/// 127.0.0.1 随机端口 spawn，`tokio::sync::OnceCell` 缓存复用——避免同步装配闭包内
/// 无法 await spawn，且 `/mode` 热切换重建 Agent 时新工具实例自然携带新审批策略。
pub struct EvalTool {
    manager: Arc<EvalManager>,
    /// 会话标签（cli 建议传 agent session_id；与 workspace 根拼成 session_key）。
    session: String,
    /// 懒加载桥（首次 execute spawn，之后复用）。
    bridge: tokio::sync::OnceCell<Arc<BridgeServer>>,
    /// 桥回调宿主工具用的工具注册表快照（不含 eval 自身，防止自我递归）。
    registry: Arc<dyn agent_tools::ToolRegistry>,
    /// 桥回调宿主工具用的工作区。
    workspace: Arc<agent_core::Workspace>,
    /// 桥回调宿主工具用的审批策略（构造时注入的当前 mode 审批）。
    approval: Arc<dyn agent_core::ApprovalPolicy>,
}

impl EvalTool {
    /// 构造（会话标签默认 `"default"`）。
    #[must_use]
    pub fn new(
        manager: Arc<EvalManager>,
        registry: Arc<dyn agent_tools::ToolRegistry>,
        workspace: Arc<agent_core::Workspace>,
        approval: Arc<dyn agent_core::ApprovalPolicy>,
    ) -> Self {
        Self {
            manager,
            session: "default".to_string(),
            bridge: tokio::sync::OnceCell::new(),
            registry,
            workspace,
            approval,
        }
    }

    /// 构造并指定会话标签（session_key = `"{workspace 根}|{session}"`）。
    #[must_use]
    pub fn with_session(
        manager: Arc<EvalManager>,
        registry: Arc<dyn agent_tools::ToolRegistry>,
        workspace: Arc<agent_core::Workspace>,
        approval: Arc<dyn agent_core::ApprovalPolicy>,
        session: impl Into<String>,
    ) -> Self {
        Self {
            manager,
            session: session.into(),
            bridge: tokio::sync::OnceCell::new(),
            registry,
            workspace,
            approval,
        }
    }

    /// 取环回桥（首次调用时按注入组件 spawn，之后复用）。
    async fn bridge(&self) -> Result<Arc<BridgeServer>, EvalError> {
        if let Some(b) = self.bridge.get() {
            return Ok(b.clone());
        }
        let b = BridgeServer::spawn(
            Arc::clone(&self.registry),
            Arc::clone(&self.workspace),
            Arc::clone(&self.approval),
        )
        .await?;
        let _ = self.bridge.set(b.clone());
        Ok(b)
    }
}

#[async_trait]
impl Tool for EvalTool {
    fn name(&self) -> &str {
        "eval"
    }

    fn description(&self) -> &str {
        "在持久 Python 内核中执行代码（同一会话共享命名空间）。\n\
         language 仅支持 \"py\"（默认）。keep=true（默认）保留内核供后续调用共享状态；\n\
         keep=false 执行后立即回收。内核内置 `tool` 代理：`tool.<name>(**kwargs)` 会调用\n\
         宿主的同名工具（如 tool.read_file(path=\"...\")），返回其输出；`tool.reset()` 清空\n\
         命名空间。适合快速原型、数据处理与多步数值探索。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "language": {
                    "type": "string",
                    "enum": ["py"],
                    "default": "py",
                    "description": "代码语言（当前仅支持 \"py\"）"
                },
                "code": {
                    "type": "string",
                    "description": "要执行的 Python 代码"
                },
                "keep": {
                    "type": "boolean",
                    "default": true,
                    "description": "执行后是否保留内核（共享命名空间）"
                }
            },
            "required": ["code"]
        })
    }

    fn capability(&self) -> CapabilityTier {
        CapabilityTier::Execute
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    fn interruptible(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let language = input
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or("py")
            .to_string();
        let code = input
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `code` 参数".into()))?
            .to_string();
        let keep = input.get("keep").and_then(Value::as_bool).unwrap_or(true);
        // session_key 建议格式："{cwd}|{session_id}"。
        let session_key = format!("{}|{}", ctx.workspace.root().display(), self.session);
        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                self.manager.abort(&session_key).await;
                Err(ToolError::Execution("eval 执行被取消".into()))
            }
            result = async {
                let bridge = match self.bridge().await {
                    Ok(b) => b,
                    Err(e) => return Err(e.to_string()),
                };
                self.manager
                    .execute(&session_key, &language, &code, keep, &bridge)
                    .await
                    .map_err(|e| e.to_string())
            } => {
                match result {
                    Ok(output) => Ok(ToolResult::text(render_output(&output))),
                    Err(msg) => Err(ToolError::Execution(msg)),
                }
            }
        }
    }
}

/// 渲染执行结果为供 LLM 阅读的文本（stdout / stderr / result / error 分段）。
fn render_output(output: &EvalOutput) -> String {
    let mut text = String::new();
    if !output.stdout.is_empty() {
        text.push_str(&output.stdout);
    }
    if !output.stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&output.stderr);
    }
    if let Some(result) = &output.result {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[result]\n");
        text.push_str(result);
    }
    if let Some(error) = &output.error {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[error]\n");
        text.push_str(error);
    }
    if text.is_empty() {
        text = "(执行成功，无输出)".to_string();
    }
    text
}
