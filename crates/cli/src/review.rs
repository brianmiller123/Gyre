//! `/review` 评审流：收集 git diff → 按 diff 权重分配 1-N 个 TaskTool 子代理并行评审 →
//! P0-P3 分级 + 置信度聚合输出。
//!
//! 评审是宿主动作：报告只经 `eprintln` 输出，不注入模型上下文（避免污染对话）。
//! 子代理复用 main 装配的 Provider/Tools/PromptCatalog/Workspace（经 [`ReviewEnv`] 传入），
//! 各自持有独立上下文；审批注入 Ask 模式规则（写操作硬拒绝，评审只读）。

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use agent_core::{ApprovalPolicy, LlmProvider, Mode, Model, ProviderCallContext, Workspace};
use agent_i18n::t;
use agent_tools::Tool as _;
use serde_json::json;

/// diff 文本体积上限：超过则只取 `--stat` 摘要 + 每文件前 [`MAX_FILE_DIFF_LINES`] 行。
pub const MAX_DIFF_BYTES: usize = 256 * 1024;
/// diff 超限时每个文件保留的 diff 行数（含头部）。
const MAX_FILE_DIFF_LINES: usize = 200;
/// 评审子代理数下限（契约：1-4）。
const MIN_REVIEWERS: usize = 1;
/// 评审子代理数上限（契约：1-4）。
const MAX_REVIEWERS: usize = 4;

/// 评审准则模板（`prompts/review.md`，编译期内嵌；子代理任务 = 模板 + diff 子集）。
const REVIEW_TEMPLATE: &str = include_str!("../../../prompts/review.md");

/// `/review` 运行所需的装配依赖（main 从既有装配中复用，不新建任何组件）。
pub struct ReviewEnv<'a> {
    /// 工作区根（git diff 执行目录）。
    pub cwd: &'a Path,
    /// LLM Provider（与主 Agent 共享）。
    pub provider: &'a Arc<dyn LlmProvider>,
    /// 子 Agent 工具集（不含 task，防递归）。
    pub sub_tools: &'a Arc<dyn agent_tools::ToolRegistry>,
    /// Prompt 目录。
    pub prompts: &'a Arc<agent_prompt::PromptCatalog>,
    /// 工作区。
    pub workspace: &'a Arc<Workspace>,
    /// 评审用模型。
    pub model: &'a Model,
    /// 评审用 Provider 调用上下文。
    pub provider_ctx: &'a ProviderCallContext,
    /// 子 Agent 最大失误轮次。
    pub max_mistakes: usize,
    /// 子 Agent 上下文窗口阈值。
    pub context_guard: f32,
    /// 子 Agent 最大输出 token。
    pub max_output: usize,
    /// 子 Agent 空上下文工厂（每次委派全新上下文）。
    pub context_factory: &'a agent::ContextFactory,
    /// 子 Agent 继承的 temperature（`None` 用模型默认）。
    pub temperature: Option<f32>,
    /// 子 Agent 继承的 thinking 配置（`None` 不思考）。
    pub thinking: Option<agent_core::ThinkingConfig>,
    /// 子 Agent 并发护栏。
    pub max_concurrent: usize,
    /// 子 Agent 监控总线（与 `/agents` 仪表盘共享）。
    pub supervisor: Arc<agent_supervisor::Supervisor>,
    /// 评审子代理审批策略（只读：写操作硬拒绝）。
    pub approval: Arc<dyn ApprovalPolicy>,
}

/// git diff 收集结果。
pub struct DiffData {
    /// 变更文件路径列表（按 diff 出现顺序）。
    pub files: Vec<String>,
    /// 新增行总数（`+` 行，不含 `+++` 文件头）。
    pub added_lines: usize,
    /// 供评审的 diff 文本（超过体积上限时为 stat + 每文件前 200 行）。
    pub body: String,
    /// diff 是否因过大被截断。
    pub truncated: bool,
}

/// git diff 收集失败原因。
pub enum DiffError {
    /// 非 git 仓库。
    NotGit,
    /// 其它执行/读取失败（携带原因）。
    Failed(String),
}

/// 单条评审发现。
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// 严重级别：0=P0（阻塞）… 3=P3（建议）。
    pub level: u8,
    /// 置信度（0.0-1.0，1 = 完全确定）。
    pub confidence: f32,
    /// 位置（`文件路径:行号` 或 `文件路径`）。
    pub location: String,
    /// 问题描述。
    pub description: String,
    /// 修复建议。
    pub suggestion: String,
}

/// 运行一次 `/review`：收集 diff → 分配子代理数 → 切分 → 并行评审 → 聚合输出。
///
/// 全程只 `eprintln`（评审是宿主动作，不进模型上下文）。
pub async fn run_review(staged: bool, explicit: Option<usize>, env: &ReviewEnv<'_>) {
    // 1. 收集 git diff（HEAD / --cached）。
    let data = match collect_diff(env.cwd, staged).await {
        Ok(d) => d,
        Err(DiffError::NotGit) => {
            eprintln!("{}", t!("review.no_git"));
            return;
        }
        Err(DiffError::Failed(e)) => {
            eprintln!("[review] diff 收集失败：{e}");
            return;
        }
    };
    if data.files.is_empty() {
        // 空 diff（无变更）：直接报告，不启动子代理。
        eprintln!("{}", t!("review.title"));
        eprintln!("{}", t!("review.empty"));
        return;
    }

    // 2. 权重 → 评审子代理数：显式 N 钳位 1-4；默认按文件数分层；不超文件数。
    let reviewers = pick_reviewer_count(explicit, data.files.len()).min(data.files.len());

    // 3. 按文件切分 diff 子集（连续切片，保持同一变更区域的上下文聚合）。
    let entries = parse_diff_entries(&data.body);
    let chunks = chunk_diff_entries(&entries, reviewers);
    debug_assert!(!chunks.is_empty());

    // 4. 复用 main 装配构造 TaskTool（mode=code、独立上下文、只读审批），
    //    以 `tasks` 数组并行执行 N 个评审子代理（TaskTool 内部按并发护栏调度）。
    let task_tool = agent::TaskTool::new(
        Arc::clone(env.provider),
        Arc::clone(env.sub_tools),
        Arc::clone(env.prompts),
        Arc::clone(env.workspace),
        env.model.clone(),
        env.provider_ctx.clone(),
        Mode::Code,
        env.max_mistakes,
        env.context_guard,
        env.max_output,
        Arc::clone(env.context_factory),
        env.temperature,
        env.thinking.clone(),
        env.max_concurrent,
    )
    .with_supervisor(env.supervisor.clone())
    .with_approval(Arc::clone(&env.approval));

    let tasks: Vec<String> = chunks
        .iter()
        .map(|chunk| format!("{REVIEW_TEMPLATE}\n```diff\n{chunk}\n```"))
        .collect();
    eprintln!("{}", t!("review.starting", n = tasks.len()));

    let cancel = tokio_util::sync::CancellationToken::new();
    let tool_ctx = agent_tools::ToolContext {
        workspace: env.workspace.as_ref(),
        approval: env.approval.as_ref(),
        cancel: &cancel,
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
    };
    let all_text = match task_tool
        .execute(json!({ "tasks": tasks }), &tool_ctx)
        .await
    {
        Ok(agent_core::ToolResult::Text(t)) => t,
        Ok(other) => other.to_llm_text(),
        Err(e) => {
            eprintln!("[review] 评审子代理执行失败：{e}");
            return;
        }
    };

    // 5. 聚合：提取结构化发现，按 P0→P3 排序（同级置信度降序），输出报告。
    let findings = extract_findings(&all_text);
    eprintln!(
        "{}",
        t!("review.done", n = reviewers, findings = findings.len())
    );
    eprintln!("{}", render_report(reviewers, &data, &findings));
}

// ── git diff 收集 ─────────────────────────────────────────────────────────────

/// 收集 git diff：`staged=false` 用 `git diff HEAD`，`staged=true` 用 `git diff --cached`。
///
/// 非 git 仓库返回 [`DiffError::NotGit`]；无提交（HEAD 不存在）视为空 diff；
/// 超限时降级为 `--stat` 摘要 + 每文件前 [`MAX_FILE_DIFF_LINES`] 行。
pub async fn collect_diff(cwd: &Path, staged: bool) -> Result<DiffData, DiffError> {
    if !is_git_repo(cwd).await {
        return Err(DiffError::NotGit);
    }
    if !has_head(cwd).await {
        // 无任何提交：HEAD 不存在，等价于空 diff。
        return Ok(DiffData {
            files: Vec::new(),
            added_lines: 0,
            body: String::new(),
            truncated: false,
        });
    }
    let range = if staged { "--cached" } else { "HEAD" };
    let diff = git(cwd, &["diff", range]).await?;
    let (body, truncated) = if diff.len() > MAX_DIFF_BYTES {
        let stat = git(cwd, &["diff", "--stat", range]).await?;
        (truncate_diff(&diff, MAX_FILE_DIFF_LINES, &stat), true)
    } else {
        (diff, false)
    };
    let entries = parse_diff_entries(&body);
    let files = entries.into_iter().map(|(f, _)| f).collect();
    let added_lines = count_added_lines(&body);
    Ok(DiffData {
        files,
        added_lines,
        body,
        truncated,
    })
}

/// 执行 git 命令（`cwd` 下），成功返回 stdout（宽松 UTF-8 解码）。
async fn git(cwd: &Path, args: &[&str]) -> Result<String, DiffError> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| DiffError::Failed(format!("git 执行失败: {e}")))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(DiffError::Failed(format!(
            "git {} 失败: {msg}",
            args.join(" ")
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 是否位于 git 工作树内。
async fn is_git_repo(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--is-inside-work-tree"])
        .await
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

/// 是否存在 HEAD（有至少一个提交）。
async fn has_head(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--verify", "-q", "HEAD"])
        .await
        .is_ok()
}

/// diff 超限降级：保留每文件前 `max_lines` 行，前置 `--stat` 摘要。
#[must_use]
fn truncate_diff(diff: &str, max_lines: usize, stat: &str) -> String {
    let mut out = String::new();
    out.push_str(stat.trim_end());
    out.push_str("\n\n");
    let mut kept = 0usize;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            kept = 0;
        }
        if kept < max_lines {
            out.push_str(line);
            out.push('\n');
        }
        kept += 1;
    }
    out
}

// ── 纯函数：解析 / 权重 / 切分（供单测直测）──────────────────────────────────

/// 从 git diff 文本解析变更文件段：按 `diff --git a/… b/…` 行切分，
/// 每个元素为 (文件路径, 该文件的完整 diff 段文本)。
///
/// 文件路径取 b 侧（目标侧），rename 时取新路径；路径含引号（空格等特殊字符）时去引号。
#[must_use]
pub fn parse_diff_entries(diff: &str) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(prev) = current.take() {
                entries.push(prev);
            }
            // 两种 git 格式：`a/old.rs b/new.rs`（常规）与 `"a/foo bar.rs" "b/foo bar.rs"`（引号包裹）。
            // 引号格式：取最后一个引号对内的路径；常规格式：取最后一个 ` b/` 之后。
            let path = if rest.starts_with('"') && rest.ends_with('"') {
                let last = rest.rfind('"').unwrap_or(rest.len() - 1);
                let start = rest[..last].rfind('"').map_or(0, |p| p + 1);
                rest[start..last].trim().to_string()
            } else {
                rest.rsplit(" b/")
                    .next()
                    .unwrap_or(rest)
                    .trim()
                    .trim_matches('"')
                    .to_string()
            };
            // 去掉 `b/` 前缀标记（引号格式下路径以 b/ 开头）。
            let path = path.strip_prefix("b/").unwrap_or(&path).to_string();
            current = Some((path, format!("{line}\n")));
        } else if let Some((_, seg)) = current.as_mut() {
            seg.push_str(line);
            seg.push('\n');
        }
    }
    if let Some(prev) = current.take() {
        entries.push(prev);
    }
    entries
}

/// 统计新增行数：以 `+` 开头的行（排除 `+++` 文件头）。
#[must_use]
pub fn count_added_lines(diff: &str) -> usize {
    diff.lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count()
}

/// diff 权重分：`文件数 × 3 + 新增行数 ÷ 20`（供报告展示与默认子代理数分层参考）。
#[must_use]
pub const fn diff_weight(files: usize, added_lines: usize) -> usize {
    files.saturating_mul(3) + added_lines.saturating_div(20)
}

/// 按文件数自动分配评审子代理数（契约：≤2 → 1，≤6 → 2，否则 4）。
#[must_use]
pub const fn auto_reviewer_count(files: usize) -> usize {
    match files {
        0..=2 => 1,
        3..=6 => 2,
        _ => 4,
    }
}

/// 最终评审子代理数：显式 N 钳位到 [`MIN_REVIEWERS`]..=[`MAX_REVIEWERS`]；
/// 未指定按文件数自动。
#[must_use]
pub fn pick_reviewer_count(explicit: Option<usize>, files: usize) -> usize {
    match explicit {
        Some(n) => n.clamp(MIN_REVIEWERS, MAX_REVIEWERS),
        None => auto_reviewer_count(files),
    }
}

/// 把变更文件段均分为 `n` 组（连续切片，组间文件数均衡；`n` 超文件数时按文件数）。
/// 返回每组拼接后的 diff 子文本（保留原文件顺序）。空输入返回空 Vec。
#[must_use]
pub fn chunk_diff_entries(entries: &[(String, String)], n: usize) -> Vec<String> {
    if entries.is_empty() || n == 0 {
        return Vec::new();
    }
    let n = n.min(entries.len());
    let mut chunks: Vec<String> = vec![String::new(); n];
    for (i, (_, seg)) in entries.iter().enumerate() {
        chunks[i * n / entries.len()].push_str(seg);
    }
    chunks
}

// ── 聚合：发现提取 / 排序 / 报告渲染 ─────────────────────────────────────────

/// 从子代理输出中提取结构化发现行：`[P<0-3>] <置信度> <位置> | <描述> | <建议>`。
///
/// 严格解析：级别/置信度/位置任一不合法即跳过该行（模板示例、普通叙述不受影响；
/// diff 上下文行以 `+`/`-`/空格 开头，天然不匹配）。
#[must_use]
pub fn extract_findings(text: &str) -> Vec<Finding> {
    text.lines().filter_map(parse_finding_line).collect()
}

/// 解析单行发现；格式不合法返回 `None`。
fn parse_finding_line(line: &str) -> Option<Finding> {
    let line = line.trim();
    // [P<0-3>] <conf> <loc> | <desc> | <sugg>
    let rest = line.strip_prefix("[P")?;
    let level: u8 = rest.chars().next()?.to_digit(10)? as u8;
    if level > 3 {
        return None;
    }
    let rest = rest.get(1..)?.strip_prefix(']')?.trim_start();
    let end = rest.find(char::is_whitespace)?;
    let confidence: f32 = rest[..end].parse().ok()?;
    if !(0.0..=1.0).contains(&confidence) {
        return None;
    }
    let rest = rest[end..].trim_start();
    if rest.is_empty() {
        return None;
    }
    let mut parts = rest.splitn(3, '|');
    let location = parts.next()?.trim().to_string();
    let description = parts.next().unwrap_or("").trim().to_string();
    let suggestion = parts.next().unwrap_or("").trim().to_string();
    if location.is_empty() {
        return None;
    }
    Some(Finding {
        level,
        confidence,
        location,
        description,
        suggestion,
    })
}

/// 按 P0→P3 升序、同级置信度降序排序。
pub fn sort_findings(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        a.level
            .cmp(&b.level)
            .then(b.confidence.total_cmp(&a.confidence))
    });
}

/// 渲染评审报告文本（标题 + 统计 + 分级发现列表）。空发现时输出 [`t!`]`("review.empty")`。
#[must_use]
pub fn render_report(reviewers: usize, data: &DiffData, findings: &[Finding]) -> String {
    let mut out = t!("review.title");
    out.push('\n');
    let _ = writeln!(
        out,
        "{} 个评审子代理 · {} 个文件 · {} 新增行 · diff 权重 {}",
        reviewers,
        data.files.len(),
        data.added_lines,
        diff_weight(data.files.len(), data.added_lines)
    );
    if data.truncated {
        out.push_str(" · diff 过大已截断\n");
    }
    if findings.is_empty() {
        out.push_str(&t!("review.empty"));
        out.push('\n');
        return out;
    }
    let mut sorted = findings.to_vec();
    sort_findings(&mut sorted);
    for f in &sorted {
        let lvl = ["P0", "P1", "P2", "P3"][f.level as usize];
        let _ = writeln!(
            out,
            "[{lvl}] ({:.2}) {} — {}",
            f.confidence, f.location, f.description
        );
        if !f.suggestion.is_empty() {
            let _ = writeln!(out, "       建议: {}", f.suggestion);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_DIFF: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,5 +1,6 @@
 fn main() {
     println!(\"hello\");
+    println!(\"world\");
 }
diff --git a/src/lib.rs b/src/lib.rs
index 3333333..4444444 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,3 +10,4 @@
 pub fn add(a: i32, b: i32) -> i32 {
     a + b
 }
+// new comment
";

    #[test]
    fn parses_diff_entries_by_file() {
        let entries = parse_diff_entries(FIXTURE_DIFF);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "src/main.rs");
        assert_eq!(entries[1].0, "src/lib.rs");
        assert!(entries[0].1.contains("diff --git a/src/main.rs"));
        assert!(!entries[0].1.contains("diff --git a/src/lib.rs"));
        // 尾部文件段完整闭合。
        assert!(entries[1].1.contains("+// new comment"));
    }

    #[test]
    fn parses_diff_rename_and_quoted_paths() {
        let entries = parse_diff_entries(
            "diff --git a/old.rs b/new.rs\n--- a/old.rs\n+++ b/new.rs\ndiff --git \"a/foo bar.rs\" \"b/foo bar.rs\"\n--- \"a/foo bar.rs\"\n+++ \"b/foo bar.rs\"\n",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "new.rs"); // rename 取新路径
        assert_eq!(entries[1].0, "foo bar.rs"); // 去引号
    }

    #[test]
    fn parses_diff_with_leading_stat_ignored() {
        // 截断降级时 body 前置 --stat 文本；stat 行不应被当成文件段。
        let diff = " src/main.rs | 5 +++--\n 1 file changed\n\ndiff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n+ok\n";
        let entries = parse_diff_entries(diff);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "src/main.rs");
    }

    #[test]
    fn counts_added_lines_excluding_headers() {
        assert_eq!(count_added_lines(FIXTURE_DIFF), 2);
        // +++ 文件头与上下文行不计。
        assert_eq!(count_added_lines("+++ b/x\n a\n-b\n+c\n"), 1);
    }

    #[test]
    fn diff_weight_scales_with_files_and_added() {
        assert_eq!(diff_weight(1, 0), 3);
        assert_eq!(diff_weight(2, 20), 7); // 2*3 + 20/20
        assert_eq!(diff_weight(6, 100), 23); // 6*3 + 100/20
    }

    #[test]
    fn auto_reviewer_count_tiers() {
        assert_eq!(auto_reviewer_count(0), 1);
        assert_eq!(auto_reviewer_count(1), 1);
        assert_eq!(auto_reviewer_count(2), 1);
        assert_eq!(auto_reviewer_count(3), 2);
        assert_eq!(auto_reviewer_count(6), 2);
        assert_eq!(auto_reviewer_count(7), 4);
        assert_eq!(auto_reviewer_count(20), 4);
    }

    #[test]
    fn pick_reviewer_count_explicit_clamped_and_auto() {
        assert_eq!(pick_reviewer_count(Some(3), 10), 3);
        assert_eq!(pick_reviewer_count(Some(0), 10), 1); // 钳位下限
        assert_eq!(pick_reviewer_count(Some(9), 10), 4); // 钳位上限
        assert_eq!(pick_reviewer_count(None, 1), 1);
        assert_eq!(pick_reviewer_count(None, 5), 2);
        assert_eq!(pick_reviewer_count(None, 8), 4);
    }

    #[test]
    fn chunks_diff_entries_contiguously() {
        let entries: Vec<(String, String)> = (0..5)
            .map(|i| (format!("f{i}.rs"), format!("[seg{i}]")))
            .collect();
        let chunks = chunk_diff_entries(&entries, 2);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].contains("[seg0]") && chunks[0].contains("[seg2]"));
        assert!(!chunks[0].contains("[seg3]"));
        assert!(chunks[1].contains("[seg3]") && chunks[1].contains("[seg4]"));
        // n 超文件数 → 按文件数分；空输入 → 空。
        assert_eq!(chunk_diff_entries(&entries, 9).len(), 5);
        assert!(chunk_diff_entries(&[], 2).is_empty());
    }

    #[test]
    fn truncate_diff_prepends_stat_and_limits_per_file() {
        let diff = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
+a
-b
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -1 +1 @@
+c
-d
";
        let out = truncate_diff(diff, 2, " a.rs | 2 +-");
        assert!(out.starts_with(" a.rs | 2 +-\n\n"));
        // 每文件只保留前 2 行（含 diff --git 头）。
        assert_eq!(out.matches("diff --git ").count(), 2);
        assert!(!out.contains("+a")); // 首文件第 3 行起被截
        assert!(!out.contains("-d")); // 次文件同样截断
    }

    #[test]
    fn extracts_and_parses_finding_lines() {
        let text = "\
总体结论：基本合格，但有空指针风险。
[P1] 0.85 src/main.rs:3 | 未处理空输入 | 增加早期返回
[P2] 0.6 src/lib.rs:12 | 重复代码 | 提取公共函数
未发现明显问题
[P0] 0.9 src/main.rs:1 | panic 风险 | 用 Option 代替
[P1] 0.85 src/main.rs:3 | 重复发现（应被保留） | 去重
[P3] 0.4 src/lib.rs | 命名建议 | 重命名
[P1] 0.85 | 缺位置 | 跳过
[P9] 0.5 src/x.rs | 级别越界 | 跳过
[P2] 1.5 src/x.rs | 置信度越界 | 跳过
[P1] <置信度> src/x.rs | 模板示例 | <建议>
";
        let findings = extract_findings(text);
        assert_eq!(findings.len(), 5);
        // 级别正确。
        assert!(
            findings
                .iter()
                .any(|f| f.level == 0 && f.location == "src/main.rs:1")
        );
        assert!(findings.iter().all(|f| f.level <= 3));
        // 模板示例（<置信度> 非浮点）与缺位置/越界行均被跳过。
        assert!(
            !findings
                .iter()
                .any(|f| f.location == "src/x.rs" && f.description == "模板示例")
        );
    }

    #[test]
    fn sorts_findings_by_level_then_confidence() {
        let mut findings = vec![
            Finding {
                level: 2,
                confidence: 0.8,
                location: "a".into(),
                description: "d".into(),
                suggestion: "s".into(),
            },
            Finding {
                level: 0,
                confidence: 0.5,
                location: "b".into(),
                description: "d".into(),
                suggestion: "s".into(),
            },
            Finding {
                level: 1,
                confidence: 0.9,
                location: "c".into(),
                description: "d".into(),
                suggestion: "s".into(),
            },
            Finding {
                level: 0,
                confidence: 0.9,
                location: "d".into(),
                description: "d".into(),
                suggestion: "s".into(),
            },
        ];
        sort_findings(&mut findings);
        let levels: Vec<u8> = findings.iter().map(|f| f.level).collect();
        assert_eq!(levels, vec![0, 0, 1, 2]);
        // 同级 P0 置信度降序。
        assert_eq!(findings[0].confidence, 0.9);
        assert_eq!(findings[1].confidence, 0.5);
    }

    #[test]
    fn renders_report_with_sorted_findings() {
        let data = DiffData {
            files: vec!["a.rs".into(), "b.rs".into()],
            added_lines: 40,
            body: String::new(),
            truncated: false,
        };
        let findings = vec![
            Finding {
                level: 2,
                confidence: 0.7,
                location: "a.rs:1".into(),
                description: "描述".into(),
                suggestion: "建议".into(),
            },
            Finding {
                level: 0,
                confidence: 0.95,
                location: "b.rs:2".into(),
                description: "阻塞".into(),
                suggestion: String::new(),
            },
        ];
        let report = render_report(2, &data, &findings);
        assert!(report.contains("[P0] (0.95) b.rs:2 — 阻塞"));
        assert!(report.contains("[P2] (0.70) a.rs:1 — 描述"));
        assert!(report.contains("建议: 建议"));
        // P0 先于 P2 输出。
        let p0 = report.find("[P0]").expect("含 P0");
        let p2 = report.find("[P2]").expect("含 P2");
        assert!(p0 < p2);
        // 空发现 → review.empty（i18n 文本随语言环境，此处断言语言无关语义）。
        let empty = render_report(1, &data, &[]);
        assert!(!empty.contains("[P"), "空发现不输出分级条目");
        assert!(!empty.contains("建议:"), "空发现不输出建议");
        assert!(
            !empty.trim().is_empty(),
            "空发现报告非空（含 review.empty）"
        );
    }
}
