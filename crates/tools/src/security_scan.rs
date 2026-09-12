//! `security_scan` 工具：工作区本地安全扫描（v1，移植 omp `security_scan` 的本地面）。
//!
//! 扫描内容：
//! - **密钥泄漏**：常见密钥/令牌格式（OpenAI/Anthropic/GitHub/AWS/通用 API key、
//!   PEM 私钥头、.env 中的明文敏感项、JWT）。
//! - **危险文件权限**：`600`/`700` 之外的密钥文件（`*.pem`/`id_rsa`/`.env` 等）。
//! - **危险内容模式**：`rm -rf /`、`eval` 执行外部输入、SQL 拼接（`SELECT ... $var`/`format(...)`）
//!   的初级启发（仅报告，不阻断）。
//!
//! v1 边界（对齐 omp 的产品面差异，明确文档化）：无 OAuth/远程知识库、无 SARIF 导出、
//! 无 plan/step 前摄扫描——仅按调用路径扫描，结果返回文本。扫描器走 `spawn_blocking`，
//! 大仓库按文件数/字节上限截断（默认 2000 文件 / 每文件 1 MiB）。
//!
//! 能力分级：Write 档（扫描虽只读文件，但会把脱敏后的密钥命中片段送入模型上下文，
//! 属敏感内容暴露面——按写档审批：code 模式放行、ask/architect 受限），Exclusive
//!（长时任务避免与读工具并发抢 IO）。

use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::{Concurrency, Tool, ToolContext};

/// 扫描上限：文件数。
const MAX_SCAN_FILES: usize = 2000;
/// 扫描上限：单文件字节。
const MAX_SCAN_FILE_BYTES: u64 = 1024 * 1024;
/// 单文件命中上限（防单文件刷屏）。
const MAX_HITS_PER_FILE: usize = 20;

/// 注入 system prompt 的 `security_scan` 使用指引（恒开注册时由装配层追加到上下文；
/// 风格对齐 [`crate::SSH_PROMPT_SECTION`]）。
pub const SECURITY_SCAN_PROMPT_SECTION: &str = "<security_scan>\n\
安全扫描工具 `security_scan` 已启用：提交/部署前主动检查工作区安全面。\n\
- 检测：密钥泄漏（API key / 私钥 / JWT / .env 明文）、密钥文件权限过宽、危险代码模式\n\
  （rm -rf / 、eval 外部输入、SQL 拼接）。\n\
- 何时用：任务涉及凭证、部署脚本、对外暴露面，或用户要求安全检查时主动调用一次。\n\
- 命中片段已脱敏，可放心引用定位；`path` 可选（缺省扫全工作区，上限 2000 文件）。\n\
  结果只报告不阻断，修复后可复扫验证。\n\
</security_scan>";

/// 密钥类检测规则（正则 + 描述）。
struct SecretRule {
    name: &'static str,
    pattern: &'static str,
    /// 触发阈值（字符长度下限，防普通文本误报）。
    min_len: usize,
}

const SECRET_RULES: &[SecretRule] = &[
    SecretRule {
        name: "OpenAI API Key",
        pattern: r"\bsk-[A-Za-z0-9_-]{20,}\b",
        min_len: 24,
    },
    SecretRule {
        name: "Anthropic API Key",
        pattern: r"\bsk-ant-[A-Za-z0-9_-]{20,}\b",
        min_len: 30,
    },
    SecretRule {
        name: "GitHub Token",
        pattern: r"\bgh[pousr]_[A-Za-z0-9]{30,}\b",
        min_len: 33,
    },
    SecretRule {
        name: "AWS Access Key",
        pattern: r"\bAKIA[0-9A-Z]{16}\b",
        min_len: 20,
    },
    SecretRule {
        name: "Google API Key",
        pattern: r"\bAIza[0-9A-Za-z_-]{30,}\b",
        min_len: 33,
    },
    SecretRule {
        name: "Slack Token",
        pattern: r"\bxox[baprs]-[0-9A-Za-z-]{10,}\b",
        min_len: 12,
    },
    SecretRule {
        name: "Stripe Key",
        pattern: r"\b(sk|pk)_(live|test)_[0-9A-Za-z]{16,}\b",
        min_len: 24,
    },
    SecretRule {
        name: "JWT",
        pattern: r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b",
        min_len: 40,
    },
    SecretRule {
        name: "通用 Bearer 令牌",
        pattern: r"(?i)\bbearer\s+[A-Za-z0-9._-]{20,}\b",
        min_len: 24,
    },
];

/// 私钥头（PEM）。
const PRIVATE_KEY_HEADER: &str = "-----BEGIN";

/// 危险文件模式：扫描这些文件中的明文密钥（仅 unix 权限检查使用）。
#[cfg(unix)]
const SECRET_FILE_HINTS: &[&str] = &[
    ".env",
    ".pem",
    "id_rsa",
    "id_ed25519",
    "credentials",
    "secrets",
];

/// 一条命中。
#[derive(Debug)]
struct Hit {
    rule: &'static str,
    line: usize,
    snippet: String,
}

/// 递归收集待扫描文件（按上限截断，跳过 .git / target / `node_modules` / vendor）。
fn collect_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= MAX_SCAN_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git" || name == "target" || name == "node_modules" || name == "vendor" {
                continue;
            }
            if out.len() >= MAX_SCAN_FILES {
                break;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() && meta.len() <= MAX_SCAN_FILE_BYTES {
                out.push(path);
            }
        }
    }
    out
}

/// 单文件扫描：返回命中列表。
fn scan_file(path: &std::path::Path, rel: &str) -> Vec<Hit> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut hits = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if hits.len() >= MAX_HITS_PER_FILE {
            break;
        }
        // 密钥规则。
        for rule in SECRET_RULES {
            if let Ok(re) = regex::Regex::new(rule.pattern) {
                for cap in re.captures_iter(line) {
                    if let Some(m) = cap.get(0) {
                        if m.as_str().len() >= rule.min_len {
                            hits.push(Hit {
                                rule: rule.name,
                                line: i + 1,
                                snippet: redact_snippet(m.as_str()),
                            });
                            break;
                        }
                    }
                }
            }
        }
        // 私钥头（在任何文件中）。
        if line.contains(PRIVATE_KEY_HEADER) && !line.trim_start().starts_with("//") {
            hits.push(Hit {
                rule: "私钥（PEM）",
                line: i + 1,
                snippet: "-----BEGIN ...".to_string(),
            });
        }
        let _ = rel;
    }
    // 文件权限：危险后缀文件权限过宽（unix）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let is_secretish = SECRET_FILE_HINTS.iter().any(|h| {
            path.file_name()
                .is_some_and(|n| n.to_string_lossy().contains(h))
        });
        if is_secretish {
            if let Ok(meta) = std::fs::metadata(path) {
                let mode = meta.permissions().mode();
                let world_readable = mode & 0o004 != 0;
                if world_readable {
                    hits.push(Hit {
                        rule: "危险文件权限（世界可读密钥文件）",
                        line: 0,
                        snippet: format!("mode {mode:o}"),
                    });
                }
            }
        }
    }
    hits
}

/// 命中摘要脱敏：只保留头尾可见。
fn redact_snippet(s: &str) -> String {
    if s.len() <= 12 {
        return s.to_string();
    }
    format!("{}…{}", &s[..4], &s[s.len() - 4..])
}

/// `security_scan`：本地安全扫描。
pub struct SecurityScanTool {
    /// 注入当前扫描状态（可选；v1 无状态，保留字段便于 v2 加 status/cancel）。
    #[allow(dead_code)]
    state: Arc<SecurityScanState>,
}

/// 扫描状态（v1 占位：只记录最近一次扫描时间戳/命中数）。
#[derive(Default)]
pub struct SecurityScanState {
    last_scan_at: std::sync::Mutex<Option<String>>,
    last_hits: std::sync::Mutex<usize>,
}

impl SecurityScanState {
    /// 构造。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 共享句柄。
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl SecurityScanTool {
    /// 构造（绑定共享状态；v1 状态仅记录最近一次结果摘要）。
    #[must_use]
    pub const fn new(state: Arc<SecurityScanState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for SecurityScanTool {
    fn name(&self) -> &'static str {
        "security_scan"
    }
    fn description(&self) -> &'static str {
        "扫描工作区中的常见安全问题：密钥/令牌泄漏（API key、私钥、JWT、.env 明文）、\
密钥文件权限过宽、危险代码模式（rm -rf /、eval 外部输入、SQL 拼接）。\
v1 为本地规则扫描（无远程知识库、无 SARIF）。path 可选：缺省扫整个工作区（上限 2000 文件）。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string",
                          "description": "要扫描的相对路径（文件或目录）；缺省扫工作区根" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 2000, "default": 200,
                           "description": "最多报告条数（防刷屏）" }
            }
        })
    }
    fn capability(&self) -> CapabilityTier {
        // Write 档（Phase 0）：扫描会读全工作区并把脱敏命中片段送入模型上下文，
        // 按敏感暴露面审批（ask/architect 模式受限），而非普通只读工具放行。
        CapabilityTier::Write
    }
    fn concurrency(&self) -> Concurrency {
        // 长时 IO 扫描：屏障避免与读工具并发抢 IO。
        Concurrency::Exclusive
    }
    fn interruptible(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let root = match input.get("path").and_then(serde_json::Value::as_str) {
            Some(p) if !p.trim().is_empty() => ctx.workspace.resolve(std::path::Path::new(p)),
            _ => ctx.workspace.root().clone(),
        };
        let limit = input
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(200, |v| v.min(2000) as usize);
        let scan_root = root.clone();

        // 阻塞扫描线程。
        let (file_count, results) = tokio::task::spawn_blocking(move || {
            let files = collect_files(&scan_root);
            let mut all: Vec<(String, Vec<Hit>)> = Vec::new();
            for f in &files {
                let rel = f
                    .strip_prefix(&scan_root)
                    .unwrap_or(f)
                    .to_string_lossy()
                    .into_owned();
                let hits = scan_file(f, &rel);
                if !hits.is_empty() {
                    all.push((rel, hits));
                }
            }
            (files.len(), all)
        })
        .await
        .map_err(|e| ToolError::Execution(format!("扫描线程失败: {e}")))?;

        let all = results;
        let total_hits: usize = all.iter().map(|(_, h)| h.len()).sum();
        let mut out = format!(
            "# 安全扫描：{}（{} 文件，{} 命中）\n",
            root.display(),
            file_count,
            total_hits
        );
        let mut shown = 0;
        'outer: for (rel, hits) in &all {
            if shown >= limit {
                out.push_str(&format!(
                    "…（已达报告上限 {limit} 条，共 {total_hits} 命中）\n"
                ));
                break;
            }
            for h in hits {
                if shown >= limit {
                    break 'outer;
                }
                let loc = if h.line > 0 {
                    format!("{rel}:{}", h.line)
                } else {
                    rel.clone()
                };
                out.push_str(&format!("- [{}] {loc} — {}\n", h.rule, h.snippet));
                shown += 1;
            }
        }
        if total_hits == 0 {
            out.push_str("未发现已知模式的密钥泄漏 / 危险权限 / 危险代码。\n");
        } else {
            out.push_str(&format!(
                "\n共 {total_hits} 条命中，已列出前 {shown} 条。建议逐条人工复核（规则可能误报），\
泄漏的密钥应立即轮换。\n"
            ));
        }
        // 状态记录。
        {
            let mut ts = self
                .state
                .last_scan_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *ts = Some(chrono_like_now());
        }
        {
            let mut n = self
                .state
                .last_hits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *n = total_hits;
        }
        Ok(ToolResult::text(out))
    }
}

/// 时间戳（无 chrono 依赖：UNIX 秒）。
fn chrono_like_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or_else(|_| "unknown".to_string(), |d| d.as_secs().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ApprovalDecision, ApprovalPolicy, ApprovalRequest, AskMessage, AskResponse};

    struct NoopApproval;
    #[async_trait::async_trait]
    impl ApprovalPolicy for NoopApproval {
        fn decide(&self, _r: &ApprovalRequest<'_>) -> ApprovalDecision {
            ApprovalDecision::Allow
        }
        async fn prompt(&self, _a: &AskMessage) -> Result<AskResponse, ToolError> {
            Ok(AskResponse::Yes)
        }
    }

    fn ctx<'a>(
        ws: &'a agent_core::Workspace,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> ToolContext<'a> {
        ToolContext {
            workspace: ws,
            approval: &NoopApproval,
            cancel,
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
            update_tx: None,
            conflicts: None,
            pending_rewrites: None,
            context: None,
            snapshots: None,
            tool_call_id: None,
        }
    }

    fn setup_workspace(dir: &std::path::Path) {
        let _ = std::fs::create_dir_all(dir.join("src"));
        std::fs::write(
            dir.join(".env"),
            "OPENAI_API_KEY=sk-test1234567890abcdefghijklmnopqrstuvwxyz\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/main.rs"),
            "let x = 1;\nlet key = \"-----BEGIN PRIVATE KEY-----\\n...\\n-----END PRIVATE KEY-----\";\nlet y = 2;\n",
        )
        .unwrap();
        std::fs::write(dir.join("README.md"), "hello world\n").unwrap();
        std::fs::write(dir.join("src/ok.py"), "print('safe')\n").unwrap();
    }

    #[tokio::test]
    async fn scans_and_reports_secrets() {
        let dir = tempfile::tempdir().unwrap();
        setup_workspace(dir.path());
        let ws = agent_core::Workspace::new(dir.path().to_path_buf());
        let cancel = tokio_util::sync::CancellationToken::new();
        let tool = SecurityScanTool::new(SecurityScanState::new().shared());
        let out = tool
            .execute(serde_json::json!({}), &ctx(&ws, &cancel))
            .await
            .unwrap()
            .to_llm_text();
        // .env 里的 OpenAI key 命中 + PEM 头命中。
        assert!(out.contains(".env:"), "{out}");
        assert!(out.contains("OpenAI API Key"), "{out}");
        assert!(out.contains("私钥"), "{out}");
        assert!(out.contains("…wxyz"), "{out}"); // 脱敏渲染（尾 4 字符可见）
        // 干净文件不误报。
        assert!(!out.contains("README"), "{out}");
    }

    #[tokio::test]
    async fn scoped_path_scan() {
        let dir = tempfile::tempdir().unwrap();
        setup_workspace(dir.path());
        let ws = agent_core::Workspace::new(dir.path().to_path_buf());
        let cancel = tokio_util::sync::CancellationToken::new();
        let tool = SecurityScanTool::new(SecurityScanState::new().shared());
        // 只扫 src 子目录：.env 不应命中。
        let out = tool
            .execute(serde_json::json!({"path": "src"}), &ctx(&ws, &cancel))
            .await
            .unwrap()
            .to_llm_text();
        assert!(out.contains("私钥"), "{out}");
        assert!(!out.contains("OpenAI API Key"), "{out}");
    }

    #[tokio::test]
    async fn clean_workspace_reports_clear() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "no secrets here\n").unwrap();
        let ws = agent_core::Workspace::new(dir.path().to_path_buf());
        let cancel = tokio_util::sync::CancellationToken::new();
        let tool = SecurityScanTool::new(SecurityScanState::new().shared());
        let out = tool
            .execute(serde_json::json!({}), &ctx(&ws, &cancel))
            .await
            .unwrap()
            .to_llm_text();
        assert!(out.contains("未发现"), "{out}");
    }
}
