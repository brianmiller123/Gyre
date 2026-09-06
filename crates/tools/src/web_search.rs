//! `web_search` 工具：多 provider 顺序回退链 + 高频站点感知抽取。
//!
//! 移植 oh-my-pi `coding-agent/src/web/search/` 的简化版：
//! - API provider（omp `providers/tavily.ts`、`brave.ts` 移植）优先：`TAVILY_API_KEY`
//!   启用 Tavily、`BRAVE_API_KEY` 启用 Brave（懒加载，未配置即跳过）；
//! - 免 key `DuckDuckGo` HTML 兜底；`searxng` 实例经 `GYRE_SEARXNG_URL` env 可选启用；
//! - `recency` 结果时限过滤：Tavily `time_range`、Brave `freshness`（pd/pw/pm/py）、
//!   DDG `df`（d/w/m/y）、searxng `time_range`（无原生周窗口，week 降级为 month）；
//!   API provider 401/403/429/5xx → provider 错误 → 链自动回退；结果头部附「来源 + 时限」注记；
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

/// 结果时限窗口（omp `recency`：纯时间过滤，不改变主题与排序策略）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recency {
    /// 近一天。
    Day,
    /// 近一周。
    Week,
    /// 近一月。
    Month,
    /// 近一年。
    Year,
}

impl Recency {
    /// 解析 schema 枚举值（`day`/`week`/`month`/`year`，大小写不敏感）。
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "day" => Some(Self::Day),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            "year" => Some(Self::Year),
            _ => None,
        }
    }

    /// 中文注记（结果头部「时限：近 X」用）。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Day => "近一天",
            Self::Week => "近一周",
            Self::Month => "近一月",
            Self::Year => "近一年",
        }
    }
}

/// web 搜索 provider。
#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    /// provider 名（诊断用）。
    fn name(&self) -> &str;
    /// 按 query 搜索，返回结果列表（空 = 无结果，不视为失败）。
    /// `recency` 为可选时限过滤：provider 应尽量原生映射，不支持时按约定降级或忽略。
    /// `max_results` 为期望结果数上限：支持的原生透传（API provider），不支持的可忽略。
    async fn search(
        &self,
        query: &str,
        recency: Option<Recency>,
        max_results: usize,
    ) -> Result<Vec<WebResult>, String>;
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
    /// 默认链：有 key 的 API provider（Tavily → Brave）优先，其后 DDG →
    /// （env 配置了 `GYRE_SEARXNG_URL` 时）searxng。
    #[must_use]
    pub fn default_chain() -> Self {
        let mut providers: Vec<Arc<dyn WebSearchProvider>> = Vec::new();
        if let Ok(key) = std::env::var(TAVILY_API_KEY_ENV) {
            if !key.trim().is_empty() {
                providers.push(Arc::new(Tavily { api_key: key }));
            }
        }
        if let Ok(key) = std::env::var(BRAVE_API_KEY_ENV) {
            if !key.trim().is_empty() {
                providers.push(Arc::new(Brave { api_key: key }));
            }
        }
        providers.push(Arc::new(DuckDuckGoHtml));
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

    /// 依序搜索，返回首个非空结果及其实际命中的 provider 名（结果头部注记用）；
    /// provider 失败（含 401/403/429/5xx）记录并自动 fallback 到下一 provider。
    /// `recency` 与 `max_results` 透传给各 provider。
    ///
    /// # Errors
    /// 所有 provider 均失败或无结果时返回 [`ToolError::Execution`]（附各 provider 错误）。
    pub async fn search(
        &self,
        query: &str,
        recency: Option<Recency>,
        max_results: usize,
    ) -> Result<(String, Vec<WebResult>), ToolError> {
        let mut errors = Vec::new();
        for p in &self.providers {
            match p.search(query, recency, max_results).await {
                Ok(results) if !results.is_empty() => {
                    return Ok((p.name().to_string(), results));
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
// ── API provider：Tavily / Brave（omp tavily.ts / brave.ts 请求、解析、错误语义移植）──

/// Tavily Search API 端点（omp `TAVILY_SEARCH_URL`）。
const TAVILY_SEARCH_URL: &str = "https://api.tavily.com/search";
/// Brave Web Search API 端点（omp `BRAVE_SEARCH_URL`）。
const BRAVE_SEARCH_URL: &str = "https://api.search.brave.com/res/v1/web/search";
/// Tavily API key 环境变量名。
const TAVILY_API_KEY_ENV: &str = "TAVILY_API_KEY";
/// Brave API key 环境变量名。
const BRAVE_API_KEY_ENV: &str = "BRAVE_API_KEY";
/// 搜索 API 成功响应体上限（与 [`crate::fs::fetch_http`] 同款 1 MiB 流式截断）。
const API_BODY_MAX_BYTES: usize = 1024 * 1024;
/// 错误响应体读取上限（omp `MAX_ERROR_BYTES`：8 KiB，仅用于提取错误消息）。
const API_ERROR_BODY_MAX_BYTES: usize = 8 * 1024;

/// 搜索 API 共享 HTTP client：复用 [`crate::fs::fetch_http`] 的超时/UA 配置
/// （15s 总超时 / 5s 连接超时 / 身份 UA / 禁自动重定向），连接池跨调用复用。
static API_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(5))
        .user_agent(concat!(
            "gyre-agent/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/Gyre)"
        ))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("构造搜索 HTTP client 失败")
});

/// 搜索 API 共享 HTTP client 句柄（[`API_CLIENT`] 的只读访问）。
fn api_client() -> &'static reqwest::Client {
    &API_CLIENT
}

/// 流式读取响应体并按 `max` 字节上限中止（防大响应体整段入内存）。
async fn read_body_capped(mut resp: reqwest::Response, max: usize) -> Result<String, String> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| format!("读取响应失败: {e}"))?;
        let Some(chunk) = chunk else { break };
        buf.extend_from_slice(&chunk);
        if buf.len() > max {
            return Err(format!("响应体超过 {max} 字节上限"));
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// 错误体是否命中配额/额度信号（omp `CREDIT_BODY_PATTERN`：
/// `credits exhausted|exceeded`、`quota`、`insufficient`，大小写不敏感）。
fn credit_body(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("quota")
        || b.contains("insufficient")
        || (b.contains("credit") && (b.contains("exhausted") || b.contains("exceeded")))
}

/// 非 2xx → 可读 provider 错误消息（omp `classifyProviderHttpError` 语义：
/// 鉴权/额度类压缩为 provider 标签短句，其余透传状态码 + 截断错误体；
/// 错误统一走链的 provider 错误路径自动 fallback）。
fn classify_api_error(provider: &str, status: u16, body: &str) -> String {
    match status {
        401 => format!("{provider}: 401 unauthorized"),
        402 => format!("{provider}: 402 credits exhausted"),
        403 => format!("{provider}: 403 forbidden"),
        _ if credit_body(body) => format!("{provider}: credits exhausted"),
        _ if body.trim().is_empty() => format!("{provider} API error ({status})"),
        _ => format!(
            "{provider} API error ({status}): {}",
            truncate(body.trim(), 200)
        ),
    }
}

/// Tavily Search API provider（`TAVILY_API_KEY` 配置启用）。
pub struct Tavily {
    api_key: String,
}

impl Tavily {
    /// recency → Tavily `time_range`（omp：recency 与原生档位一一对应；仅作时间过滤，
    /// 不切换 `topic`，避免把技术查询收窄成 news 索引）。
    fn time_range_param(recency: Recency) -> &'static str {
        match recency {
            Recency::Day => "day",
            Recency::Week => "week",
            Recency::Month => "month",
            Recency::Year => "year",
        }
    }

    /// 组装请求体（omp `buildRequestBody` 精简版）：basic 深度 + 结果数上限；
    /// `time_range` 仅在指定 recency 时附加。
    fn request_body(
        query: &str,
        recency: Option<Recency>,
        max_results: usize,
    ) -> serde_json::Value {
        let mut body = json!({
            "query": query,
            "search_depth": "basic",
            "max_results": max_results,
        });
        if let Some(r) = recency {
            body["time_range"] = json!(Self::time_range_param(r));
        }
        body
    }

    /// 解析 `results[]`：`content` 为摘要；缺 title 回退 URL，缺 URL 跳过（omp
    /// `toSearchResponse`）。
    fn parse_json(body: &str) -> Result<Vec<WebResult>, String> {
        let v: serde_json::Value =
            serde_json::from_str(body).map_err(|e| format!("tavily JSON 解析失败: {e}"))?;
        let mut out = Vec::new();
        if let Some(results) = v.get("results").and_then(serde_json::Value::as_array) {
            for r in results {
                let url = r
                    .get("url")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if url.is_empty() {
                    continue;
                }
                let title = r
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .filter(|t| !t.is_empty())
                    .unwrap_or(url);
                let snippet = r
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                out.push(WebResult {
                    title: truncate(title, 240),
                    url: url.to_string(),
                    snippet: truncate(snippet, 240),
                });
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl WebSearchProvider for Tavily {
    fn name(&self) -> &str {
        "tavily"
    }

    async fn search(
        &self,
        query: &str,
        recency: Option<Recency>,
        max_results: usize,
    ) -> Result<Vec<WebResult>, String> {
        // 端点为编译期常量公网 https，无需 SSRF 校验；禁自动重定向由 client 保证。
        let resp = api_client()
            .post(TAVILY_SEARCH_URL)
            .header("Content-Type", "application/json")
            .bearer_auth(&self.api_key)
            .body(Self::request_body(query, recency, max_results).to_string())
            .send()
            .await
            .map_err(|e| format!("请求 Tavily 失败: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let err_body = read_body_capped(resp, API_ERROR_BODY_MAX_BYTES)
                .await
                .unwrap_or_default();
            return Err(classify_api_error("tavily", status, &err_body));
        }
        let text = read_body_capped(resp, API_BODY_MAX_BYTES).await?;
        Self::parse_json(&text)
    }
}

/// Brave Web Search API provider（`BRAVE_API_KEY` 配置启用）。
pub struct Brave {
    api_key: String,
}

impl Brave {
    /// recency → Brave `freshness`（omp `RECENCY_MAP`：pd/pw/pm/py 四档原生支持）。
    fn freshness_param(recency: Recency) -> &'static str {
        match recency {
            Recency::Day => "pd",
            Recency::Week => "pw",
            Recency::Month => "pm",
            Recency::Year => "py",
        }
    }

    /// 组装请求 URL：`q`/`count`/`freshness` + `text_decorations=false`（去 `<b>` 装饰，
    /// Gyre 不做 HTML 清洗）+ `extra_snippets`（补充摘要）；`freshness` 仅在指定
    /// recency 时附加。
    fn request_url(query: &str, recency: Option<Recency>, max_results: usize) -> String {
        let mut url = format!(
            "{BRAVE_SEARCH_URL}?q={}&count={max_results}&extra_snippets=true&text_decorations=false&safesearch=moderate",
            urlencode(query)
        );
        if let Some(r) = recency {
            url.push_str("&freshness=");
            url.push_str(Self::freshness_param(r));
        }
        url
    }

    /// 解析 `web.results[]`：`description`（+ `extra_snippets` 合并）为摘要；
    /// 非 http(s) 链接跳过，缺 title 回退 URL（omp `searchBrave`）。
    fn parse_json(body: &str) -> Result<Vec<WebResult>, String> {
        let v: serde_json::Value =
            serde_json::from_str(body).map_err(|e| format!("brave JSON 解析失败: {e}"))?;
        let mut out = Vec::new();
        if let Some(results) = v
            .pointer("/web/results")
            .and_then(serde_json::Value::as_array)
        {
            for r in results {
                let url = r
                    .get("url")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    continue;
                }
                let title = r
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .unwrap_or(url);
                let mut snippet = r
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(extras) = r
                    .get("extra_snippets")
                    .and_then(serde_json::Value::as_array)
                {
                    for e in extras.iter().filter_map(serde_json::Value::as_str) {
                        if !e.is_empty() {
                            snippet.push('\n');
                            snippet.push_str(e);
                        }
                    }
                }
                out.push(WebResult {
                    title: truncate(title, 240),
                    url: url.to_string(),
                    snippet: truncate(snippet.trim(), 240),
                });
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl WebSearchProvider for Brave {
    fn name(&self) -> &str {
        "brave"
    }

    async fn search(
        &self,
        query: &str,
        recency: Option<Recency>,
        max_results: usize,
    ) -> Result<Vec<WebResult>, String> {
        let url = Self::request_url(query, recency, max_results);
        // 端点 host 为编译期常量公网域名，无需 SSRF 校验；禁自动重定向由 client 保证。
        let resp = api_client()
            .get(&url)
            .header("Accept", "application/json")
            .header("X-Subscription-Token", &self.api_key)
            .send()
            .await
            .map_err(|e| format!("请求 Brave 失败: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let err_body = read_body_capped(resp, API_ERROR_BODY_MAX_BYTES)
                .await
                .unwrap_or_default();
            return Err(classify_api_error("brave", status, &err_body));
        }
        let text = read_body_capped(resp, API_BODY_MAX_BYTES).await?;
        Self::parse_json(&text)
    }
}

impl DuckDuckGoHtml {
    /// recency → DDG HTML `df` 单字母时间过滤（omp `RECENCY_TO_DDG_DF`：d/w/m/y 四档原生支持）。
    fn df_param(recency: Recency) -> &'static str {
        match recency {
            Recency::Day => "d",
            Recency::Week => "w",
            Recency::Month => "m",
            Recency::Year => "y",
        }
    }

    /// 组装 HTML 端点请求 URL；`df` 仅在指定 recency 时附加。
    fn request_url(query: &str, recency: Option<Recency>) -> String {
        let mut url = format!("https://html.duckduckgo.com/html/?q={}", urlencode(query));
        if let Some(r) = recency {
            url.push_str("&df=");
            url.push_str(Self::df_param(r));
        }
        url
    }

    /// HTML 端点解析：宽松抓 `result__a` 链接（标题 + href），相邻 `result__snippet` 为摘要。
    /// 免 key；被限流时返回可读错误。
    fn parse_html(html: &str) -> Vec<WebResult> {
        let mut out = Vec::new();
        for block in html.split("result__a") {
            // 每个结果块：`<a ... href="URL">TITLE</a>` 后跟 snippet 区。
            let Some(href) = block
                .split("href=\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
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
    fn name(&self) -> &'static str {
        "duckduckgo-html"
    }

    async fn search(
        &self,
        query: &str,
        recency: Option<Recency>,
        _max_results: usize,
    ) -> Result<Vec<WebResult>, String> {
        let url = Self::request_url(query, recency);
        let body = crate::fs::fetch_http(&url)
            .await
            .map_err(|e| e.to_string())?;
        if body.contains("anomaly") && !body.contains("result__a") {
            return Err("DuckDuckGo 限流（anomaly 页），请稍后重试或配置 GYRE_SEARXNG_URL".into());
        }
        Ok(Self::parse_html(&body))
    }
}

impl Searxng {
    /// recency → SearXNG `time_range`（omp `RECENCY_MAP`：原生仅 day/month/year，
    /// week 无原生窗口，降级为 month）。
    fn time_range_param(recency: Recency) -> &'static str {
        match recency {
            Recency::Day => "day",
            Recency::Week | Recency::Month => "month",
            Recency::Year => "year",
        }
    }

    /// 组装 JSON API 请求 URL；`time_range` 仅在指定 recency 时附加。
    fn request_url(instance: &str, query: &str, recency: Option<Recency>) -> String {
        let mut url = format!(
            "{}/search?q={}&format=json",
            instance.trim_end_matches('/'),
            urlencode(query)
        );
        if let Some(r) = recency {
            url.push_str("&time_range=");
            url.push_str(Self::time_range_param(r));
        }
        url
    }
}

#[async_trait]
impl WebSearchProvider for Searxng {
    fn name(&self) -> &'static str {
        "searxng"
    }

    async fn search(
        &self,
        query: &str,
        recency: Option<Recency>,
        _max_results: usize,
    ) -> Result<Vec<WebResult>, String> {
        let url = Self::request_url(&self.instance, query, recency);
        crate::fs::ssrf_guard(&url).map_err(|e| e.to_string())?;
        let body = crate::fs::fetch_http(&url)
            .await
            .map_err(|e| e.to_string())?;
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("searxng JSON 解析失败: {e}"))?;
        let mut out = Vec::new();
        if let Some(results) = v.get("results").and_then(serde_json::Value::as_array) {
            for r in results {
                let title = r
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let url = r
                    .get("url")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
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
        let _ = std::fmt::Write::write_fmt(
            &mut md,
            format_args!("**作者**：{}\n\n", authors.join(", ")),
        );
    }
    if !abstract_text.is_empty() {
        let _ = std::fmt::Write::write_fmt(
            &mut md,
            format_args!("**摘要**：{}\n\n", truncate(&abstract_text, 2000)),
        );
    }
    let _ = std::fmt::Write::write_fmt(
        &mut md,
        format_args!("来源：<https://arxiv.org/abs/{id}>\n"),
    );
    Ok(Some(SitePage {
        title,
        markdown: md,
        url: format!("https://arxiv.org/abs/{id}"),
    }))
}

/// crates.io API → 最新版本/描述/依赖数。
async fn extract_crates(name: &str) -> Result<Option<SitePage>, ToolError> {
    let body = crate::fs::fetch_http(&format!("https://crates.io/api/v1/crates/{name}")).await?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| ToolError::Execution(format!("crates.io JSON: {e}")))?;
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
        let _ = std::fmt::Write::write_fmt(
            &mut md,
            format_args!("**描述**：{}\n\n", truncate(desc, 500)),
        );
    }
    let _ = std::fmt::Write::write_fmt(
        &mut md,
        format_args!(
            "**最新版本**：{latest}　**总下载量**：{downloads}\n\n来源：<https://crates.io/crates/{name}>\n"
        ),
    );
    Ok(Some(SitePage {
        title: format!("crates.io: {name}"),
        markdown: md,
        url: format!("https://crates.io/crates/{name}"),
    }))
}

/// npm registry → 最新版本/描述。
async fn extract_npm(name: &str) -> Result<Option<SitePage>, ToolError> {
    let body = crate::fs::fetch_http(&format!("https://registry.npmjs.org/{name}/latest")).await?;
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
        let _ = std::fmt::Write::write_fmt(
            &mut md,
            format_args!("**描述**：{}\n\n", truncate(desc, 500)),
        );
    }
    let _ = std::fmt::Write::write_fmt(
        &mut md,
        format_args!("**最新版本**：{version}\n\n来源：<https://www.npmjs.com/package/{name}>\n"),
    );
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
        if let Some(content) = after
            .split("content=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
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
    /// 默认链（Tavily/Brave 有 key 优先，其后 DDG + 可选 searxng）。
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
    fn name(&self) -> &'static str {
        "web_search"
    }
    fn description(&self) -> &'static str {
        "联网搜索（多 provider 顺序回退链：配置 TAVILY_API_KEY / BRAVE_API_KEY 时 \
Tavily / Brave API 优先，其后免 key DuckDuckGo；可选 searxng 实例经 GYRE_SEARXNG_URL）。\
query 支持 `site:example.com` 过滤；命中 arxiv/crates.io/npm/github 时返回结构 markdown。\
`recency` 为结果时限过滤（近一天/周/月/年）：Tavily/Brave/DuckDuckGo 原生支持；searxng 无原生周窗口，week 降级为 month。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query":      { "type": "string", "description": "搜索查询（可用 site: 限定域名）" },
                "max_results": { "type": "integer", "minimum": 1, "maximum": 10,
                                 "description": "返回结果数上限（默认 5）" },
                "recency":     { "type": "string", "enum": ["day", "week", "month", "year"],
                                 "description": "结果时限过滤（可选）；searxng 后端无原生周窗口，week 降级为 month" }
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
            .map_or(5, |n| n.clamp(1, 10) as usize);
        let recency = match input.get("recency").and_then(serde_json::Value::as_str) {
            None | Some("") => None,
            Some(s) => Some(Recency::parse(s).ok_or_else(|| {
                ToolError::InvalidArgs(format!(
                    "无效的 `recency` 值 `{s}`（可选 day/week/month/year）"
                ))
            })?),
        };

        let (provider, results) = self.chain.search(query, recency, max_results).await?;
        let results = results.into_iter().take(max_results).collect::<Vec<_>>();
        if results.is_empty() {
            return Ok(ToolResult::text(format!("「{query}」无搜索结果")));
        }

        let mut out = String::new();
        let note = recency.map_or(String::new(), |r| format!("，时限：{}", r.label()));
        out.push_str(&format!(
            "「{query}」搜索结果（{} 条，来源：{provider}{note}）：\n\n",
            results.len()
        ));
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
            fn name(&self) -> &'static str {
                "fail"
            }
            async fn search(
                &self,
                _q: &str,
                _recency: Option<Recency>,
                _max_results: usize,
            ) -> Result<Vec<WebResult>, String> {
                Err("boom".into())
            }
        }
        struct Empty;
        #[async_trait]
        impl WebSearchProvider for Empty {
            fn name(&self) -> &'static str {
                "empty"
            }
            async fn search(
                &self,
                _q: &str,
                _recency: Option<Recency>,
                _max_results: usize,
            ) -> Result<Vec<WebResult>, String> {
                Ok(vec![])
            }
        }
        struct OkP;
        #[async_trait]
        impl WebSearchProvider for OkP {
            fn name(&self) -> &'static str {
                "ok"
            }
            async fn search(
                &self,
                _q: &str,
                _recency: Option<Recency>,
                _max_results: usize,
            ) -> Result<Vec<WebResult>, String> {
                Ok(vec![WebResult {
                    title: "t".into(),
                    url: "https://e.com".into(),
                    snippet: "s".into(),
                }])
            }
        }
        // 失败 → 空 → 成功：成功者胜出。
        let chain = WebSearchChain::new(vec![Arc::new(Fail), Arc::new(Empty), Arc::new(OkP)]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (winner, results) = rt.block_on(chain.search("q", None, 5)).unwrap();
        assert_eq!(winner, "ok");
        assert_eq!(results.len(), 1);
        // 全失败 → 汇总错误。
        let chain = WebSearchChain::new(vec![Arc::new(Fail), Arc::new(Fail)]);
        let err = rt.block_on(chain.search("q", None, 5)).unwrap_err();
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
        assert!(matches!(extract_site("https://crates.io/crates/serde"), _));
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

    #[test]
    fn recency_parse_and_label() {
        assert_eq!(Recency::parse("day"), Some(Recency::Day));
        assert_eq!(Recency::parse(" week "), Some(Recency::Week));
        assert_eq!(Recency::parse("MONTH"), Some(Recency::Month));
        assert_eq!(Recency::parse("year"), Some(Recency::Year));
        assert_eq!(Recency::parse("hour"), None);
        assert_eq!(Recency::parse(""), None);
        assert_eq!(Recency::Day.label(), "近一天");
        assert_eq!(Recency::Week.label(), "近一周");
        assert_eq!(Recency::Month.label(), "近一月");
        assert_eq!(Recency::Year.label(), "近一年");
    }

    #[test]
    fn ddg_url_recency_maps_df_param() {
        // omp `RECENCY_TO_DDG_DF`：d/w/m/y 四档原生支持。
        let cases = [
            (Recency::Day, "d"),
            (Recency::Week, "w"),
            (Recency::Month, "m"),
            (Recency::Year, "y"),
        ];
        for (r, df) in cases {
            assert_eq!(
                DuckDuckGoHtml::request_url("rust lang", Some(r)),
                format!("https://html.duckduckgo.com/html/?q=rust+lang&df={df}")
            );
        }
        // 未传 → 不带 df 参数。
        assert_eq!(
            DuckDuckGoHtml::request_url("rust lang", None),
            "https://html.duckduckgo.com/html/?q=rust+lang"
        );
    }

    #[test]
    fn searxng_url_recency_maps_time_range() {
        // omp `RECENCY_MAP`：原生仅 day/month/year，week 降级为 month。
        let cases = [
            (Recency::Day, "day"),
            (Recency::Week, "month"),
            (Recency::Month, "month"),
            (Recency::Year, "year"),
        ];
        for (r, tr) in cases {
            assert_eq!(
                Searxng::request_url("http://s.local/", "rust lang", Some(r)),
                format!("http://s.local/search?q=rust+lang&format=json&time_range={tr}")
            );
        }
        // 未传 → 不带 time_range；实例尾斜杠已剥。
        assert_eq!(
            Searxng::request_url("http://s.local//", "q", None),
            "http://s.local/search?q=q&format=json"
        );
    }

    #[test]
    fn tavily_request_body_maps_recency_and_max_results() {
        // omp：recency 原生 day/week/month/year 一一映射 time_range。
        let body = Tavily::request_body("rust lang", Some(Recency::Week), 8);
        assert_eq!(body["query"], "rust lang");
        assert_eq!(body["search_depth"], "basic");
        assert_eq!(body["max_results"], 8);
        assert_eq!(body["time_range"], "week");
        // 未传 recency → 不带 time_range。
        let body = Tavily::request_body("rust lang", None, 5);
        assert!(body.get("time_range").is_none());
        assert_eq!(body["max_results"], 5);
    }

    #[test]
    fn tavily_parse_fixture() {
        let fixture = r#"{
            "query": "rust async",
            "results": [
                {"title": "Async Book", "url": "https://rust-lang.github.io/async-book/", "content": "Async programming in Rust."},
                {"title": "", "url": "https://example.com/no-title", "content": "无标题回退 URL。"},
                {"url": "https://example.com/no-content"},
                {"title": "缺 URL", "url": "", "content": "无链接条目跳过。"}
            ]
        }"#;
        let results = Tavily::parse_json(fixture).unwrap();
        assert_eq!(results.len(), 3, "{results:?}");
        assert_eq!(results[0].title, "Async Book");
        assert_eq!(results[0].url, "https://rust-lang.github.io/async-book/");
        assert_eq!(results[0].snippet, "Async programming in Rust.");
        // 缺 title → 回退 URL；缺 content → 空摘要。
        assert_eq!(results[1].title, "https://example.com/no-title");
        assert_eq!(results[2].snippet, "");
        // 非 JSON → 解析错误（provider 错误 → 链 fallback）。
        assert!(Tavily::parse_json("not json").is_err());
    }

    #[test]
    fn brave_url_recency_maps_freshness() {
        // omp `RECENCY_MAP`：pd/pw/pm/py 四档原生支持。
        let cases = [
            (Recency::Day, "pd"),
            (Recency::Week, "pw"),
            (Recency::Month, "pm"),
            (Recency::Year, "py"),
        ];
        for (r, f) in cases {
            let url = Brave::request_url("rust lang", Some(r), 5);
            assert!(
                url.starts_with("https://api.search.brave.com/res/v1/web/search?q=rust+lang&"),
                "{url}"
            );
            assert!(url.contains("&count=5&"), "{url}");
            assert!(url.contains(&format!("&freshness={f}")), "{url}");
            assert!(url.contains("text_decorations=false"), "{url}");
            assert!(url.contains("safesearch=moderate"), "{url}");
        }
        // 未传 recency → 不带 freshness。
        let url = Brave::request_url("rust lang", None, 10);
        assert!(!url.contains("freshness"), "{url}");
        assert!(url.contains("count=10"), "{url}");
    }

    #[test]
    fn brave_parse_fixture() {
        let fixture = r#"{
            "web": {
                "results": [
                    {"title": "The Rust Book", "url": "https://doc.rust-lang.org/book/", "description": "Learn Rust.", "extra_snippets": ["Chapter 1.", "Chapter 2."]},
                    {"url": "https://example.com/no-title", "description": "无标题回退 URL。"},
                    {"title": "非 http", "url": "ftp://example.com/x", "description": "非 http(s) 链接跳过。"}
                ]
            }
        }"#;
        let results = Brave::parse_json(fixture).unwrap();
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0].title, "The Rust Book");
        assert!(
            results[0].snippet.contains("Learn Rust."),
            "{:?}",
            results[0]
        );
        assert!(
            results[0].snippet.contains("Chapter 2."),
            "{:?}",
            results[0]
        );
        // 缺 title → 回退 URL。
        assert_eq!(results[1].title, "https://example.com/no-title");
        // 非 JSON → 解析错误（provider 错误 → 链 fallback）。
        assert!(Brave::parse_json("not json").is_err());
    }

    #[test]
    fn api_error_classification_matches_omp() {
        // 鉴权/额度类压缩为 provider 标签短句（omp classifyProviderHttpError）。
        assert_eq!(
            classify_api_error("tavily", 401, ""),
            "tavily: 401 unauthorized"
        );
        assert_eq!(
            classify_api_error("tavily", 402, "{}"),
            "tavily: 402 credits exhausted"
        );
        assert_eq!(classify_api_error("brave", 403, ""), "brave: 403 forbidden");
        // 错误体命中配额信号（不限状态码）→ credits exhausted。
        assert_eq!(
            classify_api_error("brave", 429, r#"{"error":"quota exceeded"}"#),
            "brave: credits exhausted"
        );
        assert_eq!(
            classify_api_error("tavily", 500, "Credits Exhausted"),
            "tavily: credits exhausted"
        );
        // 其余 → 状态码 + 截断错误体；空错误体仅状态码。
        let msg = classify_api_error("brave", 429, "rate limited");
        assert!(
            msg.contains("brave API error (429)") && msg.contains("rate limited"),
            "{msg}"
        );
        assert_eq!(
            classify_api_error("tavily", 503, "  "),
            "tavily API error (503)"
        );
    }

    #[test]
    fn chain_401_falls_back_and_reports_winner() {
        struct Unauthorized401;
        #[async_trait]
        impl WebSearchProvider for Unauthorized401 {
            fn name(&self) -> &'static str {
                "tavily"
            }
            async fn search(
                &self,
                _q: &str,
                _recency: Option<Recency>,
                _max_results: usize,
            ) -> Result<Vec<WebResult>, String> {
                Err("tavily: 401 unauthorized".into())
            }
        }
        struct OkP;
        #[async_trait]
        impl WebSearchProvider for OkP {
            fn name(&self) -> &'static str {
                "ok"
            }
            async fn search(
                &self,
                _q: &str,
                _recency: Option<Recency>,
                _max_results: usize,
            ) -> Result<Vec<WebResult>, String> {
                Ok(vec![WebResult {
                    title: "t".into(),
                    url: "https://e.com".into(),
                    snippet: "s".into(),
                }])
            }
        }
        // 401 provider 错误 → 链自动 fallback，且返回实际命中的 provider 名。
        let chain = WebSearchChain::new(vec![Arc::new(Unauthorized401), Arc::new(OkP)]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (winner, results) = rt.block_on(chain.search("q", None, 5)).unwrap();
        assert_eq!(winner, "ok");
        assert_eq!(results.len(), 1);
        // 全部 401 → 汇总错误包含分类消息。
        let chain = WebSearchChain::new(vec![Arc::new(Unauthorized401), Arc::new(Unauthorized401)]);
        let err = rt.block_on(chain.search("q", None, 5)).unwrap_err();
        assert!(err.to_string().contains("401 unauthorized"), "{err}");
    }
}
