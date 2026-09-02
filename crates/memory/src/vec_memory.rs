//! 向量记忆：旁路向量文件（vecs.jsonl）+ 双层嵌入（L1 确定性投影 / L2 fastembed 语义）。
//!
//! 设计来源：`docs/vector-memory-eval.md`（Phase 1/5）。L1 零依赖保底：
//! [`StubEmbedder`]（固定种子伪随机，离线测试/基准用）与 [`ProjectionEmbedder`]
//! （char n-gram + 固定种子随机投影，离线近似语义）；L2 语义嵌入由 `vec-embed`
//! feature 门控的 [`FastembedEmbedder`]（`all-MiniLM-L6-v2` Q，384 维）承担，
//! 初始化/推理失败自动降级 L1，永不 panic。
//!
//! 向量与 [`crate::structured::StructuredMemoryStore`] 的 `records.jsonl` 并存，
//! 以旁路文件 `vecs.jsonl` 保存 `{id, model, dim, data(base64 f32 LE)}`；records
//! 是唯一事实源，向量只是加速件——缺失/损坏向量只跳过 dense 信号，不报错。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
#[cfg(feature = "vec-embed")]
use std::sync::Mutex;

use base64::Engine;
use serde::{Deserialize, Serialize};

/// 旁路向量文件名（与 `records.jsonl` 同目录）。
pub const VECS_FILE: &str = "vecs.jsonl";

/// 嵌入器：把文本列表映射为固定维度的稠密向量。
pub trait Embedder: Send + Sync {
    /// 批量嵌入；返回与输入等长的向量列表，失败时返回错误描述。
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;

    /// 向量维度。
    fn dim(&self) -> usize;

    /// 模型标识（写入 vecs.jsonl；与当前模型不符时触发清空重嵌）。
    fn model_name(&self) -> String {
        "unknown".into()
    }
}

/// 一条旁路向量（vecs.jsonl 一行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VecEntry {
    /// 对应记录的 id。
    pub id: String,
    /// 生成该向量的模型标识（模型变更时清空重嵌）。
    pub model: String,
    /// 向量维度。
    pub dim: usize,
    /// base64 编码的 f32 小端字节。
    pub data: String,
}

/// 旁路向量存储：vecs.jsonl 的 upsert / get_many / delete / all。
///
/// 追加写新条目、全量重写替换/删除（对齐 `structured.rs` 的 `rewrite_records` 模式）；
/// 任何读失败都返回空集，不向调用方报错——向量只是加速件。
#[derive(Debug, Clone)]
pub struct VectorStore {
    path: PathBuf,
}

impl VectorStore {
    /// 构造：指向 bank 目录下的 vecs.jsonl。
    #[must_use]
    pub fn new(bank_dir: &Path) -> Self {
        Self {
            path: bank_dir.join(VECS_FILE),
        }
    }

    /// 读取全部条目（文件缺失/损坏行自动跳过）。
    #[must_use]
    pub fn all(&self) -> Vec<VecEntry> {
        read_vecs(&self.path)
    }

    /// 按 id 批量读取（缺失 id 静默跳过）。
    #[must_use]
    pub fn get_many(&self, ids: &[String]) -> HashMap<String, VecEntry> {
        let want: HashSet<&str> = ids.iter().map(String::as_str).collect();
        self.all()
            .into_iter()
            .filter(|e| want.contains(e.id.as_str()))
            .map(|e| (e.id.clone(), e))
            .collect()
    }

    /// 追加或替换单条：id 已存在则全量重写去重，否则追加一行。
    ///
    /// # Errors
    /// 写入失败时返回 IO 错误。
    pub fn upsert(&self, entry: VecEntry) -> std::io::Result<()> {
        let mut entries = self.all();
        match entries.iter().position(|e| e.id == entry.id) {
            Some(i) => {
                entries[i] = entry;
                rewrite_vecs(&self.path, &entries)
            }
            None => append_vec(&self.path, &entry),
        }
    }

    /// 按 id 删除；返回是否确有删除。重复删除/删除不存在 id 返回 `false`（幂等）。
    ///
    /// # Errors
    /// 重写失败时返回 IO 错误。
    pub fn delete(&self, id: &str) -> std::io::Result<bool> {
        let entries = self.all();
        let before = entries.len();
        let remaining: Vec<VecEntry> = entries.into_iter().filter(|e| e.id != id).collect();
        if remaining.len() == before {
            return Ok(false);
        }
        rewrite_vecs(&self.path, &remaining)?;
        Ok(true)
    }

    /// 清空全部向量（模型变更重嵌前调用）。
    ///
    /// # Errors
    /// 写入失败时返回 IO 错误。
    pub fn clear(&self) -> std::io::Result<()> {
        rewrite_vecs(&self.path, &[])
    }

    /// 全量替换全部条目（重嵌用，一次写入避免逐条重写）。
    pub(crate) fn replace_all(&self, entries: &[VecEntry]) -> std::io::Result<()> {
        rewrite_vecs(&self.path, entries)
    }
}

/// 把 f32 切片编码为 base64（小端字节序）。
#[must_use]
pub fn encode_f32s(v: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for x in v {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// 解码 base64 为 f32 向量；格式非法返回 `None`（缺失/损坏向量只跳过 dense 信号）。
#[must_use]
pub fn decode_f32s(b64: &str) -> Option<Vec<f32>> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    if bytes.len() % 4 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

/// 余弦相似度；任一为零向量返回 0.0。
#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b) {
        let x = f64::from(*x);
        let y = f64::from(*y);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom <= f64::EPSILON {
        0.0
    } else {
        dot / denom
    }
}

/// 固定种子确定性伪随机嵌入（离线测试/基准用，非语义）。
///
/// 同一文本永远得到同一向量；不同文本的向量相互独立（伪随机），
/// 维度可配，仅用于验证管线正确性，不用于语义排序。
pub struct StubEmbedder {
    dim: usize,
    seed: u64,
}

impl StubEmbedder {
    /// 构造；`dim` 至少为 1。
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self {
            dim: dim.max(1),
            seed: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

impl Embedder for StubEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts
            .iter()
            .map(|t| stub_vector(t, self.dim, self.seed))
            .collect())
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn model_name(&self) -> String {
        "stub".into()
    }
}

/// 文本哈希驱动 splitmix64 生成确定性伪随机向量（元素 ∈ [-1, 1)）。
fn stub_vector(text: &str, dim: usize, seed: u64) -> Vec<f32> {
    let mut state = fnv1a(text.as_bytes(), seed);
    (0..dim)
        .map(|_| {
            state = splitmix64(state);
            // u64 高 53 位 → [0, 1) → [-1, 1)
            (((state >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0) as f32
        })
        .collect()
}

/// splitmix64：确定性伪随机数生成器。
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// L1 离线保底：char n-gram 特征 + 固定种子随机投影。
///
/// 对每个字符 unigram/bigram 特征做确定性哈希投影到目标维度（随机符号），
/// 再 L2 归一化；对词序不敏感，共享 n-gram 越多余弦越高，适合离线近似语义。
pub struct ProjectionEmbedder {
    dim: usize,
    seed: u64,
}

impl ProjectionEmbedder {
    /// 构造；`dim` 至少为 8。
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self {
            dim: dim.max(8),
            seed: 0x243F_6A88_85A3_08D3,
        }
    }

    fn project(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        let chars: Vec<char> = text.chars().collect();
        for (i, &c) in chars.iter().enumerate() {
            project_ngram(&mut v, ngram_hash(self.seed, &[c]), self.dim);
            if let Some(&next) = chars.get(i + 1) {
                project_ngram(&mut v, ngram_hash(self.seed, &[c, next]), self.dim);
            }
        }
        // L2 归一化（零向量保持全零，余弦视其为 0）
        let norm = v
            .iter()
            .map(|x| f64::from(*x) * f64::from(*x))
            .sum::<f64>()
            .sqrt();
        if norm > 1e-9 {
            for x in &mut v {
                *x = (*x as f64 / norm) as f32;
            }
        }
        v
    }
}

impl Embedder for ProjectionEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts.iter().map(|t| self.project(t)).collect())
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn model_name(&self) -> String {
        "projection".into()
    }
}

/// n-gram 字符序列 → 确定性特征哈希。
fn ngram_hash(seed: u64, chars: &[char]) -> u64 {
    let mut bytes = Vec::with_capacity(chars.len() * 4);
    let mut buf = [0u8; 4];
    for c in chars {
        bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
    fnv1a(&bytes, seed)
}

/// 把单个特征以确定性随机符号投影到目标维度的一个坐标。
fn project_ngram(v: &mut [f32], feature: u64, dim: usize) {
    let idx = (feature % dim as u64) as usize;
    let sign = if (feature >> 33) & 1 == 0 {
        1.0f32
    } else {
        -1.0f32
    };
    v[idx] += sign;
}

/// FNV-1a 64 位散列（带种子）。
fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ seed;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// L2 语义嵌入（feature `vec-embed` 门控）：fastembed `all-MiniLM-L6-v2` Q（384 维）。
///
/// 懒加载：首次 `embed` 才初始化模型；初始化或推理失败自动降级
/// [`ProjectionEmbedder`]（L1 保底），永不 panic。
#[cfg(feature = "vec-embed")]
pub struct FastembedEmbedder {
    inner: Mutex<Option<fastembed::TextEmbedding>>,
    model: fastembed::EmbeddingModel,
    fallback: ProjectionEmbedder,
}

#[cfg(feature = "vec-embed")]
impl FastembedEmbedder {
    /// 构造（不立即加载模型，首次 embed 才初始化）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            model: fastembed::EmbeddingModel::AllMiniLML6V2Q,
            fallback: ProjectionEmbedder::new(384),
        }
    }
}

#[cfg(feature = "vec-embed")]
impl Default for FastembedEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "vec-embed")]
impl Embedder for FastembedEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = %e, "fastembed 锁中毒，降级 L1 投影嵌入");
                return self.fallback.embed(texts);
            }
        };
        // 懒加载：首次调用才 try_new；失败仅降级，不 panic
        if guard.is_none() {
            match fastembed::TextEmbedding::try_new(fastembed::TextInitOptions::new(
                self.model.clone(),
            )) {
                Ok(model) => *guard = Some(model),
                Err(e) => {
                    tracing::warn!(error = %e, "fastembed 初始化失败，降级 L1 投影嵌入");
                }
            }
        }
        match guard.as_mut() {
            Some(model) => match model.embed(texts, None) {
                Ok(v) => Ok(v),
                Err(e) => {
                    tracing::warn!(error = %e, "fastembed 推理失败，降级 L1 投影嵌入");
                    self.fallback.embed(texts)
                }
            },
            None => self.fallback.embed(texts),
        }
    }

    fn dim(&self) -> usize {
        384 // all-MiniLM-L6-v2
    }

    fn model_name(&self) -> String {
        "all-MiniLM-L6-v2".into()
    }
}

/// 读取 vecs.jsonl（损坏行跳过）。
fn read_vecs(path: &Path) -> Vec<VecEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<VecEntry>(line) {
            out.push(entry);
        }
    }
    out
}

/// 全量重写 vecs.jsonl（对齐 `rewrite_records` 模式）。
fn rewrite_vecs(path: &Path, entries: &[VecEntry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    for e in entries {
        if let Ok(line) = serde_json::to_string(e) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    std::fs::write(path, text)
}

/// 追加一行（创建父目录）。
fn append_vec(path: &Path, entry: &VecEntry) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agent-vecmem-{}-{:#x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn entry(id: &str, v: &[f32]) -> VecEntry {
        VecEntry {
            id: id.into(),
            model: "stub".into(),
            dim: v.len(),
            data: encode_f32s(v),
        }
    }

    #[test]
    fn stub_embedder_deterministic_and_dims() {
        let e = StubEmbedder::new(16);
        let a = e.embed(&["hello".into()]).unwrap();
        let b = e.embed(&["hello".into()]).unwrap();
        assert_eq!(a, b); // 确定性
        assert_eq!(a[0].len(), 16);
        let c = e.embed(&["world".into()]).unwrap();
        assert_ne!(a[0], c[0]); // 不同文本不同向量
    }

    #[test]
    fn projection_ranks_paraphrase_above_unrelated() {
        let e = ProjectionEmbedder::new(128);
        let q = e
            .embed(&["cargo workspace build".into()])
            .unwrap()
            .remove(0);
        let para = e
            .embed(&["build the cargo workspace".into()])
            .unwrap()
            .remove(0);
        let other = e
            .embed(&["lunch pasta recipe dinner".into()])
            .unwrap()
            .remove(0);
        let sim_para = cosine_similarity(&q, &para);
        let sim_other = cosine_similarity(&q, &other);
        assert!(
            sim_para > sim_other,
            "近义（同 n-gram 重排）应显著高于无关文本：{sim_para} vs {sim_other}"
        );
    }

    #[test]
    fn projection_embeds_are_normalized_and_dimensioned() {
        let e = ProjectionEmbedder::new(32);
        let v = e.embed(&["some text".into()]).unwrap().remove(0);
        assert_eq!(v.len(), 32);
        let norm: f64 = v.iter().map(|x| f64::from(*x) * f64::from(*x)).sum();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn vector_store_upsert_delete_idempotent() {
        let vs = VectorStore::new(&tmp_dir());
        vs.upsert(entry("a", &[1.0, 0.0])).unwrap();
        vs.upsert(entry("a", &[0.0, 1.0])).unwrap(); // 同 id 覆盖而非追加
        vs.upsert(entry("b", &[1.0, 1.0])).unwrap();
        let all = vs.all();
        assert_eq!(all.len(), 2);
        let a_entry = all.iter().find(|e| e.id == "a").unwrap();
        assert_eq!(decode_f32s(&a_entry.data).unwrap(), vec![0.0, 1.0]);
        // get_many：缺失 id 跳过
        let got = vs.get_many(&["a".into(), "missing".into()]);
        assert_eq!(got.len(), 1);
        assert!(got.contains_key("a"));
        // 删除幂等
        assert!(vs.delete("b").unwrap());
        assert!(!vs.delete("b").unwrap());
        assert!(!vs.delete("missing").unwrap());
        assert_eq!(vs.all().len(), 1);
    }

    #[test]
    fn f32_roundtrip_and_malformed() {
        let v = vec![1.5f32, -2.25, 0.0, f32::MIN_POSITIVE];
        assert_eq!(decode_f32s(&encode_f32s(&v)).unwrap(), v);
        assert!(decode_f32s("!!!not-base64").is_none());
        assert!(decode_f32s("AAAA").is_none()); // 3 字节，非 4 的倍数
    }

    /// 真模型集成测试：默认 `#[ignore]`；需 `GYRE_VEC_INTEGRATION=1` 且联网/预置模型缓存。
    #[cfg(feature = "vec-embed")]
    #[test]
    #[ignore = "需要联网下载 ONNX 模型；设置 GYRE_VEC_INTEGRATION=1 后显式运行"]
    fn fastembed_integration() {
        if std::env::var("GYRE_VEC_INTEGRATION").as_deref() != Ok("1") {
            return;
        }
        let e = FastembedEmbedder::new();
        let v = e
            .embed(&["hello world".to_string(), "another line".to_string()])
            .expect("fastembed 嵌入应成功");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].len(), e.dim());
        let again = e
            .embed(&["hello world".to_string()])
            .expect("重复嵌入应成功");
        assert_eq!(again[0], v[0]); // 模型确定性
    }
}
