//! H30：合并（consolidate）租约——避免多会话同时做同一份记忆的 LLM 沉淀。
//!
//! 与 omp `memories/index.ts` 的 rollout lease 同思路：LLM 沉淀昂贵且会**重写**记忆文件，
//! 同一项目的多个会话（CLI / Web / ACP）若同时触发，会互相覆盖并重复付费。
//! 这里用文件锁做互斥：
//!
//! - `<root>/.<name>.lease` 以 `create_new` 独占创建（内容记录 `pid` + 时间戳）。
//! - 租约带 TTL：持有者崩溃留下的陈旧锁在超过 TTL 后可被抢占（避免永久死锁）。
//! - [`LeaseGuard`] drop 时删除锁文件（正常路径）；删除失败只影响后续抢占时间。
//!
//! 租约只保证**同一时刻一个合并者**；拿到租约的一方负责最终落盘（原子替换）。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 默认租约 TTL（5 分钟；LLM 沉淀 + 落盘远小于该值）。
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(300);

/// 租约守卫：drop 即释放。
#[derive(Debug)]
pub struct LeaseGuard {
    path: PathBuf,
}

impl LeaseGuard {
    /// 锁文件路径（诊断/测试用）。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 尝试获取租约；已被他人持有（且未过期）时返回 `None`。
///
/// `root` 不存在时自动创建（记忆目录首次使用）。
#[must_use]
pub fn acquire(root: &Path, name: &str, ttl: Duration) -> Option<LeaseGuard> {
    let _ = std::fs::create_dir_all(root);
    let path = root.join(format!(".{name}.lease"));
    if try_create(&path) {
        return Some(LeaseGuard { path });
    }
    // 已存在：判断是否陈旧（持有者崩溃）。
    let stale = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| parse_timestamp(&s))
        .is_none_or(|ts| now_ms().saturating_sub(ts) > ttl.as_millis() as u64);
    if !stale {
        return None;
    }
    // 陈旧：清掉后重试一次（`remove_file` 与并发抢占者竞争时只有一个 create_new 成功）。
    let _ = std::fs::remove_file(&path);
    if try_create(&path) {
        Some(LeaseGuard { path })
    } else {
        None
    }
}

/// 独占创建锁文件并写入 `pid` + 时间戳。
fn try_create(path: &Path) -> bool {
    use std::io::Write;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
    else {
        return false;
    };
    let body = format!("pid={}\nts={}\n", std::process::id(), now_ms());
    f.write_all(body.as_bytes()).is_ok()
}

/// 从锁文件内容解析 `ts=<ms>`。
fn parse_timestamp(body: &str) -> Option<u64> {
    body.lines()
        .find_map(|l| l.strip_prefix("ts="))
        .and_then(|v| v.trim().parse().ok())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire(dir.path(), "consolidate", DEFAULT_LEASE_TTL).expect("首次应拿到租约");
        assert!(first.path().exists(), "锁文件应落盘");
        // 同一时刻第二个持有者拿不到（即使 TTL 很大）。
        assert!(
            acquire(dir.path(), "consolidate", DEFAULT_LEASE_TTL).is_none(),
            "已持有时不得重复获取"
        );
        // 不同名字互不影响（local / structured 各自一把锁）。
        let other = acquire(dir.path(), "sleep", DEFAULT_LEASE_TTL).expect("不同名字应可获取");
        drop(other);
        drop(first);
        assert!(
            !dir.path().join(".consolidate.lease").exists(),
            "drop 应释放"
        );
        // 释放后可再次获取。
        let again = acquire(dir.path(), "consolidate", DEFAULT_LEASE_TTL).expect("释放后应可再取");
        drop(again);
    }

    #[test]
    fn stale_lease_is_stolen_after_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".consolidate.lease");
        // 手工写一个「很久以前」的锁（模拟持有者崩溃）。
        std::fs::write(&path, format!("pid=1\nts={}\n", now_ms() - 60_000)).unwrap();
        let stolen =
            acquire(dir.path(), "consolidate", Duration::from_secs(5)).expect("陈旧租约应可被抢占");
        let body = std::fs::read_to_string(stolen.path()).unwrap();
        assert!(
            body.contains(&format!("pid={}", std::process::id())),
            "{body}"
        );
        // TTL 内不抢占。
        assert!(
            acquire(dir.path(), "consolidate", DEFAULT_LEASE_TTL).is_none(),
            "未过期不得抢占"
        );
        drop(stolen);
    }

    #[test]
    fn corrupt_lease_file_counts_as_stale() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".consolidate.lease"), "garbage").unwrap();
        let g =
            acquire(dir.path(), "consolidate", DEFAULT_LEASE_TTL).expect("无法解析的锁视为陈旧");
        drop(g);
    }
}
