//! 子 Agent 状态注册表 + 事件总线。

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};

use crate::model::{LogLevel, LogLine, SubAgentPhase, SubAgentStatus};

/// 内部环形日志缓冲容量（每个子 Agent 最多保留这么多条日志）。
const LOG_CAP: usize = 200;

/// 监控事件（总线广播）。
///
/// `Upsert` 携带变更后的完整快照，订阅方可据此维护本地副本；
/// `Log` / `Remove` 为辅助语义（聚合器通常只看 `Upsert`）。
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // 状态快照较大，但事件频率低、可接受
pub enum SupervisorEvent {
    /// 某子 Agent 状态变更（含最新快照）。
    Upsert(SubAgentStatus),
    /// 单条日志追加。
    Log {
        /// 子 Agent id。
        id: String,
        /// 日志行。
        line: LogLine,
    },
    /// 子 Agent 被移除（终态清理）。
    Remove {
        /// 子 Agent id。
        id: String,
    },
}

/// 子 Agent 监控总线：进程内状态注册表 + 广播流。
///
/// `Clone` 廉价（内部全 `Arc`）：[`TaskTool`](agent)、Server 转发器、CLI 仪表盘各持一份句柄，
/// 共享同一份状态与事件流，无需额外接线即可多端消费。
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Mutex<BTreeMap<String, SubAgentStatus>>>,
    tx: broadcast::Sender<SupervisorEvent>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    /// 构造（事件信道容量 256）。
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(256)
    }

    /// 构造并指定事件信道容量。
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        let (tx, _) = broadcast::channel(cap.max(16));
        Self {
            inner: Arc::new(Mutex::new(BTreeMap::new())),
            tx,
        }
    }

    /// 订阅事件流。
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<SupervisorEvent> {
        self.tx.subscribe()
    }

    /// 全量快照（按启动时间稳定排序）。
    pub async fn snapshot(&self) -> Vec<SubAgentStatus> {
        let mut v: Vec<SubAgentStatus> = self.inner.lock().await.values().cloned().collect();
        v.sort_by_key(|s| s.started_at);
        v
    }

    /// 当前子 Agent 总数。
    pub async fn count(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// 登记一个新的子 Agent，返回其 id。
    pub async fn spawn(&self, parent_id: Option<String>, label: String, task: String) -> String {
        let id = format!("sub-{}", short_id());
        let now = now_ms();
        let status = SubAgentStatus {
            id: id.clone(),
            parent_id,
            label,
            task,
            phase: SubAgentPhase::Pending,
            progress: 0.0,
            turns: 0,
            tool_calls: 0,
            usage: agent_core::Usage::default(),
            started_at: now,
            updated_at: now,
            current_activity: None,
            error: None,
            logs: Vec::new(),
        };
        self.inner.lock().await.insert(id.clone(), status.clone());
        let _ = self.tx.send(SupervisorEvent::Upsert(status));
        tracing::debug!(sub_agent = %id, "spawn");
        id
    }

    /// 设置阶段（相同阶段不重复记录）。
    pub async fn set_phase(&self, id: &str, phase: SubAgentPhase) {
        self.mutate(id, |s| {
            if s.phase != phase {
                s.phase = phase;
                s.logs.push(LogLine {
                    ts: now_ms(),
                    level: LogLevel::Info,
                    text: format!("阶段 → {}", phase.label()),
                });
                cap_logs(&mut s.logs);
            }
        })
        .await;
    }

    /// 记录一轮完成。
    pub async fn record_turn(&self, id: &str) {
        self.mutate(id, |s| {
            s.turns += 1;
        })
        .await;
    }

    /// 记录一次工具调用（并追加日志、更新活动）。
    pub async fn record_tool_call(&self, id: &str, name: &str) {
        self.mutate(id, |s| {
            s.tool_calls += 1;
            s.current_activity = Some(format!("调用工具: {name}"));
        })
        .await;
        self.log(id, LogLevel::Info, format!("⚡ 工具调用: {name}"))
            .await;
    }

    /// 累加用量。
    pub async fn record_usage(&self, id: &str, usage: &agent_core::Usage) {
        self.mutate(id, |s| {
            s.usage.add(usage);
        })
        .await;
    }

    /// 设置当前活动描述。
    pub async fn set_activity(&self, id: &str, activity: impl Into<String>) {
        let activity = activity.into();
        self.mutate(id, |s| {
            s.current_activity = Some(activity);
        })
        .await;
    }

    /// 追加一条日志（并广播 Upsert，便于聚合器统一处理）。
    pub async fn log(&self, id: &str, level: LogLevel, text: impl Into<String>) {
        let line = LogLine {
            ts: now_ms(),
            level,
            text: text.into(),
        };
        let mut guard = self.inner.lock().await;
        let Some(s) = guard.get_mut(id) else {
            return;
        };
        s.logs.push(line.clone());
        cap_logs(&mut s.logs);
        s.updated_at = now_ms();
        let snap = s.clone();
        drop(guard);
        let _ = self.tx.send(SupervisorEvent::Log {
            id: id.to_string(),
            line,
        });
        let _ = self.tx.send(SupervisorEvent::Upsert(snap));
    }

    /// 标记结束（成功 / 失败），写入终态日志。
    pub async fn finish(&self, id: &str, success: bool, error: Option<String>) {
        let msg = if success {
            "✓ 子 Agent 完成".to_string()
        } else {
            format!("✗ 子 Agent 失败: {}", error.as_deref().unwrap_or("未知"))
        };
        self.mutate(id, |s| {
            s.phase = if success {
                SubAgentPhase::Done
            } else {
                SubAgentPhase::Failed
            };
            s.current_activity = None;
            s.logs.push(LogLine {
                ts: now_ms(),
                level: if success {
                    LogLevel::Info
                } else {
                    LogLevel::Error
                },
                text: msg,
            });
            cap_logs(&mut s.logs);
            s.error = error;
        })
        .await;
    }

    /// 收敛内存：超过 `keep` 项时优先剔除最旧的终态项。
    ///
    /// 锁需覆盖「读终态列表 → 排序 → 移除」整个读改写，不可提前释放（nursery 告警为误报）。
    #[allow(clippy::significant_drop_tightening)]
    pub async fn prune(&self, keep: usize) {
        let mut guard = self.inner.lock().await;
        if guard.len() <= keep {
            return;
        }
        let mut terminal: Vec<(String, u64)> = guard
            .iter()
            .filter(|(_, s)| s.phase.is_terminal())
            .map(|(k, s)| (k.clone(), s.updated_at))
            .collect();
        terminal.sort_by_key(|(_, t)| *t);
        let remove = guard.len().saturating_sub(keep);
        for (k, _) in terminal.into_iter().take(remove) {
            guard.remove(&k);
            let _ = self.tx.send(SupervisorEvent::Remove { id: k });
        }
    }

    /// 通用更新：锁 → 应用 `f` → 重算进度 → 刷新时间戳 → 广播 Upsert。
    async fn mutate<F>(&self, id: &str, f: F)
    where
        F: FnOnce(&mut SubAgentStatus),
    {
        let mut guard = self.inner.lock().await;
        let Some(s) = guard.get_mut(id) else {
            return;
        };
        f(s);
        recompute_progress(s);
        s.updated_at = now_ms();
        let snap = s.clone();
        drop(guard);
        let _ = self.tx.send(SupervisorEvent::Upsert(snap));
    }
}

/// 监督阶段 → 稳定状态标签（snake_case；hub 工具 jobs 面原样透传，不转译）。
const fn phase_tag(p: SubAgentPhase) -> &'static str {
    match p {
        SubAgentPhase::Pending => "pending",
        SubAgentPhase::Running | SubAgentPhase::Streaming | SubAgentPhase::WaitingTool => "running",
        SubAgentPhase::Done => "done",
        SubAgentPhase::Failed => "failed",
        SubAgentPhase::Cancelled => "cancelled",
    }
}

/// 监督总线即 hub 监督句柄：`jobs` 提供真实快照；`cancel` 如实报错——
/// 观测注册表没有取消通道（取消令牌在任务框架侧），假装取消会谎报状态。
#[async_trait::async_trait]
impl agent_core::hub::HubSupervision for Supervisor {
    async fn jobs(&self) -> Vec<agent_core::hub::HubJobSnapshot> {
        self.snapshot()
            .await
            .into_iter()
            .map(|s| agent_core::hub::HubJobSnapshot {
                id: s.id,
                label: s.label,
                status: phase_tag(s.phase).to_string(),
                in_flight: !s.phase.is_terminal(),
                turns: Some(s.turns),
                progress: Some(s.progress),
            })
            .collect()
    }

    async fn cancel(&self, id: &str) -> Result<(), String> {
        Err(format!(
            "子任务 {id} 无法取消：监督注册表仅观测，暂无取消通道（需任务框架接入取消令牌）"
        ))
    }
}

/// 当前时间戳（毫秒 epoch）；时钟不可用时回退 0。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// 进度启发式：未完成不报满（封顶 90%），完成报 100%。
fn recompute_progress(s: &mut SubAgentStatus) {
    s.progress = match s.phase {
        SubAgentPhase::Pending => 0.0,
        SubAgentPhase::Done => 1.0,
        SubAgentPhase::Failed | SubAgentPhase::Cancelled => s.progress.clamp(0.0, 1.0),
        _ => {
            // 每轮 +15%（基线 8%），上限 90%。u16→f32 精确无损，规避 u64→f32 精度告警。
            let pct: u16 = (s.turns.min(6) * 15 + 8).min(90) as u16;
            f32::from(pct) / 100.0
        }
    };
}

/// 日志环形缓冲截断（保留最新 `LOG_CAP` 条）。
fn cap_logs(logs: &mut Vec<LogLine>) {
    if logs.len() > LOG_CAP {
        let drop = logs.len() - LOG_CAP;
        logs.drain(..drop);
    }
}

/// 进程内单调短 id。
fn short_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(1);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    format!("{n:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_progress_and_counts() {
        let s = Supervisor::new();
        let id = s.spawn(None, "t".into(), "do thing".into()).await;
        assert_eq!(s.count().await, 1);

        s.set_phase(&id, SubAgentPhase::Running).await;
        s.record_turn(&id).await;
        s.record_tool_call(&id, "shell").await;
        s.record_usage(
            &id,
            &agent_core::Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..agent_core::Usage::default()
            },
        )
        .await;
        s.finish(&id, true, None).await;

        let snap = s.snapshot().await;
        assert_eq!(snap.len(), 1);
        let a = &snap[0];
        assert_eq!(a.phase, SubAgentPhase::Done);
        assert!((a.progress - 1.0).abs() < 1e-6);
        assert_eq!(a.turns, 1);
        assert_eq!(a.tool_calls, 1);
        assert_eq!(a.usage.input_tokens, 10);
        assert!(!a.logs.is_empty());
    }

    #[tokio::test]
    async fn broadcast_upsert_on_spawn() {
        let s = Supervisor::new();
        let mut rx = s.subscribe();
        let _ = s.spawn(None, "t".into(), "x".into()).await;
        let ev = rx.recv().await;
        assert!(matches!(ev, Ok(SupervisorEvent::Upsert(_))));
    }

    #[tokio::test]
    async fn snapshot_sorted_by_started_at() {
        let s = Supervisor::new();
        let a = s.spawn(None, "a".into(), "1".into()).await;
        let b = s.spawn(None, "b".into(), "2".into()).await;
        let snap = s.snapshot().await;
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].id, a);
        assert_eq!(snap[1].id, b);
    }

    #[tokio::test]
    async fn supervision_port_maps_snapshot_and_rejects_cancel() {
        let s = Supervisor::new();
        let id = s.spawn(None, "Demo".into(), "任务".into()).await;
        s.set_phase(&id, SubAgentPhase::Running).await;

        let jobs = agent_core::hub::HubSupervision::jobs(&s).await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, id);
        assert_eq!(jobs[0].label, "Demo");
        assert_eq!(jobs[0].status, "running");
        assert!(jobs[0].in_flight);
        assert_eq!(jobs[0].turns, Some(0));

        s.finish(&id, true, None).await;
        let jobs = agent_core::hub::HubSupervision::jobs(&s).await;
        assert_eq!(jobs[0].status, "done");
        assert!(!jobs[0].in_flight);

        // cancel 如实报错（观测注册表无取消通道），而非假装取消。
        let err = agent_core::hub::HubSupervision::cancel(&s, &id).await;
        assert!(err.is_err());
    }
}
