//! `ask` 工具：模型侧向用户结构化提问（移植 oh-my-pi `ask` 工具，`canPromptUser` 门控）。
//!
//! 经 [`ApprovalPolicy::prompt`] 通道阻塞等待回答——CLI 走 stdin、Web 走 WS 回执、
//! ACP 走 `session/request_permission`（与工具审批同一通道，宿主零新增接线）。
//! 用户拒绝回答时返回**可恢复错误**并明确告知模型「基于现有信息继续，不要重复追问」
//! （omp ask 拒绝语义）。

use std::sync::atomic::{AtomicU64, Ordering};

use agent_core::{AskKind, AskMessage, AskResponse, CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;

use crate::{Concurrency, Tool, ToolContext};

/// ask 消息序号（进程内唯一 id）。
static ASK_SEQ: AtomicU64 = AtomicU64::new(1);

/// `ask`：向用户提问以消除歧义/做取舍。
pub struct AskUserTool;

impl AskUserTool {
    /// 构造（无状态工具）。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for AskUserTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &'static str {
        "ask"
    }
    fn description(&self) -> &'static str {
        "向用户提一个需要决策的问题：当任务指令含糊、存在多种 materially 不同做法、\
或缺少必要信息（密钥/账号/偏好）时使用。返回用户的文本回答（含选项 label 或自由输入）。\
用户拒绝时返回错误——此时应基于现有信息选择最保守做法继续，不要重复追问。"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": { "type": "string",
                              "description": "要问的问题（自包含，用户不看上下文也能理解）" },
                "questions": {
                    "type": "array",
                    "description": "多问题形态（对齐 omp `ask.questions`）：逐条提问并汇总回答；与 `question` 二选一，`question` 优先",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": { "type": "string", "description": "问题文本（omp 也接受 `prompt`）" },
                            "prompt": { "type": "string", "description": "`question` 的别名" },
                            "header": { "type": "string", "description": "短标题（可选）" },
                            "options": { "type": "array", "items": { "type": "object", "properties": { "label": { "type": "string" }, "description": { "type": "string" } }, "required": ["label"] } }
                        },
                        "required": []
                    }
                },
                "options": {
                    "type": "array",
                    "description": "2-5 个候选项（可选；给出时用户可回 label 或自由输入）",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": { "type": "string", "description": "短标签（如 JWT）" },
                            "description": { "type": "string", "description": "该选项的含义/代价" }
                        },
                        "required": ["label"]
                    }
                },
                "header": { "type": "string",
                            "description": "2-6 词的意图概括（显示在问题上方，如「选择认证方案」）" }
            },
            "required": []
        })
    }
    fn capability(&self) -> CapabilityTier {
        // 纯交互（无副作用），read 级；阻塞等待用户故可中断。
        CapabilityTier::ReadOnly
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    fn interruptible(&self) -> bool {
        // 阻塞在用户输入上：steering 到达时尽快让出。
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        // H32：omp `ask.questions[]` 多问形态 → 逐条提问并汇总；单问回退到下方原路径。
        if let Some(arr) = input.get("questions").and_then(serde_json::Value::as_array) {
            if !arr.is_empty() {
                let mut out = String::new();
                for (i, q) in arr.iter().enumerate() {
                    let text = q
                        .get("question")
                        .or_else(|| q.get("prompt"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .ok_or_else(|| {
                            ToolError::InvalidArgs(format!(
                                "questions[{i}] 缺少 `question`/`prompt`"
                            ))
                        })?;
                    let answer = match self.ask_single(q, ctx).await? {
                        ToolResult::Text(t) => t,
                        ToolResult::Error { message, .. } => {
                            // 单问被拒/无通道 → 与单问路径一致地整体返回可恢复错误。
                            return Ok(ToolResult::Error {
                                recoverable: true,
                                message,
                            });
                        }
                        other => other.to_llm_text(),
                    };
                    out.push_str(&format!("Q{}: {text}\nA{}: {answer}\n\n", i + 1, i + 1));
                }
                return Ok(ToolResult::text(out.trim_end().to_string()));
            }
        }
        self.ask_single(&input, ctx).await
    }
}

impl AskUserTool {
    /// 单问路径（原 `execute` 主体；`input` 既可以是整份参数，也可以是 `questions[]` 的一项）。
    async fn ask_single(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let question = input
            .get("question")
            .or_else(|| input.get("prompt"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `question`（或别名 `prompt`）".into()))?;
        let header = input
            .get("header")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .unwrap_or("需要你的输入");
        let options: Vec<(String, String)> = input
            .get("options")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| {
                        let label = o
                            .get("label")
                            .and_then(serde_json::Value::as_str)?
                            .trim()
                            .to_string();
                        if label.is_empty() {
                            return None;
                        }
                        let desc = o
                            .get("description")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        Some((label, desc))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 渲染提问（CLI stderr / Web 弹窗 / ACP request_permission 共用同一段文案）。
        let mut prompt = format!("**{header}**\n\n{question}");
        if !options.is_empty() {
            prompt.push_str("\n\n选项：");
            for (label, desc) in &options {
                if desc.is_empty() {
                    prompt.push_str(&format!("\n- {label}"));
                } else {
                    prompt.push_str(&format!("\n- **{label}** — {desc}"));
                }
            }
            prompt.push_str("\n\n回复选项 label 或自由输入。");
        }

        let ask = AskMessage {
            id: format!("ask-{}", ASK_SEQ.fetch_add(1, Ordering::Relaxed)),
            kind: AskKind::Followup,
            prompt,
        };

        // 阻塞等待用户回答；steering 取消时让出（可恢复，循环可继续）。
        let answer = tokio::select! {
            r = ctx.approval.prompt(&ask) => r,
            () = ctx.cancel.cancelled() => {
                return Ok(ToolResult::Error {
                    recoverable: true,
                    message: "提问被用户中断（steering）".into(),
                });
            }
        };

        match answer {
            Ok(AskResponse::Text(t)) if !t.trim().is_empty() => Ok(ToolResult::text(t)),
            Ok(AskResponse::Text(_)) => {
                Ok(ToolResult::text("（用户提交了空回答——按最保守默认继续）"))
            }
            // 审批通道的 Yes/No 映射为文本，供模型直接消费。
            Ok(AskResponse::Yes) => Ok(ToolResult::text("yes")),
            Ok(AskResponse::No) => Ok(ToolResult::Error {
                recoverable: true,
                message: "用户拒绝回答。基于现有信息选择最保守的做法继续，不要重复追问。".into(),
            }),
            Err(e) => Ok(ToolResult::Error {
                recoverable: true,
                message: format!("提问通道不可用（当前宿主无交互能力）：{e}。基于现有信息继续。"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ApprovalDecision, ApprovalPolicy, ApprovalRequest};
    use std::sync::Mutex;

    /// 脚本化审批策略：按队列依次返回预设响应；记录收到的 `AskMessage`。
    struct ScriptedApproval {
        responses: Mutex<Vec<AskResponse>>,
        seen: Mutex<Vec<AskMessage>>,
    }

    #[async_trait::async_trait]
    impl ApprovalPolicy for ScriptedApproval {
        fn decide(&self, _r: &ApprovalRequest<'_>) -> ApprovalDecision {
            ApprovalDecision::Allow
        }
        async fn prompt(&self, ask: &AskMessage) -> Result<AskResponse, ToolError> {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(ask.clone());
            self.responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop()
                .ok_or_else(|| ToolError::Execution("队列空".into()))
        }
    }

    fn workspace() -> agent_core::Workspace {
        agent_core::Workspace::new(std::env::temp_dir())
    }

    fn ctx<'a>(
        ws: &'a agent_core::Workspace,
        approval: &'a dyn ApprovalPolicy,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> ToolContext<'a> {
        ToolContext {
            workspace: ws,
            approval,
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

    async fn run(approval: &dyn ApprovalPolicy, input: serde_json::Value) -> ToolResult {
        let tool = AskUserTool::new();
        let ws = workspace();
        let cancel = tokio_util::sync::CancellationToken::new();
        tool.execute(input, &ctx(&ws, approval, &cancel))
            .await
            .unwrap()
    }

    /// H32：omp `ask.questions[]` 多问形态——逐条提问、按脚本顺序取答、编号汇总。
    #[tokio::test]
    async fn multi_question_array_asks_each_and_aggregates() {
        let a = ScriptedApproval {
            // `ScriptedApproval::prompt` 用 `pop()`（LIFO）→ 逆序压入以按提问顺序出货。
            responses: Mutex::new(vec![
                AskResponse::Text("Postgres".into()),
                AskResponse::Text("JWT".into()),
            ]),
            seen: Mutex::new(vec![]),
        };
        let out = run(
            &a,
            serde_json::json!({"questions": [
                {"question": "认证方案？", "options": [{"label": "JWT"}, {"label": "OAuth2"}]},
                {"prompt": "数据库？", "header": "存储"}
            ]}),
        )
        .await;
        let text = out.to_llm_text();
        assert!(text.contains("Q1: 认证方案？"), "{text}");
        assert!(text.contains("A1: JWT"), "{text}");
        assert!(text.contains("Q2: 数据库？"), "{text}");
        assert!(text.contains("A2: Postgres"), "{text}");
        // 两次提问都真实送达审批通道（作用域内取锁，避免跨 await 持 std guard）。
        {
            let seen = a.seen.lock().unwrap();
            assert_eq!(seen.len(), 2, "应逐条提问: {seen:?}");
            assert!(seen[1].prompt.contains("存储"), "第二个问题应带 header");
        }
        // 单问形态不受影响（同一路径回归）。
        let a2 = ScriptedApproval {
            responses: Mutex::new(vec![AskResponse::Text("ok".into())]),
            seen: Mutex::new(vec![]),
        };
        let out = run(&a2, serde_json::json!({"question": "单问"})).await;
        assert_eq!(out.to_llm_text(), "ok");
        // 缺 `question`/`prompt` → 明确错误。
        let tool = AskUserTool::new();
        let ws = workspace();
        let cancel = tokio_util::sync::CancellationToken::new();
        let err = tool
            .execute(
                serde_json::json!({"questions": [{"header": "x"}]}),
                &ctx(&ws, &a2, &cancel),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("questions[0]"), "{err}");
    }
    #[tokio::test]
    async fn text_answer_passthrough() {
        let a = ScriptedApproval {
            responses: Mutex::new(vec![AskResponse::Text("用 JWT".into())]),
            seen: Mutex::new(vec![]),
        };
        let out = run(
            &a,
            serde_json::json!({"question":"认证方案？","header":"选择认证"}),
        )
        .await;
        assert_eq!(out.to_llm_text(), "用 JWT");
        let seen = a.seen.lock().unwrap();
        assert!(seen[0].prompt.contains("**选择认证**"));
        assert!(matches!(seen[0].kind, AskKind::Followup));
        assert!(seen[0].id.starts_with("ask-"));
    }

    #[tokio::test]
    async fn options_rendered_into_prompt() {
        let a = ScriptedApproval {
            responses: Mutex::new(vec![AskResponse::Text("OAuth2".into())]),
            seen: Mutex::new(vec![]),
        };
        let out = run(
            &a,
            serde_json::json!({"question":"选哪个？","options":[
                {"label":"JWT","description":"无状态"},
                {"label":"OAuth2"}
            ]}),
        )
        .await;
        assert_eq!(out.to_llm_text(), "OAuth2");
        let seen = a.seen.lock().unwrap();
        assert!(seen[0].prompt.contains("- **JWT** — 无状态"));
        assert!(seen[0].prompt.contains("- OAuth2"));
        assert!(seen[0].prompt.contains("回复选项 label 或自由输入"));
    }

    #[tokio::test]
    async fn rejection_is_recoverable_error() {
        let a = ScriptedApproval {
            responses: Mutex::new(vec![AskResponse::No]),
            seen: Mutex::new(vec![]),
        };
        let out = run(&a, serde_json::json!({"question":"?"})).await;
        match out {
            ToolResult::Error {
                recoverable,
                message,
            } => {
                assert!(recoverable);
                assert!(message.contains("不要重复追问"));
            }
            other => panic!("期望 Error，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn channel_failure_is_recoverable() {
        let a = ScriptedApproval {
            responses: Mutex::new(vec![]),
            seen: Mutex::new(vec![]),
        };
        let out = run(&a, serde_json::json!({"question":"?"})).await;
        assert!(matches!(out, ToolResult::Error { .. }));
    }

    #[tokio::test]
    async fn empty_answer_maps_to_conservative_hint() {
        let a = ScriptedApproval {
            responses: Mutex::new(vec![AskResponse::Text("   ".into())]),
            seen: Mutex::new(vec![]),
        };
        let out = run(&a, serde_json::json!({"question":"?"})).await;
        assert!(out.to_llm_text().contains("空回答"));
    }

    #[tokio::test]
    async fn missing_question_is_invalid_args() {
        let a = ScriptedApproval {
            responses: Mutex::new(vec![]),
            seen: Mutex::new(vec![]),
        };
        let tool = AskUserTool::new();
        let ws = workspace();
        let cancel = tokio_util::sync::CancellationToken::new();
        let err = tool
            .execute(serde_json::json!({}), &ctx(&ws, &a, &cancel))
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn steering_cancel_interrupts() {
        struct HangingApproval;
        #[async_trait::async_trait]
        impl ApprovalPolicy for HangingApproval {
            fn decide(&self, _r: &ApprovalRequest<'_>) -> ApprovalDecision {
                ApprovalDecision::Allow
            }
            async fn prompt(&self, _ask: &AskMessage) -> Result<AskResponse, ToolError> {
                std::future::pending::<()>().await;
                unreachable!()
            }
        }
        let tool = std::sync::Arc::new(AskUserTool::new());
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel2 = cancel.clone();
        let exec = tokio::spawn(async move {
            let ws = workspace();
            let a = HangingApproval;
            tool.execute(serde_json::json!({"question":"?"}), &ctx(&ws, &a, &cancel2))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        let out = exec.await.unwrap().unwrap();
        assert!(matches!(
            out,
            ToolResult::Error {
                recoverable: true,
                ..
            }
        ));
    }
}
