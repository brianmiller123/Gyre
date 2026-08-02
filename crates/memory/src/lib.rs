//! # agent-memory
//!
//! 跨会话长期记忆（移植 oh-my-pi `memory.backend: local` 的 local summary pipeline，去 SQLite/embedding）。
//!
//! - [`LocalMemoryStore`]：按项目（cwd）作用域，markdown 存储 + LLM 合并
//! - 启动注入 `summary()` 到 system prompt；任务后 `append_note()` 追加事实；
//!   `consolidate()` 用 LLM 把 raw notes + 旧 MEMORY.md 合并为新 MEMORY.md + memory_summary.md
//! - 心智模型（hindsight 风格）：`mental_models()` 合并内置/自定义 seeds 与项目 mental_models.md
//!   注入 `<mental_models>` 段；`add_mental_model()` 追加时间戳条目；
//!   `consolidate_mental_models()` 用 LLM 去重/提炼
//! - 向量记忆（[`vec_memory`]）：旁路 `vecs.jsonl` + 双层嵌入（L1 固定种子投影 /
//!   L2 fastembed 语义，`vec-embed` feature 门控），可经
//!   [`StructuredMemoryStore::with_embedder`] 挂载做 dense 融合检索
//!
//! 存储布局：`<config_dir>/memory/<cwd 哈希十六进制>/{MEMORY.md, memory_summary.md, notes.jsonl, mental_models.md}`。
//! Zoo-Code 无此能力，本项目补齐项目作用域的跨会话记忆。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

mod mental_models;
mod store;
mod structured;
mod vec_memory;

pub use mental_models::{
    format_ts, load_seeds, merge_mental_models, mental_model_consolidation_prompt, MentalModelsConfig,
    SeedEntry, MENTAL_MODEL_CONSOLIDATION_PROMPT, SEEDS_JSON,
};
pub use store::{consolidation_prompt, LocalMemoryStore};
pub use structured::{
    MemoryRecord, MemoryStats, RecallHit, RecallOptions, SearchFilter, StructuredMemoryStore,
};
pub use vec_memory::{Embedder, ProjectionEmbedder, StubEmbedder, VecEntry, VectorStore};
#[cfg(feature = "vec-embed")]
pub use vec_memory::FastembedEmbedder;

/// 默认嵌入器（装配层入口）：`vec-embed` feature 开启时用 L2 fastembed 语义嵌入
/// （懒加载 + 失败自动降级 L1），关闭时用 L1 固定种子随机投影，确定性离线可用。
#[must_use]
pub fn default_embedder() -> std::sync::Arc<dyn Embedder> {
    #[cfg(feature = "vec-embed")]
    {
        std::sync::Arc::new(FastembedEmbedder::new())
    }
    #[cfg(not(feature = "vec-embed"))]
    {
        std::sync::Arc::new(ProjectionEmbedder::new(384))
    }
}
