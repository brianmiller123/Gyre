//! # agent-acp
//!
//! **Agent Client Protocol (ACP)** 服务端适配器：在不改动核心循环的前提下，为 ACP 兼容
//! 客户端（Zed Editor / 自定义前端）提供标准化 JSON-RPC + SSE/stdio 接口。
//!
//! 复用 [`agent_server::SessionManager`] 的会话管理与审批机制，事件流经 [`adapter`] 从
//! [`agent_server::ServerFrame`] 转换为 ACP 事件。ACP 与 Web 前端、CLI 三者对同一会话
//! 完全等价（共享同一份 `broadcast::Sender<ServerFrame>`）。
//!
//! ## 模块
//!
//! - [`types`] —— JSON-RPC 2.0 + 标准 ACP `session/update` 线协议类型
//! - [`adapter`] —— `ServerFrame` → `SessionUpdate` 转换器
//! - [`rpc`] —— `initialize` / `session/new` / `session/prompt` / `session/cancel` 分发
//! - [`http`] —— HTTP + SSE 传输层
//! - [`stdio`] —— stdio 传输层（编辑器集成）
//!
//! ## 集成
//!
//! ```ignore
//! // 挂载到 agent-server 的 Router（HTTP+SSE，与 Web 前端同端口）
//! let app = agent_server::app(state);
//! // acp_routes() 已在 app() 内 merge
//! ```
//!
//! ```ignore
//! // 纯 stdio 模式（编辑器作为子进程调用）
//! agent_acp::run_stdio(state).await?;
//! ```

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

mod adapter;
mod http;
mod rpc;
mod stdio;
mod types;

pub use http::acp_routes;
pub use stdio::run_stdio;
pub use types::{
    AcpError, JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse, RpcError,
    SessionNotification, SessionNotificationParams, SessionUpdate, TextContent,
};
