//! 本地记忆存储：项目作用域 markdown + LLM 合并。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_core::{
    AssistantEvent, CompletionRequest, LlmProvider, MemoryNote, MemoryStore, Model,
    ProviderCallContext, ProviderMessage, UserContent,
};
use async_trait::async_trait;
use futures::StreamExt;

const SUMMARY_FILE: &str = "memory_summary.md";
const MEMORY_FILE: &str = "MEMORY.md";
const NOTES_FILE: &str = "notes.jsonl";

/// 本地记忆存储（按 cwd 哈希划分项目作用域）。
pub struct LocalMemoryStore {
    root: PathBuf,
}

impl LocalMemoryStore {
    /// 构造：按 `cwd` 计算哈希，定位 `<config_dir>/memory/<hash>` 目录。
    #[must_use]
    pub fn new(cwd: &Path) -> Self {
        let mut h = DefaultHasher::new();
        cwd.hash(&mut h);
        let hash = format!("{:016x}", h.finish());
        let root = agent_core::config_dir()
            .map(|d| d.join("memory").join(&hash))
            .unwrap_or_else(|| PathBuf::from(".agent/memory").join(hash));
        Self { root }
    }

    /// 用测试自定义目录构造（避开 config_dir）。
    #[must_use]
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    fn summary_path(&self) -> PathBuf {
        self.root.join(SUMMARY_FILE)
    }
    fn memory_path(&self) -> PathBuf {
        self.root.join(MEMORY_FILE)
    }
    fn notes_path(&self) -> PathBuf {
        self.root.join(NOTES_FILE)
    }

    fn ensure_dir(&self) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(&self.root)
    }

    /// 用 LLM 合并 raw notes + 旧 MEMORY.md → 新 MEMORY.md + memory_summary.md。
    ///
    /// 合并后清空 raw notes（已吸收）。无 raw notes 时直接返回 Ok（无需合并）。
    ///
    /// # Errors
    /// LLM 调用或落盘失败时返回错误。
    pub async fn consolidate(
        &self,
        provider: &Arc<dyn LlmProvider>,
        model: &Model,
        provider_ctx: &ProviderCallContext,
    ) -> Result<(), String> {
        self.ensure_dir().map_err(|e| e.to_string())?;
        let notes: Vec<MemoryNote> = read_notes(&self.notes_path())?;
        if notes.is_empty() {
            return Ok(());
        }
        let old_memory = read_text(&self.memory_path()).unwrap_or_default();
        let prompt = consolidation_prompt(&notes, &old_memory);
        let req = CompletionRequest {
            model: model.clone(),
            system: vec![
                "你是长期记忆合并助手。把新增事实并入既有长期记忆，去除重复与过时项，\
                 保留技术决策、约束、惯例、已踩坑。输出 Markdown。"
                    .to_string(),
            ],
            messages: vec![ProviderMessage::User {
                content: vec![UserContent::Text { text: prompt }],
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: 2048,
            temperature: Some(0.0),
            thinking: None,
            cache_key: None,
            stable_prefix_len: 0,
        };
        let mut stream = provider
            .stream(req, provider_ctx)
            .await
            .map_err(|e| e.to_string())?;
        let mut memory_md = String::new();
        while let Some(ev) = stream.next().await {
            if let AssistantEvent::TextDelta(d) = ev {
                memory_md.push_str(&d);
            }
        }
        // 写 MEMORY.md
        std::fs::write(self.memory_path(), &memory_md).map_err(|e| e.to_string())?;
        // 再用同一次输出裁出简洁 summary（取前 ~2000 字符作为注入摘要；首期为 MEMORY.md 的前缀快照）
        let summary = if memory_md.len() > 2000 {
            // 回退到字符边界，避免切片 panic。
            let mut end = 2000;
            while end > 0 && !memory_md.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}\n\n（更多见 MEMORY.md）", &memory_md[..end])
        } else {
            memory_md.clone()
        };
        std::fs::write(self.summary_path(), &summary).map_err(|e| e.to_string())?;
        // 清空 raw notes（已吸收）
        let _ = std::fs::remove_file(self.notes_path());
        Ok(())
    }
}

#[async_trait]
impl MemoryStore for LocalMemoryStore {
    async fn summary(&self) -> Result<Option<String>, std::io::Error> {
        Ok(read_text(&self.summary_path()))
    }

    async fn read_full(&self) -> Result<Option<String>, std::io::Error> {
        Ok(read_text(&self.memory_path()))
    }

    async fn append_note(&self, note: &MemoryNote) -> Result<(), std::io::Error> {
        self.ensure_dir()?;
        let line = serde_json::json!({
            "content": note.content,
            "source": note.source,
            "ts": unix_now(),
        });
        let serialized = serde_json::to_string(&line).unwrap_or_else(|_| "{}".to_string());
        append_line(&self.notes_path(), &serialized)
    }

    async fn clear(&self) -> Result<(), std::io::Error> {
        for f in [self.summary_path(), self.memory_path(), self.notes_path()] {
            if f.exists() {
                std::fs::remove_file(f)?;
            }
        }
        Ok(())
    }

    fn root_dir(&self) -> &PathBuf {
        &self.root
    }
}

/// 合并 prompt：把新增事实与旧 MEMORY.md 组装给 LLM。
#[must_use]
pub fn consolidation_prompt(notes: &[MemoryNote], old_memory: &str) -> String {
    let new_facts: Vec<String> = notes
        .iter()
        .map(|n| format!("- [{}] {}", n.source, n.content))
        .collect();
    format!(
        "# 既有长期记忆\n\n{old_memory}\n\n# 新增事实（待并入）\n\n{}\n\n\
         请输出合并后的完整 MEMORY.md（Markdown），结构化分组，去重去过时。",
        new_facts.join("\n")
    )
}

fn read_text(path: &Path) -> Option<String> {
    if path.exists() {
        std::fs::read_to_string(path).ok()
    } else {
        None
    }
}

fn append_line(path: &Path, line: &str) -> Result<(), std::io::Error> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // OS 级 append 原子性：O_APPEND 保证每次 write 追加到文件末尾，
    // 消除 read-modify-write 竞态。即使多线程/多任务并发也安全。
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()
}

fn read_notes(path: &Path) -> Result<Vec<MemoryNote>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let content = val
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let source = val
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        if !content.is_empty() {
            out.push(MemoryNote { content, source });
        }
    }
    Ok(out)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn nano() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn tmp() -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("agent-mem-{}-{:#x}", std::process::id(), nano()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn append_note_then_read_notes() {
        let root = tmp();
        let store = LocalMemoryStore::with_root(root.clone());
        store
            .append_note(&MemoryNote {
                content: "项目用 Rust 2024 edition".into(),
                source: "session:1".into(),
            })
            .await
            .unwrap();
        let notes = read_notes(&store.notes_path()).unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].content.contains("Rust 2024"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn summary_absent_returns_none() {
        let root = tmp();
        let store = LocalMemoryStore::with_root(root.clone());
        assert!(store.summary().await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn clear_removes_all() {
        let root = tmp();
        let store = LocalMemoryStore::with_root(root.clone());
        store
            .append_note(&MemoryNote {
                content: "x".into(),
                source: "s".into(),
            })
            .await
            .unwrap();
        std::fs::write(store.summary_path(), "summary").unwrap();
        std::fs::write(store.memory_path(), "memory").unwrap();
        store.clear().await.unwrap();
        assert!(store.summary().await.unwrap().is_none());
        assert!(store.read_full().await.unwrap().is_none());
        assert!(read_notes(&store.notes_path()).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn consolidation_prompt_formats() {
        let notes = vec![MemoryNote {
            content: "事实 A".into(),
            source: "src".into(),
        }];
        let p = consolidation_prompt(&notes, "旧记忆");
        assert!(p.contains("旧记忆"));
        assert!(p.contains("事实 A"));
        assert!(p.contains("[src]"));
    }
}
