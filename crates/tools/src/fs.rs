//! 文件系统工具：read_file / write_file。
//! （str_replace/apply_diff 已按工具收敛移除，编辑走 apply_hashline。）

use std::path::Path;
use std::time::Duration;

use agent_ast::{SegmentKind, SummaryOptions, SupportLang, summarize_code};
use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::{Tool, ToolContext, write_with_effects};

/// 读取文件（带行号）。
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "读取工作区内文件内容并附行号。仅限只读，不修改文件。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径（相对工作区根或绝对路径），或内部协议：skill://<name>[/<rel>]（skill 内容）、memory://[summary|full]（跨会话记忆）、mcp://<server>/<uri>（MCP 资源）、local://<rel>（显式工作区相对）、artifact://<id>（shake 归档内容回读）、http(s)://（抓取网页）" },
                "summary": { "type": "boolean", "description": "可选：对支持语言的代码文件做结构摘要——折叠大体块、保留签名，行号与原文一致。适合快速浏览大文件；需逐行编辑时仍用真实行号。" }
            },
            "required": ["path"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        CapabilityTier::ReadOnly
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let path = input
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `path` 参数".into()))?;
        // 内部协议路由：skill:// memory:// mcp:// local:// http(s):// 或裸本地路径。
        let text = resolve_path(path, ctx).await?;
        // 可选摘要：对支持语言的代码文件做结构折叠，保留签名、行号保真。
        let want_summary = input
            .get("summary")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let out = if want_summary {
            match SupportLang::from_path(Path::new(path)) {
                Some(lang) => render_summary(&text, lang),
                None => render_numbered(&text),
            }
        } else {
            render_numbered(&text)
        };
        Ok(ToolResult::text(out))
    }
}

/// 逐行带行号渲染（默认行为）。
fn render_numbered(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        out.push_str(&format!("{i:>5}\t{line}\n"));
    }
    out
}

/// 结构摘要渲染：折叠大体块、保留签名；行号与原文一致（编辑须基于真实行号）。
fn render_summary(text: &str, lang: SupportLang) -> String {
    let result = summarize_code(text, lang, &SummaryOptions::default());
    // 无可折叠内容（如小文件/解析失败）：回退逐行渲染，避免无意义的摘要头。
    if !result.elided {
        return render_numbered(text);
    }
    let mut out = String::new();
    out.push_str(&format!(
        "（结构摘要：原文 {} 行，已折叠体块；行号与原文一致，如需逐行编辑请基于这些行号）\n",
        result.total_lines
    ));
    for seg in &result.segments {
        match seg.kind {
            SegmentKind::Kept => {
                if let Some(body) = &seg.text {
                    for (offset, line) in body.lines().enumerate() {
                        out.push_str(&format!(
                            "{:>5}\t{line}\n",
                            seg.start_line as usize + offset
                        ));
                    }
                }
            }
            SegmentKind::Elided => {
                out.push_str(&format!(
                    "     ⋯⋯ (折叠第 {}-{} 行，共 {} 行) ⋯⋯\n",
                    seg.start_line,
                    seg.end_line,
                    seg.end_line
                        .saturating_sub(seg.start_line)
                        .saturating_add(1),
                ));
            }
        }
    }
    out
}

/// 解析 read_file 的 `path`：内部协议路由 + 裸本地路径，返回原始文本。
///
/// 支持协议（按前缀分流）：
/// - `skill://<name>[/<rel>]` → SkillResolver（skill 内容）
/// - `memory://[summary|full]` → MemoryStore（跨会话记忆；默认 summary）
/// - `mcp://<server>/<uri>` → ResourceResolver（MCP `resources/read`）
/// - `local://<rel>` → 工作区相对路径（显式本地协议，等价裸路径）
/// - `http(s)://` → HTTP 抓取
/// - 其他 → 工作区相对/绝对本地路径
async fn resolve_path(path: &str, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
    if path.strip_prefix("skill://").is_some() {
        // 注意：`SkillResolver::resolve` 内部会再次剥离 `skill://` 前缀（见 skills/registry.rs），
        // 故此处传入完整 path，无需手动剥离。
        let Some(resolver) = ctx.skills else {
            return Err(ToolError::Execution(
                "skill:// 解析不可用（未注入 Skill 目录）".into(),
            ));
        };
        let file = resolver
            .resolve(path)
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        let bytes = tokio::fs::read(&file).await.map_err(ToolError::Io)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    } else if let Some(rest) = path.strip_prefix("memory://") {
        resolve_memory(rest, ctx).await
    } else if let Some(rest) = path.strip_prefix("mcp://") {
        resolve_mcp(rest, ctx).await
    } else if let Some(id) = path.strip_prefix("artifact://") {
        resolve_artifact(id, ctx).await
    } else if let Some(rel) = path.strip_prefix("local://") {
        let full = ctx.workspace.resolve(Path::new(rel));
        Ok(read_bounded(&full).await?)
    } else if path.starts_with("http://") || path.starts_with("https://") {
        fetch_http(path).await
    } else {
        let full = ctx.workspace.resolve(Path::new(path));
        Ok(read_bounded(&full).await?)
    }
}

/// `artifact://<id>` 路由：读取 shake 归档落盘内容（`<workspace>/.gyre/artifacts/<id>`）。
///
/// id 仅允许十六进制字符（由 shake sink 的内容哈希产生），杜绝路径穿越。
async fn resolve_artifact(id: &str, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
    let id = id.trim();
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ToolError::Execution(
            "artifact:// id 非法（仅允许十六进制字符）".into(),
        ));
    }
    let rel = format!(".gyre/artifacts/{id}");
    let full = ctx.workspace.resolve(Path::new(&rel));
    Ok(read_bounded(&full).await?)
}

/// `memory://` 路由：`` / `summary` → 启动摘要；`full` → 完整 MEMORY.md。
async fn resolve_memory(rest: &str, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
    let Some(memory) = ctx.memory else {
        return Err(ToolError::Execution(
            "memory:// 解析不可用（未启用跨会话记忆）".into(),
        ));
    };
    match rest.trim_end_matches('/') {
        "" | "summary" => memory
            .summary()
            .await
            .map_err(ToolError::Io)?
            .ok_or_else(|| ToolError::Execution("该项目暂无记忆摘要（memory://summary）".into())),
        "full" => memory
            .read_full()
            .await
            .map_err(ToolError::Io)?
            .ok_or_else(|| ToolError::Execution("该项目暂无完整记忆文档（memory://full）".into())),
        other => Err(ToolError::Execution(format!(
            "未知的 memory:// 子路径 `{other}`（可用：summary / full）"
        ))),
    }
}

/// `mcp://<server>/<uri>` 路由：经 MCP `resources/read` 读取。
async fn resolve_mcp(rest: &str, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
    let Some(resolver) = ctx.resources else {
        return Err(ToolError::Execution(
            "mcp:// 解析不可用（未注入 MCP）".into(),
        ));
    };
    let (server, uri) = rest
        .split_once('/')
        .ok_or_else(|| ToolError::Execution("mcp:// 需形式 `mcp://<server>/<uri>`".into()))?;
    if server.is_empty() || uri.is_empty() {
        return Err(ToolError::Execution(
            "mcp:// 的 server 与 uri 均不可为空".into(),
        ));
    }
    resolver
        .read_resource(server, uri)
        .await
        .map_err(|e| ToolError::Execution(format!("mcp://{server}/{uri}: {e}")))
}

/// HTTP 抓取超时（连接 + 整体）。
const HTTP_FETCH_TIMEOUT: Duration = Duration::from_secs(15);
/// HTTP 抓取响应体大小上限（流式截断，防 OOM）。
const HTTP_FETCH_MAX_BYTES: usize = 1024 * 1024; // 1 MiB
/// HTTP 抓取最大跟随重定向次数。
const HTTP_FETCH_MAX_REDIRECTS: usize = 3;

/// `http(s)://` 抓取：返回响应体文本（带超时、大小上限与基础 SSRF 防护）。
///
/// SSRF 防护对**首层 URL 与每一跳重定向目标**均执行校验（拦截字面量内网 IP 与已知元数据
/// 主机名），杜绝「公网 URL 302 → 内网/元数据地址」绕过。**不**防御 DNS rebinding
/// （主机名解析到内网）。生产环境如需更强保证，应在此之上叠加 DNS 解析钉扎。
async fn fetch_http(url: &str) -> Result<String, ToolError> {
    ssrf_guard(url)?;
    let client = fetch_client();

    let mut current =
        url::Url::parse(url).map_err(|e| ToolError::Execution(format!("非法 URL: {e}")))?;
    let mut resp = client
        .get(current.as_str())
        .send()
        .await
        .map_err(|e| ToolError::Execution(format!("fetch {url} 失败: {e}")))?;

    // 手动跟随重定向，每一跳重新校验目标 host（含 IP / 内网 / 元数据主机）。
    let mut redirects = 0usize;
    while resp.status().is_redirection() {
        if redirects >= HTTP_FETCH_MAX_REDIRECTS {
            return Err(ToolError::Execution(format!(
                "重定向次数超过 {HTTP_FETCH_MAX_REDIRECTS} 上限"
            )));
        }
        let Some(loc) = resp.headers().get(reqwest::header::LOCATION) else {
            break;
        };
        let loc_str = loc
            .to_str()
            .map_err(|e| ToolError::Execution(format!("非法 Location 头: {e}")))?;
        current = current
            .join(loc_str)
            .map_err(|e| ToolError::Execution(format!("解析重定向 URL 失败: {e}")))?;
        ssrf_guard(current.as_str())?;
        redirects += 1;
        resp = client
            .get(current.as_str())
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("fetch {} 失败: {e}", current)))?;
    }

    if !resp.status().is_success() {
        return Err(ToolError::Execution(format!("HTTP {}", resp.status())));
    }
    // 流式读取并在超限时立即中止，避免大响应体整段入内存。
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| ToolError::Execution(format!("读取响应失败: {e}")))?;
        let Some(chunk) = chunk else { break };
        buf.extend_from_slice(&chunk);
        if buf.len() > HTTP_FETCH_MAX_BYTES {
            return Err(ToolError::Execution(format!(
                "响应体超过 {HTTP_FETCH_MAX_BYTES} 字节上限"
            )));
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// 基础 SSRF 防护：仅允许 http/https，拒绝回环/私有/链路本地 IP 字面量与已知元数据主机名。
fn ssrf_guard(raw: &str) -> Result<(), ToolError> {
    let url = url::Url::parse(raw).map_err(|e| ToolError::Execution(format!("非法 URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolError::Execution(format!(
            "不允许的 scheme: {}",
            url.scheme()
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| ToolError::Execution("URL 缺少主机".into()))?;
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if ip.is_loopback() || ip.is_unspecified() || is_private_or_link_local(&ip) {
            return Err(ToolError::Execution(format!(
                "SSRF 防护：禁止访问内网地址 {host}"
            )));
        }
    }
    let h = host.to_ascii_lowercase();
    const BLOCKED_HOSTS: &[&str] = &[
        "localhost",
        "metadata.google.internal",
        "metadata",
        "metadata.azure.com",
    ];
    if BLOCKED_HOSTS.iter().any(|b| h == *b) {
        return Err(ToolError::Execution(format!("SSRF 防护：禁止访问 {host}")));
    }
    Ok(())
}

/// 判定 IPv4/IPv6 是否为私有/链路本地（含 IPv4-mapped IPv6 降级检查）。
fn is_private_or_link_local(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            // CGNAT 100.64.0.0/10：阿里云元数据 100.100.100.200 等落此段，必须拦截。
            let is_cgnat = o[0] == 100 && (64..=127).contains(&o[1]);
            v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || is_cgnat
        }
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            // Unique-local fc00::/7、链路本地 fe80::/10
            let is_ula = (s[0] & 0xfe00) == 0xfc00;
            let is_ll = (s[0] & 0xffc0) == 0xfe80;
            if is_ula || is_ll {
                return true;
            }
            // IPv4-mapped IPv6 (::ffff:a.b.c.d)：降级为 IPv4 重新检查，
            // 堵截 http://[::ffff:169.254.169.254]/ 等映射绕过。
            if let Some(v4) = v6.to_ipv4_mapped() {
                let o = v4.octets();
                let is_cgnat = o[0] == 100 && (64..=127).contains(&o[1]);
                return v4.is_loopback()
                    || v4.is_unspecified()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_broadcast()
                    || v4.is_documentation()
                    || is_cgnat;
            }
            false
        }
    }
}

/// 抓取用共享 HTTP client（禁用自动重定向 + 超时），复用连接池，避免每次 `read_file http(s)://`
/// 都新建 client（重复 TLS 握手）。
fn fetch_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(HTTP_FETCH_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            // 禁用自动重定向：手动跟随并对每一跳的目标重新做 SSRF 校验。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("构造 HTTP client 失败")
    })
}

/// 本地文件读取大小上限（超出截断并标注，避免大文件整段入内存）。
const MAX_READ_BYTES: usize = 2 * 1024 * 1024; // 2 MiB

/// 读取本地文件为文本，超大文件仅读取前 [`MAX_READ_BYTES`] 字节并追加截断标记。
async fn read_bounded(full: &Path) -> Result<String, ToolError> {
    let meta = tokio::fs::metadata(full).await.map_err(ToolError::Io)?;
    if meta.len() > MAX_READ_BYTES as u64 {
        use tokio::io::AsyncReadExt;
        let mut f = tokio::fs::File::open(full).await.map_err(ToolError::Io)?;
        let mut buf = vec![0u8; MAX_READ_BYTES];
        let mut filled = 0usize;
        // 循环填满缓冲（单次 read 可能短读，否则只读到部分字节）。
        while filled < buf.len() {
            match f.read(&mut buf[filled..]).await {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(ToolError::Io(e)),
            }
        }
        buf.truncate(filled);
        // 在合法 UTF-8 边界截断，避免多字节字符被切断后 from_utf8_lossy 产生 U+FFFD。
        let safe_end = utf8_safe_boundary(&buf);
        let mut text = String::from_utf8_lossy(&buf[..safe_end]).into_owned();
        text.push_str("\n...(文件过大，已截断)");
        return Ok(text);
    }
    let bytes = tokio::fs::read(full).await.map_err(ToolError::Io)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// 返回不超过 `bytes.len()` 的最大合法 UTF-8 边界（末尾为残缺多字节序列时回退）。
fn utf8_safe_boundary(bytes: &[u8]) -> usize {
    if std::str::from_utf8(bytes).is_ok() {
        return bytes.len();
    }
    let mut end = bytes.len();
    while end > 0 {
        if std::str::from_utf8(&bytes[..end]).is_ok() {
            break;
        }
        end -= 1;
    }
    end
}

/// 写入文件（覆盖，自动创建父目录）。
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "写入文件（覆盖）。自动创建父目录。属于写入类操作，通常需审批。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "文件路径" },
                "content": { "type": "string", "description": "完整文件内容" }
            },
            "required": ["path", "content"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        CapabilityTier::Write
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let path = input
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `path` 参数".into()))?;
        let content = input
            .get("content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `content` 参数".into()))?;
        let full = ctx.workspace.resolve(Path::new(path));
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(ToolError::Io)?;
        }
        let report = write_with_effects(&full, content, ctx).await?;
        let mut msg = format!("已写入 {path}（{} 字节）", content.len());
        msg.push_str(&report.effect_suffix());
        Ok(ToolResult::text(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;
    use agent_core::{ApprovalMode, ApprovalRequest, CapabilityTier, Workspace};

    fn dummy_ctx<'a>(ws: &'a Workspace) -> ToolContext<'a> {
        use agent_core::ApprovalDecision;
        struct AutoApprove;
        #[async_trait::async_trait]
        impl agent_core::ApprovalPolicy for AutoApprove {
            fn decide(&self, _r: &ApprovalRequest<'_>) -> ApprovalDecision {
                ApprovalDecision::Allow
            }
            async fn prompt(
                &self,
                _a: &agent_core::AskMessage,
            ) -> Result<agent_core::AskResponse, ToolError> {
                Ok(agent_core::AskResponse::Yes)
            }
        }
        static CANCEL: std::sync::OnceLock<tokio_util::sync::CancellationToken> =
            std::sync::OnceLock::new();
        let cancel = CANCEL.get_or_init(tokio_util::sync::CancellationToken::new);
        // 故意用 AlwaysAsk 但 decide 永远 Allow，避免误判工具能力
        let _ = ApprovalMode::AlwaysAsk;
        ToolContext {
            workspace: ws,
            approval: &AutoApprove,
            cancel,
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
            update_tx: None,
        }
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let tmp = tempfile_dir();
        let ws = Workspace::new(&tmp);
        let ctx = dummy_ctx(&ws);
        let write = WriteFileTool;
        let input = serde_json::json!({ "path": "a.txt", "content": "hello\nworld" });
        write.execute(input, &ctx).await.unwrap();

        let read = ReadFileTool;
        let out = read
            .execute(serde_json::json!({ "path": "a.txt" }), &ctx)
            .await
            .unwrap();
        match out {
            ToolResult::Text(t) => assert!(t.contains("hello") && t.contains("world")),
            _ => panic!("应为文本结果"),
        }
    }

    #[test]
    fn ssrf_blocks_cgnat_and_metadata() {
        // 回归：CGNAT 100.64.0.0/10（含阿里云元数据 100.100.100.200）必须被拦截。
        assert!(is_private_or_link_local(
            &"100.100.100.200".parse::<std::net::IpAddr>().unwrap()
        ));
        assert!(is_private_or_link_local(
            &"100.64.0.1".parse::<std::net::IpAddr>().unwrap()
        ));
        assert!(!is_private_or_link_local(
            &"8.8.8.8".parse::<std::net::IpAddr>().unwrap()
        ));
    }

    #[test]
    fn ssrf_blocks_ipv4_mapped_ipv6() {
        // 回归：IPv4-mapped IPv6 地址必须降级为 IPv4 检查。
        // ::ffff:169.254.169.254（AWS/GCP 元数据）必须被拦截。
        assert!(is_private_or_link_local(
            &"::ffff:169.254.169.254"
                .parse::<std::net::IpAddr>()
                .unwrap()
        ));
        // ::ffff:127.0.0.1（回环）必须被拦截。
        assert!(is_private_or_link_local(
            &"::ffff:127.0.0.1".parse::<std::net::IpAddr>().unwrap()
        ));
        // ::ffff:10.0.0.1（私有）必须被拦截。
        assert!(is_private_or_link_local(
            &"::ffff:10.0.0.1".parse::<std::net::IpAddr>().unwrap()
        ));
        // ::ffff:100.100.100.200（CGNAT）必须被拦截。
        assert!(is_private_or_link_local(
            &"::ffff:100.100.100.200"
                .parse::<std::net::IpAddr>()
                .unwrap()
        ));
        // 公网 IPv4-mapped 不应被拦截。
        assert!(!is_private_or_link_local(
            &"::ffff:8.8.8.8".parse::<std::net::IpAddr>().unwrap()
        ));
    }

    #[test]
    fn utf8_safe_boundary_keeps_valid_prefix() {
        // 「你」= E4 BD A0；切掉末字节后应在合法 UTF-8 边界截断，保留 "a"。
        let full = "a你".as_bytes();
        let safe = utf8_safe_boundary(&full[..full.len() - 1]);
        let s = std::str::from_utf8(&full[..safe]).unwrap();
        assert_eq!(s, "a");
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-test-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn uuid_v4() -> String {
        // 轻量伪随机，仅用于测试目录名，避免引入 uuid 依赖到测试
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{nanos:x}")
    }

    // 静默未使用告警
    #[allow(dead_code)]
    fn _force_capability_use() -> CapabilityTier {
        CapabilityTier::ReadOnly
    }
}
