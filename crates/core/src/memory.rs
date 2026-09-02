//! Memory 端口：跨会话长期记忆。
//!
//! 移植自 oh-my-pi `memory.backend: local`（local summary pipeline）的端口抽象。
//! 实现（如 [`crate::memory`] 的 `LocalMemoryStore`）在独立 crate；本模块仅放跨 crate 共享的端口。
//!
//! 设计：按项目（cwd）作用域，启动注入一份摘要到 system prompt；任务后可追加事实，
//! 定期经 LLM 合并为长期记忆文档。Zoo-Code 无此能力，本项目补齐。

use std::path::PathBuf;

/// 一条记忆笔记（待合并的原始事实）。
#[derive(Debug, Clone)]
pub struct MemoryNote {
    /// 笔记正文。
    pub content: String,
    /// 来源（如 "session:<id>"、"learn-tool"）。
    pub source: String,
}

/// 一条检索命中（`recall` 工具 / 注入渲染用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryHit {
    /// 记录 id。
    pub id: String,
    /// 正文。
    pub content: String,
    /// 综合得分（0..=1 量级，词法/重要性/时间/向量融合）。
    pub score: f64,
    /// 来源（如 "session:<id>"、"auto-retain"）。
    pub source: String,
    /// 重要性 0..=5。
    pub importance: u8,
}

impl MemoryHit {
    /// 渲染为 `- [score] content` 行（system 注入 / 压缩上下文用，与 omp 注入格式一致）。
    #[must_use]
    pub fn render_list(hits: &[Self]) -> String {
        let mut out = String::new();
        for h in hits {
            out.push_str(&format!(
                "- [{:.2}] {}\n",
                h.score,
                h.content.replace('\n', " ")
            ));
        }
        out
    }
}

/// 记忆存储端口（按项目作用域）。
///
/// 实现负责按 cwd（或其哈希）划分独立记忆库，跨会话持久化。
#[async_trait::async_trait]
pub trait MemoryStore: Send + Sync {
    /// 启动注入用：返回已合并的简洁摘要（`memory_summary.md`）；无则 `None`。
    ///
    /// # Errors
    /// 读取失败时返回 IO 错误。
    async fn summary(&self) -> Result<Option<String>, std::io::Error>;

    /// 完整长期记忆文档（`MEMORY.md`）；无则 `None`。
    ///
    /// # Errors
    /// 读取失败时返回 IO 错误。
    async fn read_full(&self) -> Result<Option<String>, std::io::Error>;

    /// 追加一条待合并的事实到 raw notes。
    ///
    /// # Errors
    /// 写入失败时返回 IO 错误。
    async fn append_note(&self, note: &MemoryNote) -> Result<(), std::io::Error>;

    /// 清空该项目记忆（summary + MEMORY.md + raw notes）。
    ///
    /// # Errors
    /// 删除失败时返回 IO 错误。
    async fn clear(&self) -> Result<(), std::io::Error>;

    /// 该项目记忆库根目录（调试/`memory://` 用）。
    fn root_dir(&self) -> &PathBuf;

    /// 心智模型注入段（seeds + 项目积累，`<mental_models>` 块内容）；无则 `None`。
    /// 默认无（未配置心智模型的实现 / 装配场景）。
    async fn mental_models(&self) -> Option<String> {
        None
    }

    /// 语义检索（`recall` 工具 / 注入用）：按查询返回 top-K 命中。
    /// 默认空（local 后端无检索能力，与 oh-my-pi 一致：recall 工具仅 structured 系后端启用）。
    async fn recall(&self, _query: &str, _limit: usize) -> Vec<MemoryHit> {
        Vec::new()
    }

    /// 显式保留一条事实（`retain` 工具）：importance 0..=5。
    /// 默认路由 [`Self::append_note`]（local 语义：进入 raw notes，任务末 LLM 合并）。
    ///
    /// # Errors
    /// 写入失败时返回 IO 错误。
    async fn retain(
        &self,
        content: &str,
        _importance: u8,
        source: &str,
    ) -> Result<(), std::io::Error> {
        self.append_note(&MemoryNote {
            content: content.to_string(),
            source: source.to_string(),
        })
        .await
    }

    /// 按记录 id 遗忘（删除）一条记忆（`memory_edit` forget 用）：返回是否删除成功。
    /// 默认 `Ok(false)`——后端不支持逐条删除（如 local 后端按内容追加、经 LLM
    /// 合并成文档，记录无稳定 id，按内容匹配不可靠，故不支持）。
    ///
    /// # Errors
    /// 删除失败时返回 IO 错误。
    async fn forget(&self, _id: &str) -> Result<bool, std::io::Error> {
        Ok(false)
    }

    /// 列出记忆库 bank 名（`memory_edit` banks 用）；默认空
    /// （单库后端 / 无 bank 概念的实现）。
    async fn banks(&self) -> Vec<String> {
        Vec::new()
    }

    /// 追加一条心智模型（`reflect` 工具 / LLM 提炼用）：写入项目 `mental_models.md`，
    /// 下次会话经 [`Self::mental_models`] 注入 system prompt。
    ///
    /// # Errors
    /// 写入失败时返回 IO 错误。
    async fn add_mental_model(&self, text: &str) -> Result<(), std::io::Error>;
}
