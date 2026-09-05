//! MCP tool bridge：把 server 工具包装为 agent [`Tool`]。

use std::sync::Arc;

use agent_config::McpConfig;
use agent_core::{
    CapabilityTier, ToolError, ToolResult,
    resource::{ResourceEntry, ResourceError, ResourceResolver},
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
    /// 对模型暴露的命名空间化名称 `mcp__<server>_<tool>`（防跨 server 同名冲突）。
    alias: String,
    client: Arc<McpClient>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.alias
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
/// 工具名消毒：小写化，`[^a-z0-9_]+` → `_`，折叠连续 `_`，去首尾 `_`；
/// 空结果回退 `fallback`（对齐 omp `sanitizeMCPToolNamePart`）。
fn sanitize_name_part(value: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_lowercase() || ch == '_' {
            out.push(ch);
        } else if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_underscore = false;
    for ch in out.chars() {
        if ch == '_' && prev_underscore {
            continue;
        }
        prev_underscore = ch == '_';
        collapsed.push(ch);
    }
    let trimmed = collapsed.trim_matches('_');
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// FNV-1a 64 位 → base36（跨进程稳定，供超长名哈希后缀）。
fn fnv1a_base36(value: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in value.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    let mut v = hash;
    while v > 0 {
        out.push(DIGITS[(v % 36) as usize]);
        v /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// 严格校验器工具名上限：OpenAI/Meta 系 `^[a-zA-Z0-9_-]{1,64}$`，超长 400
///（omp #9130）。超限时保留可读前缀 + 全名 8 位 base36 哈希后缀，保证唯一且稳定。
fn cap_tool_name_length(name: String) -> String {
    const MAX: usize = 64;
    if name.len() <= MAX {
        return name;
    }
    let hash = fnv1a_base36(&name);
    let hash = if hash.len() > 8 {
        hash[..8].to_string()
    } else {
        hash
    };
    let keep = MAX.saturating_sub(hash.len() + 1);
    format!("{}_{hash}", &name[..keep])
}

/// 生成 MCP 工具的命名空间化名称：`mcp__<server>_<tool>`。
///
/// 工具名已带 `<server>_` 前缀时去冗余（`puppeteer` + `puppeteer_screenshot` →
/// `mcp__puppeteer_screenshot`）。对齐 omp `createMCPToolName`。
#[must_use]
pub fn mint_tool_name(server: &str, tool: &str) -> String {
    let server = sanitize_name_part(server, "server");
    let mut tool = sanitize_name_part(tool, "tool");
    let prefix = format!("{server}_");
    if let Some(stripped) = tool.strip_prefix(&prefix) {
        tool = stripped.to_string();
    }
    cap_tool_name_length(format!("mcp__{server}_{tool}"))
}

/// 按 (别名, origin key) 元组去重：同别名冲突时保留 origin key（`server\0tool`）
/// 较小者——稳定赢家规则，与装配顺序无关（对齐 omp `deduplicateMCPToolsByName`）。
fn dedupe_by_alias<T>(items: Vec<(String, String, T)>) -> Vec<(String, T)> {
    let mut best: std::collections::HashMap<String, (String, T)> = std::collections::HashMap::new();
    for (alias, origin, payload) in items {
        match best.get(&alias) {
            Some((winner, _)) if *winner <= origin => {
                tracing::warn!(
                    alias = %alias,
                    loser = %origin,
                    winner = %winner,
                    "MCP 工具别名冲突，保留稳定赢家"
                );
            }
            Some(_) | None => {
                best.insert(alias, (origin, payload));
            }
        }
    }
    let mut out: Vec<(String, T)> = best
        .into_iter()
        .map(|(alias, (_, payload))| (alias, payload))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 多 MCP server 注册表：启动、握手、收集全部工具。
///
/// 单个 server `失败（连接/initialize/list_tools）不阻断其余；失败告警并跳过`。
#[derive(Default)]
pub struct McpRegistry {
    /// server 名 → client（按 `[mcp.servers]` 配置顺序；既保活又供 `mcp://` 资源路由）。
    named: Vec<(String, Arc<McpClient>)>,
    tools: Vec<McpTool>,
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
            match McpClient::connect(server_cfg).await {
                Ok(client) => {
                    let client = Arc::new(client);
                    if let Err(e) = client.initialize().await {
                        tracing::warn!(server = %name, error = %e, "MCP server initialize 失败，跳过");
                        client.close().await;
                        continue;
                    }
                    match client.list_tools().await {
                        Ok(infos) => {
                            let client = Arc::clone(&client);
                            for info in infos {
                                let alias = mint_tool_name(name, &info.name);
                                tools.push((
                                    alias.clone(),
                                    format!("{name}\u{0}{}", info.name),
                                    McpTool {
                                        info,
                                        alias,
                                        client: Arc::clone(&client),
                                    },
                                ));
                            }
                            named.push((name.clone(), Arc::clone(&client)));
                            tracing::info!(server = %name, "MCP server 已连接");
                        }
                        Err(e) => {
                            tracing::warn!(server = %name, error = %e, "MCP server list_tools 失败，跳过");
                            client.close().await;
                        }
                    }
                }
                Err(e) => tracing::warn!(server = %name, error = %e, "MCP server 连接失败，跳过"),
            }
        }
        let tools = dedupe_by_alias(tools)
            .into_iter()
            .map(|(_, tool)| tool)
            .collect();
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
        client
            .read_resource(uri)
            .await
            .map_err(|e| classify(server, e))
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

    #[test]
    fn mint_namespaces_server_and_tool() {
        assert_eq!(
            mint_tool_name("github", "create_issue"),
            "mcp__github_create_issue"
        );
        // server 名冗余前缀被剥离。
        assert_eq!(
            mint_tool_name("puppeteer", "puppeteer_screenshot"),
            "mcp__puppeteer_screenshot"
        );
        // 非法字符消毒为 `_`，连续折叠，去首尾。
        assert_eq!(
            mint_tool_name("My Server", "get-item!"),
            "mcp__my_server_get_item"
        );
        // 空消毒结果回退 fallback。
        assert_eq!(mint_tool_name("---", "+++"), "mcp__server_tool");
    }

    #[test]
    fn mint_caps_at_64_with_stable_hash_suffix() {
        let server = "s";
        let tool = "a".repeat(100);
        let name = mint_tool_name(server, &tool);
        assert!(name.len() <= 64, "超长名应截断: {name}");
        // 稳定性：同输入两次生成一致。
        assert_eq!(name, mint_tool_name(server, &tool));
        // 唯一性：不同超长名哈希不同。
        let other = mint_tool_name(server, &"b".repeat(100));
        assert_ne!(name, other);
    }

    #[test]
    fn dedupe_keeps_smallest_origin_per_alias() {
        let items = vec![
            ("mcp__a_tool".to_string(), "a\u{0}tool".to_string(), 1),
            ("mcp__a_tool".to_string(), "b\u{0}tool".to_string(), 2),
            ("mcp__b_tool".to_string(), "b\u{0}tool".to_string(), 3),
        ];
        let out = dedupe_by_alias(items);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], ("mcp__a_tool".to_string(), 1)); // origin 较小者胜出
        assert_eq!(out[1], ("mcp__b_tool".to_string(), 3));
        // 输出按别名排序（注册表顺序稳定）。
        assert!(out[0].0 < out[1].0);
    }

    #[tokio::test]
    async fn empty_config_yields_empty_registry() {
        let cfg = McpConfig::default();
        let reg = McpRegistry::load(&cfg).await;
        assert!(reg.is_empty());
    }
}
