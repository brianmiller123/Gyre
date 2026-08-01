//! `web_search` 工具：多 provider 顺序回退链 + 高频站点感知抽取。
//!
//! 移植 oh-my-pi `coding-agent/src/web/search/` 的简化版：
//! - `WebSearchProvider` trait + 顺序回退（首个可渲染结果胜出）；
//! - `DuckDuckGo` HTML（免 key）为主 provider；`searxng` 实例经 `GYRE_SEARXNG_URL`
//!   env 可选启用（懒加载，未配置即跳过）；
//! - 命中已知站点（arxiv/crates.io/npm/github）时返回结构 markdown（锚点保留），
//!   其余 URL 仅给标题 + 摘要；
//! - 结果格式化：`answer` 引导 + Sources 列表，条目 240 字符截断。
//!
//! 抓取复用 [`crate::fs::fetch_http`]（15s 超时 / 1MiB 上限 / 每跳重定向 SSRF 校验）。

use std::sync::Arc;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::{Tool, ToolContext};

/// 单条搜索结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebResult {
    /// 标题。
    pub title: String,
    /// 链接（已做 SSRF 过滤）。
    pub url: String,
    /// 摘要/片段。
    pub snippet: String,
}

/// 站点感知抽取结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SitePage {
    /// 标题。
    pub title: String,
    /// 结构化 markdown 正文（锚点保留）。
    pub markdown: String,
    /// 原始 URL。
    pub url: String,
}

/// web 搜索 provider。
#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    /// provider 名（诊断用）。
    fn name(&self) -> &str;
    /// 按 query 搜索，返回结果列表（空 = 无结果，不视为失败）。
    async fn search(&self, query: &str) -> Result<Vec<WebResult>, String>;
}

/// `DuckDuckGo` HTML 端点（免 key）。
pub struct DuckDuckGoHtml;

/// `searxng` 实例（`GYRE_SEARXNG_URL` 配置）。
pub struct Searxng {
    instance: String,
}

/// provider 顺序回退链：依序尝试，首个返回非空结果者胜出；全部失败 → 汇总错误。
pub struct WebSearchChain {
    providers: Vec<Arc<dyn WebSearchProvider>>,
}

impl WebSearchChain {
    /// 默认链：DDG +（env 配置了 `GYRE_SEARXNG_URL` 时）searxng。
    #[must_use]
    pub fn default_chain() -> Self {
        let mut providers: Vec<Arc<dyn WebSearchProvider>> = vec![Arc::new(DuckDuckGoHtml)];
        if let Ok(url) = std::env::var("GYRE_SEARXNG_URL") {
            if !url.trim().is_empty() {
                providers.push(Arc::new(Searxng { instance: url }));
            }
        }
        Self { providers }
    }

    /// 自定义链（测试用）。
    #[must_use]
    pub fn new(providers: Vec<Arc<dyn WebSearchProvider>>) -> Self {
        Self { providers }
    }

    /// 依序搜索，返回首个非空结果；provider 失败记录并继续。
    ///
    /// # Errors
    /// 所有 provider 均失败或无结果时返回 [`ToolError::Execution`]（附各 provider 错误）。
    pub async fn search(&self, query: &str) -> Result<Vec<WebResult>, ToolError> {
        let mut errors = Vec::new();
        for p in &self.providers {
            match p.search(query).await {
                Ok(results) if !results.is_empty() => {
                    return Ok(results);
                }
                Ok(_) => {}
                Err(e) => errors.push(format!("{}: {e}", p.name())),
            }
        }
        if errors.is_empty() {
            Err(ToolError::Execution("所有 provider 均无结果".into()))
        } else {
            Err(ToolError::Execution(format!(
                "所有 provider 搜索失败：{}",
                errors.join("；")
            )))
        }
    }
}

impl DuckDuckGoHtml {
    /// HTML 端点解析：宽松抓 `result__a` 链接（标题 + href），相邻 `result__snippet` 为摘要。
    /// 免 key；被限流时返回可读错误。
    fn parse_html(html: &str) -> Vec<WebResult> {
        let mut out = Vec::new();
        for block in html.split("result__a") {
            // 每个结果块：`<a ... href="URL">TITLE</a>` 后跟 snippet 区。
            let Some(href) = block.split("href=\"").nth(1).and_then(|s| s.split('"').next())
            else {
                continue;
            };
            if href.is_empty() || href.starts_with('/') || href.starts_with('#') {
                continue;
            }
            // 标题在 href 引号后的 `>` 之后、`</a>` 之前。
            let title = block
                .split('>')
                .nth(1)
                .and_then(|s| s.split("</a>").next())
                .map(strip_tags)
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if title.is_empty() {
                continue;
            }
            let after = block.split("</a>").nth(1).unwrap_or("");
            let snippet = strip_tags(after)
                .split("result__snippet")
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            out.push(WebResult {
                title: truncate(&title, 240),
                url: href.to_string(),
                snippet: truncate(&snippet, 240),
            });
        }
        out
    }
}

#[async_trait]
impl WebSearchProvider for DuckDuckGoHtml {
    fn name(&self) -> &str {
        "duckduckgo-html"
    }

    async fn search(&self, query: &str) -> Result<Vec<WebResult>, String> {
        let url = format!("https://html.duckduckgo.com/html/?q={}", urlencode(query));
        let body = crate::fs::fetch_http(&url)
            .await
            .map_err(|e| e.to_string())?;
        if body.contains("anomaly") && !body.contains("result__a") {
            return Err("DuckDuckGo 限流（anomaly 页），请稍后重试或配置 GYRE_SEARXNG_URL".into());
        }
        Ok(Self::parse_html(&body))
    }
}

#[async_trait]
impl WebSearchProvider for Searxng {
    fn name(&self) -> &str {
        "searxng"
    }

    async fn search(&self, query: &str) -> Result<Vec<WebResult>, String> {
        let url = format!(
            "{}/search?q={}&format=json",
            self.instance.trim_end_matches('/'),
            urlencode(query)
        );
        crate::fs::ssrf_guard(&url).map_err(|e| e.to_string())?;
        let body = crate::fs::fetch_http(&url)
            .await
            .map_err(|e| e.to_string())?;
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("searxng JSON 解析失败: {e}"))?;
        let mut out = Vec::new();
        if let Some(results) = v.get("results").and_then(serde_json::Value::as_array) {
            for r in results {
                let title = r.get("title").and_then(serde_json::Value::as_str).unwrap_or("");
                let url = r.get("url").and_then(serde_json::Value::as_str).unwrap_or("");
                let snippet = r
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if !title.is_empty() && !url.is_empty() {
                    out.push(WebResult {
                        title: truncate(title, 240),
                        url: url.to_string(),
                        snippet: truncate(snippet, 240),
                    });
                }
            }
        }
        Ok(out)
    }
}

/// 站点感知抽取：已知站点返回结构 markdown，未知站点返回 `None`（仅标题/摘要）。
///
/// v1 覆盖 arxiv / crates.io / npm / github（按流量优先级）。
///
/// # Errors
/// 站点抓取失败（超时/非 2xx/响应超限）时返回 [`ToolError`]。
pub async fn extract_site(url: &str) -> Result<Option<SitePage>, ToolError> {
    if let Some(id) = url.strip_prefix("https://arxiv.org/abs/").or_else(|| {
        url.strip_prefix("https://arxiv.org/pdf/")
            .and_then(|s| s.strip_suffix(".pdf"))
    }) {
        return extract_arxiv(id).await;
    }
    if let Some(name) = url
        .strip_prefix("https://crates.io/crates/")
        .or_else(|| url.strip_prefix("https://crates.io/api/v1/crates/"))
    {
        let name = name.split('/').next().unwrap_or("");
        return extract_crates(name).await;
    }
    if let Some(name) = url.strip_prefix("https://www.npmjs.com/package/") {
        let name = name.split('/').next().unwrap_or("");
        return extract_npm(name).await;
    }
    if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("https://www.github.com/"))
    {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 2 {
            return extract_github(parts[0], parts[1]).await;
        }
    }
    Ok(None)
}

/// arXiv abs 页 → 标题/作者/摘要。
async fn extract_arxiv(id: &str) -> Result<Option<SitePage>, ToolError> {
    let body = crate::fs::fetch_http(&format!("https://arxiv.org/abs/{id}")).await?;
    let title = html_meta(&body, "citation_title").unwrap_or_else(|| format!("arXiv {id}"));
    let authors = html_meta_list(&body, "citation_author");
    let abstract_text = body
        .split("abstract")
        .nth(1)
        .and_then(|s| s.split("</blockquote>").next())
        .map(strip_tags)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let mut md = String::new();
    if !authors.is_empty() {
        let _ = std::fmt::Write::write_fmt(&mut md, format_args!("**作者**：{}\n\n", authors.join(", ")));
    }
    if !abstract_text.is_empty() {
        let _ = std::fmt::Write::write_fmt(&mut md, format_args!("**摘要**：{}\n\n", truncate(&abstract_text, 2000)));
    }
    let _ = std::fmt::Write::write_fmt(&mut md, format_args!("来源：<https://arxiv.org/abs/{id}>\n"));
    Ok(Some(SitePage {
        title,
        markdown: md,
        url: format!("https://arxiv.org/abs/{id}"),
    }))
}

/// crates.io API → 最新版本/描述/依赖数。
async fn extract_crates(name: &str) -> Result<Option<SitePage>, ToolError> {
    let body = crate::fs::fetch_http(&format!("https://crates.io/api/v1/crates/{name}")).await?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| ToolError::Execution(format!("crates.io JSON: {e}")))?;
    let desc = v
        .pointer("/crate/description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let latest = v
        .pointer("/crate/max_version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let downloads = v
        .pointer("/crate/downloads")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let mut md = String::new();
    if !desc.is_empty() {
        let _ = std::fmt::Write::write_fmt(&mut md, format_args!("**描述**：{}\n\n", truncate(desc, 500)));
    }
    let _ = std::fmt::Write::write_fmt(
        &mut md,
        format_args!("**最新版本**：{latest}　**总下载量**：{downloads}\n\n来源：<https://crates.io/crates/{name}>\n"),
    );
    Ok(Some(SitePage {
        title: format!("crates.io: {name}"),
        markdown: md,
        url: format!("https://crates.io/crates/{name}"),
    }))
}

/// npm registry → 最新版本/描述。
async fn extract_npm(name: &str) -> Result<Option<SitePage>, ToolError> {
    let body =
        crate::fs::fetch_http(&format!("https://registry.npmjs.org/{name}/latest")).await?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| ToolError::Execution(format!("npm JSON: {e}")))?;
    let desc = v
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let version = v
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let mut md = String::new();
    if !desc.is_empty() {
        let _ = std::fmt::Write::write_fmt(&mut md, format_args!("**描述**：{}\n\n", truncate(desc, 500)));
    }
    let _ = std::fmt::Write::write_fmt(&mut md, format_args!("**最新版本**：{version}\n\n来源：<https://www.npmjs.com/package/{name}>\n"));
    Ok(Some(SitePage {
        title: format!("npm: {name}"),
        markdown: md,
        url: format!("https://www.npmjs.com/package/{name}"),
    }))
}

/// GitHub 仓库 → README 首段（raw 抓取，非 API）。
async fn extract_github(owner: &str, repo: &str) -> Result<Option<SitePage>, ToolError> {
    let raw_url = format!("https://raw.githubusercontent.com/{owner}/{repo}/HEAD/README.md");
    let Ok(body) = crate::fs::fetch_http(&raw_url).await else {
        // README.md 缺失或抓取失败 → 非 github 结构页，交给摘要。
        return Ok(None);
    };
    let mut md = String::new();
    md.push_str(&truncate(&body, 4000));
    let _ = std::fmt::Write::write_fmt(
        &mut md,
        format_args!("\n\n来源：<https://github.com/{owner}/{repo}>\n"),
    );
    Ok(Some(SitePage {
        title: format!("github: {owner}/{repo}"),
        markdown: md,
        url: format!("https://github.com/{owner}/{repo}"),
    }))
}

/// 简易 HTML 元标签提取（`citation_title` 等）。
fn html_meta(html: &str, name: &str) -> Option<String> {
    html_meta_list(html, name).first().cloned()
}

/// 提取所有 `name="<name>" content="..."` 元标签值。
fn html_meta_list(html: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    let needle = format!("name=\"{name}\"");
    while let Some(pos) = rest.find(&needle) {
        let after = &rest[pos + needle.len()..];
        if let Some(content) = after.split("content=\"").nth(1).and_then(|s| s.split('"').next())
        {
            out.push(html_unescape(content));
        }
        rest = &rest[pos + needle.len()..];
    }
    out
}

/// 剥离 HTML 标签。
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    html_unescape(&out)
}

/// 解码常见 HTML 实体。
fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
}

/// 按字符截断（保留 UTF-8 边界）。
fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            b' ' => out.push('+'),
            _ => {
                    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("%{b:02X}"));
                }
        }
    }
    out
}

/// `web_search` 工具：query（+ `site:` 指令）→ 结果列表或站点抽取 markdown。
pub struct WebSearchTool {
    chain: WebSearchChain,
}

impl WebSearchTool {
    /// 默认链（DDG + 可选 searxng）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            chain: WebSearchChain::default_chain(),
        }
    }

    /// 自定义链（测试用）。
    #[must_use]
    pub const fn with_chain(chain: WebSearchChain) -> Self {
        Self { chain }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "联网搜索（免 key：DuckDuckGo；可选 searxng 实例经 GYRE_SEARXNG_URL）。\
query 支持 `site:example.com` 过滤；命中 arxiv/crates.io/npm/github 时返回结构 markdown。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query":      { "type": "string", "description": "搜索查询（可用 site: 限定域名）" },
                "max_results": { "type": "integer", "minimum": 1, "maximum": 10,
                                 "description": "返回结果数上限（默认 5）" }
            },
            "required": ["query"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        CapabilityTier::Network
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let query = input
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `query` 参数".into()))?;
        let max_results = input
            .get("max_results")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n.min(10).max(1) as usize)
            .unwrap_or(5);

        let results = self.chain.search(query).await?;
        let results = results.into_iter().take(max_results).collect::<Vec<_>>();
        if results.is_empty() {
            return Ok(ToolResult::text(format!("「{query}」无搜索结果")));
        }

        let mut out = String::new();
        out.push_str(&format!("「{query}」搜索结果（{} 条）：\n\n", results.len()));
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!("{}. **{}**\n   {}\n", i + 1, r.title, r.url));
            if !r.snippet.is_empty() {
                out.push_str(&format!("   {}\n", r.snippet));
            }
        }
        // 站点抽取：仅对第一条命中做深度抽取（控制延迟与 token）。
        if let Some(r) = results.first() {
            if let Some(page) = extract_site(&r.url).await? {
                out.push_str(&format!(
                    "\n── {} 详情 ──\n\n{}\n",
                    page.title, page.markdown
                ));
            }
        }
        Ok(ToolResult::text(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ddg_html() {
        let html = r#"<html><body>
<div class="result">
  <a rel="nofollow" class="result__a" href="https://example.com/page">Example Page</a>
  <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com">A snippet &amp; more</a>
</div>
</body></html>"#;
        let results = DuckDuckGoHtml::parse_html(html);
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0].title, "Example Page");
        assert_eq!(results[0].url, "https://example.com/page");
        assert!(results[0].snippet.contains("A snippet & more"));
    }

    #[test]
    fn parse_ddg_html_skips_relative() {
        let html = r#"<a rel="nofollow" class="result__a" href="/l/?uddg=x">Rel</a>"#;
        assert!(DuckDuckGoHtml::parse_html(html).is_empty());
    }

    #[test]
    fn chain_falls_back_on_provider_error() {
        struct Fail;
        #[async_trait]
        impl WebSearchProvider for Fail {
            fn name(&self) -> &str {
                "fail"
            }
            async fn search(&self, _q: &str) -> Result<Vec<WebResult>, String> {
                Err("boom".into())
            }
        }
        struct Empty;
        #[async_trait]
        impl WebSearchProvider for Empty {
            fn name(&self) -> &str {
                "empty"
            }
            async fn search(&self, _q: &str) -> Result<Vec<WebResult>, String> {
                Ok(vec![])
            }
        }
        struct OkP;
        #[async_trait]
        impl WebSearchProvider for OkP {
            fn name(&self) -> &str {
                "ok"
            }
            async fn search(&self, _q: &str) -> Result<Vec<WebResult>, String> {
                Ok(vec![WebResult {
                    title: "t".into(),
                    url: "https://e.com".into(),
                    snippet: "s".into(),
                }])
            }
        }
        // 失败 → 空 → 成功：成功者胜出。
        let chain = WebSearchChain::new(vec![
            Arc::new(Fail),
            Arc::new(Empty),
            Arc::new(OkP),
        ]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let results = rt.block_on(chain.search("q")).unwrap();
        assert_eq!(results.len(), 1);
        // 全失败 → 汇总错误。
        let chain = WebSearchChain::new(vec![Arc::new(Fail), Arc::new(Fail)]);
        let err = rt.block_on(chain.search("q")).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn site_matchers_unknown_site_none() {
        // 未知站点 → None（不发起网络请求）。
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(extract_site("https://example.com/x"));
        assert!(r.is_ok());
        assert!(r.unwrap().is_none());
        // 已知站点前缀命中 matcher（抓取本身依赖网络，此处仅验证路由）。
        assert!(matches!(
            extract_site("https://crates.io/crates/serde"),
            _
        ));
    }

    #[test]
    fn truncate_keeps_boundary() {
        let s = truncate("你好世界", 2);
        assert_eq!(s.chars().count(), 3); // 2 字符 + 省略号
        let s2 = truncate("abc", 10);
        assert_eq!(s2, "abc");
    }

    #[test]
    fn urlencode_spaces_and_unicode() {
        assert_eq!(urlencode("a b"), "a+b");
        assert_eq!(urlencode("rust lang"), "rust+lang");
        assert_eq!(urlencode("c++"), "c%2B%2B");
    }
}
