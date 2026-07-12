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
}
