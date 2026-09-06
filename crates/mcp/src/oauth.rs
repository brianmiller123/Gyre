//! Remote MCP OAuth 授权（oh-my-pi `mcp/oauth-{discovery,flow,credentials}.ts` 截枝移植）。
//!
//! 覆盖 MCP 规范的 Remote MCP 授权链路：
//! - RFC 9728 受保护资源元数据 → RFC 8414/OIDC 授权服务器元数据自动发现
//!   （含网关子路径的 path-inserted / parent-relative 候选序，逐条对齐 omp `buildWellKnownUrls`）；
//! - RFC 7591 动态客户端注册（DCR，含失败详情捕获——白名单制服务商如 Figma）；
//! - 授权码 + PKCE S256 回环流（复用 [`agent_core::oauth_callback::CallbackServer`]）；
//! - RFC 8707 资源指示（同源兜底剥离、授权 URL 内嵌资源优先）；
//! - 凭据存 `oauth.toml`（key = `mcp_oauth:<server_url>`，0600），token_url / client_id /
//!   resource 随行持久化供后续刷新（omp `MCPAuthConfig` 语义）；
//! - 标准刷新授权：rotation 保留旧 refresh（server 未轮换时）。

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use agent_config::{McpOAuthConfig, OAuthCredentials};
use anyhow::{Context, bail};
use serde_json::Value;
use url::Url;

use agent_core::oauth_callback::{
    CallbackServer, generate_pkce, generate_state, parse_callback_input,
};

/// 单条发现请求的超时（omp `DISCOVERY_FETCH_TIMEOUT_MS`）。
const DISCOVERY_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// 回调等待超时（与 provider 流一致，omp `DEFAULT_TIMEOUT`）。
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
/// 刷新请求超时（对齐 provider 流 `DEFAULT_OAUTH_REFRESH_TIMEOUT_MS`）。
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);
/// 回调默认端口 / 路径（omp `DEFAULT_PORT` / `CALLBACK_PATH`）。
const DEFAULT_PORT: u16 = 3000;
const CALLBACK_PATH: &str = "/callback";

/// oauth.toml 中 MCP 凭据的存储键。
#[must_use]
pub fn credential_key(server_url: &str) -> String {
    format!("mcp_oauth:{server_url}")
}

/// 发现结果：授权 / token 端点 + 可选 DCR、客户端 id、scope 与资源指示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthEndpoints {
    pub authorization_url: String,
    pub token_url: String,
    /// 授权服务器 issuer（RFC 8414 校验用；随行持久化供刷新时同源过滤）。
    pub issuer_url: Option<String>,
    /// RFC 7591 注册端点。
    pub registration_url: Option<String>,
    /// 元数据直接公布的 client id（`client_id`/`clientId`/`default_client_id`/`public_client_id`）。
    pub client_id: Option<String>,
    /// scope（空格分隔；受保护资源元数据优先）。
    pub scopes: Option<String>,
    /// RFC 8707 资源指示。
    pub resource: Option<String>,
}

/// `WWW-Authenticate: Bearer ...` 挑战的关切字段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Challenge {
    /// RFC 9728 受保护资源元数据 URL（`resource_metadata` 参数）。
    pub resource_metadata: Option<String>,
    /// 挑战要求的 scope（`scope` 参数）。
    pub scopes: Option<String>,
}

/// 解析 `WWW-Authenticate` 头（RFC 6750 `Bearer k="v", k2=v2` 参数列表）。
///
/// 非 Bearer 方案 / 无关切参数时返回 `None`。值兼容带引号与裸 token 两种形态。
#[must_use]
pub fn parse_www_authenticate(header: &str) -> Option<Challenge> {
    let rest = header.trim();
    let rest = rest
        .strip_prefix("Bearer ")
        .or_else(|| rest.strip_prefix("bearer "))
        .map_or_else(|| rest, |r| r.trim_start());
    if rest.is_empty() {
        return None;
    }
    let mut out = Challenge::default();
    for pair in split_challenge_params(rest) {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let v = v.trim().trim_matches('"');
        match k.trim().to_ascii_lowercase().as_str() {
            "resource_metadata" => out.resource_metadata = Some(v.to_string()),
            "scope" => out.scopes = Some(v.to_string()),
            _ => {}
        }
    }
    (out.resource_metadata.is_some() || out.scopes.is_some()).then_some(out)
}

/// 按 RFC 6750 参数分隔切分（引号内的逗号不分割）。
fn split_challenge_params(rest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in rest.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            ',' if !in_quotes => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn metadata_str(m: &Value, key: &str) -> Option<String> {
    m.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// 读 scope：`scopes_supported`（数组 join）或 `scope`（空格串）。
fn read_metadata_scopes(m: &Value) -> Option<String> {
    if let Some(arr) = m.get("scopes_supported").and_then(Value::as_array) {
        let joined: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
        if !joined.is_empty() {
            return Some(joined.join(" "));
        }
    }
    metadata_str(m, "scope")
}

fn read_client_id(m: &Value) -> Option<String> {
    [
        "client_id",
        "clientId",
        "default_client_id",
        "public_client_id",
    ]
    .iter()
    .find_map(|k| metadata_str(m, k))
}

/// RFC 8414 §3.3 归一化：去尾斜杠（scheme/host 已由 URL 解析小写化）。
fn normalize_issuer(value: &str) -> Option<String> {
    let mut u = Url::parse(value).ok()?;
    if u.path() != "/" {
        let p = u.path().trim_end_matches('/').to_string();
        u.set_path(&p);
    } else {
        u.set_path("");
    }
    u.set_query(None);
    u.set_fragment(None);
    Some(u.to_string())
}

/// 元数据 `issuer` 必须与被查 base 一致（RFC 8414 §3.3 / OIDC Discovery §4.3）。
fn issuer_matches_base(metadata_issuer: Option<&str>, base_url: &str) -> bool {
    let Some(iss) = metadata_issuer.and_then(normalize_issuer) else {
        return false;
    };
    let Some(base) = normalize_issuer(base_url) else {
        return false;
    };
    iss == base
}

/// 构造有序的元数据 URL 候选（逐条对齐 omp `buildWellKnownUrls`）。
///
/// - `path` 非 `/` 开头：仅绝对拼接；
/// - base 裸 origin（无路径）：仅 origin-root 候选；
/// - issuer 候选：RFC 8414 §3.1 标准 path-inserted 优先 → parent-relative →
///   path-appended 兼容回退 → 绝对；OIDC Discovery §4 是 `<issuer>/.well-known/...`
///   前缀式，顺序相反（标准式最优先）；
/// - 资源回退（`issuer_candidate=false`）：RFC 9728 path-inserted 最优先（共享网关的
///   origin-root 文档常描述另一 issuer），再 parent-relative。
#[must_use]
pub fn build_well_known_urls(
    well_known_path: &str,
    base_url: &str,
    issuer_candidate: bool,
) -> Vec<Url> {
    let Ok(parsed) = Url::parse(base_url) else {
        return Vec::new();
    };
    let abs = match parsed.join(well_known_path) {
        Ok(u) => u,
        Err(_) => return Vec::new(),
    };
    if !well_known_path.starts_with('/') {
        return vec![abs];
    }
    let normalized_path = parsed.path().trim_end_matches('/').to_string();
    let Some(last_slash) = normalized_path.rfind('/') else {
        return vec![abs];
    };
    // 多段路径丢掉末段（通常是 MCP 端点名）；单段路径自身即网关前缀。
    let prefix_path = if last_slash == 0 {
        normalized_path.clone()
    } else {
        normalized_path[..last_slash].to_string()
    };
    let rel = Url::parse(&format!(
        "{}{}/{}",
        origin_of(&parsed),
        prefix_path,
        &well_known_path[1..]
    ))
    .ok();

    let mut candidates: Vec<Url> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let push = |u: Url, out: &mut Vec<Url>, seen: &mut HashSet<String>| {
        if seen.insert(u.to_string()) {
            out.push(u);
        }
    };
    if !issuer_candidate {
        if well_known_path.starts_with("/.well-known/") {
            if let Ok(u) = Url::parse(&format!(
                "{}{}{}",
                origin_of(&parsed),
                well_known_path,
                normalized_path
            )) {
                push(u, &mut candidates, &mut seen);
            }
        }
        if let Some(u) = rel {
            push(u, &mut candidates, &mut seen);
        }
    } else if well_known_path == "/.well-known/openid-configuration" {
        if let Ok(u) = Url::parse(&format!(
            "{}{}{}",
            origin_of(&parsed),
            normalized_path,
            well_known_path
        )) {
            push(u, &mut candidates, &mut seen);
        }
        if let Some(u) = rel.clone() {
            push(u, &mut candidates, &mut seen);
        }
        if let Ok(u) = Url::parse(&format!(
            "{}{}{}",
            origin_of(&parsed),
            well_known_path,
            normalized_path
        )) {
            push(u, &mut candidates, &mut seen);
        }
    } else if well_known_path.starts_with("/.well-known/") {
        if let Ok(u) = Url::parse(&format!(
            "{}{}{}",
            origin_of(&parsed),
            well_known_path,
            normalized_path
        )) {
            push(u, &mut candidates, &mut seen);
        }
        if let Some(u) = rel.clone() {
            push(u, &mut candidates, &mut seen);
        }
        if let Ok(u) = Url::parse(&format!(
            "{}{}{}",
            origin_of(&parsed),
            normalized_path,
            well_known_path
        )) {
            push(u, &mut candidates, &mut seen);
        }
    } else if let Some(u) = rel {
        // 非元数据路径仅有 parent-relative 网关回退。
        push(u, &mut candidates, &mut seen);
    }
    // omp 末尾重复 push(rel)（去重后为空操作）+ abs 收尾。
    push(abs, &mut candidates, &mut seen);
    candidates
}

fn origin_of(u: &Url) -> String {
    format!("{}://{}", u.scheme(), u.host_str().unwrap_or_default())
        + u.port()
            .map(|p| format!(":{p}"))
            .unwrap_or_default()
            .as_str()
}

async fn fetch_json(http: &reqwest::Client, url: &str) -> Option<Value> {
    let resp = http
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(DISCOVERY_FETCH_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

/// 从单个元数据文档提取端点（标准 AS/OIDC 形态 + `{oauth|authorization|auth}` 嵌套形态）。
fn find_endpoints(metadata: &Value, protected_resource: Option<&str>) -> Option<OAuthEndpoints> {
    if let (Some(auth), Some(token)) = (
        metadata_str(metadata, "authorization_endpoint"),
        metadata_str(metadata, "token_endpoint"),
    ) {
        return Some(OAuthEndpoints {
            authorization_url: auth,
            token_url: token,
            issuer_url: metadata_str(metadata, "issuer"),
            registration_url: metadata_str(metadata, "registration_endpoint"),
            client_id: read_client_id(metadata),
            scopes: read_metadata_scopes(metadata),
            resource: metadata_str(metadata, "resource")
                .or_else(|| protected_resource.map(ToOwned::to_owned)),
        });
    }
    let nested = ["oauth", "authorization", "auth"]
        .iter()
        .find_map(|k| metadata.get(*k));
    if let Some(o) = nested {
        if let (Some(auth), Some(token)) = (
            metadata_str(o, "authorization_url"),
            metadata_str(o, "token_url"),
        ) {
            return Some(OAuthEndpoints {
                authorization_url: auth,
                token_url: token,
                issuer_url: metadata_str(o, "issuer").or_else(|| metadata_str(metadata, "issuer")),
                registration_url: metadata_str(o, "registration_endpoint"),
                client_id: read_client_id(o),
                scopes: read_metadata_scopes(o),
                resource: metadata_str(o, "resource")
                    .or_else(|| protected_resource.map(ToOwned::to_owned)),
            });
        }
    }
    None
}

/// 自动发现（omp `discoverOAuthEndpoints` 语义，嵌套保护资源文档改为显式队列迭代）。
///
/// `auth_server_url`（挑战给出 / 元数据 authorization_servers）作为 issuer 候选优先；
/// `server_url` 本身作资源服务器回退候选。`resource_metadata_url` 先行拉取以提取
/// `authorization_servers` / scope / resource（RFC 9728 链）。
pub async fn discover_endpoints(
    http: &reqwest::Client,
    server_url: &str,
    auth_server_url: Option<&str>,
    resource_metadata_url: Option<&str>,
) -> Option<OAuthEndpoints> {
    #[derive(Clone)]
    struct Base {
        url: String,
        issuer_candidate: bool,
        protected_resource: Option<String>,
        protected_scopes: Option<String>,
    }
    const ISSUER_PATHS: [&str; 6] = [
        "/.well-known/oauth-authorization-server",
        "/.well-known/openid-configuration",
        "/.well-known/oauth-protected-resource",
        "/oauth/metadata",
        "/.mcp/auth",
        "/authorize",
    ];
    // 资源服务器回退：受保护资源文档优先于 origin-root AS/OIDC（共享网关的 origin
    // 文档常描述另一 issuer）。
    const RESOURCE_PATHS: [&str; 6] = [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-authorization-server",
        "/.well-known/openid-configuration",
        "/oauth/metadata",
        "/.mcp/auth",
        "/authorize",
    ];

    let mut queue: VecDeque<Base> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut protected_resource: Option<String> = None;
    let mut protected_scopes: Option<String> = None;

    // Step 1：给定 resource_metadata URL → 先拉（RFC 9728 链）。
    if let Some(rm) = resource_metadata_url {
        visited.insert(rm.to_string());
        if let Some(meta) = fetch_json(http, rm).await {
            protected_scopes = read_metadata_scopes(&meta).or(protected_scopes);
            if let Some(r) = metadata_str(&meta, "resource") {
                protected_resource = Some(r);
            }
            if let Some(servers) = meta.get("authorization_servers").and_then(Value::as_array) {
                for s in servers.iter().filter_map(Value::as_str) {
                    if visited.insert(s.to_string()) {
                        queue.push_back(Base {
                            url: s.to_string(),
                            issuer_candidate: true,
                            protected_resource: protected_resource.clone(),
                            protected_scopes: protected_scopes.clone(),
                        });
                    }
                }
            }
        }
    }
    if let Some(as_url) = auth_server_url {
        if visited.insert(as_url.to_string()) {
            queue.push_back(Base {
                url: as_url.to_string(),
                issuer_candidate: true,
                protected_resource: None,
                protected_scopes: None,
            });
        }
    }
    queue.push_back(Base {
        url: server_url.to_string(),
        issuer_candidate: false,
        protected_resource: None,
        protected_scopes: None,
    });

    while let Some(base) = queue.pop_front() {
        let paths: &[&str] = if base.issuer_candidate {
            &ISSUER_PATHS
        } else {
            &RESOURCE_PATHS
        };
        for path in paths {
            for url in build_well_known_urls(path, &base.url, base.issuer_candidate) {
                let Some(metadata) = fetch_json(http, url.as_str()).await else {
                    continue;
                };
                let require_issuer_match = base.issuer_candidate
                    && (*path == "/.well-known/oauth-authorization-server"
                        || *path == "/.well-known/openid-configuration");
                let issuer_ok = !require_issuer_match
                    || issuer_matches_base(
                        metadata.get("issuer").and_then(Value::as_str),
                        &base.url,
                    );
                if issuer_ok {
                    if let Some(e) = find_endpoints(&metadata, base.protected_resource.as_deref()) {
                        // 发现级 scope 优先于文档级（受保护资源携带的挑战 scope）。
                        return Some(OAuthEndpoints {
                            scopes: base.protected_scopes.clone().or(e.scopes),
                            ..e
                        });
                    }
                }
                // 受保护资源文档：端点提取失败 → 递归其 authorization_servers。
                if *path == "/.well-known/oauth-protected-resource" {
                    let discovered_resource = metadata_str(&metadata, "resource")
                        .unwrap_or_else(|| base.protected_resource.clone().unwrap_or_default());
                    let discovered_scopes =
                        read_metadata_scopes(&metadata).or_else(|| base.protected_scopes.clone());
                    let discovered_resource =
                        (!discovered_resource.is_empty()).then_some(discovered_resource);
                    if let Some(servers) = metadata
                        .get("authorization_servers")
                        .and_then(Value::as_array)
                    {
                        for s in servers.iter().filter_map(Value::as_str) {
                            if visited.contains(s) {
                                continue;
                            }
                            if let Some(found) = Box::pin(discover_endpoints_with(
                                http,
                                s,
                                discovered_resource.clone(),
                                discovered_scopes.clone(),
                            ))
                            .await
                            {
                                return Some(found);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// 带已捕获保护资源/ Scope 上下文的递归入口（ issuer 候选 + 上下文携带）。
fn discover_endpoints_with<'a>(
    http: &'a reqwest::Client,
    auth_server_url: &'a str,
    protected_resource: Option<String>,
    protected_scopes: Option<String>,
) -> futures::future::BoxFuture<'a, Option<OAuthEndpoints>> {
    Box::pin(async move {
        const ISSUER_PATHS: [&str; 6] = [
            "/.well-known/oauth-authorization-server",
            "/.well-known/openid-configuration",
            "/.well-known/oauth-protected-resource",
            "/oauth/metadata",
            "/.mcp/auth",
            "/authorize",
        ];
        for path in ISSUER_PATHS {
            for url in build_well_known_urls(path, auth_server_url, true) {
                let Some(metadata) = fetch_json(http, url.as_str()).await else {
                    continue;
                };
                let require_issuer_match = path == "/.well-known/oauth-authorization-server"
                    || path == "/.well-known/openid-configuration";
                let issuer_ok = !require_issuer_match
                    || issuer_matches_base(
                        metadata.get("issuer").and_then(Value::as_str),
                        auth_server_url,
                    );
                if issuer_ok {
                    if let Some(e) = find_endpoints(&metadata, protected_resource.as_deref()) {
                        return Some(OAuthEndpoints {
                            scopes: protected_scopes.clone().or(e.scopes),
                            ..e
                        });
                    }
                }
            }
        }
        None
    })
}

/// RFC 7591 动态客户端注册产物。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DcrClient {
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// 动态客户端注册（DCR）。失败时携带端点 + 状态 + 截断的响应首行（白名单制
/// 服务商如 Figma 对未列入目录的客户端一律 403——错误信息需能指认端点）。
pub async fn register_client(
    http: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: &str,
    scopes: Option<&str>,
) -> anyhow::Result<DcrClient> {
    let mut body = serde_json::json!({
        "client_name": "gyre",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "native",
    });
    if let Some(s) = scopes.map(str::trim).filter(|s| !s.is_empty()) {
        body["scope"] = Value::String(s.to_string());
    }
    let resp = http
        .post(registration_endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .timeout(DISCOVERY_FETCH_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("DCR 请求失败：POST {registration_endpoint}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = text.lines().next().unwrap_or_default();
        bail!("动态客户端注册被拒：POST {registration_endpoint} → HTTP {status} — {detail}");
    }
    let data: Value = serde_json::from_str(&text)
        .with_context(|| format!("DCR 响应非 JSON：{registration_endpoint}"))?;
    let Some(client_id) = metadata_str(&data, "client_id") else {
        bail!("DCR 响应缺少 client_id：{registration_endpoint}");
    };
    Ok(DcrClient {
        client_id,
        client_secret: metadata_str(&data, "client_secret"),
    })
}

/// 资源指示校验（omp `resolveResourceUri`）：禁空白包边、必须 http(s)、禁 fragment。
fn resolve_resource_uri(resource: &str) -> anyhow::Result<String> {
    let trimmed = resource.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed != resource {
        bail!("resource 指示不得包含首尾空白");
    }
    let parsed = Url::parse(trimmed)?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        bail!("resource 指示必须使用 http/https");
    }
    if parsed.fragment().is_some() {
        bail!("resource 指示不得包含 fragment");
    }
    Ok(trimmed.to_string())
}

/// 同源冗余资源指示剥离（omp `filterResourceIndicator`）：服务商公告的资源即使
/// 同源也权威保留；仅「server URL 兜底合成」的回退值（`strip=true`）同源时剥离。
fn filter_resource_indicator(resource: &str, server_url: &str, strip: bool) -> Option<String> {
    if resource.is_empty() {
        return None;
    }
    if let (Ok(origin_base), Ok(parsed_resource)) = (Url::parse(server_url), Url::parse(resource)) {
        if parsed_resource.origin() == origin_base.origin() && strip {
            return None;
        }
    }
    Some(resource.to_string())
}

fn scopes_contains(scopes: Option<&str>, scope: &str) -> bool {
    scopes.is_some_and(|s| s.split_whitespace().any(|w| w == scope))
}

/// 登录流程（授权码 + PKCE，回环回调 + 手动粘贴赛跑）。
pub struct LoginFlow<'a> {
    http: &'a reqwest::Client,
    server_url: String,
    cfg: Option<&'a McpOAuthConfig>,
    endpoints: OAuthEndpoints,
}

impl<'a> LoginFlow<'a> {
    /// 由配置 + （可选挑战上下文）自动发现端点并构建流程。
    ///
    /// # Errors
    /// 发现失败（server 未暴露任何可识别的元数据文档）时返回错误——提示改用
    /// 配置块或提供挑战头。
    pub async fn discover(
        http: &'a reqwest::Client,
        server_url: &str,
        cfg: Option<&'a McpOAuthConfig>,
        challenge: Option<&Challenge>,
    ) -> anyhow::Result<Self> {
        let endpoints = discover_endpoints(
            http,
            server_url,
            None,
            challenge.and_then(|c| c.resource_metadata.as_deref()),
        )
        .await
        .context("OAuth 元数据发现失败：server 未公布可识别的授权服务器元数据（可改用 [mcp.servers.<name>.oauth] 手工配置，或附上 401 挑战头重试）")?;
        Ok(Self {
            http,
            server_url: server_url.to_string(),
            cfg,
            endpoints,
        })
    }

    /// 静态已知的 client id（配置 / 元数据 / 授权 URL 内嵌）——决定回环端口可否随机兜底。
    fn static_client_id(&self) -> Option<String> {
        if let Some(id) = self
            .cfg
            .and_then(|c| c.client_id.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(id.to_string());
        }
        if let Some(id) = &self.endpoints.client_id {
            return Some(id.clone());
        }
        Url::parse(&self.endpoints.authorization_url)
            .ok()
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "client_id")
                    .map(|(_, v)| v.to_string())
            })
            .filter(|s| !s.trim().is_empty())
    }

    /// 回环端口：显式配置 > http 回环 redirect URI 端口 > 3000。
    fn preferred_port(&self) -> anyhow::Result<u16> {
        if let Some(p) = self.cfg.and_then(|c| c.callback_port) {
            return Ok(p);
        }
        if let Some(redirect) = self.cfg.and_then(|c| c.redirect_uri.as_deref()) {
            if let Ok(u) = Url::parse(redirect) {
                let loopback = matches!(u.host_str(), Some("localhost") | Some("127.0.0.1"));
                if u.scheme() == "http" && loopback {
                    if let Some(p) = u.port_or_known_default() {
                        return Ok(p);
                    }
                }
                // https 回环 redirect 必须显式 callback_port（TLS 终结在本地监听之前）。
                if u.scheme() == "https" && loopback {
                    bail!(
                        "https 回环 redirect URI 必须配置 oauth.callback_port 指向本地 TLS 终结后的 HTTP 监听端口"
                    );
                }
            }
        }
        Ok(DEFAULT_PORT)
    }

    fn callback_path(&self) -> String {
        if let Some(p) = self
            .cfg
            .and_then(|c| c.callback_path.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return if p.starts_with('/') {
                p.to_string()
            } else {
                format!("/{p}")
            };
        }
        if let Some(redirect) = self.cfg.and_then(|c| c.redirect_uri.as_deref()) {
            if let Ok(u) = Url::parse(redirect) {
                let path = u.path();
                if !path.is_empty() {
                    return path.to_string();
                }
            }
        }
        CALLBACK_PATH.to_string()
    }

    /// 资源指示终值 + 是否为「server URL 兜底合成」（omp `stripSameOriginResource`
    /// 语义：兜底值同源剥离，公告值权威保留）。
    fn resource(&self) -> (Option<String>, bool) {
        let (raw, strip, fallback) = if let Some(r) = self.cfg.and_then(|c| c.resource.as_deref()) {
            (r, false, false)
        } else if let Some(r) = &self.endpoints.resource {
            (r.as_str(), false, false)
        } else {
            (self.server_url.as_str(), true, true)
        };
        let Ok(resolved) = resolve_resource_uri(raw) else {
            return (None, fallback);
        };
        (
            filter_resource_indicator(&resolved, &self.endpoints.authorization_url, strip),
            fallback,
        )
    }

    /// 最终 scope：显式配置 > 发现值。
    fn scopes(&self) -> Option<String> {
        self.cfg
            .and_then(|c| c.scope.clone())
            .or_else(|| self.endpoints.scopes.clone())
    }

    /// 授权 URL + PKCE 对 + state（omp `generateAuthUrl` 全语义）。
    ///
    /// 返回 `(授权 URL, state, code_verifier)`。
    fn authorize_request(
        &self,
        redirect_uri: &str,
        resolved_client: &Option<(String, Option<String>)>,
    ) -> anyhow::Result<(String, String, String)> {
        let mut url = Url::parse(&self.endpoints.authorization_url)
            .with_context(|| format!("授权端点非法：{}", self.endpoints.authorization_url))?;
        let existing: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let has = |k: &str| existing.iter().any(|(key, _)| key == k);
        let get = |k: &str| {
            existing
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };

        let mut q = url.query_pairs_mut();
        if !has("response_type") {
            q.append_pair("response_type", "code");
        }
        // 已解析客户端（静态 / DCR）优先；授权 URL 内嵌 client_id 兜底保留。
        let client_id = match resolved_client {
            Some((id, _)) => Some(id.clone()),
            None => get("client_id")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
        };
        if let Some(id) = &client_id {
            q.append_pair("client_id", id);
        }
        let scopes = self.scopes();
        if let Some(s) = scopes.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if !has("scope") {
                q.append_pair("scope", s);
            }
        }
        // prompt：显式配置优先（`""` = 强制省略）；缺省仅 offline_access 时补 consent
        //（OIDC Core 要求 prompt=consent 才签发 refresh_token）。
        let final_scope = get("scope").or(scopes);
        let prompt = match self.cfg.and_then(|c| c.prompt.clone()) {
            Some(p) => p,
            None => {
                if scopes_contains(final_scope.as_deref(), "offline_access") {
                    "consent".to_string()
                } else {
                    String::new()
                }
            }
        };
        if !prompt.is_empty() && !has("prompt") {
            q.append_pair("prompt", &prompt);
        }
        // resource（omp 三分支）：公告资源（配置/元数据）权威，直接设置；
        // 否则授权 URL 内嵌资源经公告式过滤（同源保留），滤净则删；
        // 最后才落兜底资源值（跨源幸存的 server URL 回退）。
        let (resource, resource_is_fallback) = self.resource();
        if let Some(r) = &resource {
            if !resource_is_fallback {
                q.append_pair("resource", r);
            }
        }
        drop(q);
        if resource.is_none() || resource_is_fallback {
            if let Some(embedded) = get("resource") {
                match filter_resource_indicator(
                    &resolve_resource_uri(&embedded)?,
                    &self.endpoints.authorization_url,
                    false,
                ) {
                    Some(filtered) => {
                        url.query_pairs_mut().append_pair("resource", &filtered);
                    }
                    None => {
                        // 内嵌资源被滤净 → 从查询串删除（授权与 token 请求都不带）。
                        let mut cleaned = url.clone();
                        cleaned.set_query(None);
                        {
                            let mut q2 = cleaned.query_pairs_mut();
                            for (k, v) in existing.iter().filter(|(k, _)| k != "resource") {
                                q2.append_pair(k, v);
                            }
                        }
                        url = cleaned;
                    }
                }
            } else if let Some(r) = &resource {
                url.query_pairs_mut().append_pair("resource", r);
            }
        }
        let state = generate_state();
        let pkce = generate_pkce();
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("redirect_uri", redirect_uri);
            q.append_pair("state", &state);
            q.append_pair("code_challenge", &pkce.challenge);
            q.append_pair("code_challenge_method", "S256");
        }
        Ok((url.to_string(), state, pkce.verifier))
    }

    /// 登录入口：绑定回调 → 出授权 URL → 等 code → 换 token → 落盘。
    ///
    /// `manual_code`：TTY 粘贴兜底工厂（与回环回调赛跑）。
    ///
    /// # Errors
    /// DCR 失败 / 回调超时或拒绝 / token 交换失败 / 落盘失败。
    pub async fn run(
        self,
        config_dir: &Path,
        manual_code: Option<Pin<Box<dyn Future<Output = String> + Send>>>,
        mut on_auth: impl FnMut(&str) + Send,
    ) -> anyhow::Result<OAuthCredentials> {
        let allow_fallback = self.static_client_id().is_none();
        let server = CallbackServer::bind(
            self.preferred_port()?,
            &self.callback_path(),
            allow_fallback,
        )
        .await
        .context("绑定回环回调端口失败")?;
        let advertised_redirect = self
            .cfg
            .and_then(|c| c.redirect_uri.clone())
            .unwrap_or_else(|| server.redirect_uri());
        // 客户端解析（omp generateAuthUrl 惰性 DCR 语义）：静态配置 / 元数据 /
        // 授权 URL 内嵌 id 优先；皆无且 AS 公告 registration_endpoint → RFC 7591
        // 动态注册（失败带端点+状态详情，指引手工配置 client_id）。
        let resolved_client: Option<(String, Option<String>)> = match self.static_client_id() {
            Some(id) => Some((id, self.cfg.and_then(|c| c.client_secret.clone()))),
            None => {
                let Some(reg) = self.endpoints.registration_url.as_deref() else {
                    bail!(
                        "OAuth 服务商需要 client_id，但元数据未公告动态注册端点——请在 \
                         [mcp.servers.<name>.oauth] 配置 client_id（必要时含 client_secret）"
                    );
                };
                match register_client(
                    self.http,
                    reg,
                    &advertised_redirect,
                    self.scopes().as_deref(),
                )
                .await
                {
                    Ok(c) => Some((c.client_id, c.client_secret)),
                    Err(e) => bail!(
                        "{e}。该 server 可能将注册限制在预核准客户端——请在 \
                         [mcp.servers.<name>.oauth] 配置 client_id（必要时含 client_secret）"
                    ),
                }
            }
        };
        let (auth_url, state, verifier) =
            self.authorize_request(&advertised_redirect, &resolved_client)?;
        on_auth(&auth_url);

        // 与 provider 流同构：先物化等待 future（持有 server），select 落败分支随
        // future 丢弃自动关监听。
        let server_wait = server.wait_for_code(&state, CALLBACK_TIMEOUT);
        let code = match manual_code {
            Some(manual) => {
                tokio::select! {
                    r = server_wait => r?,
                    pasted = manual => {
                        let (code, pasted_state) = parse_callback_input(&pasted)
                            .context("无法从粘贴内容解析授权码（支持完整 URL / query 串 / 裸 code#state）")?;
                        if pasted_state.is_some_and(|s| s != state) {
                            bail!("粘贴内容的 state 与本次登录不匹配——可能粘贴了其他窗口的授权链接");
                        }
                        code
                    }
                }
            }
            None => server_wait.await?,
        };

        let mut creds = self
            .exchange_token(&code, &advertised_redirect, &verifier, &resolved_client)
            .await?;
        // 刷新物随行落盘（omp MCPAuthConfig 持久化语义）：刷新 / 资源指示必需。
        creds.token_url = Some(self.endpoints.token_url.clone());
        if let Some((id, secret)) = resolved_client {
            creds.client_id.get_or_insert_with(|| id.clone());
            if let Some(sec) = secret {
                creds.client_secret.get_or_insert_with(|| sec.clone());
            }
        }
        if let Some(r) = self.resource().0 {
            creds.resource.get_or_insert(r);
        }
        let path = agent_config::save_oauth(config_dir, &credential_key(&self.server_url), &creds)
            .map_err(|e| anyhow::anyhow!("凭据落盘失败：{e}"))?;
        tracing::info!(path = %path.display(), "MCP OAuth 凭据已保存");
        Ok(creds)
    }

    async fn exchange_token(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
        resolved_client: &Option<(String, Option<String>)>,
    ) -> anyhow::Result<OAuthCredentials> {
        let (client_id, client_secret) = match resolved_client {
            Some((id, secret)) => (Some(id.clone()), secret.clone()),
            None => (
                self.static_client_id(),
                self.cfg.and_then(|c| c.client_secret.clone()),
            ),
        };
        let creds = token_request(
            self.http,
            &self.endpoints.token_url,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
            ],
            client_id.as_deref(),
            client_secret.as_deref(),
            self.resource().0.as_deref(),
            Some(verifier),
        )
        .await?;
        Ok(creds)
    }
}

/// 通用 token 请求（授权码 / 刷新共用）：form 编码 + 错误透传 + 空访问令牌拒收。
async fn token_request(
    http: &reqwest::Client,
    token_url: &str,
    base: &[(&str, &str)],
    client_id: Option<&str>,
    client_secret: Option<&str>,
    resource: Option<&str>,
    code_verifier: Option<&str>,
) -> anyhow::Result<OAuthCredentials> {
    let mut form: Vec<(&str, &str)> = base.to_vec();
    if let Some(id) = client_id {
        form.push(("client_id", id));
    }
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    if let Some(r) = resource {
        form.push(("resource", r));
    }
    if let Some(v) = code_verifier {
        form.push(("code_verifier", v));
    }
    let resp = http
        .post(token_url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .form(&form)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("token 请求失败：POST {token_url}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("token 交换失败：HTTP {status} {}", first_line(&text));
    }
    let data: Value =
        serde_json::from_str(&text).with_context(|| format!("token 响应非 JSON：{token_url}"))?;
    if let Some(err) = data.get("error").and_then(Value::as_str) {
        let desc = data
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or("");
        bail!("token 端点返回错误：{err} {desc}");
    }
    let Some(access) = metadata_str(&data, "access_token") else {
        let provider_error = data
            .get("error_description")
            .and_then(Value::as_str)
            .or_else(|| data.get("error").and_then(Value::as_str))
            .unwrap_or("");
        if provider_error.is_empty() {
            bail!("token 交换未返回 access_token");
        }
        bail!("token 交换未返回 access_token：{provider_error}");
    };
    let expires_in = data
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    Ok(OAuthCredentials {
        access,
        refresh: data
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        expires: now + expires_in * 1000,
        ..OAuthCredentials::default()
    })
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

/// 刷新（标准 refresh_token 授权）：server 未轮换时保留旧 refresh（omp
/// `refreshMCPOAuthToken` 语义）。凭据需携带 `token_url`（登录时随行落盘）。
///
/// # Errors
/// 缺 token_url / 刷新端点失败（HTTP 4xx 视为确定性失败，调用方应清凭据）。
pub async fn refresh(
    http: &reqwest::Client,
    creds: &OAuthCredentials,
) -> anyhow::Result<OAuthCredentials> {
    let Some(token_url) = creds.token_url.as_deref() else {
        bail!("凭据缺少 token_url（需重新 `agent mcp login`）");
    };
    let fresh = token_request(
        http,
        token_url,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", creds.refresh.as_str()),
        ],
        creds.client_id.as_deref(),
        creds.client_secret.as_deref(),
        creds.resource.as_deref(),
        None,
    )
    .await?;
    let mut merged = creds.merge_refreshed(fresh);
    if merged.refresh.is_empty() {
        merged.refresh = creds.refresh.clone();
    }
    Ok(merged)
}

/// 刷新是否为确定性失败（凭据应清除）：HTTP 4xx（invalid_grant / expired token 等）。
#[must_use]
pub fn is_definitive_refresh_failure(err: &anyhow::Error) -> bool {
    err.to_string().contains("HTTP 4")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_urls_rfc8414_path_inserted_first() {
        // RFC 8414 §3.1：/.well-known/<suffix>/<issuer-path> 标准式最优先。
        let urls = build_well_known_urls(
            "/.well-known/oauth-authorization-server",
            "https://as.example.com/tenant1",
            true,
        );
        let rendered: Vec<String> = urls.iter().map(ToString::to_string).collect();
        assert_eq!(
            rendered[0],
            "https://as.example.com/.well-known/oauth-authorization-server/tenant1"
        );
        assert!(rendered.contains(
            &"https://as.example.com/tenant1/.well-known/oauth-authorization-server".to_string()
        ));
        assert!(rendered.contains(
            &"https://as.example.com/.well-known/oauth-authorization-server".to_string()
        ));
    }

    #[test]
    fn well_known_urls_oidc_standard_form_first() {
        let urls = build_well_known_urls(
            "/.well-known/openid-configuration",
            "https://as.example.com/tenant1",
            true,
        );
        assert_eq!(
            urls[0].as_str(),
            "https://as.example.com/tenant1/.well-known/openid-configuration"
        );
    }

    #[test]
    fn well_known_urls_resource_fallback_prefers_path_inserted() {
        // RFC 9728：资源回退探测 path-inserted 优先于 parent-relative。
        let urls = build_well_known_urls(
            "/.well-known/oauth-protected-resource",
            "https://gw.example.com/mcp",
            false,
        );
        assert_eq!(
            urls[0].as_str(),
            "https://gw.example.com/.well-known/oauth-protected-resource/mcp"
        );
        assert!(
            urls.contains(
                &"https://gw.example.com/.well-known/oauth-protected-resource"
                    .parse()
                    .unwrap()
            )
        );
    }

    #[test]
    fn well_known_urls_multi_segment_drops_last_segment() {
        let urls = build_well_known_urls(
            "/.well-known/oauth-authorization-server",
            "https://gw.example.com/http/mcp",
            true,
        );
        let rendered: Vec<String> = urls.iter().map(ToString::to_string).collect();
        assert!(rendered.contains(
            &"https://gw.example.com/http/.well-known/oauth-authorization-server".to_string()
        ));
        // omp 兼容回退：path-appended 候选同样产出（末段未剥离），仅排序靠后。
        assert!(rendered.contains(
            &"https://gw.example.com/http/mcp/.well-known/oauth-authorization-server".to_string()
        ));
    }

    #[test]
    fn well_known_urls_bare_origin_only_abs() {
        let urls = build_well_known_urls(
            "/.well-known/oauth-authorization-server",
            "https://as.example.com",
            true,
        );
        assert_eq!(urls.len(), 1);
        assert_eq!(
            urls[0].as_str(),
            "https://as.example.com/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn www_authenticate_parses_quoted_and_bare() {
        let c = parse_www_authenticate(
            r#"Bearer realm="x", resource_metadata="https://m.example.com/.well-known/oauth-protected-resource/mcp", scope="read write""#,
        )
        .unwrap();
        assert_eq!(
            c.resource_metadata.as_deref(),
            Some("https://m.example.com/.well-known/oauth-protected-resource/mcp")
        );
        assert_eq!(c.scopes.as_deref(), Some("read write"));

        // 仅 error 参数：无关切字段 → None（不影响「需要授权」的判定路径）。
        assert_eq!(parse_www_authenticate("Bearer error=invalid_token"), None);
        // MCP 规范常见组合：error + resource_metadata。
        let c3 = parse_www_authenticate(
            r#"Bearer error="invalid_token", resource_metadata="https://m/.well-known/oauth-protected-resource""#,
        )
        .unwrap();
        assert_eq!(
            c3.resource_metadata.as_deref(),
            Some("https://m/.well-known/oauth-protected-resource")
        );
        assert_eq!(parse_www_authenticate("Basic realm=x"), None);
        assert_eq!(parse_www_authenticate("Bearer"), None);
    }

    #[test]
    fn find_endpoints_std_and_nested_shapes() {
        let std_doc = serde_json::json!({
            "issuer": "https://as.example.com",
            "authorization_endpoint": "https://as.example.com/authorize",
            "token_endpoint": "https://as.example.com/token",
            "registration_endpoint": "https://as.example.com/register",
            "scopes_supported": ["mcp:read", "mcp:write"],
        });
        let e = find_endpoints(&std_doc, None).unwrap();
        assert_eq!(e.token_url, "https://as.example.com/token");
        assert_eq!(e.scopes.as_deref(), Some("mcp:read mcp:write"));

        let nested = serde_json::json!({
            "oauth": {
                "authorization_url": "https://x/authorize",
                "token_url": "https://x/token",
                "client_id": "cid-1",
            }
        });
        let e2 = find_endpoints(&nested, Some("https://r")).unwrap();
        assert_eq!(e2.client_id.as_deref(), Some("cid-1"));
        assert_eq!(e2.resource.as_deref(), Some("https://r"));
    }

    #[test]
    fn issuer_matching_normalizes_trailing_slash() {
        assert!(issuer_matches_base(
            Some("https://as.example.com/"),
            "https://as.example.com"
        ));
        assert!(!issuer_matches_base(
            Some("https://other.example.com"),
            "https://as.example.com"
        ));
        assert!(!issuer_matches_base(None, "https://as.example.com"));
    }

    #[test]
    fn resource_filter_strips_same_origin_fallback_only() {
        // 兜底合成（strip=true）：同源剥离。
        assert_eq!(
            filter_resource_indicator(
                "https://gw.example.com/mcp",
                "https://gw.example.com/mcp",
                true
            ),
            None
        );
        // 公告值（strip=false）：同源保留。
        assert_eq!(
            filter_resource_indicator(
                "https://gw.example.com/mcp",
                "https://gw.example.com/mcp",
                false
            )
            .as_deref(),
            Some("https://gw.example.com/mcp")
        );
        // 跨源一律保留。
        assert_eq!(
            filter_resource_indicator("https://r.example.com", "https://gw.example.com/mcp", true)
                .as_deref(),
            Some("https://r.example.com")
        );
    }

    #[test]
    fn resolve_resource_uri_rejects_bad_input() {
        assert!(resolve_resource_uri(" ftp://x ").is_err());
        assert!(resolve_resource_uri("https://x/y#frag").is_err());
        assert_eq!(resolve_resource_uri("https://x/y").unwrap(), "https://x/y");
    }

    #[test]
    fn credential_key_prefix() {
        assert_eq!(
            credential_key("https://mcp.example.com/mcp"),
            "mcp_oauth:https://mcp.example.com/mcp"
        );
    }
}
