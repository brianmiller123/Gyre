//! # agent-browser
//!
//! CDP 最小客户端：驱动 headless chromium 的浏览器自动化工具（移植自
//! [`oh-my-pi browser.ts`](../../../third/oh-my-pi/packages/coding-agent/src/tools/browser.ts) 的
//! 最小子集）。
//!
//! - [`launcher`]：spawn chromium/chrome（PATH 探测 + `--remote-debugging-port=0` +
//!   `--headless=new` + 临时 `--user-data-dir`，`--no-sandbox` 环境变量开关），解析 stderr 的
//!   `DevTools listening on ws://…` 行拿端口/端点；close 时击杀整个进程组；
//! - [`cdp`]：tokio-tungstenite WebSocket + JSON-RPC 帧收发（id 配对、事件分流、10s 超时）；
//! - [`browser_tool`]：[`BrowserTool`]（[`Tool`](agent_tools::Tool) 实现，
//!   `name = "browser"`，Execute 级、Exclusive、可中断），动作
//!   `navigate` / `evaluate` / `screenshot` / `click` / `text` / `scroll` / `close`。
//!
//! v1 明确不做：多标签页、网络拦截、headless 之外模式。

#![deny(unsafe_code)]

mod browser_tool;
mod cdp;
mod launcher;

pub use browser_tool::{BrowserTool, PROMPT_SECTION, browser_schema};
pub use cdp::{CdpConnection, CdpError, CdpEvent, DEFAULT_SEND_TIMEOUT};
pub use launcher::{BrowserProcess, DevToolsEndpoint, LaunchError, LaunchOptions};
