//! 会话持久化：JSONL 落盘 + 恢复（断点续跑）。
//!
//! 每条 [`AgentMessage`] 序列化为一行 JSON（JSON Lines）。恢复时按行反序列化重建日志。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agent_core::AgentMessage;

/// JSONL 持久化的上下文：包裹 [`InMemoryContext`](crate::InMemoryContext)，append 时自动追加写盘。
pub struct PersistentContext {
    inner: crate::InMemoryContext,
    path: PathBuf,
}

impl PersistentContext {
    /// 异步构造：加载已有 JSONL 恢复日志。若文件不存在则创建空会话。
    ///
    /// # Errors
    /// 读取已有文件解析失败时返回错误。
    pub async fn open(system: Vec<String>, path: impl Into<PathBuf>) -> Result<Self, agent_core::ContextError> {
        let path = path.into();
        let inner = crate::InMemoryContext::new(system);
        if path.exists() {
            let log = load_jsonl(&path).await?;
            inner.restore(log).await;
        }
        Ok(Self { inner, path })
    }

    /// 取底层内存上下文。
    #[must_use]
    pub fn inner(&self) -> &crate::InMemoryContext {
        &self.inner
    }

    /// 注入摘要提供器（启用 summarize 压缩）。
    pub async fn set_summarizer(&self, provider: Box<dyn crate::compaction::SummaryProvider>) {
        self.inner.set_summarizer(provider).await;
    }
}

#[async_trait::async_trait]
impl agent_core::ContextManager for PersistentContext {
    async fn append(&self, message: AgentMessage) {
        if let Err(e) = append_jsonl(&self.path, &message).await {
            tracing::warn!("持久化写入失败: {e}");
        }
        self.inner.append(message).await;
    }

    async fn set_system(&self, system: Vec<String>, tools: &[agent_core::ToolSpec]) {
        self.inner.set_system(system, tools).await;
    }

    async fn build_provider_context(
        &self,
        model: &agent_core::Model,
        tools: &[agent_core::ToolSpec],
    ) -> Result<agent_core::ProviderContext, agent_core::ContextError> {
        self.inner.build_provider_context(model, tools).await
    }

    async fn compact(&self, strategy: agent_core::CompactionStrategy) -> Result<(), agent_core::ContextError> {
        self.inner.compact(strategy).await?;
        let log = self.inner.snapshot().await;
        if let Err(e) = rewrite_jsonl(&self.path, &log).await {
            tracing::warn!("压缩后重写持久化失败: {e}");
        }
        Ok(())
    }

    async fn delete_message_at(
        &self,
        index: usize,
    ) -> Result<usize, agent_core::ContextError> {
        let removed = self.inner.delete_at(index).await;
        if removed > 0 {
            let log = self.inner.snapshot().await;
            if let Err(e) = rewrite_jsonl(&self.path, &log).await {
                tracing::warn!("删除消息后重写持久化失败: {e}");
                return Err(agent_core::ContextError::Io(e));
            }
        }
        Ok(removed)
    }

    fn token_usage(&self) -> agent_core::TokenUsage {
        self.inner.token_usage()
    }

    fn prefix_fingerprint(&self) -> String {
        self.inner.prefix_fingerprint()
    }
}

/// 会话存储：管理会话 JSONL 文件的 list / path / fork。
///
/// 项目本地：[`SessionStore::for_cwd`] 把会话放在 `<cwd>/.agent/sessions/`，随项目保存、
/// 按目录隔离；[`SessionStore::new`] 则放在用户级 `<config_dir>/sessions`（全局，不区分项目）。
pub struct SessionStore {
    dir: PathBuf,
}

/// 会话元信息。
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// 会话 id（文件名去 `.jsonl`）。
    pub id: String,
    /// 最后修改时间。
    pub mtime: std::time::SystemTime,
    /// 文件字节数。
    pub bytes: u64,
}

impl SessionStore {
    /// 默认会话目录 `<config_dir>/sessions`（无 config_dir 回退 `./.agent/sessions`）。
    #[must_use]
    pub fn new() -> Self {
        let dir = agent_core::config_dir()
            .map(|d| d.join("sessions"))
            .unwrap_or_else(|| PathBuf::from(".agent/sessions"));
        Self { dir }
    }

    /// 项目本地会话目录：`<cwd>/.agent/sessions`。
    ///
    /// 会话记录随项目保存在当前目录的 `.agent/sessions/` 下，天然按目录隔离——
    /// 不同项目互不可见，且会话随项目一起留存（可纳入 `.gitignore`）。
    #[must_use]
    pub fn for_cwd(cwd: &Path) -> Self {
        Self {
            dir: cwd.join(".agent").join("sessions"),
        }
    }

    /// 生成新会话 id（纳秒十六进制 + 进程内单调计数，避免同纳秒并发 fork 碰撞）。
    #[must_use]
    pub fn new_id() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("{nanos:x}-{seq:x}")
    }

    /// 会话 JSONL 文件路径。
    ///
    /// 安全：对 `id` 做防御性收敛，仅保留末段文件名并拒绝 `.` 开头/空，杜绝
    /// `..` 与路径分隔符导致的目录穿越（[`SessionStore::fork`] 另有严格校验）。
    #[must_use]
    pub fn path_for(&self, id: &str) -> PathBuf {
        let safe = sanitize_session_id_segment(id);
        self.dir.join(format!("{safe}.jsonl"))
    }

    /// 列出全部会话（按修改时间倒序）。
    #[must_use]
    pub fn list(&self) -> Vec<SessionInfo> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            out.push(SessionInfo {
                id,
                mtime: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                bytes: meta.len(),
            });
        }
        out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        out
    }

    /// 复制 `src_id` 会话为新 id（fork），返回新 id。
    ///
    /// # Errors
    /// 源文件不存在或复制失败时返回 IO 错误。
    pub fn fork(&self, src_id: &str) -> Result<String, std::io::Error> {
        // 严格校验源 id：fork 会读取并复制文件，必须杜绝 `..`/分隔符穿越。
        if !is_safe_session_id(src_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "非法会话 id（仅允许字母、数字、-、_，长度 1..=128）",
            ));
        }
        let src = self.path_for(src_id);
        let new_id = Self::new_id();
        let dst = self.path_for(&new_id);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&src, &dst)?;
        Ok(new_id)
    }

    /// 删除会话：移除 JSONL 落盘文件及其自定义标题（若存在）。
    ///
    /// 返回值：`Ok(true)` 表示文件存在并已删除；`Ok(false)` 表示会话文件不存在；
    /// `Err` 为非法 id 或 IO 错误。
    ///
    /// # Errors
    /// id 未通过安全校验或删除失败时返回错误。
    pub fn delete(&self, id: &str) -> Result<bool, std::io::Error> {
        if !is_safe_session_id(id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "非法会话 id",
            ));
        }
        let path = self.path_for(id);
        let existed = path.exists();
        if existed {
            std::fs::remove_file(&path)?;
        }
        // 同步清理自定义标题（缺失则无操作）。
        self.remove_title(id);
        Ok(existed)
    }

    /// 自定义标题索引文件路径（`<dir>/_titles.json`，与 JSONL 同目录、独立文件）。
    ///
    /// `list()` 仅枚举 `.jsonl`，故 `_titles.json` 不会被误列为会话。
    fn titles_path(&self) -> PathBuf {
        self.dir.join("_titles.json")
    }

    /// 读取全部自定义标题（容错：缺失/损坏返回空表，不阻塞列表）。
    fn read_titles(&self) -> HashMap<String, String> {
        let Ok(bytes) = std::fs::read(self.titles_path()) else {
            return HashMap::new();
        };
        serde_json::from_slice::<HashMap<String, String>>(&bytes).unwrap_or_default()
    }

    /// 原子写入标题索引（tmp + rename，避免中途崩溃损坏整份索引）。
    fn write_titles(&self, map: &HashMap<String, String>) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_vec(map)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = self.dir.join("_titles.json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, self.titles_path())?;
        Ok(())
    }

    /// 取某会话的自定义标题（无则 `None`，列表展示回退到首条用户输入预览）。
    #[must_use]
    pub fn title_for(&self, id: &str) -> Option<String> {
        self.read_titles().get(id).cloned()
    }

    /// 设置/更新某会话的自定义标题（重命名）。
    ///
    /// 校验：id 合法；标题去空白后非空且 ≤120 字符。
    ///
    /// # Errors
    /// id 非法或标题不合规或写入失败时返回错误。
    pub fn set_title(&self, id: &str, title: &str) -> Result<(), std::io::Error> {
        if !is_safe_session_id(id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "非法会话 id",
            ));
        }
        let trimmed = title.trim();
        if trimmed.is_empty() || trimmed.chars().count() > 120 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "标题为空或过长（需 1..=120 字符）",
            ));
        }
        let mut map = self.read_titles();
        map.insert(id.to_string(), trimmed.to_string());
        self.write_titles(&map)
    }

    /// 移除某会话的自定义标题（删除会话时联动清理；幂等）。
    fn remove_title(&self, id: &str) {
        let mut map = self.read_titles();
        if map.remove(id).is_some() {
            let _ = self.write_titles(&map);
        }
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

/// 会话 id 安全校验：仅允许 `[A-Za-z0-9_-]`，长度 1..=128（防 `/` `\` `..` 等穿越）。
fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 防御性收敛：仅取末段文件名、拒绝 `.` 开头与空，作为路径片段。
///
/// 安全：当 `file_name()` 返回 `None`（如 id 为 `..`、`/`、`.`）时**绝不回退原 id**
/// （会逃出会话目录），而是映射到固定安全名，杜绝路径穿越。
fn sanitize_session_id_segment(id: &str) -> String {
    std::path::Path::new(id)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty() && !s.starts_with('.'))
        .unwrap_or_else(|| "invalid-session".to_string())
}

/// 单条追加写（异步，O(1) append）。
async fn append_jsonl(path: &Path, msg: &AgentMessage) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let line = serde_json::to_string(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    use tokio::io::AsyncWriteExt;
    file.write_all(line.as_bytes()).await?;
    file.write_all(b"\n").await?;
    file.flush().await?;
    Ok(())
}

/// 整体重写（异步 + 原子）：先写临时文件并 sync，再 rename 覆盖，避免中途崩溃导致
/// 会话 JSONL 被截断/损坏（数据丢失）。
async fn rewrite_jsonl(path: &Path, log: &[AgentMessage]) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session.jsonl".to_string());
    let tmp = path.with_file_name(format!("{file_name}.tmp"));
    {
        let mut file = tokio::fs::File::create(&tmp).await?;
        use tokio::io::AsyncWriteExt;
        for m in log {
            let line = serde_json::to_string(m)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            file.write_all(line.as_bytes()).await?;
            file.write_all(b"\n").await?;
        }
        file.flush().await?;
        file.sync_all().await?;
    }
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// 直接从 JSONL 文件删除第 `index` 条有效消息（含孤立 tool 结果清理），原子重写落盘。
///
/// 适用于会话未加载到内存（无活跃 [`PersistentContext`）时的离线删除：读取全部
/// 有效消息 → [`crate::remove_entry_with_orphans`] → [`rewrite_jsonl`] 原子覆盖。
///
/// 返回 `Ok(0)` 表示索引越界（无变更）；`Ok(n>0)` 表示已删除 n 条并落盘。
///
/// # Errors
/// 读取/解析/写入失败时返回 IO 错误。
pub async fn delete_message_in_file(
    path: &std::path::Path,
    index: usize,
) -> Result<usize, std::io::Error> {
    let log = load_jsonl(path)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("加载会话失败: {e}")))?;
    let Some(new_log) = crate::remove_entry_with_orphans(&log, index) else {
        return Ok(0);
    };
    let removed = log.len().saturating_sub(new_log.len());
    if removed == 0 {
        return Ok(0);
    }
    rewrite_jsonl(path, &new_log).await?;
    Ok(removed)
}

/// 加载恢复：流式逐行反序列化，并对总字节数与单行长度设上限，防止超大历史会话 OOM。
async fn load_jsonl(path: &Path) -> Result<Vec<AgentMessage>, agent_core::ContextError> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    /// 会话恢复加载的最大总字节数（超出则仅加载首部并告警）。
    const MAX_LOAD_BYTES: u64 = 64 * 1024 * 1024;
    /// 单行最大字节数（防异常巨型行）。
    const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

    let file = tokio::fs::File::open(path).await?;
    let mut reader = BufReader::new(file);
    let mut log = Vec::new();
    let mut line = String::new();
    let mut consumed: u64 = 0;
    let mut i = 0usize;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        consumed = consumed.saturating_add(n as u64);
        if line.len() > MAX_LINE_BYTES {
            tracing::warn!(
                "持久化第 {i} 行超过 {MAX_LINE_BYTES} 字节，跳过（疑似损坏）"
            );
            i += 1;
            continue;
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            match serde_json::from_str::<AgentMessage>(trimmed) {
                Ok(m) => log.push(m),
                Err(e) => tracing::warn!("持久化第 {i} 行解析失败，跳过: {e}"),
            }
        }
        i += 1;
        if consumed > MAX_LOAD_BYTES {
            tracing::warn!(
                "会话加载触及 {MAX_LOAD_BYTES} 字节上限，停止加载后续行（共已读 {i} 行）"
            );
            break;
        }
    }
    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ContextManager;

    #[test]
    fn session_id_segment_never_escapes() {
        // 回归：file_name() 为 None 的 id（如 ".."）不得回退成可穿越的原 id。
        let seg = sanitize_session_id_segment("..");
        assert!(!seg.contains(".."), "{seg} 仍含穿越片段");
        assert!(!seg.is_empty());
        assert_eq!(sanitize_session_id_segment("normal-id"), "normal-id");
    }

    fn tmp_path() -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("agent-ctx-{n}.jsonl"))
    }

    #[tokio::test]
    async fn append_and_reload_roundtrip() {
        let path = tmp_path();
        // 写入 2 条
        let pc = PersistentContext::open(vec!["sys".into()], &path).await.unwrap();
        pc.append(AgentMessage::user_text("hello")).await;
        pc.append(AgentMessage::user_text("world")).await;
        drop(pc);

        // 重新打开恢复
        let pc2 = PersistentContext::open(vec!["sys".into()], &path).await.unwrap();
        let model = agent_core::Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        let built = pc2.build_provider_context(&model, &[]).await.unwrap();
        assert_eq!(built.messages.len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn for_cwd_isolates_projects() {
        // 不存在的路径：canonicalize 回退到字面路径，仍可稳定哈希
        let a = SessionStore::for_cwd(std::path::Path::new("/tmp/agent-proj-A-xyz"));
        let a2 = SessionStore::for_cwd(std::path::Path::new("/tmp/agent-proj-A-xyz"));
        let b = SessionStore::for_cwd(std::path::Path::new("/tmp/agent-proj-B-xyz"));
        assert_eq!(
            a.path_for("sess"),
            a2.path_for("sess"),
            "相同 cwd 应映射到相同会话目录"
        );
        assert_ne!(
            a.path_for("sess"),
            b.path_for("sess"),
            "不同 cwd 应隔离到不同会话目录"
        );
    }

    fn tmp_store() -> SessionStore {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        SessionStore {
            dir: std::env::temp_dir().join(format!("agent-titles-{n}")),
        }
    }

    #[test]
    fn title_roundtrip_and_delete_cleans() {
        let store = tmp_store();
        // 初始无标题。
        assert!(store.title_for("s1").is_none());
        // 写入标题后可读回（去空白）。
        store.set_title("s1", "  My Plan  ").unwrap();
        assert_eq!(store.title_for("s1").as_deref(), Some("My Plan"));
        // 空标题/超长被拒。
        assert!(store.set_title("s1", "   ").is_err());
        let long = "x".repeat(121);
        assert!(store.set_title("s1", &long).is_err());
        // 非法 id 被拒（防穿越）。
        assert!(store.set_title("../escape", "x").is_err());
        assert!(store.delete("../escape").is_err());
        // 多标题共存。
        store.set_title("s2", "Second").unwrap();
        assert_eq!(store.read_titles().len(), 2);
        // delete 清理标题（即使无 JSONL 文件也成功清理标题）。
        store.delete("s1").unwrap();
        assert!(store.title_for("s1").is_none());
        assert_eq!(store.title_for("s2").as_deref(), Some("Second"));
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    /// 删除消息后：内存日志与落盘 JSONL 同步更新——重新从磁盘打开会话，
    /// 被删消息不再存在（证明 session 内容确被持久移除，而非仅前端隐藏）。
    #[tokio::test]
    async fn delete_message_persists_to_disk() {
        use agent_core::ContextManager;
        let path = tmp_path();
        // 写入 3 条用户消息（每条 append 即落盘一行 JSONL）。
        {
            let pc = PersistentContext::open(vec!["sys".into()], &path).await.unwrap();
            pc.append(AgentMessage::user_text("a")).await;
            pc.append(AgentMessage::user_text("b")).await;
            pc.append(AgentMessage::user_text("c")).await;
        }
        // 删除索引 1（"b"）：内存日志 + 原子重写 JSONL。
        {
            let pc = PersistentContext::open(vec!["sys".into()], &path).await.unwrap();
            let removed = pc.delete_message_at(1).await.unwrap();
            assert_eq!(removed, 1, "应删除 1 条");
        }
        // 重新从磁盘打开：应只剩 a、c（b 已持久移除）。
        let pc = PersistentContext::open(vec!["sys".into()], &path).await.unwrap();
        let model =
            agent_core::Model::with_defaults("m", "openai", agent_core::Api::OpenAiCompletions);
        let built = pc.build_provider_context(&model, &[]).await.unwrap();
        let texts: Vec<String> = built
            .messages
            .iter()
            .filter_map(|m| match m {
                agent_core::ProviderMessage::User { content } => content
                    .iter()
                    .filter_map(|c| match c {
                        agent_core::UserContent::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .next(),
                _ => None,
            })
            .collect();
        assert_eq!(built.messages.len(), 2);
        assert_eq!(texts, vec!["a".to_string(), "c".to_string()]);
        let _ = std::fs::remove_file(&path);
    }
}
