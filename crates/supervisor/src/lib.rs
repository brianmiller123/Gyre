//! # agent-supervisor
//!
//! 子 Agent 监控总线：进程内状态注册表 + 广播事件流。
//!
//! 解耦：本 crate 仅依赖 `agent_core`（`Usage` 等契约）+ `serde` + `tokio`，
//! 不依赖任何 Provider/Tool/IO 实现。它是「子 Agent 可观测性」的单一事实来源：
//!
//! - `TaskTool` 把子 Agent 的 `AgentEvent` 翻译为 `Supervisor` 调用
//!   （`spawn` / `set_phase` / `record_*` / `finish` / `log`）。
//! - `agent_server` 订阅事件、聚合为 `ServerFrame::SubAgents` 下发浏览器，并提供 REST 快照。
//! - `agent_cli` 直读 `Supervisor::snapshot` 渲染终端备用屏仪表盘。
//!
//! 三者共享同一份 `Arc` 状态（`Supervisor` 廉价克隆），无需额外接线。
//!
//! 另含进程托管（`process`）：hub 工具 launch 面（start/ps/logs/send/stop/
//! restart/describe）的进程内最小实现——拉起/就绪等待/日志环形缓冲/进程组
//! 信号/自动重启看护。无 broker 子进程、无磁盘持久化（有意偏差见模块文档）。

#![deny(unsafe_code)]

mod model;
mod process;
mod registry;

pub use model::{LogLevel, LogLine, SubAgentPhase, SubAgentStatus};
pub use process::{
    LogPage, LogStream, ProcessError, ProcessInfo, ProcessLogLine, ProcessManager, ProcessSpec,
    ProcessState, ProcessStatus, ReadySpec, RestartPolicy, Signal,
};
pub use registry::{Supervisor, SupervisorEvent};
