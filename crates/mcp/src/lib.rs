//! # agent-mcp
//!
//! Model Context Protocol (MCP) 客户端：stdio JSON-RPC + tool bridge。
//!
//! - [`McpClient`]：启动 MCP server 子进程，JSON-RPC 2.0 通信（initialize / tools/list / tools/call）
//! - [`McpTool`]：把 server 工具包装为 agent [`Tool`](agent_tools::Tool)
//! - [`McpRegistry`]：从 `[mcp.servers]` 配置加载多 server 的全部工具
//!
//! 协议参考：https://modelcontextprotocol.io（stdio transport，行分隔 JSON-RPC 2.0）。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

mod client;
mod tool;

pub use client::{McpClient, McpError, McpToolInfo};
pub use tool::{McpRegistry, McpTool};
