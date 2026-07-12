//! Swarm 编排定义：YAML 解析（`swarm:`）→ 归一化 [`SwarmDefinition`] → 语义校验。
//!
//! 移植自 [`oh-my-pi swarm-extension/schema.ts`](../../../third/oh-my-pi/packages/swarm-extension/src/swarm/schema.ts)。
//! 为保留 YAML 声明顺序（隐式 pipeline 串行化依赖），解析走 `serde_yaml::Value` 而非 derive，
//! 再手动归一化为 camelCase 的 [`SwarmAgent`] / [`SwarmDefinition`]。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 执行模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SwarmMode {
    /// 流水线：按 DAG 重复 `target_count` 轮。
    Pipeline,
    /// 并行：忽略顺序，全部并行（单波）。
    Parallel,
    /// 串行（默认）：按声明顺序链式执行。
    #[default]
    Sequential,
}

/// 单个 swarm 代理（归一化后，camelCase）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmAgent {
    /// 代理名（agents map 的 key）。
    pub name: String,
    /// 角色描述（拼进 system prompt）。
    pub role: String,
    /// 任务指令（作为 user prompt）。
    pub task: String,
    /// 额外上下文（拼进 system prompt）。
    pub extra_context: Option<String>,
    /// 显式依赖：必须等待其完成的代理。
    pub reports_to: Vec<String>,
    /// 显式依赖：同 `reports_to` 语义别名（waits_for）。
    pub waits_for: Vec<String>,
    /// 模型覆盖。
    pub model: Option<String>,
}

/// Swarm 编排定义（归一化后）。
#[derive(Debug, Clone)]
pub struct SwarmDefinition {
    /// swarm 名称（仅 `[a-zA-Z0-9._-]`）。
    pub name: String,
    /// 工作区根（相对/绝对均可）。
    pub workspace: String,
    /// 执行模式。
    pub mode: SwarmMode,
    /// pipeline 模式下的重复轮数（其余模式固定 1）。
    pub target_count: usize,
    /// 全局模型覆盖。
    pub model: Option<String>,
    /// 代理表。
    pub agents: BTreeMap<String, SwarmAgent>,
    /// 保留 YAML 声明顺序（隐式串行化/波内确定性排序）。
    pub agent_order: Vec<String>,
}

const VALID_SWARM_NAME: &str = "swarm.name may only contain letters, numbers, dot, underscore, and dash";

/// 解析 swarm YAML 文本为归一化定义。
///
/// # Errors
/// 缺少顶层 `swarm`、缺必填字段、`agents` 为空或字段类型不符时返回错误信息。
pub fn parse_swarm_yaml(content: &str) -> Result<SwarmDefinition, String> {
    let root: serde_yaml::Value =
        serde_yaml::from_str(content).map_err(|e| format!("YAML 解析失败: {e}"))?;

    let swarm = root
        .get("swarm")
        .ok_or_else(|| "YAML must have a top-level 'swarm' key".to_string())?;

    let name = swarm
        .get("name")
        .and_then(serde_yaml::Value::as_str)
        .ok_or_else(|| "swarm.name is required and must be a string".to_string())?
        .to_string();
    if !is_valid_name(&name) {
        return Err(VALID_SWARM_NAME.to_string());
    }

    let workspace = swarm
        .get("workspace")
        .and_then(serde_yaml::Value::as_str)
        .ok_or_else(|| "swarm.workspace is required and must be a string".to_string())?
        .to_string();

    let raw_agents = swarm
        .get("agents")
        .and_then(|v| v.as_mapping())
        .ok_or_else(|| "swarm.agents must contain at least one agent".to_string())?;
    if raw_agents.is_empty() {
        return Err("swarm.agents must contain at least one agent".to_string());
    }

    let mode = match swarm.get("mode").and_then(serde_yaml::Value::as_str) {
        None => SwarmMode::Sequential,
        Some("pipeline") => SwarmMode::Pipeline,
        Some("parallel") => SwarmMode::Parallel,
        Some("sequential") => SwarmMode::Sequential,
        Some(other) => {
            return Err(format!(
                "Invalid mode '{other}'. Must be one of: pipeline, parallel, sequential"
            ));
        }
    };

    let target_count = swarm
        .get("target_count")
        .and_then(serde_yaml::Value::as_u64)
        .map_or(1usize, |n| n as usize);

    let global_model = swarm
        .get("model")
        .and_then(serde_yaml::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut agents = BTreeMap::new();
    let mut agent_order = Vec::new();
    for (k, v) in raw_agents {
        let name = k
            .as_str()
            .ok_or_else(|| "agent key must be a string".to_string())?
            .to_string();
        let role = v
            .get("role")
            .and_then(serde_yaml::Value::as_str)
            .ok_or_else(|| format!("Agent '{name}': 'role' is required"))?
            .to_string();
        let task = v
            .get("task")
            .and_then(serde_yaml::Value::as_str)
            .ok_or_else(|| format!("Agent '{name}': 'task' is required"))?
            .trim()
            .to_string();

        agent_order.push(name.clone());
        agents.insert(
            name.clone(),
            SwarmAgent {
                name: name.clone(),
                role,
                task,
                extra_context: v
                    .get("extra_context")
                    .and_then(serde_yaml::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                reports_to: str_list(v, "reports_to"),
                waits_for: str_list(v, "waits_for"),
                model: v
                    .get("model")
                    .and_then(serde_yaml::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            },
        );
    }

    Ok(SwarmDefinition {
        name,
        workspace,
        mode,
        target_count,
        model: global_model,
        agents,
        agent_order,
    })
}

/// 语义校验：引用合法性、自环、target_count 约束。返回错误列表（空表示通过）。
#[must_use]
pub fn validate_swarm_definition(def: &SwarmDefinition) -> Vec<String> {
    let mut errors = Vec::new();
    let names: std::collections::HashSet<&str> = def.agents.keys().map(String::as_str).collect();

    if def.target_count < 1 {
        errors.push("target_count must be at least 1".to_string());
    }
    if def.mode != SwarmMode::Pipeline && def.target_count != 1 {
        errors.push("target_count is only supported in pipeline mode".to_string());
    }

    for (name, agent) in &def.agents {
        for dep in &agent.waits_for {
            if dep == name {
                errors.push(format!("Agent '{name}' cannot wait for itself"));
            } else if !names.contains(dep.as_str()) {
                errors.push(format!("Agent '{name}' waits_for unknown agent '{dep}'"));
            }
        }
        for target in &agent.reports_to {
            if target == name {
                errors.push(format!("Agent '{name}' cannot report to itself"));
            } else if !names.contains(target.as_str()) {
                errors.push(format!("Agent '{name}' reports_to unknown agent '{target}'"));
            }
        }
    }

    errors
}

fn str_list(v: &serde_yaml::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(serde_yaml::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn is_valid_name(name: &str) -> bool {
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
swarm:
  name: build-pipeline
  workspace: ./repo
  mode: pipeline
  target_count: 2
  agents:
    planner:
      role: Planner
      task: Plan the work
      reports_to: [coder]
    coder:
      role: Coder
      task: Write code
      waits_for: [planner]
";

    #[test]
    fn parses_and_preserves_order() {
        let def = parse_swarm_yaml(SAMPLE).unwrap();
        assert_eq!(def.name, "build-pipeline");
        assert_eq!(def.mode, SwarmMode::Pipeline);
        assert_eq!(def.target_count, 2);
        assert_eq!(def.agent_order, vec!["planner", "coder"]);
        assert_eq!(def.agents["coder"].waits_for, vec!["planner"]);
        assert_eq!(def.agents["planner"].reports_to, vec!["coder"]);
    }

    #[test]
    fn rejects_missing_swarm_key() {
        let err = parse_swarm_yaml("foo: 1").unwrap_err();
        assert!(err.contains("top-level 'swarm'"));
    }

    #[test]
    fn validates_references() {
        let mut def = parse_swarm_yaml(SAMPLE).unwrap();
        def.agents.get_mut("coder").unwrap().waits_for = vec!["ghost".to_string()];
        let errs = validate_swarm_definition(&def);
        assert!(errs.iter().any(|e| e.contains("unknown agent 'ghost'")));
    }
}
