//! # 资源端口
//!
//! 外部资源读取端口（MCP `resources/read` 等），供 [`crate::tools`] 的 `read_file` 工具
//! 经 `mcp://` 协议路由时注入，避免 `tools`→`mcp` 的具体实现依赖（依赖洁癖守卫）。
//!
//! 与 [`crate::skill::SkillResolver`]（`skill://`）、[`crate::memory::MemoryStore`]（`memory://`）
//! 同构：本模块仅放跨 crate 共享的端口，实现位于 `crates/mcp`。

/// 一条外部资源描述（来自 `resources/list`）。
#[derive(Debug, Clone)]
pub struct ResourceEntry {
    /// 资源 URI（server 内唯一，传给 [`ResourceResolver::read_resource`]）。
    pub uri: String,
    /// 人类可读名称。
    pub name: String,
    /// 描述（可选）。
    pub description: Option<String>,
    /// MIME 类型（可选，如 `text/plain`、`application/json`）。
    pub mime_type: Option<String>,
}

/// 资源读取错误。
#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    /// 未找到指定 server（`mcp://<server>/...` 中的 server 名未注册）。
    #[error("未找到 MCP server: {0}")]
    UnknownServer(String),
    /// server 不支持 resources 能力（未声明 `resources` 或拒绝请求）。
    #[error("server `{0}` 不支持 resources 读取")]
    Unsupported(String),
    /// 读取失败（通信错误、资源不存在等）。
    #[error("读取资源失败: {0}")]
    Read(String),
}

/// 外部资源解析端口（`mcp://` 协议路由目标）。
///
/// 供 `ReadFileTool`（`crates/tools`）注入：装配层把 `McpRegistry`（`crates/mcp`）
/// 作为本 trait 的实现注入 read 工具，使 `read_file` 可经 `mcp://<server>/<uri>` 读取
/// MCP server 暴露的资源，而 `tools` crate 无需依赖 `crates/mcp`。
///
/// URL 形式：`mcp://<server>/<resource_uri>`，其中 `<server>` 对应 `[mcp.servers]` 配置键，
/// `<resource_uri>` 为 server 内的资源标识（透传给 MCP `resources/read` 的 `uri`）。
#[async_trait::async_trait]
pub trait ResourceResolver: Send + Sync {
    /// 列出指定 server 暴露的全部资源。
    ///
    /// # Errors
    /// server 不存在或不支持 resources 时返回 [`ResourceError`]。
    async fn list_resources(&self, server: &str) -> Result<Vec<ResourceEntry>, ResourceError>;

    /// 读取指定 server 的一个资源，返回文本内容。
    ///
    /// # Errors
    /// server 不存在、不支持 resources、或资源读取失败时返回 [`ResourceError`]。
    async fn read_resource(&self, server: &str, uri: &str) -> Result<String, ResourceError>;
}
