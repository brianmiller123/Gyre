//! 会话级文件版本快照存储 + 内容指纹。
//!
//! 移植自 [`oh-my-pi hashline/snapshots.ts`](https://github.com/can1357/oh-my-pi/blob/master/packages/hashline/src/snapshots.ts)（精简：有界内存版）。
//!
//! 写工具在落盘前把「原始正文」记录为该路径的一个版本（按内容指纹去重）；
//! `read_file` 读取真实工作区文本时同样记录（经 [`crate::ToolContext::snapshots`]）。
//! 后续当某区段带 stale hash 到来时，recovery 可凭 hash 找回对应历史版本，
//! 重放编辑到当前正文——典型场景是「模型连读带改，第二次编辑仍引用第一次读到的 hash」。
//!
//! 类型实体置于本 crate（`agent-tools`）：[`ToolContext`](crate::ToolContext) 字段须在本
//! crate 内命名该类型，而 `agent-hashline` 依赖本 crate（反向会成环）；
//! `agent_hashline::InMemorySnapshotStore` / `agent_hashline::format::compute_file_hash`
//! 为同一实体的再导出，对外路径不变。

use std::collections::{HashMap, VecDeque};

/// 指纹 hex 长度。
pub const HASH_LEN: usize = 4;

/// 内容指纹：4 hex 大写。规范化先去除每行尾随空白与 `\r`。
#[must_use]
pub fn compute_file_hash(text: &str) -> String {
    let normalized: String = normalize_for_hash(text);
    let h = fnv1a_32(normalized.as_bytes());
    let low16 = h & 0xFFFF;
    format!("{low16:0HASH_LEN$X}")
}

fn normalize_for_hash(text: &str) -> String {
    text.split('\n')
        .map(|line| line.trim_end_matches([' ', '\t', '\r']))
        .collect::<Vec<_>>()
        .join("\n")
}

/// FNV-1a 32-bit（确定性、无依赖）。
fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811C_9DC5;
    for &b in bytes {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// 一个全文件版本。
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// 规范路径。
    pub path: String,
    /// 规范化（LF、无 BOM）的全文件正文。
    pub text: String,
    /// 内容指纹（与 [`compute_file_hash`] 一致）。
    pub hash: String,
}

/// 默认最多追踪的路径数（LRU 淘汰）。
const DEFAULT_MAX_PATHS: usize = 30;
/// 默认每路径保留的版本数（最旧的先丢）。
const DEFAULT_MAX_VERSIONS_PER_PATH: usize = 4;

/// 有界内存快照存储：`path → 版本历史`（末尾为最新），并按 LRU 控路径总数。
#[derive(Debug)]
pub struct InMemorySnapshotStore {
    max_paths: usize,
    max_versions_per_path: usize,
    map: HashMap<String, Vec<Snapshot>>,
    /// LRU 路径顺序：末尾为最近访问。
    order: VecDeque<String>,
}

impl Default for InMemorySnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemorySnapshotStore {
    /// 默认上限（30 路径 × 每路径 4 版本）。
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_PATHS, DEFAULT_MAX_VERSIONS_PER_PATH)
    }

    /// 自定上限。
    #[must_use]
    pub fn with_limits(max_paths: usize, max_versions_per_path: usize) -> Self {
        Self {
            max_paths: max_paths.max(1),
            max_versions_per_path: max_versions_per_path.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// 记录 `path` 的全文件正文，返回其内容指纹。
    ///
    /// 若与该路径最新版本指纹一致则不新增版本（去重）。
    pub fn record(&mut self, path: &str, full_text: &str) -> String {
        let hash = compute_file_hash(full_text);
        let entry = self.map.entry(path.to_string()).or_default();
        if entry
            .last()
            .is_some_and(|s| s.hash.eq_ignore_ascii_case(&hash))
        {
            // 内容未变：不新增版本
        } else {
            entry.push(Snapshot {
                path: path.to_string(),
                text: full_text.to_string(),
                hash: hash.clone(),
            });
            while entry.len() > self.max_versions_per_path {
                entry.remove(0);
            }
        }
        self.touch(path);
        hash
    }

    /// 该路径的最新版本。
    #[must_use]
    pub fn head(&self, path: &str) -> Option<&Snapshot> {
        self.map.get(path).and_then(|v| v.last())
    }

    /// 该路径下指纹等于 `hash` 的版本（取最近匹配）。
    #[must_use]
    pub fn by_hash(&self, path: &str, hash: &str) -> Option<&Snapshot> {
        self.map
            .get(path)?
            .iter()
            .rev()
            .find(|s| s.hash.eq_ignore_ascii_case(hash))
    }

    /// 是否记录过该路径下指纹等于 `hash` 的版本（recovery 据此判定 `hash_recognized`）。
    #[must_use]
    pub fn recognizes(&self, path: &str, hash: &str) -> bool {
        self.by_hash(path, hash).is_some()
    }

    /// 该 hash 是否为某路径的最新版本。
    #[must_use]
    pub fn is_head(&self, path: &str, hash: &str) -> bool {
        self.head(path)
            .is_some_and(|s| s.hash.eq_ignore_ascii_case(hash))
    }

    /// 丢弃单路径历史。
    pub fn invalidate(&mut self, path: &str) {
        self.map.remove(path);
        self.order.retain(|p| p != path);
    }

    /// 清空全部。
    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    /// 把 `path` 提升为最近访问，并按上限淘汰最久未访问路径。
    fn touch(&mut self, path: &str) {
        self.order.retain(|p| p != path);
        self.order.push_back(path.to_string());
        while self.order.len() > self.max_paths {
            if let Some(evicted) = self.order.pop_front() {
                self.map.remove(&evicted);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_trim_insensitive() {
        let a = compute_file_hash("line1\nline2\n");
        let b = compute_file_hash("line1 \nline2\t\n");
        assert_eq!(a, b);
        assert_eq!(a.len(), HASH_LEN);
    }

    #[test]
    fn record_dedups_identical_content() {
        let mut store = InMemorySnapshotStore::new();
        let h1 = store.record("a.rs", "fn a() {}\n");
        let h2 = store.record("a.rs", "fn a() {}\n");
        assert_eq!(h1, h2);
        assert_eq!(store.map["a.rs"].len(), 1);
    }

    #[test]
    fn by_hash_finds_older_version_after_change() {
        let mut store = InMemorySnapshotStore::new();
        let h_old = store.record("a.rs", "fn a() {}\n");
        let _h_new = store.record("a.rs", "fn b() {}\n");
        // 旧版本仍可凭旧 hash 找回
        let snap = store.by_hash("a.rs", &h_old).expect("旧版本应保留");
        assert_eq!(snap.text, "fn a() {}\n");
        assert_eq!(store.head("a.rs").unwrap().text, "fn b() {}\n");
    }

    #[test]
    fn version_history_capped() {
        let mut store = InMemorySnapshotStore::with_limits(10, 2);
        let h1 = store.record("a.rs", "v1\n");
        store.record("a.rs", "v2\n");
        store.record("a.rs", "v3\n");
        // 仅保留最近 2 版（v2、v3），v1 被淘汰
        assert!(store.by_hash("a.rs", &h1).is_none());
        assert_eq!(store.map["a.rs"].len(), 2);
    }

    #[test]
    fn lru_evicts_oldest_path() {
        let mut store = InMemorySnapshotStore::with_limits(2, 4);
        store.record("a.rs", "a\n");
        store.record("b.rs", "b\n");
        store.record("c.rs", "c\n"); // 触发淘汰 a.rs（最久未访问）
        assert!(store.head("a.rs").is_none());
        assert!(store.head("b.rs").is_some());
        assert!(store.head("c.rs").is_some());
    }

    #[test]
    fn recognizes_and_is_head() {
        let mut store = InMemorySnapshotStore::new();
        let h = store.record("a.rs", "x\n");
        assert!(store.recognizes("a.rs", &h));
        assert!(store.is_head("a.rs", &h));
        let h2 = store.record("a.rs", "y\n");
        assert!(store.recognizes("a.rs", &h)); // 旧版仍在历史
        assert!(!store.is_head("a.rs", &h));
        assert!(store.is_head("a.rs", &h2));
    }

    #[test]
    fn invalidate_clears_path() {
        let mut store = InMemorySnapshotStore::new();
        let h = store.record("a.rs", "x\n");
        store.invalidate("a.rs");
        assert!(!store.recognizes("a.rs", &h));
    }
}
