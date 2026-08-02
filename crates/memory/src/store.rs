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

use crate::mental_models::{
    format_ts, load_seeds, merge_mental_models, mental_model_consolidation_prompt,
    MentalModelsConfig,
};

const SUMMARY_FILE: &str = "memory_summary.md";
const MEMORY_FILE: &str = "MEMORY.md";
const NOTES_FILE: &str = "notes.jsonl";
const MENTAL_MODELS_FILE: &str = "mental_models.md";

/// 本地记忆存储（按 cwd 哈希划分项目作用域）。
pub struct LocalMemoryStore {
    root: PathBuf,
    mental_models_cfg: MentalModelsConfig,
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
        Self {
            root,
            mental_models_cfg: MentalModelsConfig::default(),
        }
    }

    /// 用测试自定义目录构造（避开 config_dir）。
    #[must_use]
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            mental_models_cfg: MentalModelsConfig::default(),
        }
    }

    /// 设置心智模型配置（seeds 路径与注入预算）；链式调用，供 cli 装配时注入。
    #[must_use]
    pub fn with_mental_models_config(mut self, config: MentalModelsConfig) -> Self {
        self.mental_models_cfg = config;
        self
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
    fn mental_models_path(&self) -> PathBuf {
        self.root.join(MENTAL_MODELS_FILE)
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

    /// 返回注入用的心智模型 markdown：seeds（内置 + 自定义路径）在前，项目积累在后；
    /// 超 [`MentalModelsConfig::max_inject_chars`] 截断保留尾部最新。全空时返回 `None`。
    pub async fn mental_models(&self) -> Option<String> {
        let seeds = load_seeds(&self.mental_models_cfg);
        let project = read_text(&self.mental_models_path()).unwrap_or_default();
        merge_mental_models(&seeds, &project, self.mental_models_cfg.max_inject_chars)
    }

    /// 把单条心智模型追加到项目 `mental_models.md`（带时间戳条目）。
    ///
    /// # Errors
    /// 写入失败时返回 IO 错误。
    pub async fn add_mental_model(&self, text: &str) -> Result<(), std::io::Error> {
        self.ensure_dir()?;
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let line = format!("- [{}] {}", format_ts(unix_now()), text);
        append_line(&self.mental_models_path(), &line)
    }

    /// 任务结束时用 LLM 把 `mental_models.md` 去重/合并/提炼（复用 [`Self::consolidate`]
    /// 的流式调用模式）。文件不存在或为空时直接返回 `Ok`（无需合并）。
    ///
    /// # Errors
    /// LLM 调用或落盘失败时返回错误。
    pub async fn consolidate_mental_models(
        &self,
        provider: &Arc<dyn LlmProvider>,
        model: &Model,
        provider_ctx: &ProviderCallContext,
    ) -> Result<(), String> {
        let current = read_text(&self.mental_models_path()).unwrap_or_default();
        if current.trim().is_empty() {
            return Ok(());
        }
        let prompt = mental_model_consolidation_prompt(&current);
        let req = CompletionRequest {
            model: model.clone(),
            system: vec![
                "你是心智模型整理助手。把项目积累的心智模型去重、按主题分组、提炼成精炼的\
                 工程准则清单。保留具体可操作的条目，丢弃泛泛而谈的内容。输出 Markdown。"
                    .to_string(),
            ],
            messages: vec![ProviderMessage::User {
                content: vec![UserContent::Text { text: prompt }],
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: 1024,
            temperature: Some(0.0),
            thinking: None,
            cache_key: None,
            stable_prefix_len: 0,
        };
        let mut stream = provider
            .stream(req, provider_ctx)
            .await
            .map_err(|e| e.to_string())?;
        let mut output = String::new();
        while let Some(ev) = stream.next().await {
            if let AssistantEvent::TextDelta(d) = ev {
                output.push_str(&d);
            }
        }
        std::fs::write(self.mental_models_path(), &output).map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[async_trait]
impl MemoryStore for LocalMemoryStore {
    async fn summary(&self) -> Result<Option<String>, std::io::Error> {
        let base = read_text(&self.summary_path());
        let mental = self.mental_models().await;
        Ok(match (base, mental) {
            (None, None) => None,
            (Some(base), None) => Some(base),
            (None, Some(mental)) => Some(format!("<mental_models>\n{mental}\n</mental_models>")),
            (Some(base), Some(mental)) => Some(format!(
                "{base}\n\n<mental_models>\n{mental}\n</mental_models>"
            )),
        })
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
        for f in [
            self.summary_path(),
            self.memory_path(),
            self.notes_path(),
            self.mental_models_path(),
        ] {
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
    async fn summary_injects_mental_models_block() {
        let root = tmp();
        let store = LocalMemoryStore::with_root(root.clone());
        // 无 memory_summary.md 时，内置 seeds 仍使 summary 非空并携带 <mental_models> 段
        let s = store.summary().await.unwrap().unwrap();
        assert!(s.contains("<mental_models>"));
        assert!(s.contains("</mental_models>"));
        assert!(s.contains("先读 AGENTS.md 再动手"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn summary_keeps_existing_memory_and_appends_block() {
        let root = tmp();
        let store = LocalMemoryStore::with_root(root.clone());
        std::fs::write(store.summary_path(), "既有摘要\n第二行").unwrap();
        let s = store.summary().await.unwrap().unwrap();
        assert!(s.contains("既有摘要"));
        assert!(s.contains("<mental_models>"));
        // 心智模型段在摘要之后
        assert!(s.find("既有摘要").unwrap() < s.find("<mental_models>").unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn mental_models_merges_seeds_and_project() {
        let root = tmp();
        let store = LocalMemoryStore::with_root(root.clone());
        std::fs::write(
            store.mental_models_path(),
            "- [2026-08-02 10:00:00] 本项目用 Rust 2024 edition\n",
        )
        .unwrap();
        let mm = store.mental_models().await.unwrap();
        // seeds 在前，项目积累在后
        let seed_pos = mm.find("先读 AGENTS.md 再动手").unwrap();
        let proj_pos = mm.find("Rust 2024").unwrap();
        assert!(seed_pos < proj_pos);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn add_mental_model_appends_timestamped_entry() {
        let root = tmp();
        let store = LocalMemoryStore::with_root(root.clone());
        store
            .add_mental_model("写测试先于实现")
            .await
            .unwrap();
        let mm = store.mental_models().await.unwrap();
        assert!(mm.contains("写测试先于实现"));
        // 带时间戳条目
        assert!(mm.contains("- [20"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn mental_models_respects_inject_budget() {
        let root = tmp();
        let store = LocalMemoryStore::with_root(root.clone()).with_mental_models_config(
            MentalModelsConfig {
                seeds_paths: vec![],
                max_inject_chars: 40,
            },
        );
        std::fs::write(
            store.mental_models_path(),
            "- [2026-08-02 10:00:00] 旧条目\n- [2026-08-03 10:00:00] 最新条目\n",
        )
        .unwrap();
        let mm = store.mental_models().await.unwrap();
        assert!(mm.chars().count() <= 40, "实际 {} 字符", mm.chars().count());
        assert!(mm.contains("最新条目"), "保留尾部最新");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn clear_removes_mental_models() {
        let root = tmp();
        let store = LocalMemoryStore::with_root(root.clone());
        std::fs::write(store.mental_models_path(), "条目").unwrap();
        store.clear().await.unwrap();
        assert!(!store.mental_models_path().exists());
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
        std::fs::write(store.mental_models_path(), "心智模型条目").unwrap();
        store.clear().await.unwrap();
        // 摘要与心智模型条目被清除；但内置 seeds 使 summary 仍携带 <mental_models> 段
        let s = store.summary().await.unwrap().unwrap();
        assert!(!s.contains("summary"));
        assert!(!s.contains("心智模型条目"));
        assert!(s.contains("<mental_models>"));
        assert!(store.read_full().await.unwrap().is_none());
        assert!(read_notes(&store.notes_path()).unwrap().is_empty());
        assert!(!store.mental_models_path().exists());
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
