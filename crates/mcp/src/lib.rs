//! # agent-mcp
//!
//! Model Context Protocol (MCP) 客户端：stdio + Streamable HTTP 双传输 + tool bridge。
//!
//! - [`McpClient`]：连接 MCP server（子进程 stdio 或 HTTP 端点），JSON-RPC 2.0 通信
//!   （initialize / tools/list / tools/call / resources/*）
//! - [`McpTool`]：把 server 工具包装为 agent [`Tool`](agent_tools::Tool)
//! - [`McpRegistry`]：从 `[mcp.servers]` 配置加载多 server 的全部工具
//!
//! 分层（对标 oh-my-pi `src/mcp/transports/`）：`client` 为协议层（方法序 + id 关联），
//! `stdio` / `http` 为传输层（帧与 I/O）。
//!
//! 协议参考：https://modelcontextprotocol.io（行分隔 JSON-RPC 2.0 / Streamable HTTP）。

#![deny(unsafe_code)]

mod client;
mod http;
mod stdio;
mod tool;

pub use client::{McpClient, McpError, McpToolInfo};
pub use tool::{McpRegistry, McpTool};
