//! 搜索工具：grep / glob（包装 agent-search，移植 pi-natives）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::{InMemorySnapshotStore, Tool, ToolContext, compute_file_hash, fs::display_path_in};

/// 单页最多展示的文件数（omp `DEFAULT_FILE_LIMIT`；超出用 `skip` 翻页）。
const FILE_LIMIT: usize = 20;
/// 多目标搜索的单文件命中帽（omp `MULTI_FILE_PER_FILE_MATCHES`：防单一热点
/// 文件挤占多样性；native 多取 1 条用于判定溢出）。
const MULTI_FILE_PER_FILE: usize = 20;
/// 单文件目标的命中帽（单文件无多样性问题，omp `SINGLE_FILE_MATCHES`）。
const SINGLE_FILE_MATCHES: usize = 200;
/// native 拉取总量软帽（omp `INTERNAL_TOTAL_CAP`：覆盖 20 文件 × 20 命中 +
/// 翻页余量，保证能报出文件总数下界）。
const INTERNAL_TOTAL_CAP: usize = 2000;
/// 单次搜索的墙钟预算（omp `SEARCH_GREP_TIMEOUT_MS`）：遍历触顶即停，
/// 不留孤儿线程烧 CPU。
const SEARCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 正则搜索文件内容（参数面对齐 oh-my-pi `grep.ts`：case / gitignore / skip
/// 翻页 / 20×20 文件窗口 / 2000 总帽 / 30s 超时；尊重 .gitignore）。
pub struct GrepTool;

/// 解析 `path` 参数：字符串或字符串数组均可，条目按 `;` 再切分（omp
/// semicolon-delimited 语义）；缺省 `["."]`。
fn parse_path_list(v: Option<&serde_json::Value>) -> Vec<String> {
    let mut out = Vec::new();
    match v {
        Some(serde_json::Value::String(s)) => {
            out.extend(split_paths(s));
        }
        Some(serde_json::Value::Array(items)) => {
            for it in items {
                if let Some(s) = it.as_str() {
                    out.extend(split_paths(s));
                }
            }
        }
        _ => out.push(".".to_string()),
    }
    if out.is_empty() {
        out.push(".".to_string());
    }
    out
}

fn split_paths(s: &str) -> Vec<String> {
    s.split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// 一次分块搜索的归并结果（绝对路径命中 + 截断元数据）。
struct MergedRuns {
    hits: Vec<agent_search::GrepHit>,
    limit_reached: bool,
    timed_out: bool,
    oversized_windowed: usize,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "在工作区正则搜索文件内容（尊重 .gitignore，隐藏文件也搜）。\
         多文件时按文件分组展示（20 文件/页 × 每文件 20 命中），用 skip 翻页。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern":   { "type": "string", "description": "正则模式（Rust regex 语法；保留首尾空白，空模式报错）" },
                "path": {
                    "description": "搜索目标：文件、目录或 glob（如 src/**/*.ts）；多个用分号分隔（\"src; tests\"）；缺省搜索工作区根 (\".\")",
                    "anyOf": [
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" } }
                    ]
                },
                "case":      { "type": "boolean", "description": "区分大小写搜索（默认 true）" },
                "gitignore": { "type": "boolean", "description": "尊重 .gitignore（默认 true）" },
                "skip":      { "type": ["number", "null"], "description": "收集结果前跳过的文件数——上次调用触达文件上限时用它翻页" }
            },
            "required": ["pattern"]
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
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `pattern`".into()))?
            .to_string();
        if pattern.trim().is_empty() {
            return Err(ToolError::InvalidArgs("pattern 不能为空".into()));
        }
        let paths = parse_path_list(input.get("path"));
        let case_sensitive = input
            .get("case")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let gitignore = input
            .get("gitignore")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let skip = match input.get("skip") {
            None | Some(serde_json::Value::Null) => 0usize,
            Some(v) => {
                let n = v.as_f64().unwrap_or(f64::NAN);
                if !n.is_finite() || n < 0.0 {
                    return Err(ToolError::InvalidArgs("skip 必须为非负数".into()));
                }
                n as usize
            }
        };
        let snapshots = ctx.snapshots.map(Arc::clone);
        let root = ctx.workspace.root().to_path_buf();
        // 阻塞式遍历走 spawn_blocking，避免占用异步运行时
        tokio::task::spawn_blocking(move || {
            run_grep(
                &root,
                &pattern,
                &paths,
                case_sensitive,
                gitignore,
                skip,
                snapshots.as_ref(),
            )
        })
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?
    }
}

/// grep 主体（同步；在 spawn_blocking 内执行）：分类目标 → 逐目标搜索归并 →
/// 分组/翻页窗口 → 锚点段头 → 渲染。
#[allow(clippy::too_many_arguments)]
fn run_grep(
    ws_root: &Path,
    pattern: &str,
    paths: &[String],
    case_sensitive: bool,
    gitignore: bool,
    skip: usize,
    snapshots: Option<&Arc<std::sync::RwLock<InMemorySnapshotStore>>>,
) -> Result<ToolResult, ToolError> {
    use std::time::Instant;

    // 1. 目标分类：存在文件 / 目录；缺失进警告。含通配符的条目先经 glob 展开。
    let mut file_targets: Vec<PathBuf> = Vec::new();
    let mut dir_targets: Vec<PathBuf> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for entry in paths {
        let abs = ws_root.join(entry.trim_start_matches("./"));
        if abs.is_file() {
            file_targets.push(abs);
        } else if abs.is_dir() {
            dir_targets.push(abs);
        } else if entry.contains('*') || entry.contains('?') || entry.contains('[') {
            // glob 条目：相对工作区根展开（尊重 .gitignore；上限 200 兜底）。
            match agent_search::glob_match(ws_root, entry, 200) {
                Ok(rels) if !rels.is_empty() => {
                    for rel in rels {
                        let p = ws_root.join(rel);
                        if p.is_file() {
                            file_targets.push(p);
                        }
                    }
                }
                _ => missing.push(entry.clone()),
            }
        } else {
            missing.push(entry.clone());
        }
    }
    if file_targets.is_empty() && dir_targets.is_empty() {
        return Err(ToolError::InvalidArgs(format!(
            "路径不存在: {}；请在分号分隔的 `path` 中逐个列出",
            missing.join(", ")
        )));
    }
    let mut seen_targets: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    file_targets.retain(|p| seen_targets.insert(p.clone()));
    dir_targets.retain(|p| seen_targets.insert(p.clone()));

    // 2. 单文件作用域（恰好一个目标且为文件）无多样性问题 → 200 帽（omp 语义）。
    let single_file_scope = file_targets.len() == 1 && dir_targets.is_empty();
    let per_file_cap = if single_file_scope {
        SINGLE_FILE_MATCHES
    } else {
        MULTI_FILE_PER_FILE
    };
    let opts = agent_search::GrepOptions {
        ignore_case: !case_sensitive,
        git_ignore: gitignore,
        hidden: true,
        max_total: INTERNAL_TOTAL_CAP,
        max_per_file: per_file_cap + 1, // 多取一条供溢出判定
        deadline: Some(Instant::now() + SEARCH_TIMEOUT),
        ..Default::default()
    };

    // 3. 逐目标搜索并归并（重叠目标以 (路径, 行) 去重）。
    let mut merged = MergedRuns {
        hits: Vec::new(),
        limit_reached: false,
        timed_out: false,
        oversized_windowed: 0,
    };
    for target in file_targets.iter().chain(dir_targets.iter()) {
        let outcome =
            agent_search::grep_opts(target, pattern, &opts).map_err(ToolError::Execution)?;
        merged.limit_reached |= outcome.limit_reached;
        merged.timed_out |= outcome.timed_out;
        merged.oversized_windowed += outcome.oversized_windowed;
        merged.hits.extend(outcome.hits);
    }
    if merged.timed_out {
        return Err(ToolError::Execution(format!(
            "搜索超过 {}s 未完成；请收窄 path 或 pattern，或先用 glob 收窄范围",
            SEARCH_TIMEOUT.as_secs()
        )));
    }
    merged
        .hits
        .sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    merged
        .hits
        .dedup_by(|a, b| a.path == b.path && a.line == b.line);

    // 4. 按文件分组（排序后即稳定文件序），单文件帽裁剪。
    let mut file_order: Vec<PathBuf> = Vec::new();
    let mut by_file: std::collections::HashMap<PathBuf, Vec<agent_search::GrepHit>> =
        std::collections::HashMap::new();
    for h in merged.hits {
        let file = h.path.clone();
        let bucket = by_file.entry(file.clone()).or_insert_with(|| {
            file_order.push(file);
            Vec::new()
        });
        bucket.push(h);
    }
    let mut per_file_limit_reached = false;
    for list in by_file.values_mut() {
        if list.len() > per_file_cap {
            per_file_limit_reached = true;
            list.truncate(per_file_cap);
        }
    }
    let total_files = file_order.len();
    let total_label = if merged.limit_reached {
        format!("{total_files}+")
    } else {
        total_files.to_string()
    };

    // 5. 翻页窗口（单文件作用域不可翻页——只有一个文件）。
    let can_paginate = !single_file_scope;
    let skip_files = if can_paginate {
        skip.min(total_files)
    } else {
        0
    };
    let window: Vec<PathBuf> = file_order
        .iter()
        .skip(skip_files)
        .take(FILE_LIMIT)
        .cloned()
        .collect();
    let file_limit_reached = can_paginate && total_files > skip_files + window.len();
    let next_skip = skip_files + window.len();

    // 6. 渲染：每文件 `[路径#指纹]` 段头（全文快照可锚定 apply_hashline）+
    //    `{行号:>5}\t` 正文（与 read 一致），不连续行间空行。
    let mut out = String::new();
    for file in &window {
        let hits = &by_file[file];
        if !out.is_empty() {
            out.push('\n');
        }
        let display = display_path_in(file, ws_root);
        let within_cap = file
            .metadata()
            .map(|m| m.len() <= opts.max_file_bytes)
            .unwrap_or(false);
        let full_text = if within_cap {
            file_head_text(file)
        } else {
            None
        };
        match full_text {
            Some(text) => {
                let tag = match snapshots {
                    Some(store) => store
                        .write()
                        .expect("snapshot 锁中毒")
                        .record(&display, &text),
                    None => compute_file_hash(&text),
                };
                out.push_str(&format!("[{display}#{tag}]\n"));
            }
            None => out.push_str(&format!("# {display}（未生成锚点）\n")),
        }
        let mut last: Option<usize> = None;
        for h in hits {
            if let Some(prev) = last {
                if h.line > prev + 1 {
                    out.push('\n');
                }
            }
            out.push_str(&format!("{:>5}\t{}\n", h.line, h.text));
            last = Some(h.line);
        }
    }
    if per_file_limit_reached {
        out.push_str(&format!(
            "\n个别文件命中超过 {per_file_cap} 条，仅显示前 {per_file_cap} 条；收窄 pattern 或用 path 定点。\n"
        ));
    }
    if file_limit_reached {
        out.push_str(&format!(
            "\n已显示文件 {}-{} / 共 {total_label} 个。用 skip={next_skip} 看下一页，或收窄 path/pattern。\n",
            skip_files + 1,
            next_skip
        ));
    }
    // 警示注记：缺失路径 / 显式超大文件 / 窗口化扫描。
    if !missing.is_empty() {
        out.push_str(&format!("\n已跳过不存在的路径: {}\n", missing.join(", ")));
    }
    let oversized_explicit: Vec<String> = file_targets
        .iter()
        .filter(|f| {
            f.metadata()
                .map(|m| m.len() > agent_search::GrepOptions::default().max_file_bytes)
                .unwrap_or(false)
        })
        .map(|f| display_path_in(f, ws_root))
        .collect();
    if !oversized_explicit.is_empty() {
        out.push_str(&format!(
            "\n仅搜索了超大文件的前 4MB（其余部分未显示；用 read 读取）: {}\n",
            oversized_explicit.join(", ")
        ));
    } else if merged.oversized_windowed > 0 {
        out.push_str(&format!(
            "\n{} 个超大文件仅搜索了前 4MB；用 read 定点读取\n",
            merged.oversized_windowed
        ));
    }
    if out.is_empty() {
        let text = if can_paginate && skip > 0 && total_files > 0 && skip_files >= total_files {
            format!("无更多结果（共 {total_label} 个文件；skip={skip} 已超出末尾）")
        } else {
            "无匹配".to_string()
        };
        return Ok(ToolResult::text(text));
    }
    Ok(ToolResult::text(out))
}

/// 读文件全文（渲染锚点用）；失败返回 None（该文件退化为无锚点输出）。
fn file_head_text(file: &Path) -> Option<String> {
    std::fs::read_to_string(file).ok()
}

/// 按 glob 模式发现文件。
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }
    fn description(&self) -> &'static str {
        "按 glob 模式（如 **/*.rs）发现工作区内文件路径。支持分号分隔多模式；         可按需包含隐藏文件或忽略 .gitignore（H33 对齐 omp `find`）。"
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "glob 模式（可用 `;` 分隔多个，如 `src/**/*.ts; test/**/*.ts`）" },
                "path": { "type": "string", "description": "`pattern` 的别名（对齐 omp `find.path`）：可为 glob、文件或目录；缺省搜索工作区根" },
                "hidden": { "type": "boolean", "description": "是否包含隐藏文件（`.*`）；默认 true（对齐 omp）" },
                "gitignore": { "type": "boolean", "description": "是否尊重 .gitignore；默认 true" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "结果上限（默认 200，最大 200）" }
            }
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
        // H33：`pattern` 与 omp 的 `path` 二者皆可（都缺 → 搜索工作区根全部文件）。
        let pattern = input
            .get("pattern")
            .or_else(|| input.get("path"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| "*".to_string(), str::to_string);
        let hidden = input
            .get("hidden")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let gitignore = input
            .get("gitignore")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let limit = input
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(200usize, |n| (n as usize).clamp(1, 200));
        let root = ctx.workspace.root();
        let files = tokio::task::spawn_blocking(move || {
            agent_search::glob_match_opts(&root, &pattern, limit, hidden, gitignore)
        })
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?
        .map_err(ToolError::Execution)?;
        if files.is_empty() {
            return Ok(ToolResult::text("无匹配文件".to_string()));
        }
        Ok(ToolResult::text(
            files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolContext;
    // 快照锁类型由 ToolContext 公共 API 钉死（std RwLock；fs.rs 同款），测试照用。
    use std::sync::RwLock;

    struct NoopApproval;
    #[async_trait::async_trait]
    impl agent_core::ApprovalPolicy for NoopApproval {
        fn decide(&self, _req: &agent_core::ApprovalRequest<'_>) -> agent_core::ApprovalDecision {
            agent_core::ApprovalDecision::Allow
        }
        async fn prompt(
            &self,
            _ask: &agent_core::AskMessage,
        ) -> Result<agent_core::AskResponse, agent_core::ToolError> {
            Err(agent_core::ToolError::Execution("测试桩不交互".into()))
        }
    }

    /// 最小 ToolContext（快照存储可选注入）。
    fn grep_ctx<'a>(
        ws: &'a agent_core::Workspace,
        cancel: &'a tokio_util::sync::CancellationToken,
        snapshots: &'a Arc<RwLock<InMemorySnapshotStore>>,
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
            snapshots: Some(snapshots),
            tool_call_id: None,
        }
    }

    /// 夹具：src/ 下 a.rs(2 命中) + b.rs(1 命中) + .gitignore 掉的 gen.rs + 隐藏 .env。
    fn small_fixture() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(
            src.join("a.rs"),
            "fn alpha() {}\n// TODO alpha\nfn beta() {}\n",
        )
        .unwrap();
        std::fs::write(src.join("b.rs"), "// TODO beta\n").unwrap();
        std::fs::write(src.join("gen.rs"), "// TODO generated\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "gen.rs\n").unwrap();
        std::fs::write(dir.path().join(".env"), "TODO=hidden\n").unwrap();
        dir
    }

    async fn run(input: serde_json::Value, ctx: &ToolContext<'_>) -> Result<ToolResult, ToolError> {
        GrepTool.execute(input, ctx).await
    }

    #[tokio::test]
    async fn groups_by_file_with_anchor_headers() {
        let fixture = small_fixture();
        let ws = agent_core::Workspace::new(fixture.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let store = Arc::new(RwLock::new(InMemorySnapshotStore::new()));
        let c = &grep_ctx(&ws, &cancel, &store);
        let out = run(json!({"pattern": "TODO"}), c)
            .await
            .unwrap()
            .to_llm_text();
        // 分组段头（带锚点指纹）+ 行号正文；默认根搜索：src/ 与隐藏 .env 都命中
        assert!(out.contains("[src/a.rs#"), "{out}");
        assert!(out.contains("[src/b.rs#"), "{out}");
        assert!(out.contains("    2\t// TODO alpha"), "{out}");
        // gen.rs 被 gitignore 排除；.env（隐藏文件）仍搜索
        assert!(!out.contains("gen.rs"), "{out}");
        assert!(out.contains("[.env#"), "{out}");
        // 快照已入册：可按段头指纹锚定
        assert!(store.read().unwrap().head("src/a.rs").is_some());
    }

    #[tokio::test]
    async fn case_param_toggles_sensitivity() {
        let fixture = small_fixture();
        let ws = agent_core::Workspace::new(fixture.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let store = Arc::new(RwLock::new(InMemorySnapshotStore::new()));
        let c = &grep_ctx(&ws, &cancel, &store);
        let q = |extra: serde_json::Value| {
            let mut v = json!({"pattern": "ALPHA", "path": "src"});
            if let (Some(obj), Some(e)) = (v.as_object_mut(), extra.as_object()) {
                for (k, val) in e {
                    obj.insert(k.clone(), val.clone());
                }
            }
            v
        };
        let cs = run(q(json!({})), c).await.unwrap().to_llm_text();
        assert_eq!(cs, "无匹配", "{cs}");
        let ci = run(q(json!({"case": false})), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(ci.contains("alpha"), "{ci}");
    }

    #[tokio::test]
    async fn gitignore_param_toggles_respect() {
        let fixture = small_fixture();
        let ws = agent_core::Workspace::new(fixture.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let store = Arc::new(RwLock::new(InMemorySnapshotStore::new()));
        let c = &grep_ctx(&ws, &cancel, &store);
        let out = run(
            json!({"pattern": "TODO", "path": "src", "gitignore": false}),
            c,
        )
        .await
        .unwrap()
        .to_llm_text();
        assert!(out.contains("gen.rs"), "{out}");
    }

    #[tokio::test]
    async fn skip_paginates_file_window() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        for i in 0..25 {
            std::fs::write(dir.path().join(format!("f{i:02}.txt")), "needle\n").unwrap();
        }
        let ws = agent_core::Workspace::new(dir.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let store = Arc::new(RwLock::new(InMemorySnapshotStore::new()));
        let c = &grep_ctx(&ws, &cancel, &store);
        let p1 = run(json!({"pattern": "needle"}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(p1.contains("已显示文件 1-20 / 共 25 个"), "{p1}");
        assert!(p1.contains("skip=20"), "{p1}");
        assert!(p1.contains("f00.txt") && p1.contains("f19.txt"));
        assert!(!p1.contains("f24.txt"), "{p1}");
        let p2 = run(json!({"pattern": "needle", "skip": 20}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(p2.contains("f20.txt") && p2.contains("f24.txt"), "{p2}");
        assert!(!p2.contains("f00.txt"), "{p2}");
        assert!(!p2.contains("已显示文件"), "{p2}");
        let p3 = run(json!({"pattern": "needle", "skip": 30}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(p3.contains("无更多结果"), "{p3}");
        assert!(p3.contains("skip=30"), "{p3}");
    }

    #[tokio::test]
    async fn per_file_cap_trims_hot_file() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let hot: String = (0..25).map(|i| format!("hit {i}\n")).collect();
        std::fs::write(dir.path().join("hot.txt"), hot).unwrap();
        std::fs::write(dir.path().join("cold.txt"), "hit cold\n").unwrap();
        let ws = agent_core::Workspace::new(dir.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let store = Arc::new(RwLock::new(InMemorySnapshotStore::new()));
        let c = &grep_ctx(&ws, &cancel, &store);
        let out = run(json!({"pattern": "hit"}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(out.contains("hot.txt") && out.contains("cold.txt"), "{out}");
        assert!(out.contains("仅显示前 20 条"), "{out}");
        assert!(!out.contains("hit 24"), "{out}");
    }

    #[tokio::test]
    async fn single_file_scope_allows_200_and_no_pagination() {
        let dir = tempfile::TempDir::new().unwrap();
        let big: String = (0..250).map(|i| format!("needle {i}\n")).collect();
        let f = dir.path().join("big.txt");
        std::fs::write(&f, big).unwrap();
        let ws = agent_core::Workspace::new(dir.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let store = Arc::new(RwLock::new(InMemorySnapshotStore::new()));
        let c = &grep_ctx(&ws, &cancel, &store);
        let out = run(json!({"pattern": "needle", "path": "big.txt"}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(
            out.contains("needle 199"),
            "单文件帽 200 应放行第 200 条: {out}"
        );
        assert!(!out.contains("needle 205"), "{out}");
        assert!(!out.contains("已显示文件"), "单文件作用域不可翻页: {out}");
    }

    #[tokio::test]
    async fn overlapping_targets_dedup() {
        let fixture = small_fixture();
        let ws = agent_core::Workspace::new(fixture.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let store = Arc::new(RwLock::new(InMemorySnapshotStore::new()));
        let c = &grep_ctx(&ws, &cancel, &store);
        let out = run(json!({"pattern": "TODO", "path": ["src", "src/a.rs"]}), c)
            .await
            .unwrap()
            .to_llm_text();
        // a.rs 的 TODO alpha 行只出现一次（重叠目标按 (路径,行) 去重）
        assert_eq!(out.matches("// TODO alpha").count(), 1, "{out}");
    }

    #[tokio::test]
    async fn glob_entry_expands() {
        let fixture = small_fixture();
        let ws = agent_core::Workspace::new(fixture.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let store = Arc::new(RwLock::new(InMemorySnapshotStore::new()));
        let c = &grep_ctx(&ws, &cancel, &store);
        let out = run(json!({"pattern": "TODO", "path": "src/**/*.rs"}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(out.contains("a.rs") && out.contains("b.rs"), "{out}");
    }

    /// H33：`glob` 四参数（`path` 别名 / `hidden` / `gitignore` / `limit`）。
    #[tokio::test]
    async fn glob_accepts_omp_four_params() {
        let fixture = small_fixture();
        let ws = agent_core::Workspace::new(fixture.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let store = Arc::new(RwLock::new(InMemorySnapshotStore::new()));
        let c = &grep_ctx(&ws, &cancel, &store);

        // `path` 作为 `pattern` 的 omp 别名。
        let out = GlobTool
            .execute(json!({"path": "**/*.rs"}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(out.contains("a.rs"), "{out}");

        // `limit` 截断。
        let out = GlobTool
            .execute(json!({"path": "**/*.rs", "limit": 1}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert_eq!(out.lines().count(), 1, "limit=1 应只回一行: {out}");

        // 分号分隔多模式。
        let out = GlobTool
            .execute(json!({"pattern": "src/a.rs; src/b.rs"}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(out.contains("a.rs") && out.contains("b.rs"), "{out}");

        // `gitignore: false` 命中被 .gitignore 排除的 gen.rs；默认 true 不命中。
        let ignored = GlobTool
            .execute(json!({"pattern": "**/gen.rs"}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(
            !ignored.contains("gen.rs"),
            "默认应尊重 .gitignore: {ignored}"
        );
        let included = GlobTool
            .execute(json!({"pattern": "**/gen.rs", "gitignore": false}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(
            included.contains("gen.rs"),
            "gitignore=false 应命中: {included}"
        );

        // `hidden` 默认 true（.env 命中）；显式 false 不命中。
        let hidden_default = GlobTool
            .execute(json!({"pattern": ".env"}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(
            hidden_default.contains(".env"),
            "默认含隐藏文件: {hidden_default}"
        );
        let hidden_off = GlobTool
            .execute(json!({"pattern": ".env", "hidden": false}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(
            !hidden_off.contains(".env"),
            "hidden=false 应排除: {hidden_off}"
        );

        // 两者都给时 `pattern` 优先。
        let out = GlobTool
            .execute(json!({"pattern": "src/a.rs", "path": "src/b.rs"}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(out.contains("a.rs") && !out.contains("b.rs"), "{out}");

        // 都缺省 → 搜索工作区根（不报错）。
        assert!(
            GlobTool.execute(json!({}), c).await.is_ok(),
            "缺省应回退到工作区根扫描"
        );
    }
    #[tokio::test]
    async fn missing_paths_note_and_hard_error() {
        let fixture = small_fixture();
        let ws = agent_core::Workspace::new(fixture.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let store = Arc::new(RwLock::new(InMemorySnapshotStore::new()));
        let c = &grep_ctx(&ws, &cancel, &store);
        // 全部缺失 → 硬错误
        let err = run(json!({"pattern": "x", "path": "nope"}), c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("路径不存在"), "{err}");
        // 部分缺失 → 警示注记
        let out = run(json!({"pattern": "TODO", "path": ["src", "nope"]}), c)
            .await
            .unwrap()
            .to_llm_text();
        assert!(out.contains("已跳过不存在的路径: nope"), "{out}");
    }

    #[tokio::test]
    async fn empty_pattern_and_negative_skip_rejected() {
        let fixture = small_fixture();
        let ws = agent_core::Workspace::new(fixture.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let store = Arc::new(RwLock::new(InMemorySnapshotStore::new()));
        let c = &grep_ctx(&ws, &cancel, &store);
        let err = run(json!({"pattern": "   "}), c).await.unwrap_err();
        assert!(err.to_string().contains("pattern 不能为空"), "{err}");
        let err = run(json!({"pattern": "x", "skip": -1}), c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("skip 必须为非负数"), "{err}");
    }

    #[tokio::test]
    async fn invalid_regex_surfaces_error() {
        let fixture = small_fixture();
        let ws = agent_core::Workspace::new(fixture.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let store = Arc::new(RwLock::new(InMemorySnapshotStore::new()));
        let c = &grep_ctx(&ws, &cancel, &store);
        let err = run(json!({"pattern": "(unclosed"}), c).await.unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
