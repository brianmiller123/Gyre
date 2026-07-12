//! # agent-lsp
//!
//! 全功能 LSP（Language Server Protocol）客户端，为代码智能体提供
//! 深度代码理解能力。
//!
//! # 功能矩阵
//!
//! | 功能 | LSP 方法 | 说明 |
//! |------|----------|------|
//! | 诊断 | `textDocument/publishDiagnostics` | 错误、警告、提示的实时收集 |
//! | 跳转定义 | `textDocument/definition` | 跳转到符号定义处 |
//! | 查找引用 | `textDocument/references` | 查找所有引用位置 |
//! | 悬停信息 | `textDocument/hover` | 类型、文档、签名 |
//! | 文档符号 | `textDocument/documentSymbol` | 文件大纲（函数/类/变量） |
//! | 工作区符号 | `workspace/symbol` | 全局符号搜索 |
//! | 重命名 | `textDocument/rename` | 语义重命名 |
//! | 代码操作 | `textDocument/codeAction` | 快速修复、重构 |
//!
//! # 自动检测
//!
//! [`detect::detect_servers`] 根据项目根目录的配置文件自动推断语言服务器：
//!
//! - `Cargo.toml` → rust-analyzer
//! - `package.json` → typescript-language-server / deno
//! - `go.mod` → gopls
//! - `pyproject.toml` / `setup.py` → pyright / pylsp
//! - `.csproj` / `.sln` → omnisharp
//! - `pom.xml` / `build.gradle` → jdtls
//!
//! # 架构
//!
//! ```text
//! LspManager (多服务器编排)
//!   ├── LspClient (Rust)     → rust-analyzer
//!   ├── LspClient (TS)       → ts-ls
//!   └── LspClient (Python)   → pyright
//!       每个 LspClient 内部:
//!         └── LspTransport (JSON-RPC 2.0 over stdio)
//! ```
//!
//! # 示例
//!
//! ```no_run
//! use agent_lsp::{LspManager, detect::detect_servers};
//! use std::path::Path;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let root = Path::new("/my-project");
//! let servers = detect_servers(root);
//! let mut manager = LspManager::start(root, &servers).await?;
//!
//! let uri = url::Url::parse("file:///my-project/src/lib.rs")?;
//! let diagnostics = manager.diagnostics(&uri).await;
//! let defs = manager.goto_definition(&uri, 10, 5).await?;
//!
//! manager.shutdown_all().await?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::pedantic)]

pub mod client;
pub mod diagnostics_ledger;
pub mod detect;
pub mod manager;
pub mod transport;

pub use client::{LspClient, LspError};
pub use diagnostics_ledger::DiagnosticsLedger;
pub use detect::{detect_servers, find_server_for_file, language_id_from_path, LspServerInfo};
pub use manager::LspManager;
pub use transport::TransportError;
