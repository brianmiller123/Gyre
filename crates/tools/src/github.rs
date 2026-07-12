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
use serde_json::{json, Value};

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

/// 只读动作集合（`graphql` 不在此列——GraphQL 可执行 mutation，需 allow_write）。
const READ_ACTIONS: &[&str] = &[
    "get_pr",
    "list_prs",
    "get_issue",
    "list_issues",
    "list_runs",
    "get_run",
    "get_run_logs",
];

/// 写动作集合（需 `allow_write`）。`graphql` 用 POST 且可含 mutation，归入写门槛。
const WRITE_ACTIONS: &[&str] = &["create_pr", "merge_pr", "comment", "graphql"];

#[async_trait]
impl Tool for GithubTool {
    fn name(&self) -> &'static str {
        "github"
    }

    fn description(&self) -> &'static str {
        "查询/操作 GitHub PR、issue 与 Actions（CI）。\
         action ∈ {get_pr,list_prs,get_issue,list_issues,list_runs,get_run,get_run_logs,graphql\
         ,create_pr,merge_pr,comment}；repo=\"owner/name\"。\
         get_* 需 number；list_* 可选 limit；graphql 需 query（+可选 variables）；\
         create_pr 需 title/head/base；comment 需 body；merge_pr 可选 method。\
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
                    "description": "查询/操作动作"
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
            "required": ["action", "repo"]
        })
    }

    fn capability(&self) -> CapabilityTier {
        CapabilityTier::Network
    }

    fn describe<'a>(&'a self, input: &'a serde_json::Value) -> ApprovalRequest<'a> {
        // 写动作提升到 Write 审批门禁；其余为 Network。
        let is_write = input
            .get("action")
            .and_then(Value::as_str)
            .is_some_and(|a| WRITE_ACTIONS.contains(&a));
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

    async fn execute(
        &self,
        input: Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `action` 参数".into()))?;
        let repo = input
            .get("repo")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `repo` 参数".into()))?;
        validate_repo(repo)?;

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
        Ok(ToolResult::text(body))
    }
}

// ── 参数校验与解析 ────────────────────────────────────────────────────────────

/// 校验 `owner/repo`，仅允许 `[A-Za-z0-9._-]`，防路径注入。
fn validate_repo(repo: &str) -> Result<(), ToolError> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| ToolError::InvalidArgs("`repo` 须为 `owner/repo` 形式".into()))?;
    if owner.is_empty() || name.is_empty() {
        return Err(ToolError::InvalidArgs("`owner` 与 `repo` 均不得为空".into()));
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
            Ok(common_post("/graphql", json!({ "query": query, "variables": variables })))
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
    CLIENT.get().ok_or_else(|| {
        ToolError::Execution("GitHub HTTP 客户端初始化失败".into())
    })
}

/// 读取鉴权 token（`GH_TOKEN` 优先于 `GITHUB_TOKEN`），空值视为无。
fn auth_token() -> Option<String> {
    std::env::var("GH_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok()
        .filter(|s| !s.is_empty())
}

/// 发送请求并返回响应体文本；二进制响应（如 logs 的 zip）回退为元信息。
///
/// # Errors
/// 网络错误或 HTTP 非 2xx（错误体截断到 [`ERROR_BODY_MAX`]）时返回 [`ToolError::Execution`]。
async fn fetch(req: RequestBuilder, ctx: &ToolContext<'_>) -> Result<String, ToolError> {
    let cancel = ctx.cancel;
    let resp = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
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
    let ct = ct.split(';').next().unwrap_or(ct).trim().to_ascii_lowercase();
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
        assert_eq!(endpoint_path("list_prs", "o/r", None).unwrap(), "/repos/o/r/pulls");
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
        let write = json!({"action": "create_pr", "repo": "o/r", "title": "t", "head": "h", "base": "b"});
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
        for a in ["get_pr", "graphql", "create_pr", "merge_pr", "comment", "get_run_logs"] {
            assert!(actions.iter().any(|v| v == a), "缺 action {a}");
        }
    }
}
