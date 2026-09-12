//! GitHub 工具：经 GitHub REST/GraphQL API 查询与操作 PR、issue、Actions。
//!
//! 启用：config `[github]` 段 `enabled = true`；写操作（`create_pr`/`merge_pr`/`comment`）
//! 另需 `allow_write = true`。启用时由装配层把 [`PROMPT_SECTION`] 注入 system prompt。
//!
//! 鉴权：依次读取 `GH_TOKEN` / `GITHUB_TOKEN` 环境变量；缺省匿名访问公共仓库。

use std::time::Duration;

use agent_core::{ApprovalRequest, CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use reqwest::RequestBuilder;
use serde_json::{Value, json};

use crate::{Tool, ToolContext};

const API_BASE: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("agent-tools/", env!("CARGO_PKG_VERSION"));
const DEFAULT_LIMIT: u64 = 10;
const MAX_LIMIT: u64 = 100;
const ERROR_BODY_MAX: usize = 512;

/// 注入 system prompt 的 GitHub 工具使用指引（启用时由装配层追加到 `context_files`）。
pub const PROMPT_SECTION: &str = "<github>\n\
GitHub 工具 `github` 已启用，可查询/操作 PR、issue 与 Actions（CI）。\n\
- 只读：get_pr / list_prs / get_issue / list_issues / list_runs / get_run / get_run_logs\n\
- 写操作（需配置 allow_write）：create_pr / merge_pr / comment / graphql\n\
repo 形如 \"owner/name\"；鉴权读取 GH_TOKEN 或 GITHUB_TOKEN 环境变量。\n\
优先用 graphql 一次取多字段以减少往返；get_run_logs 返回 CI 日志（zip 二进制时仅回元信息）。\n\
</github>";

/// GitHub REST/GraphQL 工具。
pub struct GithubTool {
    /// 是否允许写操作（`create_pr`/`merge_pr`/`comment`）。
    allow_write: bool,
}

impl GithubTool {
    /// 构造；`allow_write` 为真时启用写操作。
    #[must_use]
    pub const fn new(allow_write: bool) -> Self {
        Self { allow_write }
    }

    /// 是否允许写操作。
    #[must_use]
    pub const fn allow_write(&self) -> bool {
        self.allow_write
    }
}

impl Default for GithubTool {
    fn default() -> Self {
        Self::new(false)
    }
}

/// 只读动作集合（`graphql` 不在此列——GraphQL 可执行 mutation，需 `allow_write`）。
const READ_ACTIONS: &[&str] = &[
    "get_pr",
    "list_prs",
    "get_issue",
    "list_issues",
    "list_runs",
    "get_run",
    "get_run_logs",
    // H32：omp `gh` 只读 op 的对应动作（`repo_view`/`file_read`/`search_*`）。
    "repo_view",
    "file_read",
    "search_issues",
    "search_prs",
    "search_code",
    "search_repos",
];

/// omp `gh` 的 op 名 → Gyre action 名别名（H32）。
///
/// 仅收录**语义等价**的命名差异；omp 的 `pr_checkout`/`pr_push`/`run_watch` 涉及
/// worktree 隔离与长驻订阅（属 H18/后续批次），不在此假装支持。
#[must_use]
pub fn normalize_action(raw: &str) -> &str {
    match raw {
        "pr_create" => "create_pr",
        other => other,
    }
}

/// 读取动作名：`action`（Gyre 原名）优先，其次 omp 的 `op`；两者都做别名归一。
///
/// # Errors
/// 两者都缺或非字符串时返回 [`ToolError::InvalidArgs`]。
pub fn action_of(input: &Value) -> Result<&str, ToolError> {
    input
        .get("action")
        .or_else(|| input.get("op"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(normalize_action)
        .ok_or_else(|| ToolError::InvalidArgs("缺少 `action`（omp 别名 `op`）参数".into()))
}

/// 写动作集合（需 `allow_write`）。`graphql` 用 POST 且可含 mutation，归入写门槛。
const WRITE_ACTIONS: &[&str] = &["create_pr", "merge_pr", "comment", "graphql"];

#[async_trait]
impl Tool for GithubTool {
    fn name(&self) -> &'static str {
        "github"
    }

    fn description(&self) -> &'static str {
        "查询/操作 GitHub PR、issue、仓库文件、搜索与 Actions（CI）。\
         action（omp 别名 `op`）∈ {get_pr,list_prs,get_issue,list_issues,list_runs,get_run,get_run_logs,\
         repo_view,file_read,search_issues,search_prs,search_code,search_repos,graphql,\
         create_pr,merge_pr,comment}；repo=\"owner/name\"（search_repos 可省）。\
         get_* 需 number；list_* 可选 limit；file_read 需 path（可选 branch）；search_* 需 query；\
         graphql 需 query（+可选 variables）；create_pr 需 title/head/base；comment 需 body；\
         merge_pr 可选 method。omp 的 `pr_create` 等价于 `create_pr`。\
         写操作需配置 allow_write。鉴权读取 GH_TOKEN/GITHUB_TOKEN。"
    }

    fn schema(&self) -> serde_json::Value {
        let mut actions: Vec<&str> = READ_ACTIONS.to_vec();
        actions.extend_from_slice(WRITE_ACTIONS);
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": actions,
                    "description": "查询/操作动作（omp 别名 `op`）"
                },
                "op": {
                    "type": "string",
                    "enum": actions,
                    "description": "`action` 的 omp 别名（两者给一个即可，`action` 优先）"
                },
                "path": {
                    "type": "string",
                    "description": "file_read：仓库内相对路径"
                },
                "branch": {
                    "type": "string",
                    "description": "file_read：分支/commit（缺省 = 默认分支）"
                },
                "repo": {
                    "type": "string",
                    "description": "owner/repo，例如 \"octocat/Hello-World\""
                },
                "number": {
                    "type": "integer",
                    "description": "PR/issue/run 编号（get_*/merge_pr/comment 必填）",
                    "minimum": 1
                },
                "limit": {
                    "type": "integer",
                    "description": "列表返回条数上限（list_*；默认 10，上限 100）",
                    "minimum": 1,
                    "maximum": 100
                },
                "title": { "type": "string", "description": "create_pr 的标题" },
                "head": { "type": "string", "description": "create_pr 的源分支（head）" },
                "base": { "type": "string", "description": "create_pr 的目标分支（base）" },
                "body": { "type": "string", "description": "create_pr/comment 的正文" },
                "method": {
                    "type": "string",
                    "enum": ["merge", "squash", "rebase"],
                    "description": "merge_pr 的合并方式（默认 merge）"
                },
                "query": { "type": "string", "description": "graphql 动作的 GraphQL 查询" },
                "variables": { "type": "object", "description": "graphql 动作的变量" }
            },
            "required": ["action"]
        })
    }

    fn capability(&self) -> CapabilityTier {
        CapabilityTier::Network
    }

    fn describe<'a>(&'a self, input: &'a serde_json::Value) -> ApprovalRequest<'a> {
        // 写动作提升到 Write 审批门禁；其余为 Network。
        let is_write = action_of(input).is_ok_and(|a| WRITE_ACTIONS.contains(&a));
        ApprovalRequest {
            tool: self.name(),
            capability: if is_write {
                CapabilityTier::Write
            } else {
                CapabilityTier::Network
            },
            command: None,
            args: input,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let action = action_of(&input)?;
        // `search_repos` 按 omp 语义忽略 `repo`（作用域用 query 里的 `org:`/`language:` 等限定）。
        let repo = input.get("repo").and_then(Value::as_str).unwrap_or("");
        if repo.is_empty() && action != "search_repos" {
            return Err(ToolError::InvalidArgs("缺少 `repo` 参数".into()));
        }
        if !repo.is_empty() {
            validate_repo(repo)?;
        }

        let number = parse_number(&input);
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_LIMIT, |n| n.clamp(1, MAX_LIMIT));

        if WRITE_ACTIONS.contains(&action) && !self.allow_write {
            return Err(ToolError::InvalidArgs(format!(
                "写操作 `{action}` 未启用：请在配置 [github] 段设置 allow_write = true"
            )));
        }

        let client = build_client()?;
        let req = build_request(action, repo, number, limit, &input, client)?;
        let body = fetch(req, ctx).await?;
        // H32：`file_read` 走 contents API，文本内容是 base64——解码后直接给纯文本。
        if action == "file_read" {
            return Ok(ToolResult::text(decode_contents_body(&body)));
        }
        Ok(ToolResult::text(body))
    }
}

/// 解码 GitHub contents API 响应（`{"encoding":"base64","content":"…"}`）为纯文本；
/// 目录响应（数组）/ 非 base64（如 `download_url` 大文件）/ 非法 base64 时原样返回。
fn decode_contents_body(body: &str) -> String {
    use base64::Engine as _;
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    let Some(obj) = value.as_object() else {
        return body.to_string();
    };
    if obj.get("encoding").and_then(Value::as_str) != Some("base64") {
        return body.to_string();
    }
    let Some(content) = obj.get("content").and_then(Value::as_str) else {
        return body.to_string();
    };
    let compact: String = content.chars().filter(|c| !c.is_whitespace()).collect();
    match base64::engine::general_purpose::STANDARD.decode(compact) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => body.to_string(),
    }
}

// ── 参数校验与解析 ────────────────────────────────────────────────────────────

/// 校验 `owner/repo`，仅允许 `[A-Za-z0-9._-]`，防路径注入。
fn validate_repo(repo: &str) -> Result<(), ToolError> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| ToolError::InvalidArgs("`repo` 须为 `owner/repo` 形式".into()))?;
    if owner.is_empty() || name.is_empty() {
        return Err(ToolError::InvalidArgs(
            "`owner` 与 `repo` 均不得为空".into(),
        ));
    }
    if !owner.chars().all(is_repo_char) || !name.chars().all(is_repo_char) {
        return Err(ToolError::InvalidArgs(
            "`repo` 含非法字符（仅允许字母、数字、`.`、`_`、`-`）".into(),
        ));
    }
    Ok(())
}

/// `owner`/`repo` 合法字符。
const fn is_repo_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// 解析 `number`（>0）；缺失或 0 视为未提供。
fn parse_number(input: &Value) -> Option<u64> {
    match input.get("number").and_then(Value::as_u64) {
        None | Some(0) => None,
        Some(n) => Some(n),
    }
}

/// 取必填字符串字段。
fn require_str<'a>(input: &'a Value, key: &str, action: &str) -> Result<&'a str, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidArgs(format!("`{action}` 需 `{key}` 参数")))
}

// ── 请求构造 ──────────────────────────────────────────────────────────────────

/// 构造 GET 请求路径（不含 base，不含 query）。
///
/// # Errors
/// 未知 action 或 get_* 缺 `number` 时返回 [`ToolError::InvalidArgs`]。
fn endpoint_path(action: &str, repo: &str, number: Option<u64>) -> Result<String, ToolError> {
    match action {
        "list_prs" => Ok(format!("/repos/{repo}/pulls")),
        "get_pr" => Ok(format!(
            "/repos/{repo}/pulls/{}",
            number.ok_or_else(|| ToolError::InvalidArgs("get_pr 需 `number`".into()))?
        )),
        "list_issues" => Ok(format!("/repos/{repo}/issues")),
        "get_issue" => Ok(format!(
            "/repos/{repo}/issues/{}",
            number.ok_or_else(|| ToolError::InvalidArgs("get_issue 需 `number`".into()))?
        )),
        "list_runs" => Ok(format!("/repos/{repo}/actions/runs")),
        "get_run" => Ok(format!(
            "/repos/{repo}/actions/runs/{}",
            number.ok_or_else(|| ToolError::InvalidArgs("get_run 需 `number`".into()))?
        )),
        "get_run_logs" => Ok(format!(
            "/repos/{repo}/actions/runs/{}/logs",
            number.ok_or_else(|| ToolError::InvalidArgs("get_run_logs 需 `number`".into()))?
        )),
        // H32：omp `gh` 只读 op 对应动作。
        "repo_view" => Ok(format!("/repos/{repo}")),
        _ => Err(ToolError::InvalidArgs(format!("未知 action `{action}`"))),
    }
}

/// 按动作构造 [`RequestBuilder`]（含鉴权与必要 query）。
///
/// # Errors
/// 参数缺失或构造失败时返回 [`ToolError`]。
fn build_request(
    action: &str,
    repo: &str,
    number: Option<u64>,
    limit: u64,
    input: &Value,
    client: &reqwest::Client,
) -> Result<RequestBuilder, ToolError> {
    let common = |path: &str| -> RequestBuilder {
        let url = format!("{API_BASE}{path}");
        let mut r = client
            .get(&url)
            .header("Accept", "application/vnd.github+json");
        if let Some(token) = auth_token() {
            r = r.bearer_auth(token);
        }
        r
    };
    let common_post = |path: &str, body: Value| -> RequestBuilder {
        let url = format!("{API_BASE}{path}");
        let mut r = client
            .post(&url)
            .header("Accept", "application/vnd.github+json")
            .header("Content-Type", "application/json")
            .body(body.to_string());
        if let Some(token) = auth_token() {
            r = r.bearer_auth(token);
        }
        r
    };

    match action {
        "graphql" => {
            let query = require_str(input, "query", "graphql")?;
            let variables = input.get("variables").cloned().unwrap_or(Value::Null);
            Ok(common_post(
                "/graphql",
                json!({ "query": query, "variables": variables }),
            ))
        }
        "create_pr" => {
            let title = require_str(input, "title", "create_pr")?;
            let head = require_str(input, "head", "create_pr")?;
            let base = require_str(input, "base", "create_pr")?;
            let body = json!({
                "title": title,
                "head": head,
                "base": base,
                "body": input.get("body").and_then(Value::as_str).unwrap_or(""),
            });
            Ok(common_post(&format!("/repos/{repo}/pulls"), body))
        }
        "merge_pr" => {
            let n = number.ok_or_else(|| ToolError::InvalidArgs("merge_pr 需 `number`".into()))?;
            let method = input
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("merge");
            let mut req = client
                .put(format!("{API_BASE}/repos/{repo}/pulls/{n}/merge"))
                .header("Accept", "application/vnd.github+json")
                .header("Content-Type", "application/json")
                .body(json!({ "merge_method": method }).to_string());
            if let Some(token) = auth_token() {
                req = req.bearer_auth(token);
            }
            Ok(req)
        }
        "comment" => {
            let n = number.ok_or_else(|| ToolError::InvalidArgs("comment 需 `number`".into()))?;
            let body_text = require_str(input, "body", "comment")?;
            Ok(common_post(
                &format!("/repos/{repo}/issues/{n}/comments"),
                json!({ "body": body_text }),
            ))
        }
        // H32：仓库文件读取（contents API；base64 文本由 `fetch` 解码）。
        "file_read" => {
            let path = require_str(input, "path", "file_read")?;
            let mut req = common(&format!("/repos/{repo}/contents/{path}"));
            if let Some(branch) = input
                .get("branch")
                .and_then(Value::as_str)
                .filter(|b| !b.is_empty())
            {
                req = req.query(&[("ref", branch)]);
            }
            Ok(req)
        }
        // H32：omp `search_*`（GitHub Search API）。`search_repos` 忽略 `repo`。
        "search_issues" | "search_prs" | "search_code" | "search_repos" => {
            let query = require_str(input, "query", action)?;
            let scope = if repo.is_empty() {
                String::new()
            } else {
                format!(" repo:{repo}")
            };
            let (path, q) = match action {
                "search_issues" => ("/search/issues", format!("{query}{scope} is:issue")),
                "search_prs" => ("/search/issues", format!("{query}{scope} is:pr")),
                "search_code" => ("/search/code", format!("{query}{scope}")),
                _ => ("/search/repositories", query.to_string()),
            };
            Ok(common(path).query(&[("q", q.as_str()), ("per_page", &limit.to_string())]))
        }
        "list_prs" | "list_issues" | "list_runs" => {
            let path = endpoint_path(action, repo, None)?;
            Ok(common(&path).query(&[("state", "open"), ("per_page", &limit.to_string())]))
        }
        _ => {
            // get_* 与 get_run_logs：单项，无分页
            let path = endpoint_path(action, repo, number)?;
            Ok(common(&path))
        }
    }
}

// ── HTTP ──────────────────────────────────────────────────────────────────────

/// 共享 HTTP 客户端（带超时与 User-Agent + 连接池复用，避免每次请求新建 client 重复 TLS 握手）。
fn build_client() -> Result<&'static reqwest::Client, ToolError> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(USER_AGENT)
            .build()
            .expect("构建 GitHub HTTP 客户端失败")
    });
    // OnceLock::get 在 init 后必定返回 Some。
    CLIENT
        .get()
        .ok_or_else(|| ToolError::Execution("GitHub HTTP 客户端初始化失败".into()))
}

/// 读取鉴权 token（`GH_TOKEN` 优先于 `GITHUB_TOKEN`），空值视为无。
pub fn auth_token() -> Option<String> {
    std::env::var("GH_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok()
        .filter(|s| !s.is_empty())
}

/// `pr://` / `issue://` 内部协议路由：GET `<owner>/<repo>/<path>` 并缓存到
/// `<workspace>/.gyre/cache/github/`（文件缓存：auth 指纹 + TTL 300s；列表 TTL 60s）。
///
/// `path` 例：`pulls/42`、`issues/7`、`pulls?state=open&per_page=5`。
///
/// # Errors
/// 网络错误、HTTP 非 2xx 或缓存读写失败时返回 [`ToolError::Execution`]。
pub async fn api_get_json(
    owner: &str,
    repo: &str,
    path: &str,
    ctx: &ToolContext<'_>,
) -> Result<serde_json::Value, ToolError> {
    use std::time::SystemTime;

    let url = format!("{API_BASE}/repos/{owner}/{repo}/{path}");
    // auth 指纹：token 前 8 字符 + 有无（匿名缓存与鉴权缓存隔离）。
    let auth = auth_token();
    let fingerprint = auth.as_ref().map_or_else(
        || "anon".into(),
        |t| format!("tok-{}", &t[..t.len().min(8)]),
    );
    // 列表（含 `?`）TTL 短：60s；单对象 TTL 长：300s。
    let ttl = if path.contains('?') { 60 } else { 300 };
    let safe_key = format!("{owner}__{repo}__{path}").replace(['/', '?', '=', '&'], "_");
    let cache_dir = ctx.workspace.root().join(".gyre/cache/github");
    let cache_file = cache_dir.join(format!("{fingerprint}__{safe_key}.json"));

    // 命中新鲜缓存 → 直接返回。
    if let Ok(meta) = tokio::fs::metadata(&cache_file).await {
        if let Ok(modified) = meta.modified() {
            let age = SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default()
                .as_secs();
            if age < ttl {
                if let Ok(text) = tokio::fs::read_to_string(&cache_file).await {
                    return serde_json::from_str(&text)
                        .map_err(|e| ToolError::Execution(format!("缓存 JSON 损坏: {e}")));
                }
            }
        }
    }

    let mut builder = build_client()?.get(&url);
    if let Some(t) = &auth {
        builder = builder.bearer_auth(t);
    }
    let body = fetch(builder, ctx).await?;
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| ToolError::Execution(format!("GitHub 响应 JSON 解析失败: {e}")))?;
    // 写缓存（尽力而为；失败不影响返回）。
    let _ = tokio::fs::create_dir_all(&cache_dir).await;
    let _ = tokio::fs::write(&cache_file, &body).await;
    Ok(value)
}

/// `pr://` / `issue://` 路由渲染：单对象 → 结构化 markdown；列表 → 逐条摘要。
/// 供 `read_file` 内部协议调用。
///
/// # Errors
/// 网络/解析失败时返回 [`ToolError::Execution`]。
pub async fn render_gh_uri(
    kind: &str,
    owner: &str,
    repo: &str,
    number: Option<u64>,
    query: &str,
    ctx: &ToolContext<'_>,
) -> Result<String, ToolError> {
    let endpoint = match kind {
        "pr" => "pulls",
        "issue" => "issues",
        other => {
            return Err(ToolError::InvalidArgs(format!(
                "未知 GitHub 类型 `{other}`（pr|issue）"
            )));
        }
    };
    let display = format!("{owner}/{repo}");
    if let Some(n) = number {
        let v = api_get_json(owner, repo, &format!("{endpoint}/{n}"), ctx).await?;
        let title = v
            .get("title")
            .and_then(|x| x.as_str())
            .unwrap_or("(无标题)");
        let state = v.get("state").and_then(|x| x.as_str()).unwrap_or("?");
        let user = v
            .pointer("/user/login")
            .and_then(|x| x.as_str())
            .unwrap_or("?");
        let created = v.get("created_at").and_then(|x| x.as_str()).unwrap_or("?");
        let body = v.get("body").and_then(|x| x.as_str()).unwrap_or("");
        let comments = v
            .get("comments")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let mut out = format!("#{n} {title} [{state}] — @{user} · {created}\n\n");
        if kind == "pr" {
            let merged = v
                .get("merged")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let additions = v
                .pointer("/additions")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let deletions = v
                .pointer("/deletions")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            out.push_str(&format!(
                "合并: {} · +{additions}/-{deletions} 行\n\n",
                if merged { "是" } else { "否" }
            ));
        }
        if !body.is_empty() {
            out.push_str(&format!("{body}\n\n"));
        }
        out.push_str(&format!(
            "评论 {comments} 条 · 来源: <https://github.com/{display}/{endpoint}/{n}>\n"
        ));
        Ok(out)
    } else {
        let v = api_get_json(owner, repo, &format!("{endpoint}?{query}"), ctx).await?;
        let arr = match v.as_array() {
            Some(a) => a,
            None => {
                return Err(ToolError::Execution(format!(
                    "{display} {endpoint} 列表响应非数组（可能是速率限制或 repo 不存在）"
                )));
            }
        };
        let mut out = format!(
            "{display} 的 {} 列表（共 {} 条，{query}）：\n",
            kind,
            arr.len()
        );
        for item in arr {
            let n = item
                .get("number")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let title = item
                .get("title")
                .and_then(|x| x.as_str())
                .unwrap_or("(无标题)");
            let state = item.get("state").and_then(|x| x.as_str()).unwrap_or("?");
            let user = item
                .pointer("/user/login")
                .and_then(|x| x.as_str())
                .unwrap_or("?");
            out.push_str(&format!("  #{n} [{state}] {title} — @{user}\n"));
        }
        out.push_str(&format!(
            "\n`read_file` 可读 `{kind}://{display}/<编号>` 看详情\n"
        ));
        Ok(out)
    }
}

/// 发送请求并返回响应体文本；二进制响应（如 logs 的 zip）回退为元信息。
///
/// # Errors
/// 网络错误或 HTTP 非 2xx（错误体截断到 [`ERROR_BODY_MAX`]）时返回 [`ToolError::Execution`]。
async fn fetch(req: RequestBuilder, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
    let cancel = ctx.cancel;
    let resp = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return Err(ToolError::Execution("GitHub 请求被取消".into()));
        }
        r = req.send() => r.map_err(|e| ToolError::Execution(format!("GitHub 请求失败：{e}")))?,
    };

    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = read_capped_bytes(resp).await?;

    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        return Err(ToolError::Execution(format!(
            "GitHub API 返回 {status}：{}",
            truncate_chars(&text, ERROR_BODY_MAX)
        )));
    }

    if is_text_content_type(&content_type) {
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        // 如 Actions logs 的 zip：不回灌二进制，仅返回元信息。
        Ok(format!(
            "[binary response: content-type={content_type}, {} bytes；若是 zip 日志，请本地下载解压]",
            bytes.len()
        ))
    }
}

/// 流式读取响应体并在超限时立即中止，避免恶意/异常响应（如巨型日志 zip）撑爆内存。
async fn read_capped_bytes(mut resp: reqwest::Response) -> Result<Vec<u8>, ToolError> {
    /// GitHub 响应体大小上限（超出即报错截断，防 OOM）。
    const GITHUB_MAX_BYTES: usize = 16 * 1024 * 1024;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| ToolError::Execution(format!("读取响应失败：{e}")))?;
        let Some(chunk) = chunk else { break };
        buf.extend_from_slice(&chunk);
        if buf.len() > GITHUB_MAX_BYTES {
            return Err(ToolError::Execution(format!(
                "GitHub 响应体超过 {GITHUB_MAX_BYTES} 字节上限"
            )));
        }
    }
    Ok(buf)
}

/// 是否为文本/JSON 类响应（可安全转字符串）。
fn is_text_content_type(ct: &str) -> bool {
    let ct = ct
        .split(';')
        .next()
        .unwrap_or(ct)
        .trim()
        .to_ascii_lowercase();
    ct.is_empty()
        || ct.contains("json")
        || ct.contains("text")
        || ct.contains("xml")
        || ct.contains("javascript")
        || ct.contains("urlencoded")
}

/// 安全截断到最多 `max` 个 Unicode 标量值，超长则追加 `…`。
fn truncate_chars(s: &str, max: usize) -> String {
    let mut iter = s.chars();
    let mut out: String = iter.by_ref().take(max).collect();
    if iter.next().is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_disallows_write() {
        let tool = GithubTool::default();
        assert!(!tool.allow_write());
        let tool = GithubTool::new(true);
        assert!(tool.allow_write());
    }

    #[test]
    fn validates_well_formed_repo() {
        assert!(validate_repo("octocat/Hello-World").is_ok());
        assert!(validate_repo("a.b_c/d-e.f").is_ok());
    }

    #[test]
    fn rejects_malformed_repo() {
        assert!(validate_repo("no-slash").is_err());
        assert!(validate_repo("/missing-owner").is_err());
        assert!(validate_repo("owner/").is_err());
        assert!(validate_repo("owner/repo/extra").is_err());
        assert!(validate_repo("owner/repo;rm -rf").is_err());
    }

    #[test]
    fn builds_read_endpoints() {
        assert_eq!(
            endpoint_path("get_pr", "o/r", Some(7)).unwrap(),
            "/repos/o/r/pulls/7"
        );
        assert_eq!(
            endpoint_path("list_prs", "o/r", None).unwrap(),
            "/repos/o/r/pulls"
        );
        assert_eq!(
            endpoint_path("get_run_logs", "o/r", Some(9)).unwrap(),
            "/repos/o/r/actions/runs/9/logs"
        );
    }

    #[test]
    fn get_actions_require_number() {
        assert!(endpoint_path("get_pr", "o/r", None).is_err());
        assert!(endpoint_path("get_run_logs", "o/r", None).is_err());
    }

    #[test]
    fn rejects_unknown_endpoint_action() {
        // create_pr/merge_pr/comment/graphql 不经 endpoint_path（走 build_request 的专门分支）
        assert!(endpoint_path("create_pr", "o/r", None).is_err());
    }

    #[test]
    fn describe_tiers_write_higher() {
        let tool = GithubTool::new(true);
        let read = json!({"action": "get_pr", "repo": "o/r", "number": 1});
        assert_eq!(tool.describe(&read).capability, CapabilityTier::Network);
        let write =
            json!({"action": "create_pr", "repo": "o/r", "title": "t", "head": "h", "base": "b"});
        assert_eq!(tool.describe(&write).capability, CapabilityTier::Write);
    }

    #[test]
    fn content_type_classification() {
        assert!(is_text_content_type("application/json"));
        assert!(is_text_content_type("text/plain; charset=utf-8"));
        assert!(!is_text_content_type("application/zip"));
        assert!(!is_text_content_type("image/png"));
    }

    #[test]
    fn prompt_section_mentions_actions() {
        assert!(PROMPT_SECTION.contains("create_pr"));
        assert!(PROMPT_SECTION.contains("graphql"));
        assert!(PROMPT_SECTION.contains("get_run_logs"));
    }

    #[test]
    fn schema_enumerates_all_actions() {
        let schema = GithubTool::default().schema();
        let actions = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("enum array");
        for a in [
            "get_pr",
            "graphql",
            "create_pr",
            "merge_pr",
            "comment",
            "get_run_logs",
            // H32：omp `gh` 只读 op 对应动作。
            "repo_view",
            "file_read",
            "search_issues",
            "search_prs",
            "search_code",
            "search_repos",
        ] {
            assert!(actions.iter().any(|v| v == a), "缺 action {a}");
        }
        // omp 的 `op` 别名与 `action` 同枚举；`repo` 不再强制（search_repos 可省）。
        assert!(schema["properties"]["op"]["enum"].is_array());
        let required = schema["required"].as_array().unwrap();
        assert!(!required.iter().any(|v| v == "repo"), "{required:?}");
    }

    /// H32：omp op 名 → Gyre action 的归一（含 `op` 字段别名）。
    #[test]
    fn action_alias_and_op_field() {
        let parse = |v: serde_json::Value| action_of(&v).map(str::to_string);
        assert_eq!(
            parse(serde_json::json!({"action": "pr_create"})).unwrap(),
            "create_pr",
            "omp 的 pr_create 等价 create_pr"
        );
        assert_eq!(
            parse(serde_json::json!({"op": "repo_view"})).unwrap(),
            "repo_view",
            "omp 用 op 字段"
        );
        assert_eq!(
            parse(serde_json::json!({"action": "get_pr", "op": "repo_view"})).unwrap(),
            "get_pr",
            "两个字段都给时 action 优先"
        );
        assert!(parse(serde_json::json!({})).is_err());
        assert!(parse(serde_json::json!({"op": "  "})).is_err());
        assert_eq!(normalize_action("get_pr"), "get_pr", "未登记的名字原样");
    }

    /// H32：contents API 的 base64 文本解码（目录/异常响应原样返回）。
    #[test]
    fn file_read_decodes_base64_contents() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode("hello\n世界");
        let body = serde_json::json!({
            "encoding": "base64",
            "content": format!("{encoded}\n"),  // GitHub 会插入换行
            "path": "a.txt"
        })
        .to_string();
        assert_eq!(decode_contents_body(&body), "hello\n世界");

        // 目录响应（数组）与非 base64 响应原样返回。
        let dir = r#"[{"name":"a.txt","type":"file"}]"#;
        assert_eq!(decode_contents_body(dir), dir);
        let large = serde_json::json!({"encoding": "none", "content": ""}).to_string();
        assert_eq!(decode_contents_body(&large), large);
        assert_eq!(decode_contents_body("not json"), "not json");
    }
}
