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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_core::{MemoryNote, MemoryStore};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::intent::{adjust_weights, classify_intent};
use crate::mental_models::{load_seeds, merge_mental_models, MentalModelsConfig};
use crate::mmr::{containment_similarity, jaccard_similarity, mmr_rerank_indices, strip_cjk_stop_chars};
use crate::synonyms::canonicalize_tokens;
use crate::temporal::{extract_temporal, query_asks_current, temporal_boost};
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
    /// MMR 多样性重排强度（λ）；`None` 关闭。
    pub mmr_lambda: Option<f64>,
    /// 查询意图分类偏置（时间/偏好等类别调整四维权重）。
    pub use_intent: bool,
    /// 同义词归一化（查询与内容 token 映射到 canonical）。
    pub use_synonyms: bool,
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
            mmr_lambda: Some(0.7), // 对齐 mnemopi recallEnhanced 默认
            use_intent: true,
            use_synonyms: true,
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

/// 会话末维护报告（[`StructuredMemoryStore::sleep`]）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SleepReport {
    /// 处理的 bank 数。
    pub banks: usize,
    /// 清理的过期记录数（`valid_until` 已到）。
    pub expired_removed: usize,
    /// 去重合并的记录数（内容归一化相同，保留高重要性/新时间戳/长正文）。
    pub duplicates_removed: usize,
}

/// 内容归一化去重键：折叠空白 + 小写。
fn normalize_dedupe_key(content: &str) -> String {
    content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
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
        // P1：同义词归一化（查询侧）——`db` 查询命中 `database` 文档
        let query_tokens = if opts.use_synonyms {
            canonicalize_tokens(tokenize(query))
        } else {
            tokenize(query)
        };
        // P1：意图偏置——偏好类查询更重视 importance，流程类更重视词法命中
        let (fts_w, imp_w, temp_w, vec_w) = if opts.use_intent && !query_tokens.is_empty() {
            let intent = classify_intent(query);
            adjust_weights(
                opts.fts_weight,
                opts.importance_weight,
                opts.temporal_weight,
                opts.vec_weight,
                &intent,
            )
        } else {
            (opts.fts_weight, opts.importance_weight, opts.temporal_weight, opts.vec_weight)
        };
        // P1：时间表达解析——`yesterday`/`3 days ago`/`三天前`/`上周一` 等将时间信号
        // 切换为相对目标日期。P1b：queryAsksCurrent——`now/latest/recent/current` 类
        // 查询把时间锚点固定为当前时刻并抬升 temporal 权重到至少 0.45（直译 omp
        // `queryTime ??= now` + `temporalWeight ??= 0.45`；Gyre 默认时间锚即当前，
        // 可观测效果=权重抬升，显式配置 >0.45 的权重保持不变）。
        let now = now_ms();
        let temporal = if query_tokens.is_empty() {
            None
        } else {
            Some(extract_temporal(query, None))
        };
        let (query_time_ms, temp_w) = match temporal {
            Some(t) if query_asks_current(query) => (t.event_date_ms.or(Some(now)), temp_w.max(0.45)),
            Some(t) => (t.event_date_ms, temp_w),
            None => (None, temp_w),
        };
        let now_hours = now as f64 / 3_600_000.0;
        // 向量（dense）融合：仅挂载嵌入器、vec_weight > 0 且查询非空时启用；
        // 嵌入失败/向量缺失一律静默降级，严格回退词法公式（行为兼容）。
        let dense: Option<(Vec<f32>, HashMap<String, VecEntry>)> =
            if vec_w > 0.0 && !query_tokens.is_empty() {
                self.prepare_dense(bank, query)
            } else {
                None
            };
        let mut hits: Vec<RecallHit> = self
            .read_bank(bank)
            .into_iter()
            .filter(|r| r.valid_until.is_none_or(|v| v >= now))
            .map(|record| {
                let content_tokens = if opts.use_synonyms {
                    canonicalize_tokens(tokenize(&record.content))
                } else {
                    tokenize(&record.content)
                };
                let relevance = bm25_relevance(&query_tokens, &content_tokens);
                let recency = match query_time_ms {
                    // P1：查询含时间表达 → 指数衰减相对目标时刻（过去衰减、当天/未来满值）
                    Some(qt) => temporal_boost(record.ts, qt, opts.halflife_hours),
                    None => 2f64
                        .powf(-((now_hours - record.ts as f64 / 3_600_000.0) / opts.halflife_hours)),
                };
                let importance = f64::from(record.importance.min(5)) / 5.0;
                let mut score = fts_w * relevance
                    + imp_w * importance
                    + temp_w * recency.clamp(0.0, 1.0);
                // dense 融合：`vec_weight · max(0, cosine)`；缺失/损坏向量只跳过，不报错
                if let Some((qv, by_id)) = &dense {
                    if let Some(entry) = by_id.get(&record.id) {
                        if let Some(v) = decode_f32s(&entry.data) {
                            if v.len() == qv.len() {
                                score += vec_w * cosine_similarity(qv, &v).max(0.0);
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
        // P1b：diversifyByCoverage（直译 omp：MMR 前的查询词覆盖贪心）——查询 ≥4 词且
        // 候选多于 limit 时，逐轮选 `score + 0.06×新增查询词覆盖数` 最大的候选，选中后
        // 其命中的查询词标记已覆盖。与 MMR 互补：MMR 压重复，覆盖贪心保查询词全盖。
        if query_tokens.len() >= 4 && hits.len() > opts.limit {
            let query_set: HashSet<&str> = query_tokens.iter().map(String::as_str).collect();
            let mut covered: HashSet<String> = HashSet::new();
            let mut selected: Vec<(RecallHit, Vec<String>)> = Vec::with_capacity(opts.limit);
            let mut pool: Vec<(RecallHit, Vec<String>)> = std::mem::take(&mut hits)
                .into_iter()
                .map(|h| {
                    let tokens = if opts.use_synonyms {
                        canonicalize_tokens(tokenize(&h.record.content))
                    } else {
                        tokenize(&h.record.content)
                    };
                    (h, tokens)
                })
                .collect();
            while !pool.is_empty() && selected.len() < opts.limit {
                let mut best_idx = 0usize;
                let mut best_score = f64::NEG_INFINITY;
                for (i, (row, tokens)) in pool.iter().enumerate() {
                    let additions = tokens
                        .iter()
                        .filter(|t| query_set.contains(t.as_str()) && !covered.contains(*t))
                        .count();
                    // 新增覆盖词数 ≤ 查询词数（个位数级），f64 精度损失无影响
                    #[allow(clippy::cast_precision_loss)]
                    let score = (additions as f64).mul_add(0.06, row.score);
                    if score > best_score {
                        best_score = score;
                        best_idx = i;
                    }
                }
                let (picked, picked_tokens) = pool.remove(best_idx);
                for t in &picked_tokens {
                    if query_set.contains(t.as_str()) {
                        covered.insert(t.clone());
                    }
                }
                selected.push((picked, picked_tokens));
            }
            hits = selected.into_iter().map(|(h, _)| h).collect();
        }
        // P1：MMR 多样性重排（λ·relevance − (1−λ)·maxSim）；首元素恒为相关性第一，
        // 空查询（浏览模式）无相关性锚点，跳过。中文调优（2026-08）：
        // ① 近重复折叠：与更高分记录 content 近同（containment ≥ 0.9）的候选压到尾部，
        //    让独立话题上位——中文单字级 Jaccard 对「追加型重复」低估（0.85 级），
        //    MMR 惩罚压不过词法分数差，折叠直接攻击重复概念（对齐 mnemopi 语义级
        //    向量相似度对同源记录 ≈1 的行为）；
        // ② 保序分数缩放（÷max）：Gyre 词法分数无界（omp 有界），缩放让 (1−λ)·sim
        //    惩罚与 λ·rel 同尺度，否则重复压制在分数差大时数学上不可能；
        // ③ 相似度输入过滤中文停用字：去掉“的/了”等共享虚词的假交集。
        if !query_tokens.is_empty() {
            if let Some(lambda) = opts.mmr_lambda {
                if hits.len() > 1 {
                    let mut kept: Vec<RecallHit> = Vec::new();
                    let mut dup: Vec<RecallHit> = Vec::new();
                    for h in &hits {
                        let t = strip_cjk_stop_chars(&tokenize(&h.record.content));
                        let is_dup = kept.iter().any(|k| {
                            let kt = strip_cjk_stop_chars(&tokenize(&k.record.content));
                            containment_similarity(&kt, &t) >= 0.9
                        });
                        if is_dup {
                            dup.push(h.clone());
                        } else {
                            kept.push(h.clone());
                        }
                    }
                    // 先落位：kept（无近重复）在前、dup 按原分数序在后；
                    // MMR 仅在前 folded 项上重排（dup 不参与，避免被相关性再提回）
                    let dup_len = dup.len();
                    hits = kept;
                    hits.append(&mut dup);
                    let folded = hits.len() - dup_len;
                    let max_score = hits.iter().map(|h| h.score).fold(0.0f64, f64::max);
                    let scores: Vec<f64> = if max_score > 0.0 {
                        hits.iter().map(|h| h.score / max_score).collect()
                    } else {
                        hits.iter().map(|h| h.score).collect()
                    };
                    let contents: Vec<Vec<String>> = hits
                        .iter()
                        .map(|h| strip_cjk_stop_chars(&tokenize(&h.record.content)))
                        .collect();
                    let indices = mmr_rerank_indices(
                        folded,
                        &scores[..folded],
                        |a, b| jaccard_similarity(&contents[a], &contents[b]),
                        lambda,
                        opts.limit,
                    );
                    let mut out: Vec<RecallHit> =
                        indices.into_iter().map(|i| hits[i].clone()).collect();
                    out.extend(hits[folded..].iter().cloned());
                    out.truncate(opts.limit.max(1));
                    return out;
                }
            }
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

    /// 会话末维护（对齐 mnemopi `sleep` 的轻量版，无 LLM 合并）：
    /// 逐 bank 清理过期记录、按内容归一化去重（保留高重要性 / 较新时间戳 / 较长正文），
    /// 并在嵌入模型变更时全量重嵌（复用 recall 惰性重嵌的判定）。
    ///
    /// # Errors
    /// 重写任一 bank 的 records.jsonl 失败时返回 IO 错误。
    pub fn sleep(&self) -> std::io::Result<SleepReport> {
        let mut report = SleepReport::default();
        for bank in self.list_banks() {
            let path = self.records_path(&bank);
            let records = read_records(&path);
            if records.is_empty() {
                continue;
            }
            report.banks += 1;
            let now = now_ms();
            let original = records.len();
            let mut kept: Vec<MemoryRecord> = Vec::new();
            let mut seen: HashMap<String, usize> = HashMap::new();
            for r in records {
                if r.valid_until.is_some_and(|v| v < now) {
                    report.expired_removed += 1;
                    continue;
                }
                let norm = normalize_dedupe_key(&r.content);
                if let Some(&idx) = seen.get(&norm) {
                    let existing = &mut kept[idx];
                    if r.importance > existing.importance {
                        existing.importance = r.importance;
                    }
                    if r.ts > existing.ts {
                        existing.ts = r.ts;
                    }
                    if r.content.len() > existing.content.len() {
                        existing.content = r.content.clone();
                    }
                    report.duplicates_removed += 1;
                } else {
                    seen.insert(norm, kept.len());
                    kept.push(r);
                }
            }
            if kept.len() != original {
                rewrite_records(&path, &kept)?;
            }
            // 嵌入模型变更 → 全量重嵌（按清理后的存活集重嵌）。
            if let Some(embedder) = &self.embedder {
                let vs = VectorStore::new(&self.bank_dir(&bank));
                let entries = vs.all();
                let need = entries.is_empty()
                    || entries.iter().any(|e| e.model != embedder.model_name());
                if need {
                    self.reembed(&bank, embedder.as_ref());
                }
            }
        }
        Ok(report)
    }

    /// 把命中/记录渲染为 Markdown 列表（供 system prompt 注入）。
    /// 对齐 oh-my-pi 注入格式：`- [score] content`（分数保留两位，语义锚点）。
    #[must_use]
    pub fn render_summary(hits: &[RecallHit]) -> String {
        if hits.is_empty() {
            return String::new();
        }
        let mut out = String::from("# 长期记忆（相关条目）\n\n");
        for h in hits {
            out.push_str(&format!(
                "- [{:.2}] {}\n",
                h.score,
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
        Ok(if hits.is_empty() {
            None
        } else {
            Some(Self::render_summary(&hits))
        })
    }

    async fn mental_models(&self) -> Option<String> {
        self.mental_models()
    }

    async fn recall(&self, query: &str, limit: usize) -> Vec<agent_core::MemoryHit> {
        let opts = RecallOptions {
            limit: limit.max(1),
            ..RecallOptions::default()
        };
        self.recall_in(DEFAULT_BANK, query, &opts)
            .into_iter()
            .map(|h| agent_core::MemoryHit {
                id: h.record.id,
                content: h.record.content,
                score: h.score,
                source: h.record.source,
                importance: h.record.importance,
            })
            .collect()
    }

    async fn retain(
        &self,
        content: &str,
        importance: u8,
        source: &str,
    ) -> Result<(), std::io::Error> {
        let record = MemoryRecord {
            id: String::new(),
            content: content.to_string(),
            source: source.to_string(),
            importance: importance.min(5),
            scope: String::new(),
            metadata: BTreeMap::new(),
            tags: Vec::new(),
            ts: now_ms(),
            valid_until: None,
        };
        self.retain(record)
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

    async fn add_mental_model(&self, text: &str) -> Result<(), std::io::Error> {
        use std::io::Write;
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let line = format!(
            "- [{}] {}\n",
            crate::mental_models::format_ts(now_ms() / 1000),
            text
        );
        let path = self.root.join(MENTAL_MODELS_FILE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        f.write_all(line.as_bytes())
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

/// 小写化 + 非字母数字切分；CJK 表意字符逐字切分（无空格中文不粘连成整串），
/// 拉丁/数字保持连续词。P1 起启用：同义词/MMR/Jaccard 对中文内容才有意义。
fn tokenize(s: &str) -> Vec<String> {
    fn is_cjk(c: char) -> bool {
        matches!(c as u32,
            0x3400..=0x4DBF   // CJK 扩展 A
            | 0x4E00..=0x9FFF // CJK 统一表意文字
            | 0xF900..=0xFAFF // CJK 兼容表意文字
            | 0x3040..=0x30FF // 平假名/片假名
            | 0xAC00..=0xD7AF // 谚文音节
        )
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() {
            if is_cjk(c) {
                if !current.is_empty() {
                    out.push(current.to_ascii_lowercase());
                    current.clear();
                }
                out.push(c.to_string());
            } else {
                current.push(c);
            }
        } else if !current.is_empty() {
            out.push(current.to_ascii_lowercase());
            current.clear();
        }
    }
    if !current.is_empty() {
        out.push(current.to_ascii_lowercase());
    }
    out
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
        // 纳秒时钟实际精度为微秒级：并行测试可能同微秒取到相同 nano → 同 root 互相踩踏。
        // 加原子序号保证目录唯一。
        static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "agent-mnemopi-{}-{seq}-{:#x}",
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

    // P0-4：会话末维护——清理过期 + 内容去重（保留高重要性/新时间戳）。
    #[tokio::test]
    async fn sleep_cleans_expired_and_deduplicates() {
        let s = store();
        let mut expired = rec("过期条目", 3);
        expired.valid_until = Some(now_ms() - 1); // 已过期
        s.retain(expired).unwrap();
        s.retain(rec("用户偏好 Rust 2024 edition", 1)).unwrap();
        let mut dup_low = rec("用户偏好 Rust 2024 edition", 1);
        dup_low.ts = now_ms() + 10; // 更新但低重要性
        s.retain(dup_low).unwrap();
        let mut dup_high = rec("用户偏好 Rust 2024 edition", 4);
        dup_high.ts = now_ms() + 20;
        s.retain(dup_high).unwrap();
        s.retain(rec("另一个独立条目", 2)).unwrap();

        let report = s.sleep().unwrap();
        assert_eq!(report.expired_removed, 1);
        assert_eq!(report.duplicates_removed, 2);
        assert_eq!(report.banks, 1);

        let rest = s.read_bank("default");
        assert_eq!(rest.len(), 2, "应为 去重后 1 条 + 独立 1 条");
        let kept = rest.iter().find(|r| r.content.contains("Rust 2024")).unwrap();
        assert_eq!(kept.importance, 4, "保留最高重要性");
        // 幂等：再次 sleep 无操作。
        let again = s.sleep().unwrap();
        assert_eq!(again.expired_removed + again.duplicates_removed, 0);
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

    // ── P1：同义词 / 意图 / 时间 / MMR 集成 ─────────────────────────────────

    fn rec_at(content: &str, importance: u8, age_hours: u64) -> MemoryRecord {
        let mut r = rec(content, importance);
        r.ts = now_ms() - age_hours * 3_600_000;
        r
    }

    #[test]
    fn recall_synonym_expansion_hits_canonical() {
        let s = store();
        s.retain(rec("database 连接超时排查记录", 2)).unwrap();
        // 查询用同义词 `db`：canonical 归一化后 `db`→`database` 词面命中
        let opts = RecallOptions::default();
        let hits = s.recall("db 连接超时", &opts);
        assert!(
            hits.iter().any(|h| h.record.content.contains("database")),
            "同义词扩展应命中 database 文档"
        );
        // 对照组：关闭同义词时 `db` 不映射，词法命中少一词 → 分数更低
        let opts_off = RecallOptions { use_synonyms: false, ..opts };
        let on_score = hits.first().map_or(0.0, |h| h.score);
        let off_score = s.recall("db 连接超时", &opts_off).first().map_or(0.0, |h| h.score);
        assert!(on_score > off_score, "同义词扩展应提升词法命中分数");
    }

    #[test]
    fn recall_preference_intent_boosts_importance() {
        let s = store();
        // 内容不含查询词（纯 importance 信号）+ 一条含词但低重要性
        s.retain(rec_at("用户签名偏好确认", 5, 24)).unwrap();
        s.retain(rec_at("用户偏好", 1, 24)).unwrap();
        let opts = RecallOptions { use_intent: true, ..RecallOptions::default() };
        let hits = s.recall("which approach do you recommend?", &opts);
        // 无词面重叠时 relevance=0；importance bias 1.5 让高重要性记录胜出
        assert!(!hits.is_empty());
        assert!(hits[0].record.content.contains("签名偏好确认"));
    }

    #[test]
    fn recall_temporal_query_boosts_matching_age() {
        let s = store();
        // 3 天前的记录 vs 30 天前的记录（内容均与查询无词面重叠）
        s.retain(rec_at("部署排障记录 alpha", 1, 72)).unwrap();
        s.retain(rec_at("部署排障记录 beta", 1, 720)).unwrap();
        let opts = RecallOptions { temporal_weight: 0.5, ..RecallOptions::default() };
        let hits = s.recall("yesterday 之前发生了什么", &opts);
        // 查询解析出 yesterday（03-04 类目标日）→ 3 天前记录 boost 高，排前
        assert_eq!(hits[0].record.content, "部署排障记录 alpha");
        assert_eq!(hits[1].record.content, "部署排障记录 beta");
    }

    #[test]
    fn recall_mmr_diversifies_duplicate_content() {
        // 注：词级（英文）验证与 mnemopi 测试同构；中文近重复走折叠路径
        // （见 recall_mmr_folds_chinese_near_duplicates）。
        let s = store();
        s.retain(rec("fix the login auth bug", 2)).unwrap();
        s.retain(rec("fix the login auth bug please", 2)).unwrap();
        s.retain(rec("login payment provider", 2)).unwrap();
        let opts = RecallOptions::default();
        let hits = s.recall("login auth", &opts);
        assert_eq!(hits.len(), 3);
        // 相关性第一为原始第一名（MMR 首元素 = 最高分候选）
        assert_eq!(hits[0].record.content, "fix the login auth bug");
        // 对照组：关闭 MMR 时两条 login/auth 兄弟记录相邻（分数序）
        let mmr_off = RecallOptions { mmr_lambda: None, ..opts };
        let plain = s.recall("login auth", &mmr_off);
        assert!(plain[0].record.content.contains("auth"));
        assert!(plain[1].record.content.contains("auth"), "对照组前提：重复记录相邻");
        // MMR 开：重复兄弟记录被多样性压后，独立话题（login payment provider）上位
        assert_eq!(hits[1].record.content, "login payment provider");
        assert_eq!(hits[2].record.content, "fix the login auth bug please");
    }

    #[test]
    fn recall_mmr_folds_chinese_near_duplicates() {
        // 中文近重复：单字级 Jaccard 对「追加型重复」只有 0.85 级，MMR 惩罚（λ=0.7）
        // 压不过词法分数差；折叠路径（containment ≥ 0.9）把重复记录压到尾部，
        // 独立话题（importance 5）上位第二。
        let s = store();
        s.retain(rec_at("修复了登录页的认证问题", 2, 1)).unwrap();
        s.retain(rec_at("修复了登录页的认证问题请尽快处理", 2, 1)).unwrap();
        s.retain(rec_at("部署了新的支付网关", 5, 1)).unwrap();
        let opts = RecallOptions::default();
        let hits = s.recall("登录认证", &opts);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].record.content, "修复了登录页的认证问题");
        assert_eq!(hits[1].record.content, "部署了新的支付网关", "重复记录应被压后");
        assert_eq!(hits[2].record.content, "修复了登录页的认证问题请尽快处理");
        // 对照组（关闭 MMR）：不折叠，重复兄弟按分数相邻。
        let mmr_off = RecallOptions { mmr_lambda: None, ..opts };
        let plain = s.recall("登录认证", &mmr_off);
        assert!(plain[0].record.content.contains("登录页"));
        assert!(plain[1].record.content.contains("登录页"), "对照组前提：重复记录相邻");
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

    // 接线：心智模型走 trait `mental_models()`（engine 单独注入，排在 `<memories>` 之前）；
    // summary() 只含记忆摘要（对齐 oh-my-pi：稳定语义锚点在前、易变召回在后）。
    #[tokio::test]
    async fn summary_merges_mental_models_when_configured() {
        let (s, root) = store_root();
        // 未配置：无 mental 段，纯记录摘要。
        assert!(!s.mental_models().is_some());
        s.retain(rec("rust workspace notes", 2)).unwrap();
        let plain = s.summary().await.unwrap().unwrap();
        assert!(plain.contains("rust workspace notes"));
        assert!(!plain.contains("<mental_models>"));
        // 配置后：项目积累条目经 trait 注入（内置 seeds 非空，必然出现）。
        std::fs::write(
            root.join(MENTAL_MODELS_FILE),
            "- [2026-08-02 12:00:00] 项目约定：先读 AGENTS.md 再动手\n",
        )
        .unwrap();
        let s2 = s.with_mental_models_config(crate::MentalModelsConfig::default());
        let mental = s2.mental_models().unwrap();
        assert!(mental.contains("项目约定"), "缺少项目积累: {mental}");
        // summary() 不再吞 mental 段（顺序交由 engine 组装）。
        let merged = s2.summary().await.unwrap().unwrap();
        assert!(!merged.contains("<mental_models>"), "summary 不应含 mental 段: {merged}");
        assert!(merged.contains("rust workspace notes"), "记录摘要被覆盖");
        // 空记忆 + 配置心智模型：summary 为 None，mental 仍可注入。
        let (s3, _) = store_root();
        let s3 = s3.with_mental_models_config(crate::MentalModelsConfig::default());
        assert!(s3.summary().await.unwrap().is_none());
        assert!(s3.mental_models().is_some());
    }

    // P1b：queryAsksCurrent——「latest/current」类查询把 temporal 权重抬到 0.45。
    // 构造：两记录内容/词法命中相同，旧记录重要性更高（imp 4 vs 3）；
    // temporal 权重 0.0 时旧记录领先（recency 无效），current 查询抬权后新记录反超。
    #[tokio::test]
    async fn recall_current_query_boosts_recency_weight() {
        let s = store();
        s.retain(rec_at("system latest status record", 4, 600)).unwrap(); // 旧，r≈0.29
        s.retain(rec_at("system latest status record", 3, 1)).unwrap(); // 新，r≈1
        let opts = RecallOptions {
            mmr_lambda: None,
            use_intent: false,
            temporal_weight: 0.0,
            ..Default::default()
        };
        // 无 current 词：recency 无权重 → 重要性高的旧记录在前
        let plain = s.recall("status", &opts);
        assert_eq!(plain[0].record.importance, 4, "对照前提：旧记录领先");
        // current 词：temporal 权重抬到 0.45 → 新记录（recency≈1）反超
        let cur = s.recall("latest status", &opts);
        assert_eq!(cur[0].record.importance, 3, "current 查询应抬 temporal 权重");
        assert_eq!(cur[0].record.content, "system latest status record");
    }

    // P1b：diversifyByCoverage——查询 ≥4 词且候选多于 limit 时，覆盖贪心按
    // `score + 0.06×新增覆盖词数` 择优。构造：C 纯分数第三（0.902），但覆盖
    // gamma/delta/epsilon 三个新词（+0.18）→ 第二轮反超 B（beta 已被 A 覆盖）。
    #[tokio::test]
    async fn recall_diversifies_query_coverage() {
        let s = store();
        s.retain(rec_at("alpha record", 5, 1)).unwrap(); // 1.507，覆盖 alpha
        s.retain(rec_at("beta record", 3, 1)).unwrap(); // 1.307，覆盖 beta
        s.retain(
            rec_at("gamma delta epsilon r1 r2 r3 r4 r5 r6 r7 r8 r9 r10 record", 2, 1),
        )
        .unwrap(); // 1.275，覆盖 gamma/delta/epsilon
        s.retain(rec_at("delta r1 r2 r3 r4 r5 r6 r7 r8 r9 r10 r11 record", 0, 1))
            .unwrap(); // 0.577，覆盖 delta（已被 C 覆盖）
        let opts = RecallOptions {
            mmr_lambda: None,
            use_intent: false,
            limit: 3,
            ..Default::default()
        };
        // 覆盖贪心：A 最高分先选；第二轮 C（1.275+0.18）胜过 B（1.307+0.06）
        let div = s.recall("alpha beta gamma delta epsilon", &opts);
        assert!(div[0].record.content.starts_with("alpha"), "{}: {}", div[0].record.content, div[0].score);
        assert!(
            div[1].record.content.starts_with("gamma"),
            "覆盖贪心应把多词覆盖的 C 提前: {:?}",
            div.iter().map(|h| (&h.record.content, h.score)).collect::<Vec<_>>()
        );
        assert!(div[2].record.content.starts_with("beta"));
        // 对照：limit=4（候选 4 ≤ limit → 不触发覆盖贪心）→ 纯分数序 [A, B, C, D]
        let no_div = RecallOptions { limit: 4, ..opts };
        let plain = s.recall("alpha beta gamma delta epsilon", &no_div);
        assert!(plain[1].record.content.starts_with("beta"), "对照前提：纯分数序");
    }

    // P1b：中文时间表达——「三天前」把时间信号切到相对目标日（72h）。
    // 构造：alpha 300h/imp5（对 now 很近→默认查询靠重要性领先），beta 88h/imp1
    // （距目标日仅 16h）。无时间词时重要性主导 → alpha 前；「三天前」查询下
    // recency 相对目标日衰减，beta（0.953）反超 alpha（0.508）→ 顺序翻转，
    // 证明信号切换生效（temporal_boost 对目标日之后的记录满值 1.0，
    // 故用目标日过去侧的记录构造对比）。
    #[tokio::test]
    async fn recall_chinese_temporal_query_switches_signal() {
        let s = store();
        s.retain(rec_at("部署排障记录 alpha", 5, 300)).unwrap();
        s.retain(rec_at("部署排障记录 beta", 1, 88)).unwrap();
        let plain = RecallOptions {
            use_intent: false,
            ..Default::default()
        };
        let hits = s.recall("部署排障", &plain);
        assert_eq!(hits[0].record.content, "部署排障记录 alpha", "对照前提：无时间词时重要性主导");
        let near = RecallOptions {
            use_intent: false,
            temporal_weight: 2.0,
            ..Default::default()
        };
        let hits = s.recall("三天前部署排障", &near);
        assert_eq!(hits[0].record.content, "部署排障记录 beta", "beta 距目标日（72h）更近");
    }
}
