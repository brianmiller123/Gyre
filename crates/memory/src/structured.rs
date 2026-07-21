//! Mnemopi 结构化记忆后端：retain / recall / search / forget + banks + 加权检索。
//!
//! 移植自 [`oh-my-pi mnemopi`](../../../third/oh-my-pi/packages/mnemopi) 的核心检索模型
//! （原版含 SQLite + 向量；本实现用 JSONL 存储 + BM25-ish 词项重叠 + 重要性 + 时间半衰期，
//! 覆盖 recall/retain/forget/banks 的语义，无外部依赖，按项目作用域）。
//!
//! 检索打分（[`StructuredMemoryStore::recall`]）：
//! ```text
//! score = fts_weight · relevance + importance_weight · (importance/5) + temporal_weight · recency
//! relevance   = |query_tokens ∩ content_tokens| / sqrt(|content_tokens|)
//! recency     = 2 ^ (-age_hours / halflife_hours)
//! ```

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use agent_core::{MemoryNote, MemoryStore};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

const DEFAULT_BANK: &str = "default";
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
        Self { root }
    }

    /// 测试用自定义根目录。
    #[must_use]
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
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
                let score = opts.fts_weight * relevance
                    + opts.importance_weight * importance
                    + opts.temporal_weight * recency.clamp(0.0, 1.0);
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
        if hits.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Self::render_summary(&hits)))
        }
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

    fn store() -> StructuredMemoryStore {
        let d = std::env::temp_dir().join(format!(
            "agent-mnemopi-{}-{:#x}",
            std::process::id(),
            nano()
        ));
        std::fs::create_dir_all(&d).unwrap();
        StructuredMemoryStore::with_root(d.join("mem"))
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
}
