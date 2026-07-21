//! DAG 操作：从 `waits_for` / `reports_to` 构建依赖图、Kahn 拓扑检测环、产出执行波。
//!
//! 移植自 [`oh-my-pi swarm-extension/dag.ts`](../../../third/oh-my-pi/packages/swarm-extension/src/swarm/dag.ts)。
//!
//! 依赖来源：
//! 1. 显式 `waits_for`
//! 2. `reports_to` 的隐含反向（A reports_to B ⇒ B 依赖 A）
//! 3. pipeline/sequential 且无显式依赖时：按 YAML 声明顺序串成链

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use crate::schema::{SwarmDefinition, SwarmMode};

/// 依赖图：`agent → 它依赖的代理集合`（集合元素须先于 agent 完成）。
pub type DependencyGraph = BTreeMap<String, BTreeSet<String>>;

/// 构建依赖图。
#[must_use]
pub fn build_dependency_graph(def: &SwarmDefinition) -> DependencyGraph {
    let mut deps: DependencyGraph = def
        .agents
        .keys()
        .map(|k| (k.clone(), BTreeSet::new()))
        .collect();

    // 显式 waits_for
    for agent in def.agents.values() {
        let to_insert: Vec<String> = agent
            .waits_for
            .iter()
            .filter(|dep| deps.contains_key(*dep))
            .cloned()
            .collect();
        if let Some(entry) = deps.get_mut(&agent.name) {
            for dep in to_insert {
                entry.insert(dep);
            }
        }
    }

    // reports_to 反向：被汇报者依赖汇报者
    for agent in def.agents.values() {
        for target in &agent.reports_to {
            if let Some(entry) = deps.get_mut(target) {
                entry.insert(agent.name.clone());
            }
        }
    }

    // pipeline/sequential 无显式依赖时，按声明顺序串链
    if matches!(def.mode, SwarmMode::Pipeline | SwarmMode::Sequential) && !has_explicit_deps(&deps)
    {
        for i in 1..def.agent_order.len() {
            if let Some(entry) = deps.get_mut(&def.agent_order[i]) {
                entry.insert(def.agent_order[i - 1].clone());
            }
        }
    }

    deps
}

fn has_explicit_deps(deps: &DependencyGraph) -> bool {
    deps.values().any(|s| !s.is_empty())
}

/// Kahn 算法检测环；返回成环节点列表（无环返回 `None`）。
#[must_use]
pub fn detect_cycles(deps: &DependencyGraph) -> Option<Vec<String>> {
    // in-degree = 节点的依赖数；forward[dep] = 依赖 dep 的节点列表
    let mut in_degree: BTreeMap<&str, usize> = deps.keys().map(|k| (k.as_str(), 0usize)).collect();
    let mut forward: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

    for (node, node_deps) in deps {
        if let Some(d) = in_degree.get_mut(node.as_str()) {
            *d += node_deps.len();
        }
        for dep in node_deps {
            forward.entry(dep.as_str()).or_default().push(node.as_str());
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| *n)
        .collect();

    let mut sorted = HashSet::new();
    while let Some(node) = queue.pop_front() {
        sorted.insert(node);
        for &dependent in forward.get(node).into_iter().flatten() {
            if let Some(d) = in_degree.get_mut(dependent) {
                *d = d.saturating_sub(1);
                if *d == 0 {
                    queue.push_back(dependent);
                }
            }
        }
    }

    if sorted.len() < deps.len() {
        let cyclic: Vec<String> = deps
            .keys()
            .filter(|k| !sorted.contains(k.as_str()))
            .cloned()
            .collect();
        Some(cyclic)
    } else {
        None
    }
}

/// 由依赖图产出执行波（同波内可并行，波间串行）。
///
/// # Errors
/// 无法推进（说明 `detect_cycles` 漏检的环）时返回错误。
pub fn build_execution_waves(deps: &DependencyGraph) -> Result<Vec<Vec<String>>, String> {
    let mut waves = Vec::new();
    let mut completed: BTreeSet<String> = BTreeSet::new();
    let mut remaining: BTreeSet<String> = deps.keys().cloned().collect();

    while !remaining.is_empty() {
        let mut wave: Vec<String> = remaining
            .iter()
            .filter(|node| {
                deps.get(*node)
                    .is_some_and(|node_deps| node_deps.iter().all(|d| completed.contains(d)))
            })
            .cloned()
            .collect();

        if wave.is_empty() {
            return Err(format!(
                "Deadlock: agents [{}] cannot make progress — undetected cycle",
                remaining.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }

        wave.sort();
        for node in &wave {
            remaining.remove(node);
            completed.insert(node.clone());
        }
        waves.push(wave);
    }

    Ok(waves)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SwarmAgent;

    fn def(mode: SwarmMode, agents: Vec<(&str, Vec<&str>)>) -> SwarmDefinition {
        let mut map = BTreeMap::new();
        let mut order = Vec::new();
        for (name, waits) in agents {
            order.push(name.to_string());
            map.insert(
                name.to_string(),
                SwarmAgent {
                    name: name.to_string(),
                    role: "r".into(),
                    task: "t".into(),
                    extra_context: None,
                    reports_to: vec![],
                    waits_for: waits.iter().map(|s| s.to_string()).collect(),
                    model: None,
                },
            );
        }
        SwarmDefinition {
            name: "t".into(),
            workspace: ".".into(),
            mode,
            target_count: 1,
            model: None,
            agents: map,
            agent_order: order,
        }
    }

    #[test]
    fn parallel_single_wave() {
        let d = def(SwarmMode::Parallel, vec![("a", vec![]), ("b", vec![])]);
        let g = build_dependency_graph(&d);
        let waves = build_execution_waves(&g).unwrap();
        assert_eq!(waves, vec![vec!["a".to_string(), "b".to_string()]]);
    }

    #[test]
    fn sequential_chain_two_waves() {
        let d = def(SwarmMode::Sequential, vec![("a", vec![]), ("b", vec![])]);
        let g = build_dependency_graph(&d);
        let waves = build_execution_waves(&g).unwrap();
        assert_eq!(waves, vec![vec!["a".to_string()], vec!["b".to_string()]]);
    }

    #[test]
    fn detects_cycle() {
        let d = def(
            SwarmMode::Parallel,
            vec![("a", vec!["b"]), ("b", vec!["a"])],
        );
        let g = build_dependency_graph(&d);
        assert!(detect_cycles(&g).is_some());
    }
}
