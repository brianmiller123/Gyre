//! MCP tool bridge：把 server 工具包装为 agent [`Tool`]。

use std::sync::Arc;

use agent_config::McpConfig;
use agent_core::{
    resource::{ResourceEntry, ResourceError, ResourceResolver},
    CapabilityTier, ToolError, ToolResult,
};
use agent_tools::{Tool, ToolContext};
use async_trait::async_trait;

use crate::client::{McpClient, McpError, McpResource, McpToolInfo};

/// 把单个 MCP server 工具包装为 agent [`Tool`]。
///
/// 持有共享的 [`McpClient`]（多工具复用同一连接）。execute 时经 `tools/call` 调用远端工具。
#[derive(Clone)]
pub struct McpTool {
    info: McpToolInfo,
    client: Arc<McpClient>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.info.name
    }
    fn description(&self) -> &str {
        &self.info.description
    }
    fn schema(&self) -> serde_json::Value {
        self.info.schema.clone()
    }
    fn capability(&self) -> CapabilityTier {
        // MCP 工具副作用未知（可能删文件、执行命令、网络请求），保守归为 Execute，
        // 确保破坏性 MCP 工具须经审批门禁而非自动放行。
        CapabilityTier::Execute
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let text = self
            .client
            .call_tool(&self.info.name, input)
            .await
            .map_err(|e| ToolError::Execution(format!("MCP `{}`: {e}", self.info.name)))?;
        Ok(ToolResult::text(text))
    }
}

/// 多 MCP server 注册表：启动、握手、收集全部工具。
///
/// 单个 server 失败（spawn/initialize/list_tools）不阻断其余；失败告警并跳过。
pub struct McpRegistry {
    /// server 名 → client（按 `[mcp.servers]` 配置顺序；既保活又供 `mcp://` 资源路由）。
    named: Vec<(String, Arc<McpClient>)>,
    tools: Vec<McpTool>,
}

impl Default for McpRegistry {
    fn default() -> Self {
        Self {
            named: Vec::new(),
            tools: Vec::new(),
        }
    }
}

impl McpRegistry {
    /// 从 `[mcp.servers]` 配置加载所有 server 的全部工具。
    ///
    /// 无 server 或全部失败时返回空注册表（不报错，适配无 MCP 的环境）。
    #[must_use]
    pub async fn load(cfg: &McpConfig) -> Self {
        let mut named: Vec<(String, Arc<McpClient>)> = Vec::new();
        let mut tools = Vec::new();
        for (name, server_cfg) in &cfg.servers {
            match McpClient::spawn(server_cfg).await {
                Ok(client) => {
                    let client = Arc::new(client);
                    if let Err(e) = client.initialize().await {
                        tracing::warn!(server = %name, error = %e, "MCP server initialize 失败，跳过");
                        client.kill().await;
                        continue;
                    }
                    match client.list_tools().await {
                        Ok(infos) => {
                            for info in infos {
                                tools.push(McpTool {
                                    info,
                                    client: Arc::clone(&client),
                                });
                            }
                            named.push((name.clone(), Arc::clone(&client)));
                            tracing::info!(server = %name, "MCP server 已连接");
                        }
                        Err(e) => {
                            tracing::warn!(server = %name, error = %e, "MCP server list_tools 失败，跳过");
                            client.kill().await;
                        }
                    }
                }
                Err(e) => tracing::warn!(server = %name, error = %e, "MCP server spawn 失败，跳过"),
            }
        }
        Self { named, tools }
    }

    /// 所有已加载的 MCP 工具。
    #[must_use]
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// 是否未加载任何工具。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 已连接的 server 名列表（调试 / `mcp://` 提示用）。
    #[must_use]
    pub fn server_names(&self) -> Vec<&str> {
        self.named.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// 按 server 名查找 client（`mcp://` 路由用）。
    fn client(&self, server: &str) -> Result<&Arc<McpClient>, ResourceError> {
        self.named
            .iter()
            .find(|(n, _)| n == server)
            .map(|(_, c)| c)
            .ok_or_else(|| ResourceError::UnknownServer(server.to_string()))
    }
}

/// `mcp://<server>/<uri>` 资源读取：按 server 名路由到对应 MCP client。
#[async_trait]
impl ResourceResolver for McpRegistry {
    async fn list_resources(&self, server: &str) -> Result<Vec<ResourceEntry>, ResourceError> {
        let client = self.client(server)?;
        client
            .list_resources()
            .await
            .map(|rs| {
                rs.into_iter()
                    .map(|r: McpResource| ResourceEntry {
                        uri: r.uri,
                        name: r.name,
                        description: r.description,
                        mime_type: r.mime_type,
                    })
                    .collect()
            })
            .map_err(|e| classify(server, e))
    }

    async fn read_resource(&self, server: &str, uri: &str) -> Result<String, ResourceError> {
        let client = self.client(server)?;
        client.read_resource(uri).await.map_err(|e| classify(server, e))
    }
}

/// 把 MCP 错误分类为资源错误：`method not found` / `not supported` 视为不支持 resources。
fn classify(server: &str, e: McpError) -> ResourceError {
    let msg = e.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("-32601") || lower.contains("not found") || lower.contains("not supported") {
        ResourceError::Unsupported(server.to_string())
    } else {
        ResourceError::Read(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_config_yields_empty_registry() {
        let cfg = McpConfig::default();
        let reg = McpRegistry::load(&cfg).await;
        assert!(reg.is_empty());
    }
}
