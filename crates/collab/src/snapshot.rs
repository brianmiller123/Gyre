//! 快照分块 / 聚合 / 落盘恢复（P2-12 第三项）。
//!
//! 超大会话（多 MB）不能塞进单帧：WS 桥单帧上限 64 KiB，广播通道同样有内存压力。本模块提供：
//!\n//! - [`chunk_snapshot`]：按 **UTF-8 字符边界**切成字节有界的 [`SnapshotChunk`] 帧序列；
//! - [`SnapshotAssembler`]：接收端按 `seq` 严格递增聚合，收 `final_chunk` 后 `finish`；
//! - [`append_snapshot_log`] / [`read_snapshot_log`]：把聚合完成的快照以 JSONL 追加落盘
//!   （`~/.gyre/collab/<room_id>.jsonl`），断线重连后按最后一行恢复。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::CollabError;
use crate::frame::WireFrame;

/// 默认单块上限（字节）：WS 桥 64 KiB 上限留 1/4 余量（密封开销 + 帧头 + 中继缓冲）。
pub const SNAPSHOT_CHUNK_MAX: usize = 48 * 1024;

/// 快照落盘目录（相对 HOME 的默认路径）。
pub const DEFAULT_LOG_DIR: &str = ".gyre/collab";

/// 把快照文本切成 [`WireFrame::SnapshotChunk`] 帧序列（`seq` 从 0 起）。
///
/// 切割按 UTF-8 字符边界（`char_indices`），绝不把多字节字符劈开；空快照产出
/// 单块（空串 + `final_chunk: true`）。块数 = `ceil(len / max_chunk)`。
#[must_use]
pub fn chunk_snapshot(snapshot: &str, max_chunk: usize) -> Vec<WireFrame> {
    let max = max_chunk.max(1);
    if snapshot.is_empty() {
        return vec![WireFrame::SnapshotChunk {
            client_id: String::new(),
            seq: 0,
            final_chunk: true,
            chunk: String::new(),
            ts: 0,
        }];
    }
    let mut out = Vec::new();
    let mut seq = 0u32;
    let mut start = 0usize;
    // 按字符边界推进：每次找到 ≤ max 的边界。
    while start < snapshot.len() {
        let mut end = (start + max).min(snapshot.len());
        if end < snapshot.len() && !snapshot.is_char_boundary(end) {
            // 回退到上一个字符边界。
            while end > start && !snapshot.is_char_boundary(end) {
                end -= 1;
            }
        }
        let final_chunk = end >= snapshot.len();
        out.push(WireFrame::SnapshotChunk {
            client_id: String::new(),
            seq,
            final_chunk,
            chunk: snapshot[start..end].to_string(),
            ts: 0,
        });
        seq += 1;
        start = end;
        if final_chunk {
            break;
        }
    }
    out
}

/// 快照分块聚合器：按 `seq` 严格递增收集，收 `final_chunk` 后完成。
#[derive(Debug, Default)]
pub struct SnapshotAssembler {
    next_seq: u32,
    parts: Vec<String>,
    bytes: usize,
    finalized: bool,
}

impl SnapshotAssembler {
    /// 新建空聚合器。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 推入一块。`seq` 必须等于期望的下一个序号（重复/乱序被拒）。
    ///
    /// # Errors
    /// `seq` 不连续时返回 [`CollabError::ChunkOutOfOrder`]。
    pub fn push(&mut self, seq: u32, chunk: &str, final_chunk: bool) -> Result<(), CollabError> {
        if self.finalized {
            return Err(CollabError::ChunkOutOfOrder {
                expected: self.next_seq,
                got: seq,
            });
        }
        if seq != self.next_seq {
            return Err(CollabError::ChunkOutOfOrder {
                expected: self.next_seq,
                got: seq,
            });
        }
        self.next_seq += 1;
        self.bytes += chunk.len();
        self.parts.push(chunk.to_string());
        if final_chunk {
            self.finalized = true;
        }
        Ok(())
    }

    /// 是否已收 `final_chunk`（可 `finish`）。
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.finalized
    }

    /// 已聚合的字节数。
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    /// 取出完整快照。
    ///
    /// # Errors
    /// 尚未收到 `final_chunk` 时返回 [`CollabError`]（帧协议错误描述）。
    pub fn finish(self) -> Result<String, CollabError> {
        if !self.finalized {
            return Err(CollabError::Frame("snapshot incomplete: final chunk not received".into()));
        }
        Ok(self.parts.concat())
    }
}

/// 把完整快照以 JSONL 追加到 `<dir>/<room_id>.jsonl`（目录自动创建）。
///
/// 每行 `{"ts": <ms>, "len": <字节数>, "snapshot": "…"}`；断线重连恢复时取**最后一行**。
/// `dir` 缺省为 `~/.gyre/collab`（`HOME` 不可用时回退当前目录下 `.gyre/collab`）。
///
/// # Errors
/// 目录创建或写入失败时返回 [`std::io::Error`]。
pub fn append_snapshot_log(
    dir: &Path,
    room_id: &str,
    snapshot: &str,
    ts_ms: u64,
) -> std::io::Result<PathBuf> {
    let dir = if dir.as_os_str().is_empty() {
        default_log_dir()
    } else {
        dir.to_path_buf()
    };
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{room_id}.jsonl"));
    let line = serde_json::json!({
        "ts": ts_ms,
        "len": snapshot.len(),
        "snapshot": snapshot,
    })
    .to_string();
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{line}")?;
    Ok(path)
}

/// 读取快照日志的**最后一行**（最近一次完整快照）。
///
/// 文件不存在或为空时返回 `None`；行解析失败跳过继续向前找。
#[must_use]
pub fn read_snapshot_log(dir: &Path, room_id: &str) -> Option<String> {
    let dir = if dir.as_os_str().is_empty() {
        default_log_dir()
    } else {
        dir.to_path_buf()
    };
    let path = dir.join(format!("{room_id}.jsonl"));
    let content = fs::read_to_string(path).ok()?;
    for line in content.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(s) = v.get("snapshot").and_then(serde_json::Value::as_str) {
            return Some(s.to_string());
        }
    }
    None
}

/// 默认日志目录：`$HOME/.gyre/collab`；无 HOME 时回退 `.gyre/collab`（相对当前目录）。
#[must_use]
pub fn default_log_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home).join(DEFAULT_LOG_DIR),
        _ => PathBuf::from(DEFAULT_LOG_DIR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_small_snapshot_single_final_chunk() {
        let frames = chunk_snapshot("hello", 48 * 1024);
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            WireFrame::SnapshotChunk { seq, final_chunk, chunk, .. } => {
                assert_eq!(*seq, 0);
                assert!(*final_chunk);
                assert_eq!(chunk, "hello");
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn chunk_empty_snapshot_yields_one_empty_chunk() {
        let frames = chunk_snapshot("", 8);
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            WireFrame::SnapshotChunk { chunk, final_chunk, .. } => {
                assert!(chunk.is_empty());
                assert!(*final_chunk);
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn chunk_splits_on_utf8_boundaries() {
        // 每个汉字 3 字节；max=4 → 每块必须含整字符，落在 1 个字符（3B）上。
        let s = "中文测试字符串";
        let frames = chunk_snapshot(s, 4);
        assert!(frames.len() >= 2, "应被切多块: {}", frames.len());
        let assembled = assemble_all(&frames);
        assert_eq!(assembled, s);
        // 每块都是合法 UTF-8（chunk 字段是 String 本身保证），且字节数 ≤ max。
        for f in &frames {
            if let WireFrame::SnapshotChunk { chunk, .. } = f {
                assert!(chunk.len() <= 4, "块超限: {}B", chunk.len());
            }
        }
    }

    #[test]
    fn chunk_roundtrip_large_snapshot() {
        let s: String = (0..10_000).map(|i| char::from_u32(0x4e00 + (i % 100) as u32).unwrap()).collect();
        let frames = chunk_snapshot(&s, 48 * 1024);
        assert_eq!(assembled_len(&frames), s.len());
        assert_eq!(assemble_all(&frames), s);
        // 最后一块 final=true，其余 false。
        for (i, f) in frames.iter().enumerate() {
            if let WireFrame::SnapshotChunk { final_chunk, .. } = f {
                assert_eq!(*final_chunk, i == frames.len() - 1);
            }
        }
    }

    fn assemble_all(frames: &[WireFrame]) -> String {
        let mut a = SnapshotAssembler::new();
        for f in frames {
            if let WireFrame::SnapshotChunk { seq, final_chunk, chunk, .. } = f {
                a.push(*seq, chunk, *final_chunk).expect("顺序合法");
            }
        }
        a.finish().expect("final 已收")
    }

    fn assembled_len(frames: &[WireFrame]) -> usize {
        frames
            .iter()
            .map(|f| match f {
                WireFrame::SnapshotChunk { chunk, .. } => chunk.len(),
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn assembler_rejects_out_of_order() {
        let mut a = SnapshotAssembler::new();
        a.push(0, "aa", false).expect("首块");
        let err = a.push(2, "bb", false).unwrap_err();
        match err {
            CollabError::ChunkOutOfOrder { expected, got } => {
                assert_eq!(expected, 1);
                assert_eq!(got, 2);
            }
            other => panic!("unexpected: {other}"),
        }
        // 乱序块失败后 next_seq 未变：合法的下一块仍可推入。
        a.push(1, "bb", false).expect("乱序失败后恢复");
        // 重复块（seq 已消费）拒绝。
        assert!(matches!(
            a.push(1, "dup?", false),
            Err(CollabError::ChunkOutOfOrder { .. })
        ));
    }

    #[test]
    fn assembler_incomplete_finish_errors() {
        let mut a = SnapshotAssembler::new();
        a.push(0, "aa", false).expect("首块");
        assert!(!a.is_complete());
        assert!(matches!(a.finish(), Err(_)), "未收 final 不可 finish");
        // finish 消费聚合器，完整路径重建。
        let mut a = SnapshotAssembler::new();
        a.push(0, "aa", false).expect("首块");
        a.push(1, "bb", true).expect("final");
        assert!(a.is_complete());
        assert_eq!(a.finish().expect("完成"), "aabb");
    }

    #[test]
    fn snapshot_log_append_and_read_last() {
        let dir = std::env::temp_dir().join(format!("gyre-collab-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = append_snapshot_log(&dir, "roomA", "snap-1", 100).expect("落盘");
        assert!(path.exists());
        append_snapshot_log(&dir, "roomA", "snap-2", 200).expect("追加");
        assert_eq!(
            read_snapshot_log(&dir, "roomA").as_deref(),
            Some("snap-2"),
            "应取最后一行"
        );
        assert_eq!(read_snapshot_log(&dir, "missing-room"), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
