//! Swarm 流水线编排器：迭代 × 波，波内并行、波间串行。
//!
//! 移植自 [`oh-my-pi swarm-extension/pipeline.ts`](../../../third/oh-my-pi/packages/swarm-extension/src/swarm/pipeline.ts)。
//!
//! 模式语义：
//! - `parallel`：单波（忽略 DAG，全部并行）—— 由 `build_execution_waves` 产出单波实现
//! - `sequential`/`pipeline`：按 DAG 波依次推进；pipeline 模式重复 `target_count` 轮

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use agent_core::Model;
use tokio_util::sync::CancellationToken;

use crate::executor::{ProgressFn, SwarmAgentResult, SwarmAgentRunner};
use crate::schema::SwarmDefinition;
use crate::state::{
    AgentStateUpdate, AgentStatus, PipelineStateUpdate, PipelineStatus, StateTracker,
};

/// 流水线进度快照。
#[derive(Debug, Clone)]
pub struct PipelineProgress {
    /// 当前轮次（0-indexed）。
    pub iteration: usize,
    /// 目标轮数。
    pub target_count: usize,
    /// 当前波（0-indexed）。
    pub current_wave: usize,
    /// 总波数。
    pub total_waves: usize,
    /// 逐代理状态快照。
    pub agents: BTreeMap<String, AgentStatus>,
}

/// 流水线运行结果。
#[derive(Debug, Clone)]
pub struct PipelineResult {
    /// 终态。
    pub status: PipelineStatus,
    /// 已完成轮数。
    pub iterations: usize,
    /// 逐代理 × 轮 的结果。
    pub agent_results: BTreeMap<String, Vec<SwarmAgentResult>>,
    /// 错误列表。
    pub errors: Vec<String>,
}

/// 流水线运行选项。
pub struct PipelineOptions {
    /// 工作区根。
    pub workspace: Arc<Path>,
    /// 取消令牌。
    pub cancel: CancellationToken,
    /// 模型覆盖（None 用 swarm 默认）。
    pub model_override: Option<Model>,
    /// 进度回调。
    pub on_progress: Option<ProgressFn>,
    /// 同波并发护栏（≥1）；取自 `[subagent].max_concurrent`。
    pub max_concurrent: usize,
}

/// 流水线控制器。
pub struct PipelineController {
    def: SwarmDefinition,
    waves: Vec<Vec<String>>,
    state: Arc<StateTracker>,
}

impl PipelineController {
    /// 构造。
    #[must_use]
    pub fn new(def: SwarmDefinition, waves: Vec<Vec<String>>, state: Arc<StateTracker>) -> Self {
        Self { def, waves, state }
    }

    /// 运行整个流水线。
    pub async fn run(
        &self,
        runner: &Arc<dyn SwarmAgentRunner>,
        options: PipelineOptions,
    ) -> PipelineResult {
        let PipelineOptions {
            workspace: _,
            cancel,
            model_override,
            on_progress,
            max_concurrent,
        } = options;
        // 护栏：max_concurrent 至少 1（防止配置为 0 导致死锁）。
        let max_concurrent = max_concurrent.max(1);

        let mut all_results: BTreeMap<String, Vec<SwarmAgentResult>> = BTreeMap::new();
        for name in self.def.agents.keys() {
            all_results.insert(name.clone(), Vec::new());
        }
        let mut errors: Vec<String> = Vec::new();
        let target_count = self.def.target_count;

        let _ = self
            .state
            .append_orchestrator_log(&format!(
                "Pipeline '{}' starting: mode={:?} iterations={} waves={} agents={}",
                self.def.name,
                self.def.mode,
                target_count,
                self.waves.len(),
                self.def.agents.len()
            ))
            .await;

        for iteration in 0..target_count {
            if cancel.is_cancelled() {
                let _ = self
                    .state
                    .update_pipeline(PipelineStateUpdate {
                        status: Some(PipelineStatus::Aborted),
                        ..Default::default()
                    })
                    .await;
                return PipelineResult {
                    status: PipelineStatus::Aborted,
                    iterations: iteration,
                    agent_results: all_results,
                    errors,
                };
            }

            let _ = self
                .state
                .update_pipeline(PipelineStateUpdate {
                    iteration: Some(iteration),
                    ..Default::default()
                })
                .await;
            let _ = self
                .state
                .append_orchestrator_log(&format!(
                    "--- Iteration {}/{} ---",
                    iteration + 1,
                    target_count
                ))
                .await;

            let iteration_results = self
                .run_iteration(
                    runner,
                    iteration,
                    &cancel,
                    model_override.as_ref(),
                    on_progress.as_ref(),
                    max_concurrent,
                )
                .await;

            for (agent_name, result) in iteration_results {
                if let Some(bucket) = all_results.get_mut(&agent_name) {
                    bucket.push(result.clone());
                }
                if result.exit_code != 0 {
                    errors.push(format!(
                        "{agent_name} (iteration {}): {}",
                        iteration + 1,
                        result
                            .error
                            .clone()
                            .unwrap_or_else(|| format!("exit code {}", result.exit_code))
                    ));
                }
            }
        }

        let status = if errors.is_empty() {
            PipelineStatus::Completed
        } else {
            PipelineStatus::Failed
        };
        let _ = self
            .state
            .update_pipeline(PipelineStateUpdate {
                status: Some(status),
                completed_at: Some(Some(now_ms())),
                ..Default::default()
            })
            .await;
        let _ = self
            .state
            .append_orchestrator_log(&format!("Pipeline {:?} ({} errors)", status, errors.len()))
            .await;

        PipelineResult {
            status,
            iterations: target_count,
            agent_results: all_results,
            errors,
        }
    }

    async fn run_iteration(
        &self,
        runner: &Arc<dyn SwarmAgentRunner>,
        iteration: usize,
        cancel: &CancellationToken,
        model_override: Option<&Model>,
        on_progress: Option<&ProgressFn>,
        max_concurrent: usize,
    ) -> BTreeMap<String, SwarmAgentResult> {
        let mut results: BTreeMap<String, SwarmAgentResult> = BTreeMap::new();

        for (wave_idx, wave) in self.waves.iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            let _ = self
                .state
                .append_orchestrator_log(&format!(
                    "Wave {}/{}: [{}]",
                    wave_idx + 1,
                    self.waves.len(),
                    wave.join(", ")
                ))
                .await;

            // 标记本波代理为 waiting
            for agent_name in wave {
                let _ = self
                    .state
                    .update_agent(
                        agent_name,
                        AgentStateUpdate {
                            status: Some(AgentStatus::Waiting),
                            iteration: Some(iteration),
                            wave: Some(wave_idx),
                            ..Default::default()
                        },
                    )
                    .await;
            }

            self.emit_progress(iteration, wave_idx, on_progress).await;

            // 同波并发执行（受 max_concurrent 信号量护栏：限制同时 in-flight 子 Agent 数，
            // 防止 parallel 模式下瞬时打出 N 路并发请求触发上游速率限制/资源耗尽）。
            let sem = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
            let mut join = Vec::new();
            for agent_name in wave {
                let agent = self
                    .def
                    .agents
                    .get(agent_name)
                    .expect("wave agent must exist in definition")
                    .clone();
                // 在 spawn 前 acquire 许可：超出并发上限时在此等待，自然形成背压。
                //
                // 注：`sem` 为本函数局部变量、从不调用 `.close()`，故 `acquire_owned` 的
                // `Err` 分支在当前实现下实际不可达。保留 `match` 是防御性的——一旦未来将
                // `sem` 提升为跨迭代共享（如全局并发池）并支持显式关闭，此处即可正确响应。
                let permit = match sem.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(e) => {
                        results.insert(
                            agent.name.clone(),
                            SwarmAgentResult {
                                exit_code: 1,
                                output: String::new(),
                                error: Some(format!("并发信号量已关闭: {e}")),
                            },
                        );
                        continue;
                    }
                };
                let runner = Arc::clone(runner);
                let cancel = cancel.clone();
                let model_override = model_override.cloned();
                let progress = on_progress.cloned();
                let task_name = agent.name.clone();
                let handle = tokio::spawn(async move {
                    let _permit = permit; // 持有至任务结束，归还许可
                    runner
                        .run(
                            &agent,
                            &agent.task,
                            model_override.as_ref(),
                            &cancel,
                            progress.as_ref(),
                        )
                        .await
                });
                // 将 agent 名与句柄配对：panic 时仍能归属到真实 agent（而非固定 __panic__ 键）。
                join.push((task_name, handle));
            }

            for (name, handle) in join {
                match handle.await {
                    Ok(res) => {
                        let status = if res.exit_code == 0 {
                            AgentStatus::Completed
                        } else {
                            AgentStatus::Failed
                        };
                        let _ = self
                            .state
                            .update_agent(
                                &name,
                                AgentStateUpdate {
                                    status: Some(status),
                                    completed_at: Some(now_ms()),
                                    error: Some(res.error.clone()),
                                    ..Default::default()
                                },
                            )
                            .await;
                        let _ = self
                            .state
                            .append_log(
                                &name,
                                &format!(
                                    "Iteration {} {:?}{}",
                                    iteration + 1,
                                    status,
                                    res.error
                                        .as_ref()
                                        .map(|e| format!(": {e}"))
                                        .unwrap_or_default()
                                ),
                            )
                            .await;
                        results.insert(name, res);
                    }
                    Err(join_err) => {
                        // 子任务 panic：归属到真实 agent 名，避免同波多 panic 在固定键上互相覆盖。
                        let msg = format!("agent task panicked: {join_err}");
                        let _ = self
                            .state
                            .update_agent(
                                &name,
                                AgentStateUpdate {
                                    status: Some(AgentStatus::Failed),
                                    completed_at: Some(now_ms()),
                                    error: Some(Some(msg.clone())),
                                    ..Default::default()
                                },
                            )
                            .await;
                        results.insert(
                            name,
                            SwarmAgentResult {
                                exit_code: 1,
                                output: String::new(),
                                error: Some(msg),
                            },
                        );
                    }
                }
            }
        }

        results
    }

    async fn emit_progress(
        &self,
        iteration: usize,
        wave_idx: usize,
        on_progress: Option<&ProgressFn>,
    ) {
        if on_progress.is_none() {
            return;
        }
        let state = self.state.snapshot().await;
        let agents = state
            .agents
            .iter()
            .map(|(k, v)| (k.clone(), v.status))
            .collect();
        let _progress = PipelineProgress {
            iteration,
            target_count: state.target_count,
            current_wave: wave_idx,
            total_waves: self.waves.len(),
            agents,
        };
        // 进度回调仅做通知；详细渲染见 render 模块（由调用方拉取 snapshot）。
        if let Some(f) = on_progress {
            f(
                "__wave__",
                &format!("iteration={iteration} wave={wave_idx}"),
            );
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::{build_dependency_graph, build_execution_waves};
    use crate::executor::{SwarmAgentResult, SwarmAgentRunner};
    use crate::schema::{SwarmAgent, SwarmDefinition, SwarmMode};
    use crate::state::{PipelineStatus, StateTracker};
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// 记录峰值并发的 mock runner：进入时自增、退出时自减，并追踪历史峰值。
    struct CountingRunner {
        current: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SwarmAgentRunner for CountingRunner {
        async fn run(
            &self,
            _agent: &SwarmAgent,
            _task: &str,
            _model_override: Option<&agent_core::Model>,
            _cancel: &CancellationToken,
            _on_progress: Option<&crate::executor::ProgressFn>,
        ) -> SwarmAgentResult {
            let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            let mut p = self.peak.load(Ordering::SeqCst);
            while now > p {
                match self
                    .peak
                    .compare_exchange(p, now, Ordering::SeqCst, Ordering::SeqCst)
                {
                    Ok(_) => break,
                    Err(actual) => p = actual,
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
            self.current.fetch_sub(1, Ordering::SeqCst);
            SwarmAgentResult::default()
        }
    }

    fn parallel_def(n: usize) -> SwarmDefinition {
        let mut agents = BTreeMap::new();
        let mut order = Vec::new();
        for i in 0..n {
            let name = format!("a{i}");
            order.push(name.clone());
            agents.insert(
                name.clone(),
                SwarmAgent {
                    name,
                    role: "r".into(),
                    task: "t".into(),
                    extra_context: None,
                    reports_to: vec![],
                    waits_for: vec![],
                    model: None,
                },
            );
        }
        SwarmDefinition {
            name: "captest".into(),
            workspace: ".".into(),
            mode: SwarmMode::Parallel,
            target_count: 1,
            model: None,
            agents,
            agent_order: order,
        }
    }

    #[tokio::test]
    async fn pipeline_caps_wave_concurrency() {
        let def = parallel_def(6);
        let deps = build_dependency_graph(&def);
        let waves = build_execution_waves(&deps).unwrap();
        // parallel 模式 → 单波 6 个代理
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].len(), 6);

        let dir = tempfile::tempdir().unwrap();
        let tracker = Arc::new(StateTracker::new(dir.path(), "captest"));
        tracker
            .init(&def.agent_order, def.target_count, "parallel")
            .await
            .unwrap();

        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let runner: Arc<dyn SwarmAgentRunner> = Arc::new(CountingRunner {
            current: Arc::clone(&current),
            peak: Arc::clone(&peak),
        });

        let controller = PipelineController::new(def, waves, Arc::clone(&tracker));
        let result = controller
            .run(
                &runner,
                PipelineOptions {
                    workspace: Arc::from(dir.path()),
                    cancel: CancellationToken::new(),
                    model_override: None,
                    on_progress: None,
                    max_concurrent: 2,
                },
            )
            .await;

        assert_eq!(result.status, PipelineStatus::Completed);
        let observed = peak.load(Ordering::SeqCst);
        assert!(
            observed <= 2,
            "峰值并发 {observed} 超过护栏 max_concurrent=2（护栏失效）"
        );
        assert!(
            observed >= 2,
            "峰值并发 {observed} 未达到护栏 2（未实际并发）"
        );
    }
}
