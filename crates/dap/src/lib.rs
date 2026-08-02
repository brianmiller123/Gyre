//! # agent-dap
//!
//! DAP（Debug Adapter Protocol）调试器客户端与 `debug` 工具。
//!
//! 对标 oh-my-pi `coding-agent/src/dap/` 的最小集，按职责分四块：
//! - [`frame`]：`Content-Length` 帧编解码（纯函数，可单测，支持跨 chunk 分片）；
//! - [`session`]：stdio 传输的 [`DapSession`] —— `initialize` → `initialized` → `launch`
//!   握手、请求按 id 配对（pending map + oneshot）、事件广播、`send_sync` 同步发送；
//! - [`manager`]：14 动作的 [`DebugTool`]（`launch`/`attach`/`continue`/`pause`/`next`/
//!   `step_in`/`step_out`/`threads`/`stack_trace`/`scopes`/`variables`/`evaluate`/
//!   `set_breakpoint`/`remove_breakpoint`）与单会话句柄 [`DapManager`]；
//! - [`probe`]：适配器命令的 PATH 探测（`resolve` 时进行）。
//!
//! 内置适配器：`lldb-dap`、`dlv dap`（Go）、`python -m debugpy.adapter`（Python），
//! 见 [`default_adapters`]。未找到时返回明确错误（含尝试列表）。
//!
//! 事件不主动推送：v1 中暂停/断点等事件在 execute 返回后仍可经后续动作
//! （`stack_trace` / `threads` / `variables` 等）读取，不阻塞工具返回。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

mod error;
mod frame;
mod manager;
mod probe;
mod session;

pub use error::DapError;
pub use frame::{decode_frames, encode_frame};
pub use manager::{BreakpointRecord, Breakpoints, DapManager, DebugTool};
pub use probe::probe_executable;
pub use session::{DapEvent, DapSession};

/// DAP 调试器配置（[`DebugTool`] 构造参数）。
#[derive(Debug, Clone)]
pub struct DapSettings {
    /// 可用适配器列表。`adapter` 参数缺省（auto）时按此顺序逐个 PATH 探测，
    /// 取第一个可用者；显式指定名时只探测该适配器。
    pub adapters: Vec<AdapterSpec>,
}

impl Default for DapSettings {
    fn default() -> Self {
        Self {
            adapters: default_adapters(),
        }
    }
}

/// 单个调试适配器的进程规格。
#[derive(Debug, Clone)]
pub struct AdapterSpec {
    /// 适配器名（工具参数 `adapter` 使用；auto 探测的候选名）。
    pub name: &'static str,
    /// 可执行命令：裸命令名按 PATH 探测；含路径分隔符则直接使用。
    pub command: String,
    /// 命令参数。
    pub args: Vec<String>,
}

/// 内置三个适配器：lldb-dap（C/C++/Rust…）、dlv dap（Go）、debugpy（Python）。
///
/// `python -m debugpy.adapter` 会在 resolve 时额外验证 `debugpy` 模块可导入，
/// 避免 spawn 后握手超时。
#[must_use]
pub fn default_adapters() -> Vec<AdapterSpec> {
    vec![
        AdapterSpec {
            name: "lldb-dap",
            command: "lldb-dap".into(),
            args: Vec::new(),
        },
        AdapterSpec {
            name: "dlv",
            command: "dlv".into(),
            args: vec!["dap".into()],
        },
        AdapterSpec {
            name: "debugpy",
            command: "python".into(),
            args: vec!["-m".into(), "debugpy.adapter".into()],
        },
    ]
}
