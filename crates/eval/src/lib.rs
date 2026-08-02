//! # agent-eval
//!
//! Python 持久内核（`NDJSON` 协议）+ 127.0.0.1 环回桥 + `eval` 工具。
//!
//! 架构三件套：
//!
//! - [`EvalManager`]：会话级内核池。每个 `session_key` 对应一个 `python3 -u -c <driver>`
//!   内核进程（共享命名空间）；同一会话并发 `execute` 串行化；`keep=false` / 空闲超时
//!   （后台清扫）/ 取消（abort）三种路径回收内核。环回桥 token 按 run 轮换注册与注销。
//! - [`BridgeServer`]：仅监听 127.0.0.1 随机端口的 axum 环回桥。内核内 `tool.<name>`
//!   回调经 bearer token POST /call 调用宿主工具（token 无效 → 401），在 run 的
//!   `cancel.child_token()` 下构造 [`ToolContext`](agent_tools::ToolContext) 执行。
//! - [`EvalTool`]：向 LLM 暴露 `eval` 工具（Execute 级、Exclusive、可中断），内部调用
//!   [`EvalManager::execute`]，输出拼为 [`ToolResult::text`](agent_core::ToolResult)。
//!
//! 内核驱动源码见 [`PYTHON_DRIVER`]（内嵌 const，经 `python3 -u -c` 传入）。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

mod bridge;
mod driver;
mod error;
mod manager;
mod tool;

pub use bridge::{BridgeServer, RunGuard};
pub use driver::PYTHON_DRIVER;
pub use error::EvalError;
pub use manager::{EvalManager, EvalOutput, EvalSettings};
pub use tool::EvalTool;

/// 测试共享支持（测试用审批策略等）。
#[cfg(test)]
pub(crate) mod test_support {
    use agent_core::{
        ApprovalDecision, ApprovalPolicy, ApprovalRequest, AskMessage, AskResponse, ToolError,
    };

    /// 测试用审批策略：一律放行。
    pub(crate) struct NoopApproval;

    #[async_trait::async_trait]
    impl ApprovalPolicy for NoopApproval {
        fn decide(&self, _request: &ApprovalRequest<'_>) -> ApprovalDecision {
            ApprovalDecision::Allow
        }

        async fn prompt(&self, _ask: &AskMessage) -> Result<AskResponse, ToolError> {
            Ok(AskResponse::Yes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use agent_core::Workspace;
    use agent_tools::DefaultToolRegistry;
    use crate::test_support::NoopApproval;

    /// 探测可用的 python3（缺失则返回 `None`，测试跳过）。
    fn find_python3() -> Option<String> {
        let probe = std::process::Command::new("python3").arg("--version").output().ok()?;
        probe.status.success().then(|| "python3".to_string())
    }

    /// 集成测试：真实 python3 内核 + 环回桥（空 registry）。
    /// `execute("1+1")` 断言 result 含 "2"；`print(1+1)` 断言 stdout 含 "2"。
    #[tokio::test]
    async fn kernel_executes_code_through_bridge() {
        let Some(python) = find_python3() else {
            return;
        };
        let manager = Arc::new(EvalManager::new(EvalSettings {
            python,
            idle_timeout: Duration::from_secs(60),
        }));
        manager.ensure_sweeper();
        let bridge = BridgeServer::spawn(
            Arc::new(DefaultToolRegistry::new()),
            Arc::new(Workspace::current_dir()),
            Arc::new(NoopApproval),
        )
        .await
        .expect("环回桥启动失败");
        let key = "integration|sess";
        let out = manager
            .execute(key, "py", "1+1", false, &bridge)
            .await
            .expect("执行失败");
        assert!(out.error.is_none(), "不应有错误：{out:?}");
        assert!(
            out.result.as_deref().is_some_and(|r| r.contains("2")),
            "result 应含 2：{out:?}"
        );
        // print 输出走 stdout 帧；同一会话共享命名空间（上一 run 已 keep=false 回收，故为全新内核）。
        let out2 = manager
            .execute(key, "py", "print(1+1)", false, &bridge)
            .await
            .expect("执行失败");
        assert!(
            out2.stdout.contains("2"),
            "stdout 应含 2：{out2:?}"
        );
        // keep=true 保留命名空间：跨 run 共享变量。
        let out3 = manager
            .execute(key, "py", "k = 40\nk + 2", true, &bridge)
            .await
            .expect("执行失败");
        assert!(
            out3.result.as_deref().is_some_and(|r| r.contains("42")),
            "result 应含 42：{out3:?}"
        );
        let out4 = manager
            .execute(key, "py", "k + 1", false, &bridge)
            .await
            .expect("执行失败");
        assert!(
            out4.result.as_deref().is_some_and(|r| r.contains("41")),
            "共享命名空间应保留 k=40：{out4:?}"
        );
        // 用户代码异常经 EvalOutput::error 返回（非子系统错误）。
        let out5 = manager
            .execute(key, "py", "raise ValueError('boom')", false, &bridge)
            .await
            .expect("子系统不应报错");
        assert!(
            out5.error.as_deref().is_some_and(|m| m.contains("ValueError")),
            "error 应含 ValueError：{out5:?}"
        );
    }

    /// 不支持的语言直接报错。
    #[tokio::test]
    async fn unsupported_language_rejected() {
        let manager = Arc::new(EvalManager::new(EvalSettings {
            python: "python3".into(),
            idle_timeout: Duration::from_secs(60),
        }));
        manager.ensure_sweeper();
        let bridge = BridgeServer::spawn(
            Arc::new(DefaultToolRegistry::new()),
            Arc::new(Workspace::current_dir()),
            Arc::new(NoopApproval),
        )
        .await
        .expect("环回桥启动失败");
        let err = manager
            .execute("u|s", "js", "1+1", false, &bridge)
            .await
            .expect_err("应返回 UnsupportedLanguage");
        assert!(matches!(err, EvalError::UnsupportedLanguage(ref l) if l == "js"));
    }
}
