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

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

mod model;
mod registry;

pub use model::{LogLevel, LogLine, SubAgentPhase, SubAgentStatus};
pub use registry::{Supervisor, SupervisorEvent};
