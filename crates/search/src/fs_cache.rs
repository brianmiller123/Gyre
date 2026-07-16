//! 文件系统扫描缓存：缓存目录遍历条目，TTL + 空结果快速重检 + 有界容量。
//!
//! 移植自 [`oh-my-pi pi-natives/fs_cache.rs`](../../../third/oh-my-pi/crates/pi-natives/src/fs_cache.rs:1)（精简：
//! Minimal detail、`Mutex<HashMap>` 而非 DashMap、无 N-API）。
//!
//! glob / list_files 经 [`get_or_scan`] 共享同一份扫描结果，避免对同一根的重复遍历。
//! 写工具改文件后可调 [`invalidate`] 失效（当前主由短 TTL 兜底陈旧）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;

/// 默认 TTL（毫秒）。
const DEFAULT_TTL_MS: u64 = 1000;
/// 空结果快速重检阈值（毫秒）：空结果更易陈旧，缩短重检间隔。
const DEFAULT_EMPTY_RECHECK_MS: u64 = 200;
/// 最大缓存条目数。
const DEFAULT_MAX_ENTRIES: usize = 16;

/// 一条扫描结果（Minimal detail：相对路径 + 是否文件）。
#[derive(Debug, Clone)]
pub struct FsEntry {
    /// 相对搜索根的路径。
    pub rel_path: PathBuf,
    /// 是否普通文件。
    pub is_file: bool,
}

/// 缓存键：根 + 遍历策略（hidden / gitignore 组合分区）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    root: PathBuf,
    include_hidden: bool,
    use_gitignore: bool,
}

struct Cache {
    ttl: Duration,
    empty_recheck: Duration,
    max_entries: usize,
    map: HashMap<CacheKey, (Instant, Vec<FsEntry>)>,
}

impl Cache {
    fn new() -> Self {
        Self {
            ttl: Duration::from_millis(DEFAULT_TTL_MS),
            empty_recheck: Duration::from_millis(DEFAULT_EMPTY_RECHECK_MS),
            max_entries: DEFAULT_MAX_ENTRIES,
            map: HashMap::new(),
        }
    }

    /// 获取（或扫描并缓存）`root` 下的条目列表。
    fn get_or_scan(
        &mut self,
        root: &Path,
        include_hidden: bool,
        use_gitignore: bool,
    ) -> Vec<FsEntry> {
        let key = CacheKey {
            root: root.to_path_buf(),
            include_hidden,
            use_gitignore,
        };
        let now = Instant::now();
        if let Some((created, entries)) = self.map.get(&key) {
            let age = now.duration_since(*created);
            // 空结果用更短的重检阈值，降低「假阴性」陈旧窗口。
            let ttl_for = if entries.is_empty() {
                self.empty_recheck
            } else {
                self.ttl
            };
            if age < ttl_for {
                return entries.clone();
            }
        }
        let entries = scan(root, include_hidden, use_gitignore);
        self.insert(key, entries.clone());
        entries
    }

    fn insert(&mut self, key: CacheKey, entries: Vec<FsEntry>) {
        // 容量超限时淘汰最旧条目。
        if self.map.len() >= self.max_entries && !self.map.contains_key(&key) {
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (t, _))| *t)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&oldest);
            }
        }
        self.map.insert(key, (Instant::now(), entries));
    }

    /// 失效某 root 的全部缓存分区。
    fn invalidate(&mut self, root: &Path) {
        self.map.retain(|k, _| k.root != root);
    }

    fn clear(&mut self) {
        self.map.clear();
    }
}

/// 扫描 `root` 下的条目（尊重 hidden/gitignore 策略，跳过 `.git`）。
fn scan(root: &Path, include_hidden: bool, use_gitignore: bool) -> Vec<FsEntry> {
    let mut out = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(!include_hidden)
        .git_ignore(use_gitignore)
        .build();
    for entry in walker.flatten() {
        let path = entry.path();
        if path == root {
            continue;
        }
        let Some(ft) = entry.file_type() else { continue };
        let is_file = ft.is_file();
        let is_dir = ft.is_dir();
        if !is_file && !is_dir {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        out.push(FsEntry { rel_path: rel, is_file });
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    out
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::new()))
}

/// 获取（或扫描并缓存）`root` 下的条目列表。
///
/// 锁中毒时降级为直接扫描（不缓存），保证可用性。
#[must_use]
pub fn get_or_scan(root: &Path, include_hidden: bool, use_gitignore: bool) -> Vec<FsEntry> {
    match cache().lock() {
        Ok(mut c) => c.get_or_scan(root, include_hidden, use_gitignore),
        Err(_) => scan(root, include_hidden, use_gitignore),
    }
}

/// 失效某 root 的缓存（写工具改文件后可调用，缩短陈旧窗口）。
pub fn invalidate(root: &Path) {
    if let Ok(mut c) = cache().lock() {
        c.invalidate(root);
    }
}

/// 清空全部缓存。
pub fn clear_cache() {
    if let Ok(mut c) = cache().lock() {
        c.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-search-fscache-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn get_or_scan_returns_files() {
        let dir = tmp("scan");
        fs::write(dir.join("a.rs"), "x").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/b.rs"), "y").unwrap();
        clear_cache();
        let entries = get_or_scan(&dir, false, true);
        assert!(entries.iter().any(|e| e.rel_path == Path::new("a.rs")));
        assert!(entries.iter().any(|e| e.rel_path == Path::new("sub/b.rs")));
        // 目录条目也在（is_file=false）
        assert!(entries.iter().any(|e| e.rel_path == Path::new("sub") && !e.is_file));
    }

    #[test]
    fn invalidate_drops_root_entries() {
        let dir = tmp("invalidate");
        fs::write(dir.join("a.txt"), "x").unwrap();
        clear_cache();
        let entries = get_or_scan(&dir, false, true);
        assert!(entries.iter().any(|e| e.rel_path == Path::new("a.txt")));
        invalidate(&dir);
        // 失效后再扫应重新扫描（结果一致，但缓存已被清除——此处仅验证不 panic 且仍可取）
        let again = get_or_scan(&dir, false, true);
        assert!(again.iter().any(|e| e.rel_path == Path::new("a.txt")));
    }

    #[test]
    fn cache_is_bounded_and_evicts() {
        // 间接验证：大量不同根不会无限增长（max 16）。此处主要确认 API 不 panic。
        let dir = tmp("bound");
        fs::write(dir.join("x"), "1").unwrap();
        clear_cache();
        for i in 0..32 {
            let sub = dir.join(format!("r{i}"));
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join("f"), "1").unwrap();
            let _ = get_or_scan(&sub, false, true);
        }
        clear_cache();
    }
}
