//! Swarm 流水线文件系统状态跟踪器。
//!
//! 持久化 pipeline 与逐代理状态到 `.swarm_<name>/`，支持从磁盘恢复（resumability）。
//! 移植自 [`oh-my-pi swarm-extension/state.ts`](../../../third/oh-my-pi/packages/swarm-extension/src/swarm/state.ts)。
//!
//! 目录布局：
//! ```text
//! .swarm_<name>/
//! ├── state/pipeline.json   # SwarmState 快照
//! ├── logs/<agent>.log      # 逐代理日志
//! ├── logs/orchestrator.log # 编排日志
//! └── context/              # 子代理产物落盘目录
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// 流水线状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PipelineStatus {
    #[default]
    Idle,
    Running,
    Completed,
    Failed,
    Aborted,
}

/// 代理状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    #[default]
    Pending,
    Waiting,
    Running,
    Completed,
    Failed,
}

/// 逐代理状态。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentState {
    /// 代理名。
    pub name: String,
    /// 状态。
    pub status: AgentStatus,
    /// 当前轮次。
    pub iteration: usize,
    /// 所在波序号。
    pub wave: usize,
    /// 开始时间戳（毫秒）。
    pub started_at: Option<u64>,
    /// 完成时间戳（毫秒）。
    pub completed_at: Option<u64>,
    /// 错误信息。
    pub error: Option<String>,
}

/// 流水线全局状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmState {
    /// swarm 名。
    pub name: String,
    /// 状态。
    pub status: PipelineStatus,
    /// 模式（字符串化，便于 JSON）。
    pub mode: String,
    /// 当前轮次。
    pub iteration: usize,
    /// 目标轮数。
    pub target_count: usize,
    /// 逐代理状态。
    pub agents: BTreeMap<String, AgentState>,
    /// 开始时间戳（毫秒）。
    pub started_at: u64,
    /// 完成时间戳（毫秒）。
    pub completed_at: Option<u64>,
}

/// 文件系统状态跟踪器（线程安全）。
pub struct StateTracker {
    swarm_dir: PathBuf,
    state: Arc<Mutex<SwarmState>>,
}

impl StateTracker {
    /// 构造：状态目录为 `<workspace>/.swarm_<name>`。
    #[must_use]
    pub fn new(workspace_dir: &Path, name: &str) -> Self {
        let swarm_dir = workspace_dir.join(format!(".swarm_{name}"));
        Self {
            swarm_dir,
            state: Arc::new(Mutex::new(SwarmState {
                name: name.to_string(),
                status: PipelineStatus::Idle,
                mode: "sequential".to_string(),
                iteration: 0,
                target_count: 1,
                agents: BTreeMap::new(),
                started_at: now_ms(),
                completed_at: None,
            })),
        }
    }

    /// `.swarm_<name>` 根目录。
    #[must_use]
    pub fn swarm_dir(&self) -> &Path {
        &self.swarm_dir
    }

    /// 初始化子目录 + 代理状态。
    ///
    /// # Errors
    /// 创建目录失败时返回 IO 错误。
    pub async fn init(
        &self,
        agent_names: &[String],
        target_count: usize,
        mode: &str,
    ) -> Result<(), std::io::Error> {
        tokio::fs::create_dir_all(self.swarm_dir.join("state")).await?;
        tokio::fs::create_dir_all(self.swarm_dir.join("logs")).await?;
        tokio::fs::create_dir_all(self.swarm_dir.join("context")).await?;

        let mut state = self.state.lock().await;
        state.target_count = target_count;
        state.mode = mode.to_string();
        state.status = PipelineStatus::Running;
        state.started_at = now_ms();
        state.completed_at = None;
        state.agents.clear();
        for name in agent_names {
            state.agents.insert(
                name.clone(),
                AgentState {
                    name: name.clone(),
                    status: AgentStatus::Pending,
                    iteration: 0,
                    wave: 0,
                    started_at: None,
                    completed_at: None,
                    error: None,
                },
            );
        }
        drop(state);
        self.persist().await
    }

    /// 局部更新单代理状态。
    pub async fn update_agent(&self, name: &str, update: AgentStateUpdate) -> Result<(), std::io::Error> {
        {
            let mut state = self.state.lock().await;
            if let Some(agent) = state.agents.get_mut(name) {
                update.apply(agent);
            }
        }
        self.persist().await
    }

    /// 局部更新流水线状态。
    pub async fn update_pipeline(&self, update: PipelineStateUpdate) -> Result<(), std::io::Error> {
        {
            let mut state = self.state.lock().await;
            update.apply(&mut state);
        }
        self.persist().await
    }

    /// 追加逐代理日志。
    pub async fn append_log(&self, agent_name: &str, message: &str) -> Result<(), std::io::Error> {
        self.append(self.swarm_dir.join("logs").join(format!("{agent_name}.log")), message)
            .await
    }

    /// 追加编排日志。
    pub async fn append_orchestrator_log(&self, message: &str) -> Result<(), std::io::Error> {
        self.append(self.swarm_dir.join("logs").join("orchestrator.log"), message)
            .await
    }

    /// 从磁盘恢复状态（无则 `None`）。
    pub async fn load(&self) -> Result<Option<SwarmState>, std::io::Error> {
        let path = self.swarm_dir.join("state").join("pipeline.json");
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => {
                let state: SwarmState = serde_json::from_str(&text).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                })?;
                *self.state.lock().await = state.clone();
                Ok(Some(state))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 当前状态快照。
    pub async fn snapshot(&self) -> SwarmState {
        self.state.lock().await.clone()
    }

    async fn persist(&self) -> Result<(), std::io::Error> {
        let state = self.state.lock().await.clone();
        let json = serde_json::to_string_pretty(&state)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        tokio::fs::write(self.swarm_dir.join("state").join("pipeline.json"), json).await
    }

    async fn append(&self, path: PathBuf, message: &str) -> Result<(), std::io::Error> {
        use tokio::io::AsyncWriteExt;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let line = format!("[{}] {message}\n", iso_now());
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        Ok(())
    }
}

/// 单代理状态更新（builder）。
#[derive(Debug, Default, Clone)]
pub struct AgentStateUpdate {
    pub status: Option<AgentStatus>,
    pub iteration: Option<usize>,
    pub wave: Option<usize>,
    pub started_at: Option<u64>,
    pub completed_at: Option<u64>,
    pub error: Option<Option<String>>,
}

impl AgentStateUpdate {
    fn apply(self, agent: &mut AgentState) {
        if let Some(s) = self.status {
            agent.status = s;
        }
        if let Some(i) = self.iteration {
            agent.iteration = i;
        }
        if let Some(w) = self.wave {
            agent.wave = w;
        }
        if let Some(t) = self.started_at {
            agent.started_at = Some(t);
        }
        if let Some(t) = self.completed_at {
            agent.completed_at = Some(t);
        }
        if let Some(e) = self.error {
            agent.error = e;
        }
    }
}

/// 流水线状态更新。
#[derive(Debug, Default, Clone)]
pub struct PipelineStateUpdate {
    pub status: Option<PipelineStatus>,
    pub iteration: Option<usize>,
    pub completed_at: Option<Option<u64>>,
}

impl PipelineStateUpdate {
    fn apply(self, state: &mut SwarmState) {
        if let Some(s) = self.status {
            state.status = s;
        }
        if let Some(i) = self.iteration {
            state.iteration = i;
        }
        if let Some(c) = self.completed_at {
            state.completed_at = c;
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn iso_now() -> String {
    // RFC3339 近似（无 chrono 依赖）：用 unix 秒。
    format!("t={}s", now_ms() / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn persists_and_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let tracker = StateTracker::new(tmp.path(), "demo");
        tracker
            .init(&["a".to_string(), "b".to_string()], 1, "sequential")
            .await
            .unwrap();
        tracker
            .update_agent(
                "a",
                AgentStateUpdate {
                    status: Some(AgentStatus::Completed),
                    completed_at: Some(123),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let reloaded = StateTracker::new(tmp.path(), "demo");
        let state = reloaded.load().await.unwrap().unwrap();
        assert_eq!(state.agents["a"].status, AgentStatus::Completed);
        assert_eq!(state.agents["b"].status, AgentStatus::Pending);
    }
}
