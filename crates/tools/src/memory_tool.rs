//! `recall` / `retain` 记忆工具：跨会话长期记忆的读写面。
//!
//! 移植 oh-my-pi `memory-backend` 工具组（`memory-recall.ts` / `memory-retain.ts`）：
//! - `recall`：按查询检索跨会话记忆，返回带分数的命中列表（`1. [0.92] content` 格式）。
//!   仅 structured 后端有检索能力；local 后端返回空（与 omp 一致：recall 仅
//!   hindsight/mnemopi 系后端启用）。
//! - `retain`：显式保存一条事实（决策/偏好/项目上下文），importance 0..=5。
//!   omp 对 retain 使用 read 级审批（只写记忆库、不触工作区），沿用。
//!
//! 工具经 [`ToolContext::memory`] 访问存储（agent 循环注入同一 `MemoryStore` 实例），
//! 装配层仅在 `[memory].enabled` 时注册；未启用时工具不存在（零上下文成本）。

use std::sync::Arc;

use agent_core::{
    AssistantEvent, CapabilityTier, CompletionRequest, LlmProvider, MemoryStore, Model,
    ProviderCallContext, ProviderMessage, ToolError, ToolResult, UserContent,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;

use super::{Concurrency, Tool, ToolContext};

use std::fmt::Write as _;

/// `recall`：跨会话记忆检索（只读）。
pub struct MemoryRecallTool {
    memory: Arc<dyn MemoryStore>,
}

impl MemoryRecallTool {
    /// 绑定记忆存储。
    #[must_use]
    pub fn new(memory: Arc<dyn MemoryStore>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "recall"
    }
    fn description(&self) -> &str {
        "检索跨会话长期记忆：按查询返回最相关的过往记忆（带相关度分数）。\
在回答关于过往会话、项目历史或用户偏好的问题前主动使用。\
仅 structured 记忆后端支持语义检索；local 后端返回空。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "检索查询（自然语言，如「用户偏好哪个包管理器」）" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 8,
                           "description": "返回条数上限（默认 8）" }
            },
            "required": ["query"]
        })
    }
    fn capability(&self) -> CapabilityTier {
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
        let query = input
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `query` 参数".into()))?;
        let limit = input
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(8, |v| usize::try_from(v).unwrap_or(8))
            .clamp(1, 50);
        let hits = self.memory.recall(query, limit).await;
        if hits.is_empty() {
            return Ok(ToolResult::text(
                "未找到相关记忆（本地记忆库无匹配条目，或当前为 local 后端无检索能力）。",
            ));
        }
        let mut out = String::new();
        for (i, h) in hits.iter().enumerate() {
            let _ = writeln!(
                out,
                "{}. [{:.2}] {}",
                i + 1,
                h.score,
                h.content.replace('\n', " ")
            );
        }
        let sources = {
            let mut sources = hits.iter().map(|h| h.source.as_str()).collect::<Vec<_>>();
            sources.dedup();
            sources.join("、")
        };
        let _ = writeln!(out, "\n（{} 条命中，来源：{sources}）", hits.len());
        Ok(ToolResult::text(out))
    }
}

/// `retain`：显式保存一条事实到跨会话记忆。
pub struct MemoryRetainTool {
    memory: Arc<dyn MemoryStore>,
}

impl MemoryRetainTool {
    /// 绑定记忆存储。
    #[must_use]
    pub fn new(memory: Arc<dyn MemoryStore>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for MemoryRetainTool {
    fn name(&self) -> &str {
        "retain"
    }
    fn description(&self) -> &str {
        "把一条事实（决策、偏好、项目约定）保存到跨会话长期记忆，供未来会话 recall。\
importance 0..=5（默认 1）：高价值事实用 3-5（如用户偏好、关键架构决策）。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string",
                             "description": "要记住的事实（陈述句，自包含，不依赖当前上下文）" },
                "importance": { "type": "integer", "minimum": 0, "maximum": 5, "default": 1,
                                "description": "重要性 0..=5（默认 1）" }
            },
            "required": ["content"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        // 只写记忆库、不触工作区；沿用 omp 对 retain 的 read 级审批。
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
        let content = input
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `content` 参数".into()))?;
        let importance = input
            .get("importance")
            .and_then(serde_json::Value::as_u64)
            .map_or(1, |v| u8::try_from(v).unwrap_or(1))
            .min(5);
        // 防爆库：单条上限 4000 字符（对齐 omp retain 内容预算）。
        let content = truncate_chars(content, 4000);
        self.memory
            .retain(&content, importance, "tool:retain")
            .await
            .map_err(|e| ToolError::Execution(format!("记忆写入失败: {e}")))?;
        Ok(ToolResult::text(format!(
            "已记住（importance={importance}）：{content}"
        )))
    }
}

/// 按字符数截断（保留头部，对齐 mnemopi 长内容 head 保留策略）。
fn truncate_chars(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

/// `reflect`：把会话中浮现的观察/思考提炼为心智模型（经 LLM 综合），
/// 追加到项目 `mental_models.md`，下次会话注入 system prompt。
///
/// 移植 oh-my-pi `memory-reflect` 语义：reflect = 「LLM 提炼 + 心智模型落库」；
/// 去重/合并由任务末的 `consolidate_mental_models` 统一完成（local 后端）。
/// 与 `retain`（存结构化记录、recall 可检索）互补：reflect 面向**准则/偏好**，
/// 注入可见而非检索命中。
pub struct MemoryReflectTool {
    memory: Arc<dyn MemoryStore>,
    provider: Arc<dyn LlmProvider>,
    model: Model,
    provider_ctx: ProviderCallContext,
}

impl MemoryReflectTool {
    /// 绑定记忆存储与 LLM（提炼用）。
    #[must_use]
    pub fn new(
        memory: Arc<dyn MemoryStore>,
        provider: Arc<dyn LlmProvider>,
        model: Model,
        provider_ctx: ProviderCallContext,
    ) -> Self {
        Self { memory, provider, model, provider_ctx }
    }
}

#[async_trait]
impl Tool for MemoryReflectTool {
    fn name(&self) -> &str {
        "reflect"
    }
    fn description(&self) -> &str {
        "把会话中浮现的观察/思考提炼为心智模型（工程准则、用户偏好、项目约定）：\
经 LLM 综合为精炼条目后写入长期记忆，下次会话自动注入 system prompt。\
适合在任务收尾或发现规律时使用（如「用户偏好 X」「本项目约定 Y」）。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string",
                             "description": "要提炼的原始观察/对话片段/思考（可长可短）" }
            },
            "required": ["content"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        // 只写记忆库、不触工作区；沿用 omp 对记忆工具的 read 级审批。
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
        let content = input
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `content` 参数".into()))?;
        let content = truncate_chars(content, 4000);

        let req = CompletionRequest {
            model: self.model.clone(),
            system: vec![format!(
                "你是记忆提炼助手。把用户的原始观察提炼为精炼、自包含、可直接复用的\
工程准则或用户偏好陈述。只输出提炼结果：每条一行，不要编号、不要 markdown \
列表前缀、不要解释或客套。"
            )],
            messages: vec![ProviderMessage::User {
                content: vec![UserContent::Text {
                    text: format!(
                        "把以下内容提炼为 1-3 条精炼准则/偏好（每条一行，无编号无前缀）：\n\n{content}"
                    ),
                }],
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: 512,
            temperature: Some(0.0),
            thinking: None,
            cache_key: None,
            stable_prefix_len: 0,
        };
        let mut stream = self
            .provider
            .stream(req, &self.provider_ctx)
            .await
            .map_err(|e| ToolError::Execution(format!("LLM 提炼失败: {e}")))?;
        let mut output = String::new();
        while let Some(ev) = stream.next().await {
            if let AssistantEvent::TextDelta(d) = ev {
                output.push_str(&d);
            }
        }
        let models = distill_lines(&output);
        if models.is_empty() {
            return Err(ToolError::Execution(
                "LLM 未提炼出有效条目（输出为空或全为无效行）".into(),
            ));
        }
        for m in &models {
            self.memory
                .add_mental_model(m)
                .await
                .map_err(|e| ToolError::Execution(format!("心智模型写入失败: {e}")))?;
        }
        let mut out = format!("已提炼 {} 条心智模型（下次会话自动注入）：", models.len());
        for m in &models {
            let _ = writeln!(out, "\n- {m}");
        }
        Ok(ToolResult::text(out))
    }
}

/// 把 LLM 输出拆成精炼条目：逐行剥离 markdown 列表/编号前缀，去空行、去重（保序）。
fn distill_lines(output: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in output.lines() {
        let mut line = raw.trim();
        // 剥 markdown 前缀：`- ` / `* ` / `# ` / `> ` / `1. ` / `1) `
        while let Some(stripped) = line.strip_prefix(['-', '*', '#', '>', '•']) {
            line = stripped.trim_start();
        }
        if let Some(rest) = line.strip_prefix(|c: char| c.is_ascii_digit()) {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix(['.', ')']) {
                line = rest.trim_start();
            }
        }
        if line.is_empty() || seen.contains(line) {
            continue;
        }
        seen.insert(line.to_string());
        out.push(line.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distill_strips_markdown_and_dedups() {
        let out = "- 准则一\n* 准则二\n1. 准则三\n2) 准则四\n\n准则一\n  - 缩进条目\n";
        let lines = distill_lines(out);
        assert_eq!(
            lines,
            vec!["准则一", "准则二", "准则三", "准则四", "缩进条目"]
        );
        assert_eq!(distill_lines("\n\n  \n"), Vec::<String>::new());
    }

    #[test]
    fn truncate_keeps_head_and_marks_tail() {
        let s = "中文内容".repeat(1000);
        let t = truncate_chars(&s, 10);
        assert!(t.chars().count() <= 11, "实际 {}", t.chars().count());
        assert!(t.ends_with('…'));
        assert_eq!(truncate_chars("短文本", 100), "短文本");
    }

    // P0-1：recall/retain 工具与真实 structured 存储的往返闭环（无向量，纯词法）。
    #[tokio::test]
    async fn retain_then_recall_roundtrip() {
        let root = std::env::temp_dir().join(format!(
            "mem-tool-test-{}-{:#x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store: std::sync::Arc<dyn MemoryStore> =
            std::sync::Arc::new(agent_memory::StructuredMemoryStore::with_root(root.clone()));
        let retain = MemoryRetainTool::new(std::sync::Arc::clone(&store));
        let recall = MemoryRecallTool::new(std::sync::Arc::clone(&store));

        let out = retain
            .execute(
                serde_json::json!({ "content": "用户偏好 Bun 而非 Node 写脚本", "importance": 4 }),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(text_of(&out).contains("已记住"));
        retain
            .execute(
                serde_json::json!({ "content": "部署用 systemd 单元" }),
                &ctx(),
            )
            .await
            .unwrap();

        // 相关查询命中偏好条目。
        let out = recall
            .execute(serde_json::json!({ "query": "用户喜欢哪个运行时" }), &ctx())
            .await
            .unwrap();
        let text = text_of(&out);
        assert!(text.contains("1. ["), "应带分数前缀: {text}");
        assert!(text.contains("Bun 而非 Node"), "未命中目标条目: {text}");
        assert!(text.contains("2."), "应有多条: {text}");

        // 空查询报错。
        assert!(recall
            .execute(serde_json::json!({}), &ctx())
            .await
            .is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    /// 取出 `ToolResult` 文本（测试断言用）。
    fn text_of(result: &ToolResult) -> String {
        match result {
            ToolResult::Text(t) => t.clone(),
            other => format!("{other:?}"),
        }
    }

    /// 固定输出的桩 Provider：忽略请求，吐出指定文本（reflect 提炼测试用）。
    struct FixedProvider(&'static str);

    #[async_trait]
    impl agent_core::LlmProvider for FixedProvider {
        fn id(&self) -> &'static str {
            "fixed"
        }
        fn supports(&self) -> &[agent_core::Api] {
            &[]
        }
        async fn stream(
            &self,
            _req: CompletionRequest,
            _ctx: &ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, agent_core::LlmError> {
            let msg = agent_core::AssistantMessage {
                content: vec![agent_core::ContentBlock::Text {
                    text: self.0.to_string(),
                }],
                usage: agent_core::Usage::default(),
                model: "fixed".into(),
                stop_reason: Some(agent_core::StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                AssistantEvent::TextDelta(self.0.to_string()),
                AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    // P0-1b：reflect 工具 = LLM 提炼 + 心智模型落库（注入可见，非检索命中）。
    #[tokio::test]
    async fn reflect_distills_and_persists_mental_models() {
        let root = std::env::temp_dir().join(format!(
            "mem-tool-test-{}-{:#x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store: std::sync::Arc<dyn MemoryStore> =
            std::sync::Arc::new(agent_memory::StructuredMemoryStore::with_root(root.clone()));
        let provider: std::sync::Arc<dyn LlmProvider> = std::sync::Arc::new(FixedProvider(
            "- 用户偏好 Bun 而非 Node\n* 部署用 systemd 单元\n1. 认证走 OAuth2\n- 用户偏好 Bun 而非 Node\n",
        ));
        let tool = MemoryReflectTool::new(
            std::sync::Arc::clone(&store),
            std::sync::Arc::clone(&provider),
            Model::with_defaults("fixed", "fixed", agent_core::Api::OpenAiCompletions),
            ProviderCallContext::default(),
        );

        let out = tool
            .execute(
                serde_json::json!({ "content": "用户表示喜欢 Bun；部署用 systemd；认证走 OAuth2" }),
                &ctx(),
            )
            .await
            .unwrap();
        let text = text_of(&out);
        assert!(text.contains("已提炼 3 条心智模型"), "{text}");
        assert!(text.contains("用户偏好 Bun 而非 Node"));
        assert!(!text.contains("1."), "编号前缀应剥离: {text}");

        // 落库：mental_models.md 三行，格式 `- [ts] text`，重复行只落一次。
        let mm = std::fs::read_to_string(root.join("mental_models.md")).unwrap();
        let lines: Vec<&str> = mm.lines().collect();
        assert_eq!(lines.len(), 3, "应含 3 条（去重后）：{mm}");
        for line in &lines {
            assert!(line.starts_with("- [20"), "时间戳格式: {line}");
            assert!(line.contains("用户偏好") || line.contains("systemd") || line.contains("OAuth2"));
        }

        // 注入面：structured 未配置 mental_models_cfg 时 trait mental_models() 为 None
        //（注入段由 engine 按配置组装，本测试只锁落库格式）。

        // 空输入报错。
        assert!(tool.execute(serde_json::json!({}), &ctx()).await.is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    /// 最小 ToolContext（本测试工具不触碰工作区/审批）。
    fn ctx() -> ToolContext<'static> {
        use std::sync::OnceLock;
        static W: OnceLock<agent_core::Workspace> = OnceLock::new();
        static C: OnceLock<tokio_util::sync::CancellationToken> = OnceLock::new();
        struct NoopApproval;
        #[async_trait::async_trait]
        impl agent_core::ApprovalPolicy for NoopApproval {
            fn decide(&self, _req: &agent_core::ApprovalRequest<'_>) -> agent_core::ApprovalDecision {
                agent_core::ApprovalDecision::Allow
            }
            async fn prompt(
                &self,
                _ask: &agent_core::AskMessage,
            ) -> Result<agent_core::AskResponse, agent_core::ToolError> {
                Ok(agent_core::AskResponse::Yes)
            }
        }
        static A: NoopApproval = NoopApproval;
        ToolContext {
            workspace: W.get_or_init(|| agent_core::Workspace::new("/tmp")),
            approval: &A,
            cancel: C.get_or_init(tokio_util::sync::CancellationToken::new),
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
            update_tx: None,
            conflicts: None,
            pending_rewrites: None,
        }
    }
}

