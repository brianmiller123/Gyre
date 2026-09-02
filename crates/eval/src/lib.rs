//! # agent-eval
//!
//! Python / JavaScript 双语言持久内核（`NDJSON` 协议）+ 127.0.0.1 环回桥 + `eval` 工具。
//!
//! 架构三件套：
//!
//! - [`EvalManager`]：会话级内核池。每个 `session_key` 按语言各对应一个内核进程
//!   （py → `python3 -u -c <driver>`；js → `node --input-type=module -e <driver>`，
//!   同一会话两种语言内核独立、均共享命名空间）；同一会话并发 `execute` 串行化；
//!   `keep=false` / 空闲超时（后台清扫）/ 取消（abort，两种语言一并回收）三种路径回收
//!   内核。环回桥 token 按 run 轮换注册与注销。
//! - [`BridgeServer`]：仅监听 127.0.0.1 随机端口的 axum 环回桥（语言无关）。内核内
//!   `tool` 代理回调经 bearer token POST /call 调用宿主工具（token 无效 → 401），在
//!   run 的 `cancel.child_token()` 下构造 [`ToolContext`](agent_tools::ToolContext) 执行。
//! - [`EvalTool`]：向 LLM 暴露 `eval` 工具（Execute 级、Exclusive、可中断），`language`
//!   入参选 `"py"`（默认）或 `"js"`（需 node），内部调用 [`EvalManager::execute`]，
//!   输出拼为 [`ToolResult::text`](agent_core::ToolResult)。
//!
//! 内核驱动源码见 [`PYTHON_DRIVER`] / [`JS_DRIVER`]（内嵌 const，分别经
//! `python3 -u -c` 与 `node --input-type=module -e` 传入）。

#![deny(unsafe_code)]

mod bridge;
mod driver;
mod driver_js;
mod error;
mod manager;
mod tool;

pub use bridge::{BridgeServer, RunGuard};
pub use driver::PYTHON_DRIVER;
pub use driver_js::JS_DRIVER;
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
    pub struct NoopApproval;

    #[async_trait::async_trait]
    impl ApprovalPolicy for NoopApproval {
        fn decide(&self, _request: &ApprovalRequest<'_>) -> ApprovalDecision {
            ApprovalDecision::Allow
        }

        async fn prompt(&self, _ask: &AskMessage) -> Result<AskResponse, ToolError> {
            Ok(AskResponse::Yes)
        }
    }

    /// 测试用回显工具：原样返回输入 JSON（验证内核内 tool 回调链路）。
    pub struct EchoTool;

    #[async_trait::async_trait]
    impl agent_tools::Tool for EchoTool {
        fn name(&self) -> &'static str {
            "echo"
        }

        fn description(&self) -> &'static str {
            "测试回显工具"
        }

        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn capability(&self) -> agent_core::CapabilityTier {
            agent_core::CapabilityTier::ReadOnly
        }

        async fn execute(
            &self,
            input: serde_json::Value,
            _ctx: &agent_tools::ToolContext<'_>,
        ) -> Result<agent_core::ToolResult, agent_core::ToolError> {
            Ok(agent_core::ToolResult::text(input.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::test_support::NoopApproval;
    use agent_core::Workspace;
    use agent_tools::DefaultToolRegistry;

    /// 探测可用的 python3（缺失则返回 `None`，测试跳过）。
    fn find_python3() -> Option<String> {
        let probe = std::process::Command::new("python3")
            .arg("--version")
            .output()
            .ok()?;
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
            node: "node".into(),
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
            out.result.as_deref().is_some_and(|r| r.contains('2')),
            "result 应含 2：{out:?}"
        );
        // print 输出走 stdout 帧；同一会话共享命名空间（上一 run 已 keep=false 回收，故为全新内核）。
        let out2 = manager
            .execute(key, "py", "print(1+1)", false, &bridge)
            .await
            .expect("执行失败");
        assert!(out2.stdout.contains('2'), "stdout 应含 2：{out2:?}");
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
            out5.error
                .as_deref()
                .is_some_and(|m| m.contains("ValueError")),
            "error 应含 ValueError：{out5:?}"
        );
    }

    /// 不支持的语言直接报错。
    #[tokio::test]
    async fn unsupported_language_rejected() {
        let manager = Arc::new(EvalManager::new(EvalSettings {
            python: "python3".into(),
            node: "node".into(),
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
        match manager.execute("u|s", "rb", "1+1", false, &bridge).await {
            Err(EvalError::UnsupportedLanguage(lang)) => assert_eq!(lang, "rb"),
            other => panic!("应返回 UnsupportedLanguage：{other:?}"),
        }
    }

    /// 探测可用的 node（缺失则返回 `None`，测试跳过；需 ≥18 自带 fetch）。
    fn find_node() -> Option<String> {
        let probe = std::process::Command::new("node")
            .arg("--version")
            .output()
            .ok()?;
        probe.status.success().then(|| "node".to_string())
    }

    /// 集成测试：真实 node 内核 + 环回桥（含 echo 工具回调）。
    /// 末表达式完成值、console 捕获、共享命名空间、async IIFE、错误栈、桥回调全覆盖。
    #[tokio::test]
    async fn js_kernel_executes_code_through_bridge() {
        if find_node().is_none() {
            return;
        }
        let manager = Arc::new(EvalManager::new(EvalSettings {
            python: "python3".into(),
            node: "node".into(),
            idle_timeout: Duration::from_secs(60),
        }));
        manager.ensure_sweeper();
        let registry = DefaultToolRegistry::new().with(Box::new(crate::test_support::EchoTool));
        let bridge = BridgeServer::spawn(
            Arc::new(registry),
            Arc::new(Workspace::current_dir()),
            Arc::new(NoopApproval),
        )
        .await
        .expect("环回桥启动失败");
        let key = "integration|sess";
        // 末表达式完成值（REPL 语义）+ 声明共享命名空间。
        let out = manager
            .execute(key, "js", "var k = 40; k + 2", true, &bridge)
            .await
            .expect("执行失败");
        assert!(out.error.is_none(), "不应有错误：{out:?}");
        assert_eq!(out.result.as_deref(), Some("42"), "result 应为 42：{out:?}");
        // console 捕获：log → stdout，error → stderr。
        let out2 = manager
            .execute(
                key,
                "js",
                "console.log('hello'); console.error('bad'); k + 1",
                false,
                &bridge,
            )
            .await
            .expect("执行失败");
        assert!(out2.stdout.contains("hello"), "stdout 应含 hello：{out2:?}");
        assert!(out2.stderr.contains("bad"), "stderr 应含 bad：{out2:?}");
        assert_eq!(
            out2.result.as_deref(),
            Some("41"),
            "命名空间应保留 k=40：{out2:?}"
        );
        // 顶层 await（async IIFE 包裹，显式 return）+ 桥回调宿主工具。
        let out3 = manager
            .execute(
                key,
                "js",
                "const r = await tool.call('echo', {value: 'ok'}); return r.toUpperCase()",
                false,
                &bridge,
            )
            .await
            .expect("执行失败");
        assert!(out3.error.is_none(), "桥回调不应报错：{out3:?}");
        assert!(
            out3.result.as_deref().is_some_and(|r| r.contains("OK")),
            "桥回调 echo 结果应含 OK：{out3:?}"
        );
        // 用户代码异常经 EvalOutput::error 返回（含堆栈文本）。
        let out4 = manager
            .execute(key, "js", "throw new Error('boom')", false, &bridge)
            .await
            .expect("子系统不应报错");
        assert!(
            out4.error
                .as_deref()
                .is_some_and(|m| m.contains("Error: boom")),
            "error 应含 Error: boom：{out4:?}"
        );
    }

    /// 语言别名归一：python/javascript 与 py/js 等价。
    #[tokio::test]
    async fn language_aliases_accepted() {
        let (Some(python), Some(node)) = (find_python3(), find_node()) else {
            return;
        };
        let manager = Arc::new(EvalManager::new(EvalSettings {
            python,
            node,
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
        let out = manager
            .execute("alias|s", "python", "2 * 21", false, &bridge)
            .await
            .expect("别名 python 应可用");
        assert!(out.result.as_deref().is_some_and(|r| r.contains("42")));
        let out2 = manager
            .execute("alias|s", "javascript", "2 * 21", false, &bridge)
            .await
            .expect("别名 javascript 应可用");
        assert!(out2.result.as_deref().is_some_and(|r| r.contains("42")));
    }
}
