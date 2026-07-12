//! # agent-swarm
//!
//! Swarm 多代理编排：YAML 定义 → DAG 拓扑波 → 并行子 Agent 编排。
//!
//! 移植自 [`oh-my-pi swarm-extension`](../../../third/oh-my-pi/packages/swarm-extension)。
//!
//! 流水线：
//! 1. [`schema::parse_swarm_yaml`] + [`schema::validate_swarm_definition`] —— 解析校验
//! 2. [`dag::build_dependency_graph`] + [`dag::detect_cycles`] + [`dag::build_execution_waves`] —— 拓扑成波
//! 3. [`state::StateTracker`] —— 文件系统状态持久化（`.swarm_<name>/`，可恢复）
//! 4. [`pipeline::PipelineController`] —— 迭代 × 波编排，波内并行
//! 5. [`executor::AgentSwarmRunner`] —— 每代理一个子 Agent（独立上下文）
//! 6. [`render::render_swarm_progress`] —— TUI 进度渲染
//!
//! 典型用法见 [`run_swarm`]。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

pub mod dag;
pub mod executor;
pub mod pipeline;
pub mod render;
pub mod schema;
pub mod state;

pub use dag::{build_dependency_graph, build_execution_waves, detect_cycles, DependencyGraph};
pub use executor::{
    build_role_prompt, AgentSwarmRunner, ContextFactory, ProgressFn, SwarmAgentResult, SwarmAgentRunner,
};
pub use pipeline::{PipelineController, PipelineOptions, PipelineProgress, PipelineResult};
pub use render::render_swarm_progress;
pub use schema::{parse_swarm_yaml, validate_swarm_definition, SwarmAgent, SwarmDefinition, SwarmMode};
pub use state::{
    AgentState, AgentStateUpdate, AgentStatus, PipelineStateUpdate, PipelineStatus, StateTracker,
    SwarmState,
};

use std::path::Path;
use std::sync::Arc;

use agent_core::Model;
use tokio_util::sync::CancellationToken;

/// 解析 + 校验 + 成波。返回 `(定义, 波)`，或首个错误。
///
/// # Errors
/// YAML 解析、语义校验、环检测、成波失败时返回错误信息。
pub fn plan_swarm(yaml: &str) -> Result<(SwarmDefinition, Vec<Vec<String>>), String> {
    let def = parse_swarm_yaml(yaml)?;
    let errs = validate_swarm_definition(&def);
    if !errs.is_empty() {
        return Err(errs.join("; "));
    }
    let deps = build_dependency_graph(&def);
    if let Some(cyclic) = detect_cycles(&deps) {
        return Err(format!("swarm 定义存在环依赖: {}", cyclic.join(", ")));
    }
    let waves = build_execution_waves(&deps)?;
    Ok((def, waves))
}

/// 运行 swarm 的选项。
pub struct SwarmRunOptions {
    /// 工作区根。
    pub workspace: Arc<Path>,
    /// 取消令牌。
    pub cancel: CancellationToken,
    /// 模型覆盖。
    pub model_override: Option<Model>,
    /// 进度回调。
    pub on_progress: Option<ProgressFn>,
    /// 同波并发护栏（≥1）；取自 `[subagent].max_concurrent`。
    pub max_concurrent: usize,
}

/// 顶层入口：解析 YAML → 校验 → 成波 → 初始化状态 → 运行流水线。
///
/// # Errors
/// 任何阶段失败时返回错误信息；状态已落盘便于恢复。
pub async fn run_swarm(
    yaml: &str,
    runner: &Arc<dyn SwarmAgentRunner>,
    options: SwarmRunOptions,
) -> Result<PipelineResult, String> {
    let (def, waves) = plan_swarm(yaml)?;

    let tracker = Arc::new(StateTracker::new(&options.workspace, &def.name));
    tracker
        .init(
            &def.agent_order,
            def.target_count,
            &format!("{:?}", def.mode).to_lowercase(),
        )
        .await
        .map_err(|e| e.to_string())?;

    let controller = PipelineController::new(def, waves, Arc::clone(&tracker));
    let result = controller
        .run(
            runner,
            PipelineOptions {
                workspace: options.workspace,
                cancel: options.cancel,
                model_override: options.model_override,
                on_progress: options.on_progress,
                max_concurrent: options.max_concurrent.max(1),
            },
        )
        .await;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = "\
swarm:
  name: demo
  workspace: .
  agents:
    a:
      role: R
      task: do a
    b:
      role: R
      task: do b
      waits_for: [a]
";

    #[test]
    fn plan_produces_two_waves() {
        let (def, waves) = plan_swarm(SIMPLE).unwrap();
        assert_eq!(def.agent_order, vec!["a", "b"]);
        // b waits_for a ⇒ a 在波 0，b 在波 1（无显式依赖时 sequential 也会串链）
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[0], vec!["a".to_string()]);
        assert_eq!(waves[1], vec!["b".to_string()]);
    }

    #[test]
    fn plan_rejects_cycle() {
        let cyclic = "\
swarm:
  name: demo
  workspace: .
  agents:
    a:
      role: R
      task: t
      waits_for: [b]
    b:
      role: R
      task: t
      waits_for: [a]
";
        assert!(plan_swarm(cyclic).is_err());
    }
}
