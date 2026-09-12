//! 工具白名单过滤注册表（H18）：把任意 [`ToolRegistry`] 收窄为「只暴露允许的工具名」。
//!
//! 用途：task agent 定义（`.agent/agents/*.md` 的 `tools:`）需要给子 Agent 一个**受限工具面**
//! ——只读代理不应拿到 `write_file`/`run_command`，而不是「注册了但靠审批拦住」。
//! 与审批门禁的区别：白名单是**能力面裁剪**（工具对模型不可见、无法被调用），审批是
//! 运行期逐次判定；两者互补（omp `read-only-policy` 同思路）。
//!
//! 动态源同样被过滤（MCP 等实时增删的工具也按名裁剪）。

use std::collections::HashSet;
use std::sync::Arc;

use agent_core::ToolSpec;

use crate::{Tool, ToolRegistry};

/// 白名单过滤包装。
pub struct FilteredRegistry {
    inner: Arc<dyn ToolRegistry>,
    allowed: Arc<HashSet<String>>,
}

impl FilteredRegistry {
    /// 构造：仅暴露 `allowed` 中的工具名。
    #[must_use]
    pub fn new(inner: Arc<dyn ToolRegistry>, allowed: HashSet<String>) -> Self {
        Self {
            inner,
            allowed: Arc::new(allowed),
        }
    }

    /// 允许的工具名集合。
    #[must_use]
    pub fn allowed(&self) -> &HashSet<String> {
        &self.allowed
    }
}

impl ToolRegistry for FilteredRegistry {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner
            .specs()
            .into_iter()
            .filter(|s| self.allowed.contains(&s.name))
            .collect()
    }

    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if !self.allowed.contains(name) {
            return None;
        }
        self.inner.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct Dummy(&'static str);

    #[async_trait]
    impl Tool for Dummy {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "dummy"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn capability(&self) -> agent_core::CapabilityTier {
            agent_core::CapabilityTier::ReadOnly
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &crate::ToolContext<'_>,
        ) -> Result<agent_core::ToolResult, agent_core::ToolError> {
            Ok(agent_core::ToolResult::text("ok"))
        }
    }

    #[test]
    fn filters_specs_and_lookup() {
        let inner: Arc<dyn ToolRegistry> = Arc::new(
            crate::DefaultToolRegistry::new()
                .with(Box::new(Dummy("read_file")))
                .with(Box::new(Dummy("write_file")))
                .with(Box::new(Dummy("grep"))),
        );
        let filtered = FilteredRegistry::new(
            Arc::clone(&inner),
            ["read_file".to_string(), "grep".to_string()]
                .into_iter()
                .collect(),
        );
        let names: Vec<String> = filtered.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["read_file".to_string(), "grep".to_string()]);
        assert!(filtered.get("grep").is_some());
        // 未在名单内 → 对模型完全不可见（get 也为 None，不只是从 specs 隐藏）。
        assert!(filtered.get("write_file").is_none());
        assert!(inner.get("write_file").is_some(), "内层不受影响");
        assert_eq!(filtered.allowed().len(), 2);
    }
}
