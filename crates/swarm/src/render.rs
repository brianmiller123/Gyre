//! Swarm 进度渲染：把 [`SwarmState`] 渲染为多行文本（对标 oh-my-pi `renderSwarmProgress`）。
//!
//! 移植自 [`oh-my-pi swarm-extension/render.ts`](../../../third/oh-my-pi/packages/swarm-extension/src/swarm/render.ts)。

use crate::state::{AgentStatus, PipelineStatus, SwarmState};

const STATUS_LABELS: [(AgentStatus, &str); 5] = [
    (AgentStatus::Pending, "[    ]"),
    (AgentStatus::Waiting, "[wait]"),
    (AgentStatus::Running, "[....]"),
    (AgentStatus::Completed, "[done]"),
    (AgentStatus::Failed, "[FAIL]"),
];

/// 渲染 swarm 进度为多行文本。
#[must_use]
pub fn render_swarm_progress(state: &SwarmState) -> Vec<String> {
    let mut lines = Vec::new();
    let status_label = format!("{:?}", state.status).to_uppercase();
    lines.push(format!("Swarm: {} [{}]", state.name, status_label));
    lines.push(format!(
        "Mode: {} | Iteration: {}/{}",
        state.mode,
        state.iteration + 1,
        state.target_count
    ));
    lines.push(String::new());

    let agents: Vec<_> = state.agents.values().collect();
    if agents.is_empty() {
        lines.push("  (no agents)".to_string());
        return lines;
    }

    let now = unix_now_ms();
    for agent in &agents {
        let icon = status_label_for(agent.status);
        let duration = format_agent_duration(*agent, now);
        let error_suffix = agent
            .error
            .as_ref()
            .map(|e| format!(" - {}", truncate(e, 60)))
            .unwrap_or_default();
        lines.push(format!(
            "  {icon} {}: {:?}{duration}{error_suffix}",
            agent.name, agent.status
        ));
    }

    lines.push(String::new());
    let total = agents.len();
    let completed = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Completed)
        .count();
    let failed = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Failed)
        .count();
    let running = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Running)
        .count();

    let mut parts = vec![format!("{completed}/{total} done")];
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    parts.push(format!(
        "elapsed: {}",
        format_duration(now.saturating_sub(state.started_at))
    ));
    lines.push(format!("  {}", parts.join(" | ")));

    // 终态额外提示
    if state.status == PipelineStatus::Completed {
        lines.push("  ✓ pipeline completed".to_string());
    } else if state.status == PipelineStatus::Failed {
        lines.push("  ✗ pipeline failed (see errors)".to_string());
    } else if state.status == PipelineStatus::Aborted {
        lines.push("  ⊘ pipeline aborted".to_string());
    }

    lines
}

fn status_label_for(status: AgentStatus) -> &'static str {
    STATUS_LABELS
        .iter()
        .find(|(s, _)| *s == status)
        .map(|(_, label)| *label)
        .unwrap_or("[????]")
}

fn format_agent_duration(agent: &crate::state::AgentState, now: u64) -> String {
    if let (Some(start), Some(end)) = (agent.started_at, agent.completed_at) {
        return format!(" ({})", format_duration(end.saturating_sub(start)));
    }
    if let Some(start) = agent.started_at {
        if matches!(agent.status, AgentStatus::Running | AgentStatus::Waiting) {
            return format!(" ({})...", format_duration(now.saturating_sub(start)));
        }
    }
    String::new()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let secs = ms / 1000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    let rem = secs % 60;
    format!("{mins}m{rem}s")
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AgentState;
    use std::collections::BTreeMap;

    #[test]
    fn renders_header_and_agents() {
        let mut agents = BTreeMap::new();
        agents.insert(
            "a".into(),
            AgentState {
                name: "a".into(),
                status: AgentStatus::Completed,
                iteration: 0,
                wave: 0,
                started_at: Some(100),
                completed_at: Some(200),
                error: None,
            },
        );
        let state = SwarmState {
            name: "demo".into(),
            status: PipelineStatus::Running,
            mode: "sequential".into(),
            iteration: 0,
            target_count: 1,
            agents,
            started_at: 50,
            completed_at: None,
        };
        let lines = render_swarm_progress(&state);
        assert!(lines[0].contains("Swarm: demo"));
        assert!(lines.iter().any(|l| l.contains("[done] a")));
    }
}
