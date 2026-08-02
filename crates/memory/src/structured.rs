//! Mnemopi 结构化记忆后端：retain / recall / search / forget + banks + 加权检索。
//!
//! 移植自 [`oh-my-pi mnemopi`](../../../third/oh-my-pi/packages/mnemopi) 的核心检索模型
//! （原版含 SQLite + 向量；本实现用 JSONL 存储 + BM25-ish 词项重叠 + 重要性 + 时间半衰期，
//! 覆盖 recall/retain/forget/banks 的语义，无外部依赖，按项目作用域）。
//!
//! 检索打分（[`StructuredMemoryStore::recall`]）：
//! ```text
//! score = vec_weight · max(0, cosine) + fts_weight · relevance + importance_weight · (importance/5) + temporal_weight · recency
//! relevance   = |query_tokens ∩ content_tokens| / sqrt(|content_tokens|)
//! recency     = 2 ^ (-age_hours / halflife_hours)
//! ```
//! 向量（dense）项仅在挂载 [`crate::vec_memory::Embedder`] 且 `vec_weight > 0` 时参与；
//! 嵌入失败/向量缺失一律静默跳过，严格回退词法公式（行为兼容）。

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_core::{MemoryNote, MemoryStore};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::mental_models::{load_seeds, merge_mental_models, MentalModelsConfig};
use crate::vec_memory::{
    cosine_similarity, decode_f32s, encode_f32s, Embedder, VecEntry, VectorStore,
};

const DEFAULT_BANK: &str = "default";
const MENTAL_MODELS_FILE: &str = "mental_models.md";
const RECORDS_FILE: &str = "records.jsonl";

/// 一条结构化记忆。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// 唯一 id。
    pub id: String,
    /// 正文。
    pub content: String,
    /// 来源（如 "session:<id>"、"learn-tool"）。
    pub source: String,
    /// 重要性 0..=5（默认 1）。
    #[serde(default = "default_importance")]
    pub importance: u8,
    /// 作用域标签（如 "project"、"global"）。
    #[serde(default)]
    pub scope: String,
    /// 自由元数据。
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    /// 标签。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 创建毫秒时间戳。
    pub ts: u64,
    /// 失效毫秒时间戳（None 表示永久）。
    #[serde(default)]
    pub valid_until: Option<u64>,
}

fn default_importance() -> u8 {
    1
}

/// 检索选项（权重）。
#[derive(Debug, Clone)]
pub struct RecallOptions {
    /// 词项相关性权重。
    pub fts_weight: f64,
    /// 重要性权重。
    pub importance_weight: f64,
    /// 时间衰减权重。
    pub temporal_weight: f64,
    /// 向量（dense）权重；为 0 或未挂载嵌入器时严格回退纯词法公式。
    pub vec_weight: f64,
    /// 时间半衰期（小时）。
    pub halflife_hours: f64,
    /// 返回 top-K。
    pub limit: usize,
}

impl Default for RecallOptions {
    fn default() -> Self {
        Self {
            fts_weight: 1.0,
            importance_weight: 0.5,
            temporal_weight: 0.3,
            vec_weight: 0.5, // 对齐 mnemopi 默认向量权重
            halflife_hours: 24.0 * 14.0, // 两周
            limit: 8,
        }
    }
}

/// 检索命中。
#[derive(Debug, Clone)]
pub struct RecallHit {
    /// 命中记录。
    pub record: MemoryRecord,
    /// 综合得分。
    pub score: f64,
}

/// 搜索过滤条件。
#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    /// 来源前缀过滤。
    pub source: Option<String>,
    /// 作用域过滤。
    pub scope: Option<String>,
    /// 标签过滤（命中任一）。
    pub tags: Vec<String>,
    /// 文本子串过滤。
    pub contains: Option<String>,
}

/// 记忆库统计。
#[derive(Debug, Clone, Default)]
pub struct MemoryStats {
    /// 总记录数（所有 bank）。
    pub total: usize,
    /// 逐 bank 计数。
    pub banks: BTreeMap<String, usize>,
    /// 最近一条时间戳。
    pub last_ts: Option<u64>,
}

/// Mnemopi 结构化记忆存储（按项目 cwd 哈希作用域，JSONL 持久化）。
pub struct StructuredMemoryStore {
    root: PathBuf,
    /// 可选的嵌入器（L1/L2）；`None` 时严格保持纯词法检索。
    embedder: Option<Arc<dyn Embedder>>,
    /// 可选的心智模型配置（seeds + 项目积累注入）；`None` 时不注入。
    mental_models_cfg: Option<MentalModelsConfig>,
}

impl StructuredMemoryStore {
    /// 按 `cwd` 哈希定位 `<config_dir>/memory-structured/<hash>`。
    #[must_use]
    pub fn new(cwd: &Path) -> Self {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        cwd.hash(&mut h);
        let hash = format!("{:016x}", h.finish());
        let root = agent_core::config_dir()
            .map(|d| d.join("memory-structured").join(&hash))
            .unwrap_or_else(|| PathBuf::from(".agent/memory-structured").join(hash));
        Self {
            root,
            embedder: None,
            mental_models_cfg: None,
        }
    }

    /// 测试用自定义根目录。
    #[must_use]
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            embedder: None,
            mental_models_cfg: None,
        }
    }

    /// 挂载嵌入器（向量融合检索）；不挂载时行为与旧版完全一致。
    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// 配置心智模型注入（seeds + 项目 mental_models.md）；默认不注入，行为与旧版一致。
    #[must_use]
    pub fn with_mental_models_config(mut self, cfg: MentalModelsConfig) -> Self {
        self.mental_models_cfg = Some(cfg);
        self
    }

    /// 注入用的心智模型 markdown（seeds 在前、项目积累在后）；未配置或全空返回 `None`。
    #[must_use]
    pub fn mental_models(&self) -> Option<String> {
        let cfg = self.mental_models_cfg.as_ref()?;
        let seeds = load_seeds(cfg);
        let project =
            std::fs::read_to_string(self.root.join(MENTAL_MODELS_FILE)).unwrap_or_default();
        merge_mental_models(&seeds, &project, cfg.max_inject_chars)
    }

    /// 准备 dense 信号：(查询向量, id → 向量条目)；无嵌入器/嵌入失败/空向量返回 `None`。
    /// 模型与当前不符（或首用空库）时清空并按当前模型重嵌全部存活记录。
    fn prepare_dense(
        &self,
        bank: &str,
        query: &str,
    ) -> Option<(Vec<f32>, HashMap<String, VecEntry>)> {
        let embedder = self.embedder.as_ref()?;
        let qv = embedder.embed(&[query.to_string()]).ok()?.into_iter().next()?;
        let vs = VectorStore::new(&self.bank_dir(bank));
        let entries = vs.all();
        let need_reembed =
            entries.is_empty() || entries.iter().any(|e| e.model != embedder.model_name());
        if need_reembed {
            self.reembed(bank, embedder.as_ref());
        }
        let by_id = vs.all().into_iter().map(|e| (e.id.clone(), e)).collect();
        Some((qv, by_id))
    }

    /// 用当前嵌入器重嵌 bank 内全部存活记录（失败静默跳过，向量只是加速件）。
    fn reembed(&self, bank: &str, embedder: &dyn Embedder) {
        let now = now_ms();
        let records: Vec<MemoryRecord> = self
            .read_bank(bank)
            .into_iter()
            .filter(|r| r.valid_until.is_none_or(|v| v >= now))
            .collect();
        if records.is_empty() {
            return;
        }
        let texts: Vec<String> = records.iter().map(|r| r.content.clone()).collect();
        let Ok(vecs) = embedder.embed(&texts) else {
            return; // 嵌入失败 → 保持无向量，dense 缺失即跳过
        };
        let entries: Vec<VecEntry> = records
            .iter()
            .zip(vecs)
            .map(|(r, v)| VecEntry {
                id: r.id.clone(),
                model: embedder.model_name(),
                dim: embedder.dim(),
                data: encode_f32s(&v),
            })
            .collect();
        let vs = VectorStore::new(&self.bank_dir(bank));
        if let Err(e) = vs.replace_all(&entries) {
            tracing::warn!(error = %e, "重写 vecs.jsonl 失败");
        }
    }

    fn bank_dir(&self, bank: &str) -> PathBuf {
        self.root
            .join(if bank.is_empty() { DEFAULT_BANK } else { bank })
    }

    fn records_path(&self, bank: &str) -> PathBuf {
        self.bank_dir(bank).join(RECORDS_FILE)
    }

    /// 保留一条记忆（默认 bank）。
    ///
    /// # Errors
    /// 写入失败时返回 IO 错误。
    pub fn retain(&self, record: MemoryRecord) -> std::io::Result<()> {
        self.retain_in(DEFAULT_BANK, record)
    }

    /// 保留一条记忆到指定 bank。
    ///
    /// # Errors
    /// 写入失败时返回 IO 错误。
    pub fn retain_in(&self, bank: &str, mut record: MemoryRecord) -> std::io::Result<()> {
        if record.id.is_empty() {
            record.id = uuid::Uuid::new_v4().to_string();
        }
        if record.ts == 0 {
            record.ts = now_ms();
        }
        std::fs::create_dir_all(self.bank_dir(bank))?;
        let line = serde_json::to_string(&record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        append_line(&self.records_path(bank), &line)
    }

    /// 读出某 bank 的全部记录。
    fn read_bank(&self, bank: &str) -> Vec<MemoryRecord> {
        read_records(&self.records_path(bank))
    }

    /// 列出存在的 bank 名。
    fn list_banks(&self) -> Vec<String> {
        let mut banks = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        banks.push(name.to_string());
                    }
                }
            }
        }
        banks.sort();
        banks
    }

    /// 检索：按 `query` 加权打分返回 top-K。
    #[must_use]
    pub fn recall(&self, query: &str, opts: &RecallOptions) -> Vec<RecallHit> {
        self.recall_in(DEFAULT_BANK, query, opts)
    }

    /// 指定 bank 检索。
    #[must_use]
    pub fn recall_in(&self, bank: &str, query: &str, opts: &RecallOptions) -> Vec<RecallHit> {
        let query_tokens = tokenize(query);
        let now = now_ms();
        let now_hours = now as f64 / 3_600_000.0;
        // 向量（dense）融合：仅挂载嵌入器、vec_weight > 0 且查询非空时启用；
        // 嵌入失败/向量缺失一律静默降级，严格回退词法公式（行为兼容）。
        let dense: Option<(Vec<f32>, HashMap<String, VecEntry>)> =
            if opts.vec_weight > 0.0 && !query_tokens.is_empty() {
                self.prepare_dense(bank, query)
            } else {
                None
            };
        let mut hits: Vec<RecallHit> = self
            .read_bank(bank)
            .into_iter()
            .filter(|r| r.valid_until.is_none_or(|v| v >= now))
            .map(|record| {
                let content_tokens = tokenize(&record.content);
                let relevance = bm25_relevance(&query_tokens, &content_tokens);
                let recency = 2f64
                    .powf(-((now_hours - record.ts as f64 / 3_600_000.0) / opts.halflife_hours));
                let importance = f64::from(record.importance.min(5)) / 5.0;
                let mut score = opts.fts_weight * relevance
                    + opts.importance_weight * importance
                    + opts.temporal_weight * recency.clamp(0.0, 1.0);
                // dense 融合：`vec_weight · max(0, cosine)`；缺失/损坏向量只跳过，不报错
                if let Some((qv, by_id)) = &dense {
                    if let Some(entry) = by_id.get(&record.id) {
                        if let Some(v) = decode_f32s(&entry.data) {
                            if v.len() == qv.len() {
                                score += opts.vec_weight * cosine_similarity(qv, &v).max(0.0);
                            }
                        }
                    }
                }
                RecallHit { record, score }
            })
            .filter(|h| !query_tokens.is_empty() && h.score > 0.0 || query_tokens.is_empty())
            .collect();
        // 无查询时按「重要性 + 新近度」排，有查询时按综合分排
        if query_tokens.is_empty() {
            hits.sort_by(|a, b| {
                b.record
                    .importance
                    .cmp(&a.record.importance)
                    .then_with(|| b.record.ts.cmp(&a.record.ts))
            });
        } else {
            hits.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            hits.retain(|h| h.score > 0.0);
        }
        hits.truncate(opts.limit.max(1));
        hits
    }

    /// 按过滤条件搜索（无打分，按时间倒序）。
    #[must_use]
    pub fn search(&self, filter: &SearchFilter) -> Vec<MemoryRecord> {
        self.search_in(DEFAULT_BANK, filter)
    }

    /// 指定 bank 搜索。
    #[must_use]
    pub fn search_in(&self, bank: &str, filter: &SearchFilter) -> Vec<MemoryRecord> {
        let now = now_ms();
        let mut out: Vec<MemoryRecord> = self
            .read_bank(bank)
            .into_iter()
            .filter(|r| r.valid_until.is_none_or(|v| v >= now))
            .filter(|r| {
                filter
                    .source
                    .as_ref()
                    .is_none_or(|s| r.source.starts_with(s))
            })
            .filter(|r| filter.scope.as_ref().is_none_or(|s| &r.scope == s))
            .filter(|r| {
                filter
                    .contains
                    .as_ref()
                    .is_none_or(|c| r.content.contains(c))
            })
            .filter(|r| {
                filter.tags.is_empty()
                    || filter.tags.iter().any(|t| r.tags.iter().any(|rt| rt == t))
            })
            .collect();
        out.sort_by(|a, b| b.ts.cmp(&a.ts));
        out
    }

    /// 按 id 遗忘（删除）。返回是否删除成功。
    ///
    /// # Errors
    /// 重写失败时返回 IO 错误。
    pub fn forget(&self, id: &str) -> std::io::Result<bool> {
        self.forget_in(DEFAULT_BANK, id)
    }

    /// 指定 bank 遗忘。
    ///
    /// # Errors
    /// 重写失败时返回 IO 错误。
    pub fn forget_in(&self, bank: &str, id: &str) -> std::io::Result<bool> {
        let path = self.records_path(bank);
        let records = read_records(&path);
        let remaining: Vec<&MemoryRecord> = records.iter().filter(|r| r.id != id).collect();
        if remaining.len() == records.len() {
            return Ok(false);
        }
        rewrite_records(
            &path,
            &remaining.iter().map(|r| (*r).clone()).collect::<Vec<_>>(),
        )?;
        Ok(true)
    }

    /// 全量统计（所有 bank）。
    #[must_use]
    pub fn stats(&self) -> MemoryStats {
        let mut total = 0usize;
        let mut banks = BTreeMap::new();
        let mut last_ts: Option<u64> = None;
        for bank in self.list_banks() {
            let records = self.read_bank(&bank);
            let count = records.len();
            total += count;
            banks.insert(bank, count);
            for r in &records {
                last_ts = Some(last_ts.map_or(r.ts, |t| t.max(r.ts)));
            }
        }
        MemoryStats {
            total,
            banks,
            last_ts,
        }
    }

    /// 把命中/记录渲染为 Markdown 列表（供 system prompt 注入）。
    #[must_use]
    pub fn render_summary(hits: &[RecallHit]) -> String {
        if hits.is_empty() {
            return String::new();
        }
        let mut out = String::from("# 长期记忆（相关条目）\n\n");
        for h in hits {
            out.push_str(&format!(
                "- [{}·{}] {}\n",
                h.record.source,
                h.record.importance,
                h.record.content.replace('\n', " ")
            ));
        }
        out
    }
}

#[async_trait]
impl MemoryStore for StructuredMemoryStore {
    async fn summary(&self) -> Result<Option<String>, std::io::Error> {
        let hits = self.recall("", &RecallOptions::default());
        let base = if hits.is_empty() {
            None
        } else {
            Some(Self::render_summary(&hits))
        };
        let mental = self.mental_models();
        Ok(match (base, mental) {
            (None, None) => None,
            (Some(base), None) => Some(base),
            (None, Some(mental)) => Some(format!("<mental_models>\n{mental}\n</mental_models>")),
            (Some(base), Some(mental)) => {
                Some(format!("{base}\n\n<mental_models>\n{mental}\n</mental_models>"))
            }
        })
    }

    async fn read_full(&self) -> Result<Option<String>, std::io::Error> {
        let records = self.read_bank(DEFAULT_BANK);
        if records.is_empty() {
            return Ok(None);
        }
        let mut out = String::from("# MEMORY\n\n");
        for r in &records {
            out.push_str(&format!(
                "- [{}·{}·scope={}] {}\n",
                r.source,
                r.importance,
                r.scope,
                r.content.replace('\n', " ")
            ));
        }
        Ok(Some(out))
    }

    async fn append_note(&self, note: &MemoryNote) -> Result<(), std::io::Error> {
        let record = MemoryRecord {
            id: String::new(),
            content: note.content.clone(),
            source: note.source.clone(),
            importance: 1,
            scope: String::new(),
            metadata: BTreeMap::new(),
            tags: Vec::new(),
            ts: now_ms(),
            valid_until: None,
        };
        self.retain(record)
    }

    async fn clear(&self) -> Result<(), std::io::Error> {
        if self.root.exists() {
            std::fs::remove_dir_all(&self.root)?;
        }
        Ok(())
    }

    fn root_dir(&self) -> &PathBuf {
        &self.root
    }
}

// ── 评分与工具 ─────────────────────────────────────────────────────────────

/// 简单 BM25-ish 词项重叠相关性。
fn bm25_relevance(query_tokens: &[String], content_tokens: &[String]) -> f64 {
    if query_tokens.is_empty() || content_tokens.is_empty() {
        return 0.0;
    }
    let content_set: HashMap<&str, usize> = {
        let mut m = HashMap::new();
        for t in content_tokens {
            *m.entry(t.as_str()).or_insert(0) += 1;
        }
        m
    };
    let mut overlap = 0usize;
    for qt in query_tokens {
        if content_set.contains_key(qt.as_str()) {
            overlap += 1;
        }
    }
    let n = content_tokens.len() as f64;
    overlap as f64 / n.sqrt()
}

/// 小写化 + 非字母数字切分。
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

fn read_records(path: &Path) -> Vec<MemoryRecord> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<MemoryRecord>(line) {
            out.push(rec);
        }
    }
    out
}

fn rewrite_records(path: &Path, records: &[MemoryRecord]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    for r in records {
        if let Ok(line) = serde_json::to_string(r) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    std::fs::write(path, text)
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut content = String::new();
    if path.exists() {
        content = std::fs::read_to_string(path).unwrap_or_default();
    }
    content.push_str(line);
    content.push('\n');
    std::fs::write(path, content)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vec_memory::{ProjectionEmbedder, StubEmbedder};

    fn store() -> StructuredMemoryStore {
        store_root().0
    }

    /// 返回 (store, root)；root 供直接检查旁路 vecs.jsonl。
    fn store_root() -> (StructuredMemoryStore, PathBuf) {
        let d = std::env::temp_dir().join(format!(
            "agent-mnemopi-{}-{:#x}",
            std::process::id(),
            nano()
        ));
        std::fs::create_dir_all(&d).unwrap();
        let root = d.join("mem");
        (StructuredMemoryStore::with_root(root.clone()), root)
    }
    fn nano() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn rec(content: &str, importance: u8) -> MemoryRecord {
        MemoryRecord {
            id: String::new(),
            content: content.into(),
            source: "test".into(),
            importance,
            scope: "project".into(),
            metadata: BTreeMap::new(),
            tags: vec![],
            ts: now_ms(),
            valid_until: None,
        }
    }

    #[tokio::test]
    async fn retain_recall_ranks_relevant() {
        let s = store();
        s.retain(rec("The build uses cargo workspace", 2)).unwrap();
        s.retain(rec("Dinner recipe for pasta", 1)).unwrap();
        s.retain(rec("cargo test runs the suite", 3)).unwrap();

        let hits = s.recall("cargo build", &RecallOptions::default());
        assert!(!hits.is_empty());
        // 最相关应含 cargo/build 字样
        assert!(hits[0].record.content.contains("cargo"));
    }

    #[tokio::test]
    async fn search_filters_by_tag_and_scope() {
        let s = store();
        let mut a = rec("alpha note", 1);
        a.tags = vec!["important".into()];
        let mut b = rec("beta note", 1);
        b.scope = "global".into();
        s.retain(a).unwrap();
        s.retain(b).unwrap();

        let by_tag = s.search(&SearchFilter {
            tags: vec!["important".into()],
            ..Default::default()
        });
        assert_eq!(by_tag.len(), 1);
        assert_eq!(by_tag[0].content, "alpha note");

        let by_scope = s.search(&SearchFilter {
            scope: Some("global".into()),
            ..Default::default()
        });
        assert_eq!(by_scope.len(), 1);
        assert_eq!(by_scope[0].content, "beta note");
    }

    #[tokio::test]
    async fn forget_removes_by_id() {
        let s = store();
        let r = rec("to be forgotten", 1);
        let id = if r.id.is_empty() {
            let id = uuid::Uuid::new_v4().to_string();
            id
        } else {
            r.id.clone()
        };
        let mut r = r;
        r.id = id.clone();
        s.retain(r).unwrap();
        assert!(s.forget(&id).unwrap());
        assert!(!s.forget(&id).unwrap());
    }

    #[tokio::test]
    async fn memory_store_trait_wiring() {
        let s = store();
        s.append_note(&MemoryNote {
            content: "trait wiring".into(),
            source: "unit".into(),
        })
        .await
        .unwrap();
        let summary = s.summary().await.unwrap();
        assert!(summary.is_some());
        let full = s.read_full().await.unwrap().unwrap();
        assert!(full.contains("trait wiring"));
    }

    #[test]
    fn bm25_zero_on_disjoint() {
        assert_eq!(bm25_relevance(&tokenize("foo"), &tokenize("bar baz")), 0.0);
        assert!(bm25_relevance(&tokenize("foo"), &tokenize("foo bar")) > 0.0);
    }

    /// 测试用固定向量嵌入器：手工指定「文本 → 向量」，便于构造 dense 高分/低分场景。
    struct FixedEmbedder {
        map: HashMap<String, Vec<f32>>,
        model: String,
    }

    impl Embedder for FixedEmbedder {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts
                .iter()
                .map(|t| self.map.get(t).cloned().unwrap_or_else(|| vec![0.0; 4]))
                .collect())
        }

        fn dim(&self) -> usize {
            4
        }

        fn model_name(&self) -> String {
            self.model.clone()
        }
    }

    /// 永远失败的嵌入器（验证降级路径不 panic）。
    struct ErrEmbedder;

    impl Embedder for ErrEmbedder {
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Err("故意失败".into())
        }

        fn dim(&self) -> usize {
            4
        }

        fn model_name(&self) -> String {
            "err".into()
        }
    }

    #[test]
    fn recall_options_default_vec_weight() {
        assert_eq!(RecallOptions::default().vec_weight, 0.5);
    }

    #[tokio::test]
    async fn recall_vec_weight_zero_matches_legacy() {
        // 相同记录、相同 ts：挂载嵌入器但 vec_weight=0 时应与无嵌入器逐项相等
        let mut a1 = rec("cargo workspace build", 2);
        a1.ts = 1_700_000_000_000;
        let mut a2 = rec("pasta dinner recipe", 1);
        a2.ts = 1_700_000_000_000;
        let base = store();
        let embedded = store().with_embedder(Arc::new(StubEmbedder::new(8)));
        base.retain(a1.clone()).unwrap();
        base.retain(a2.clone()).unwrap();
        embedded.retain(a1).unwrap();
        embedded.retain(a2).unwrap();

        let opts = RecallOptions {
            vec_weight: 0.0,
            ..Default::default()
        };
        let a = base.recall("cargo build", &opts);
        let b = embedded.recall("cargo build", &opts);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.record.content, y.record.content);
            assert_eq!(x.score, y.score); // 逐项相等
        }
    }

    #[tokio::test]
    async fn recall_dense_beats_lexical() {
        let mut map = HashMap::new();
        map.insert("cargo build".to_string(), vec![0.0f32, 0.0, 1.0, 0.0]); // 查询向量
        map.insert("cargo build docs".to_string(), vec![1.0, 0.0, 0.0, 0.0]); // 词法高分、dense 零
        map.insert("rust memory notes".to_string(), vec![0.0, 0.0, 1.0, 0.0]); // 词法零、dense 满分
        let s = store().with_embedder(Arc::new(FixedEmbedder {
            map,
            model: "fixed".into(),
        }));
        s.retain(rec("cargo build docs", 1)).unwrap();
        s.retain(rec("rust memory notes", 1)).unwrap();

        let opts = RecallOptions {
            fts_weight: 1.0,
            importance_weight: 0.0,
            temporal_weight: 0.0,
            vec_weight: 2.0,
            ..Default::default()
        };
        let hits = s.recall("cargo build", &opts);
        assert_eq!(hits.len(), 2);
        // dense 满分（+2.0）越过词法高分（relevance≈1.155）
        assert_eq!(hits[0].record.content, "rust memory notes");
        assert!(hits[0].score > hits[1].score);
    }

    #[tokio::test]
    async fn recall_semantic_ranks_paraphrase() {
        // L1 投影嵌入：词法零重叠但 n-gram 高度重合的改写文本应排前
        let mut r1 = rec("build the cargo workspace", 1);
        r1.ts = 1_700_000_000_000;
        let mut r2 = rec("lunch pasta recipe dinner", 1);
        r2.ts = 1_700_000_000_000;
        let s = store().with_embedder(Arc::new(ProjectionEmbedder::new(128)));
        s.retain(r1).unwrap();
        s.retain(r2).unwrap();

        let opts = RecallOptions {
            fts_weight: 0.0, // 纯向量排序：验证 dense 融合确实生效
            importance_weight: 0.0,
            temporal_weight: 0.0,
            vec_weight: 1.0,
            ..Default::default()
        };
        let hits = s.recall("cargo workspace build", &opts);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].record.content, "build the cargo workspace");
    }

    #[tokio::test]
    async fn recall_model_change_triggers_reembed() {
        let (s1, root) = store_root();
        s1.retain(rec("cargo build notes", 2)).unwrap();
        s1.retain(rec("rust memory notes", 2)).unwrap();
        let s1 = s1.with_embedder(Arc::new(StubEmbedder::new(8)));
        assert!(!s1.recall("cargo", &RecallOptions::default()).is_empty());
        // 首次 recall 触发重嵌，模型为 stub
        let entries = VectorStore::new(&root.join("default")).all();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.model == "stub"));

        // 换用另一模型 → 检测到模型变更 → 清空重嵌（不残留旧模型条目）
        let s2 = StructuredMemoryStore::with_root(root.clone())
            .with_embedder(Arc::new(ProjectionEmbedder::new(8)));
        assert!(!s2.recall("cargo", &RecallOptions::default()).is_empty());
        let entries2 = VectorStore::new(&root.join("default")).all();
        assert_eq!(entries2.len(), 2);
        assert!(entries2.iter().all(|e| e.model == "projection"));
    }

    #[tokio::test]
    async fn recall_missing_vectors_skip_dense() {
        let (s, _root) = store_root();
        let mut r1 = rec("cargo build notes", 2);
        r1.ts = 1_700_000_000_000;
        s.retain(r1).unwrap();
        let s = s.with_embedder(Arc::new(StubEmbedder::new(8)));
        let opts = RecallOptions::default();
        assert!(!s.recall("cargo", &opts).is_empty()); // 首次 recall 重嵌 r1
        // 之后新 retain 的记录没有向量：缺失向量只跳过 dense 信号，不 panic
        let mut r2 = rec("cargo build more", 2);
        r2.ts = 1_700_000_000_000;
        s.retain(r2).unwrap();
        let hits = s.recall("cargo", &opts);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| h.record.content.contains("more")));
    }

    #[tokio::test]
    async fn recall_failing_embedder_falls_back_silently() {
        let mut r = rec("cargo build notes", 2);
        r.ts = 1_700_000_000_000;
        let s = store().with_embedder(Arc::new(ErrEmbedder));
        s.retain(r.clone()).unwrap();
        let plain = store();
        plain.retain(r).unwrap();
        let opts = RecallOptions::default();
        let hits = s.recall("cargo", &opts);
        assert!(!hits.is_empty()); // 嵌入失败 → 纯词法检索仍可用
        let base = plain.recall("cargo", &opts);
        assert_eq!(hits[0].score, base[0].score);
    }

    // 接线：心智模型配置后 summary 注入 seeds + 项目积累（对齐 LocalMemoryStore 格式）。
    #[tokio::test]
    async fn summary_merges_mental_models_when_configured() {
        let (s, root) = store_root();
        // 未配置：无 mental 段，纯记录摘要。
        assert!(!s.mental_models().is_some());
        s.retain(rec("rust workspace notes", 2)).unwrap();
        let plain = s.summary().await.unwrap().unwrap();
        assert!(plain.contains("rust workspace notes"));
        assert!(!plain.contains("<mental_models>"));
        // 配置后：项目积累条目注入（内置 seeds 非空，必然出现 mental 段）。
        std::fs::write(
            root.join(MENTAL_MODELS_FILE),
            "- [2026-08-02 12:00:00] 项目约定：先读 AGENTS.md 再动手\n",
        )
        .unwrap();
        let s2 = s.with_mental_models_config(crate::MentalModelsConfig::default());
        let merged = s2.summary().await.unwrap().unwrap();
        assert!(merged.contains("<mental_models>"), "缺少 mental 段: {merged}");
        assert!(merged.contains("项目约定"), "缺少项目积累: {merged}");
        assert!(merged.contains("rust workspace notes"), "记录摘要被覆盖");
        // 空记忆 + 配置心智模型：只出 mental 段。
        let (s3, _) = store_root();
        let s3 = s3.with_mental_models_config(crate::MentalModelsConfig::default());
        let m3 = s3.summary().await.unwrap().unwrap();
        assert!(m3.starts_with("<mental_models>"));
    }
}
