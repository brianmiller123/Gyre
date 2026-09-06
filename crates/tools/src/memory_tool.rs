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

use agent_core::{CapabilityTier, MemoryStore, ToolError, ToolResult};
use async_trait::async_trait;
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
    fn name(&self) -> &'static str {
        "recall"
    }
    fn description(&self) -> &'static str {
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
    fn name(&self) -> &'static str {
        "retain"
    }
    fn description(&self) -> &'static str {
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

/// `reflect`：基于长期记忆的综合问答（只读）——带一个问题与可选上下文，
/// 检索记忆库并综合为回答，**不写入**任何内容。
///
/// 对齐 oh-my-pi `tools/memory-reflect.ts`：reflect = 「recall 结果 → 综合回答」，
/// approval=read、loadMode=discoverable。与 `recall`（返回带分数的原始命中列表）
/// 互补：reflect 面向「基于记忆回答一个开放问题」。写入走 `retain` / `learn`
///（mental_model）；任务末整合仍由 `consolidate_mental_models` 完成（local 后端）。
pub struct MemoryReflectTool {
    memory: Arc<dyn MemoryStore>,
}

impl MemoryReflectTool {
    /// 绑定记忆存储。
    #[must_use]
    pub fn new(memory: Arc<dyn MemoryStore>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for MemoryReflectTool {
    fn name(&self) -> &'static str {
        "reflect"
    }
    fn description(&self) -> &'static str {
        "基于跨会话长期记忆综合回答一个开放问题（只读，不写入）：\
检索记忆库中与问题相关的过往记忆并综合为回答。\
适合「我们从过往会话中学到了什么」「关于 X 的既有结论/偏好是什么」类提问；\
要保存新知识请改用 retain 或 learn。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string",
                           "description": "要回答的问题" },
                "context": { "type": "string",
                             "description": "可选补充上下文（帮助限定检索范围）" }
            },
            "required": ["query"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        // 只读检索；沿用 omp 对 reflect 的 read 级审批。
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
        let context = input
            .get("context")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty());
        // omp：context 非空时并入检索查询（query\n\nAdditional context:\n{context}）。
        let search = match context {
            Some(c) => format!(
                "{query}\n\nAdditional context:\n{}",
                truncate_chars(c, 2000)
            ),
            None => query.to_string(),
        };
        let hits = self.memory.recall(&search, 8).await;
        if hits.is_empty() {
            return Ok(ToolResult::text(
                "No relevant information found to reflect on.",
            ));
        }
        let summary = hits
            .iter()
            .map(|h| {
                format!(
                    "- {}（相关度 {:.2}）",
                    h.content.replace('\n', " "),
                    h.score
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult::text(format!(
            "Based on recalled memories:\n\n{summary}"
        )))
    }
}

// ── memory_edit / learn：记忆管理面与主动学习入口 ─────────────────────────────

/// `memory_edit`：跨会话记忆的管理面（检索 / 遗忘 / 库列表 / 清空）。
///
/// 移植 oh-my-pi `memory-edit.ts` 的 forget 语义并按 Gyre 后端能力裁剪：
/// - `search`：同 `recall` 的加权检索，命中渲染带记录 id（供 forget 链式使用）；
/// - `forget`：按 id 删除一条记录（仅 structured 后端支持；其余后端返回未找到）；
/// - `banks`：列出记忆库 bank（structured 多库；local 单库为空）；
/// - `clear`：清空该项目全部记忆，须显式 `confirm: true`（防误触）。
///
/// 只读写自管理记忆库、不触工作区——审批取 read 级（与 [`MemoryRetainTool`] 约定一致）。
pub struct MemoryEditTool {
    memory: Arc<dyn MemoryStore>,
}

impl MemoryEditTool {
    /// 绑定记忆存储。
    #[must_use]
    pub fn new(memory: Arc<dyn MemoryStore>) -> Self {
        Self { memory }
    }

    /// search：加权检索，渲染 `1. [0.92] content（id: …）`（id 供 forget 链用）。
    async fn op_search(&self, input: &serde_json::Value) -> Result<ToolResult, ToolError> {
        let query = input
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("search 缺少 `query` 参数".into()))?;
        let limit = input
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(8, |v| usize::try_from(v).unwrap_or(8))
            .clamp(1, 50);
        let hits = self.memory.recall(query, limit).await;
        if hits.is_empty() {
            return Ok(ToolResult::text(
                "未找到相关记忆（无匹配条目，或当前后端无检索能力）。",
            ));
        }
        let mut out = String::new();
        for (i, h) in hits.iter().enumerate() {
            let _ = writeln!(
                out,
                "{}. [{:.2}] {}（id: {}）",
                i + 1,
                h.score,
                h.content.replace('\n', " "),
                h.id
            );
        }
        let _ = writeln!(out, "\n（{} 条命中，id 可用于 forget 删除）", hits.len());
        Ok(ToolResult::text(out))
    }

    /// forget：按 id 删除一条记录。
    async fn op_forget(&self, input: &serde_json::Value) -> Result<ToolResult, ToolError> {
        let id = input
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("forget 缺少 `id` 参数".into()))?;
        let hit = self
            .memory
            .forget(id)
            .await
            .map_err(|e| ToolError::Execution(format!("记忆删除失败: {e}")))?;
        Ok(ToolResult::text(if hit {
            format!("已遗忘记忆 {id}。")
        } else {
            format!("未找到记忆 {id}（可能已被删除，或当前后端不支持逐条删除）。")
        }))
    }

    /// banks：列出记忆库。
    async fn op_banks(&self) -> Result<ToolResult, ToolError> {
        let banks = self.memory.banks().await;
        if banks.is_empty() {
            return Ok(ToolResult::text(
                "无记忆库 bank（当前后端单库存储，或记忆为空）。",
            ));
        }
        let mut out = format!("记忆库（{} 个）：", banks.len());
        for (i, b) in banks.iter().enumerate() {
            let _ = writeln!(out, "\n{}. {b}", i + 1);
        }
        Ok(ToolResult::text(out))
    }

    /// clear：清空全部项目记忆（须 confirm）。
    async fn op_clear(&self, input: &serde_json::Value) -> Result<ToolResult, ToolError> {
        let confirmed = input
            .get("confirm")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !confirmed {
            return Err(ToolError::InvalidArgs(
                "clear 将清空该项目全部记忆（不可恢复），须显式传 `confirm: true`。".into(),
            ));
        }
        self.memory
            .clear()
            .await
            .map_err(|e| ToolError::Execution(format!("记忆清空失败: {e}")))?;
        Ok(ToolResult::text(format!(
            "已清空项目记忆（{}）。",
            self.memory.root_dir().display()
        )))
    }
}

#[async_trait]
impl Tool for MemoryEditTool {
    fn name(&self) -> &'static str {
        "memory_edit"
    }
    fn description(&self) -> &'static str {
        "管理跨会话长期记忆：search 检索（命中带记录 id）、forget 按 id 删除过期或错误记忆、\
banks 列出记忆库、clear 清空全部（须 confirm: true）。\
先 search 拿 id 再 forget；仅 structured 后端支持删除。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "op": { "type": "string", "enum": ["search", "forget", "banks", "clear"],
                        "description": "操作" },
                "query": { "type": "string", "description": "search：检索查询（自然语言）" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 8,
                           "description": "search：返回条数上限（默认 8）" },
                "id": { "type": "string",
                        "description": "forget：要删除的记录 id（来自 search 命中）" },
                "confirm": { "type": "boolean",
                             "description": "clear：须显式传 true 才执行清空" }
            },
            "required": ["op"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        // 只读写自管理记忆库、不触工作区（与 retain 同约定）。
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
            .map(str::trim)
            .filter(|o| !o.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `op` 参数".into()))?;
        match op {
            "search" => self.op_search(&input).await,
            "forget" => self.op_forget(&input).await,
            "banks" => self.op_banks().await,
            "clear" => self.op_clear(&input).await,
            other => Err(ToolError::InvalidArgs(format!(
                "未知 op `{other}`（可选 search/forget/banks/clear）"
            ))),
        }
    }
}

/// `learn`：把会话中学到的可复用知识固化为长期记忆的统一入口。
///
/// 移植 oh-my-pi `learn.ts` 的「学到即存」语义，按 `kind` 路由存储形态：
/// - `fact`（默认）/ `lesson`：结构化记录（importance=4，recall 可检索）；
/// - `mental_model`：心智模型（`mental_models.md`，下次会话注入 system prompt）。
///
/// 与 [`MemoryRetainTool`] / [`MemoryReflectTool`] 的分工：learn 面向 agent 主动
/// 学习——一条调用按类型选库；retain 面向显式存事实、reflect 面向 LLM 提炼。
/// 只写自管理记忆库，审批取 read 级（同 retain 约定）。
pub struct MemoryLearnTool {
    memory: Arc<dyn MemoryStore>,
}

impl MemoryLearnTool {
    /// 绑定记忆存储。
    #[must_use]
    pub fn new(memory: Arc<dyn MemoryStore>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for MemoryLearnTool {
    fn name(&self) -> &'static str {
        "learn"
    }
    fn description(&self) -> &'static str {
        "把本会话学到的可复用知识存入跨会话记忆：fact/lesson（默认 fact）存为可检索记录，\
mental_model 存为心智模型（下次会话自动注入）。\
内容须自包含、不依赖当前上下文（是什么、何时、为何）。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string",
                             "description": "要记住的知识（自包含陈述句：是什么、何时、为何）" },
                "kind": { "type": "string", "enum": ["fact", "lesson", "mental_model"],
                          "default": "fact",
                          "description": "知识类型：fact=事实、lesson=经验教训（均入检索库）；\
        mental_model=心智模型（注入式，不参与检索）" }
            },
            "required": ["content"]
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
        let content = input
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `content` 参数".into()))?;
        let kind = input
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("fact");
        // 防爆库：与 retain 同一单条预算。
        let content = truncate_chars(content, 4000);
        match kind {
            "fact" | "lesson" => {
                self.memory
                    .retain(&content, 4, "tool:learn")
                    .await
                    .map_err(|e| ToolError::Execution(format!("记忆写入失败: {e}")))?;
                Ok(ToolResult::text(format!(
                    "已学习并保存（kind={kind}）：{content}"
                )))
            }
            "mental_model" => {
                self.memory
                    .add_mental_model(&content)
                    .await
                    .map_err(|e| ToolError::Execution(format!("心智模型写入失败: {e}")))?;
                Ok(ToolResult::text(format!("已保存心智模型：{content}")))
            }
            other => Err(ToolError::InvalidArgs(format!(
                "未知 kind `{other}`（可选 fact/lesson/mental_model）"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(recall.execute(serde_json::json!({}), &ctx()).await.is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    /// 取出 `ToolResult` 文本（测试断言用）。
    fn text_of(result: &ToolResult) -> String {
        match result {
            ToolResult::Text(t) => t.clone(),
            other => format!("{other:?}"),
        }
    }

    // 对齐 omp memory-reflect.ts：reflect = 检索 + 综合回答（只读，不写入）。
    #[tokio::test]
    async fn reflect_answers_from_memory_without_writing() {
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
        let reflect = MemoryReflectTool::new(std::sync::Arc::clone(&store));

        // 空库：固定空回答（omp 同文案）。
        let empty = reflect
            .execute(serde_json::json!({"query": "我们怎么部署服务？"}), &ctx())
            .await
            .unwrap();
        assert_eq!(
            text_of(&empty),
            "No relevant information found to reflect on."
        );

        // 写入一条事实（经 retain 工具走真实链路）。
        retain
            .execute(
                serde_json::json!({"content": "本项目部署使用 systemd 单元"}),
                &ctx(),
            )
            .await
            .unwrap();

        // 有命中：综合回答，含来源内容，且不产生任何写入。
        let out = reflect
            .execute(
                serde_json::json!({"query": "我们怎么部署服务？", "context": "部署流程"}),
                &ctx(),
            )
            .await
            .unwrap();
        let text = text_of(&out);
        assert!(text.starts_with("Based on recalled memories:"), "{text}");
        assert!(text.contains("systemd"), "{text}");

        // 缺 query 报错。
        assert!(
            reflect
                .execute(serde_json::json!({}), &ctx())
                .await
                .is_err()
        );
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
            fn decide(
                &self,
                _req: &agent_core::ApprovalRequest<'_>,
            ) -> agent_core::ApprovalDecision {
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
            context: None,
            snapshots: None,
            tool_call_id: None,
        }
    }

    // ── memory_edit / learn（P0 记忆管理面）──────────────────────────────────

    /// `记录调用的测试记忆库（memory_edit` / learn 路由断言用）：
    /// forget 按 id 真实删除 hits，可经 search 观察副作用。
    struct MockStore {
        hits: std::sync::Mutex<Vec<agent_core::MemoryHit>>,
        banks: Vec<String>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl MockStore {
        /// 以 mark 为前缀统计调用次数。
        fn calls(&self, mark: &str) -> usize {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|c| c.starts_with(mark))
                .count()
        }

        fn push(&self, call: String) {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(call);
        }
    }

    #[async_trait]
    impl MemoryStore for MockStore {
        async fn summary(&self) -> Result<Option<String>, std::io::Error> {
            Ok(None)
        }
        async fn read_full(&self) -> Result<Option<String>, std::io::Error> {
            Ok(None)
        }
        async fn append_note(&self, _note: &agent_core::MemoryNote) -> Result<(), std::io::Error> {
            Ok(())
        }
        async fn clear(&self) -> Result<(), std::io::Error> {
            self.push("clear".into());
            Ok(())
        }
        fn root_dir(&self) -> &std::path::PathBuf {
            static ROOT: std::sync::LazyLock<std::path::PathBuf> =
                std::sync::LazyLock::new(|| std::path::PathBuf::from("/tmp/mock-memory"));
            &ROOT
        }
        async fn add_mental_model(&self, text: &str) -> Result<(), std::io::Error> {
            self.push(format!("mental_model|{text}"));
            Ok(())
        }
        async fn recall(&self, _query: &str, _limit: usize) -> Vec<agent_core::MemoryHit> {
            self.hits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
        async fn retain(
            &self,
            content: &str,
            importance: u8,
            source: &str,
        ) -> Result<(), std::io::Error> {
            self.push(format!("retain|{source}|{importance}|{content}"));
            Ok(())
        }
        async fn forget(&self, id: &str) -> Result<bool, std::io::Error> {
            self.push(format!("forget|{id}"));
            let mut hits = self
                .hits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let len_before = hits.len();
            hits.retain(|h| h.id != id);
            Ok(hits.len() < len_before)
        }
        async fn banks(&self) -> Vec<String> {
            self.banks.clone()
        }
    }

    fn mock_hit(id: &str, content: &str, score: f64) -> agent_core::MemoryHit {
        agent_core::MemoryHit {
            id: id.into(),
            content: content.into(),
            score,
            source: "test".into(),
            importance: 3,
        }
    }

    fn mock_store(banks: Vec<&str>) -> std::sync::Arc<MockStore> {
        std::sync::Arc::new(MockStore {
            hits: std::sync::Mutex::new(vec![
                mock_hit("h1", "用户偏好 Bun 而非 Node", 0.923),
                mock_hit("h2", "部署用 systemd 单元", 0.5),
            ]),
            banks: banks.into_iter().map(String::from).collect(),
            calls: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// `Arc<MockStore>` → `Arc<dyn MemoryStore>`（返回位 unsize 强转）。
    fn dyn_mem(store: std::sync::Arc<MockStore>) -> std::sync::Arc<dyn MemoryStore> {
        store
    }

    // search：渲染 `1. [0.92] content（id: …）`，缺 query 拒绝。
    #[tokio::test]
    async fn memory_edit_search_renders_scored_hits_with_id() {
        let tool = MemoryEditTool::new(mock_store(vec![]));
        let out = tool
            .execute(
                serde_json::json!({ "op": "search", "query": "用户喜欢哪个运行时" }),
                &ctx(),
            )
            .await
            .unwrap();
        let text = text_of(&out);
        assert!(
            text.contains("1. [0.92] 用户偏好 Bun 而非 Node（id: h1）"),
            "{text}"
        );
        assert!(
            text.contains("2. [0.50] 部署用 systemd 单元（id: h2）"),
            "{text}"
        );
        assert!(text.contains("2 条命中"), "{text}");

        assert!(
            tool.execute(serde_json::json!({ "op": "search" }), &ctx())
                .await
                .is_err()
        );
    }

    // forget：命中删除（search 可观察）；未命中报告；缺 id 拒绝。
    #[tokio::test]
    async fn memory_edit_forget_hit_and_miss() {
        let store = mock_store(vec![]);
        let tool = MemoryEditTool::new(dyn_mem(std::sync::Arc::clone(&store)));

        let out = tool
            .execute(serde_json::json!({ "op": "forget", "id": "h1" }), &ctx())
            .await
            .unwrap();
        assert!(text_of(&out).contains("已遗忘记忆 h1"));

        // 副作用可观察：h1 已不在检索结果中。
        let out = tool
            .execute(serde_json::json!({ "op": "search", "query": "q" }), &ctx())
            .await
            .unwrap();
        assert!(!text_of(&out).contains("id: h1"), "h1 应已被删除");

        let out = tool
            .execute(
                serde_json::json!({ "op": "forget", "id": "missing" }),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(text_of(&out).contains("未找到记忆 missing"));
        assert_eq!(store.calls("forget|"), 2);

        assert!(
            tool.execute(serde_json::json!({ "op": "forget" }), &ctx())
                .await
                .is_err()
        );
    }

    // banks：列出库；空列表给单库提示。
    #[tokio::test]
    async fn memory_edit_banks_lists() {
        let tool = MemoryEditTool::new(mock_store(vec!["default", "proj-x"]));
        let out = tool
            .execute(serde_json::json!({ "op": "banks" }), &ctx())
            .await
            .unwrap();
        let text = text_of(&out);
        assert!(text.contains("记忆库（2 个）"), "{text}");
        assert!(text.contains("1. default"), "{text}");
        assert!(text.contains("2. proj-x"), "{text}");

        let tool = MemoryEditTool::new(mock_store(vec![]));
        let out = tool
            .execute(serde_json::json!({ "op": "banks" }), &ctx())
            .await
            .unwrap();
        assert!(text_of(&out).contains("无记忆库 bank"));
    }

    // clear：confirm 缺失/false 拒绝且零副作用；true 执行一次。
    #[tokio::test]
    async fn memory_edit_clear_requires_confirm() {
        let store = mock_store(vec![]);
        let tool = MemoryEditTool::new(dyn_mem(std::sync::Arc::clone(&store)));
        assert!(
            tool.execute(serde_json::json!({ "op": "clear" }), &ctx())
                .await
                .is_err()
        );
        assert!(
            tool.execute(
                serde_json::json!({ "op": "clear", "confirm": false }),
                &ctx()
            )
            .await
            .is_err()
        );
        assert_eq!(store.calls("clear"), 0, "confirm 缺失时不应触达存储");

        let out = tool
            .execute(
                serde_json::json!({ "op": "clear", "confirm": true }),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(text_of(&out).contains("已清空项目记忆"));
        assert_eq!(store.calls("clear"), 1);
    }

    // 非法 op / 缺 op 拒绝。
    #[tokio::test]
    async fn memory_edit_rejects_unknown_and_missing_op() {
        let tool = MemoryEditTool::new(mock_store(vec![]));
        assert!(
            tool.execute(serde_json::json!({ "op": "nuke" }), &ctx())
                .await
                .is_err()
        );
        assert!(tool.execute(serde_json::json!({}), &ctx()).await.is_err());
    }

    // learn 三 kind 路由：fact（默认）/lesson → retain(4, tool:learn)；
    // mental_model → add_mental_model；未知 kind 拒绝。
    #[tokio::test]
    async fn learn_routes_by_kind() {
        let store = mock_store(vec![]);
        let tool = MemoryLearnTool::new(dyn_mem(std::sync::Arc::clone(&store)));

        // 默认 kind=fact。
        tool.execute(
            serde_json::json!({ "content": "用户偏好 pnpm 而非 npm" }),
            &ctx(),
        )
        .await
        .unwrap();
        // 显式 lesson。
        tool.execute(
            serde_json::json!({ "content": "改动解析器前先跑基准", "kind": "lesson" }),
            &ctx(),
        )
        .await
        .unwrap();
        assert_eq!(
            store.calls("retain|tool:learn|"),
            2,
            "fact/lesson 均路由 retain"
        );
        assert_eq!(store.calls("retain|tool:learn|4|"), 2, "importance 固定 4");

        // mental_model 路由 add_mental_model，不落检索库。
        tool.execute(
            serde_json::json!({ "content": "测试金字塔优于单一 E2E", "kind": "mental_model" }),
            &ctx(),
        )
        .await
        .unwrap();
        assert_eq!(
            store.calls("retain|tool:learn|"),
            2,
            "mental_model 不走 retain"
        );
        assert_eq!(store.calls("mental_model|"), 1);

        // 未知 kind 拒绝；缺 content 拒绝。
        assert!(
            tool.execute(
                serde_json::json!({ "content": "x", "kind": "skill" }),
                &ctx()
            )
            .await
            .is_err()
        );
        assert!(
            tool.execute(serde_json::json!({ "kind": "fact" }), &ctx())
                .await
                .is_err()
        );
    }
}
