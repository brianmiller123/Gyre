//! # agent-mcp
//!
//! Model Context Protocol (MCP) 客户端：stdio + Streamable HTTP + legacy SSE 三传输 + tool bridge。
//!
//! - [`McpClient`]：连接 MCP server（子进程 stdio / HTTP 端点 / legacy SSE），JSON-RPC 2.0 通信
//!   （initialize / tools/list / tools/call / resources/* / prompts/*）
//! - [`McpTool`]：把 server 工具包装为 agent [`Tool`](agent_tools::Tool)；连接未就绪的
//!   server 以「缓存元信息 + 延迟连接槽」形态注册（对齐 omp `DeferredMCPTool`）
//! - [`McpRegistry`]：从 `[mcp.servers]` 配置加载多 server 的全部工具；启动预算
//!   （[`McpLoadOptions::startup_budget`]）外的 server 转后台连接不阻塞启动；消费
//!   server→client 通知（`tools/list_changed` 自动重拉 + 变更监听器）；每 server
//!   重连监督（指数退避 + 爆发熔断，见 [`reconnect`] 模块文档）；工具清单磁盘缓存
//!   （版本 + 配置指纹 + TTL，见 [`cache`]）；连接状态经 [`McpRegistry::server_status`]
//!   暴露（`connecting` / `connected` / `reconnecting` / `open` / `failed` + 最近错误）
//!
//! 分层（对标 oh-my-pi `src/mcp/transports/`）：`client` 为协议层（方法序 + id 关联），
//! `stdio` / `http` / `sse` 为传输层（帧与 I/O）。
//!
//! 协议参考：https://modelcontextprotocol.io（行分隔 JSON-RPC 2.0 / Streamable HTTP）。

#![deny(unsafe_code)]

pub mod cache;
mod client;
mod http;
pub mod oauth;
mod reconnect;
mod sse;
mod stdio;
mod tool;

pub use cache::{CachedTool, CachedTools};
pub use client::{
    McpClient, McpError, McpNotification, McpPromptArg, McpPromptInfo, McpResource, McpRoot,
    McpToolInfo, ServerCapabilities, ServerInfo,
};
pub use reconnect::{McpConnState, McpServerStatus};
pub use tool::{McpLoadOptions, McpRegistry, McpTool, McpToolSource, ToolsChangedListener};
