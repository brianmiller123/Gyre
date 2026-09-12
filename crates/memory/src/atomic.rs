//! H30：记忆落盘的原子原语。
//!
//! 记忆文件是多会话共享的（同一项目下 CLI / Web / ACP 可能同时在写），因此：
//!
//! - **追加**必须走 `O_APPEND`（单次 `write` 在 POSIX 下对同一文件是原子的）：不再
//!   「读全文 → 拼接 → 整体写回」，消除 read-modify-write 竞态（并发追加不会互相覆盖）。
//! - **整体替换**必须走「同目录临时文件 + `rename`」：`rename` 在 POSIX 上是原子替换，
//!   读者要么看到旧版本、要么看到新版本，不会读到写了一半的 JSONL（截断风险）。
//!
//! 两处都用 `sync_all` 尽力确保 rename 前的字节已落盘（崩溃时宁可保留旧版本）。

use std::io::Write;
use std::path::Path;

/// 追加一行（自动补 `\n`）到文件末尾——`O_APPEND` 原子追加。
///
/// # Errors
/// 目录创建或写入失败。
pub fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // OS 级 append 原子性：O_APPEND 保证每次 write 追加到文件末尾，
    // 消除 read-modify-write 竞态。即使多线程/多进程并发也安全。
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // 关键：行 + 换行必须**一次 write**——O_APPEND 只保证单次 write 原子，
    // 分成两次会让并发写交错（两行粘成一行、JSON 解析失败）。
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    file.write_all(buf.as_bytes())?;
    file.flush()
}

/// 原子整体替换文件内容（同目录临时文件 + `rename`）。
///
/// # Errors
/// 目录创建、临时文件写入或 rename 失败。
pub fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    // 临时名带 pid + 纳秒，避免多进程同名互踩；同目录保证 rename 不跨设备。
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("memory"),
        std::process::id(),
        stamp
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        // 尽力 fsync：崩溃时宁可保留旧文件，也不要留下半截新文件。
        let _ = f.sync_all();
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_is_concurrency_safe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bank").join("records.jsonl");
        // 8 线程 × 50 行并发追加：O_APPEND 下不应丢行、不应写坏。
        let mut handles = Vec::new();
        for t in 0..8 {
            let p = path.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..50 {
                    append_line(&p, &format!("{{\"t\":{t},\"i\":{i}}}")).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 400, "并发追加不得丢行");
        // 每行都应是完整 JSON（无交错/截断）。
        for line in lines {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("坏行 {line:?}: {e}"));
        }
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("MEMORY.md");
        write_atomic(&path, "v1").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");
        write_atomic(&path, "v2-longer").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2-longer");
        // 同目录不留临时文件（成功路径 rename 已消费）。
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");
        // 目标目录不存在时也应能创建（记忆目录首次落盘）。
        let nested = dir.path().join("a/b/c.md");
        write_atomic(&nested, "deep").unwrap();
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "deep");
    }
}
