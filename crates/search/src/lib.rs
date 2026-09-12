//! # agent-search
//!
//! 代码搜索（移植 oh-my-pi `pi-natives` 的 grep/glob 能力为纯 Rust）：
//! - [`grep`] 基于 `ignore`（ripgrep 核心），并行遍历并尊重 `.gitignore`。
//! - [`glob_match`] 基于 `globset`，按 glob 模式发现文件。

#![deny(unsafe_code)]

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub mod extras;
pub mod fs_cache;
pub mod tokens;

pub use extras::{find_files, highlight_to_ansi};
pub use tokens::count_tokens;

use std::time::Instant;

/// 一次 grep 命中。
#[derive(Debug, Clone)]
pub struct GrepHit {
    /// 命中文件路径（绝对路径）。
    pub path: PathBuf,
    /// 1-indexed 行号。
    pub line: usize,
    /// 该行文本。
    pub text: String,
}

/// grep 选项（对齐 oh-my-pi `pi-natives` grep 面 / `grep.ts` 常量）。
#[derive(Debug, Clone)]
pub struct GrepOptions {
    /// 大小写不敏感（omp `case:false` 语义；默认区分大小写）。
    pub ignore_case: bool,
    /// 尊重 `.gitignore`（omp `gitignore`，默认 true）。
    pub git_ignore: bool,
    /// 遍历隐藏文件（omp 恒为 true）。
    pub hidden: bool,
    /// 命中总数软上限（omp `INTERNAL_TOTAL_CAP` = 2000；触顶即停并置
    /// [`GrepOutcome::limit_reached`]，此时文件总数只是下界）。
    pub max_total: usize,
    /// 单文件命中上限（omp native `maxCountPerFile` = 展示帽 + 1：多取一条
    /// 供调用方判定「该文件还有更多」）。
    pub max_per_file: usize,
    /// 单文件搜索字节窗口（pi-natives `MAX_FILE_BYTES` = 4MB）：更大文件只搜
    /// 前 N 字节（部分覆盖），计入 [`GrepOutcome::oversized_windowed`]。
    pub max_file_bytes: u64,
    /// 遍历截止时刻（omp 30s 超时；触顶即停并置 [`GrepOutcome::timed_out`]）。
    pub deadline: Option<Instant>,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            ignore_case: false,
            git_ignore: true,
            hidden: true,
            max_total: 2000,
            max_per_file: 21,
            max_file_bytes: 4 * 1024 * 1024,
            deadline: None,
        }
    }
}

/// 一次 grep 的结果与截断元数据。
#[derive(Debug, Clone, Default)]
pub struct GrepOutcome {
    /// 命中（`path` 为**绝对路径**，展示层自行相对化）。
    pub hits: Vec<GrepHit>,
    /// 触达 `max_total`（文件总数因此只是下界）。
    pub limit_reached: bool,
    /// 触达 `deadline`（结果不完整）。
    pub timed_out: bool,
    /// 仅搜索了前 `max_file_bytes` 字节的超大文件数。
    pub oversized_windowed: usize,
}

/// 在 `root`（文件或目录）下正则搜索（**并行遍历**；行为对齐 pi-natives）。
///
/// - 尊重 `.gitignore`/隐藏文件开关由 [`GrepOptions`] 控制；
/// - 二进制（非 UTF-8）跳过；超过 `max_file_bytes` 的文件只搜前缀窗口；
/// - 逐文件帽 `max_per_file`、总量软帽 `max_total`、`deadline` 触顶即停；
/// - 支持跨行模式：模式含换行时对全文 `find_iter`（命中记起始行）；
/// - 收集后按 (path, line) 排序，保证并行下输出仍稳定。
///
/// # Errors
/// 正则非法时返回错误字符串。
pub fn grep_opts(root: &Path, pattern: &str, opts: &GrepOptions) -> Result<GrepOutcome, String> {
    let mut builder = regex::RegexBuilder::new(pattern);
    builder.case_insensitive(opts.ignore_case);
    let re = Arc::new(builder.build().map_err(|e| e.to_string())?);
    // 跨行模式：模式本身含换行（真换行或 `\n` 转义）时逐行扫描永远失配，
    // 改为全文匹配后按字节偏移回算行号。
    let multiline = pattern.contains('\n') || pattern.contains("\\n");
    let hits: Arc<Mutex<Vec<GrepHit>>> = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let oversized = Arc::new(AtomicUsize::new(0));
    let limit = Arc::new(AtomicUsize::new(0));
    let timed_out = Arc::new(AtomicUsize::new(0));
    let root_owned = root.to_path_buf();
    // ignore crate 的 `hidden(true)` 语义是「过滤掉隐藏文件」，与 omp `hidden:
    // true`（搜索隐藏文件）相反，故取反；`.git` 子树对齐 pi-natives
    // `skip_git(true)` 恒跳过（内部对象库不是工作区内容）。
    let walker = ignore::WalkBuilder::new(&root_owned)
        .hidden(!opts.hidden)
        .git_ignore(opts.git_ignore)
        .filter_entry(|e| e.file_name() != ".git")
        .build_parallel();

    walker.run(|| {
        // 每个工作线程克隆一份共享句柄（Arc clone 廉价；满足 visitor 的 'static 要求）。
        let re = Arc::clone(&re);
        let hits = Arc::clone(&hits);
        let count = Arc::clone(&count);
        let oversized = Arc::clone(&oversized);
        let limit = Arc::clone(&limit);
        let timed_out = Arc::clone(&timed_out);
        let opts = opts.clone();
        Box::new(move |entry| {
            if let Some(d) = opts.deadline {
                if Instant::now() > d {
                    timed_out.store(1, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }
            }
            if count.load(Ordering::Relaxed) >= opts.max_total {
                limit.store(1, Ordering::Relaxed);
                return ignore::WalkState::Quit;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return ignore::WalkState::Continue,
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return ignore::WalkState::Continue;
            }
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                return ignore::WalkState::Continue;
            };
            // 超大文件：只读前 max_file_bytes 字节窗口（部分覆盖；对齐 pi-natives）。
            let windowed = meta.len() > opts.max_file_bytes;
            let text = match read_text_window(path, windowed.then_some(opts.max_file_bytes)) {
                Some(t) => t,
                None => return ignore::WalkState::Continue,
            };
            if windowed {
                oversized.fetch_add(1, Ordering::Relaxed);
            }
            let matched: Vec<(usize, String)> = if multiline {
                multiline_matches(&re, &text, opts.max_per_file)
            } else {
                line_matches(&re, &text, opts.max_per_file)
            };
            if matched.is_empty() {
                return ignore::WalkState::Continue;
            }
            let mut local = hits.lock();
            let matched_count = matched.len();
            local.extend(matched.into_iter().map(|(line, text)| GrepHit {
                path: path.to_path_buf(),
                line,
                text,
            }));
            count.fetch_add(matched_count, Ordering::Relaxed);
            if count.load(Ordering::Relaxed) >= opts.max_total {
                limit.store(1, Ordering::Relaxed);
                return ignore::WalkState::Quit;
            }
            ignore::WalkState::Continue
        })
    });

    let mut hits = hits.lock().clone();
    // 并行收集顺序不确定，按 (path, line) 排序保证输出稳定。
    hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    // 总量帽精确化：并行遍历下 walker 的提前退出只能近似（在飞文件可越过帽），
    // 收集后统一截断到 max_total——输出永不超过帽，limit_reached 标记不完整。
    let mut limit_reached = limit.load(Ordering::Relaxed) == 1;
    if hits.len() > opts.max_total {
        hits.truncate(opts.max_total);
        limit_reached = true;
    }
    Ok(GrepOutcome {
        hits,
        limit_reached,
        timed_out: timed_out.load(Ordering::Relaxed) == 1,
        oversized_windowed: oversized.load(Ordering::Relaxed),
    })
}

/// 读取文本（`window_bytes = Some(n)` 时只读前 n 字节并修剪到字符边界）；
/// 非 UTF-8 / IO 失败返回 None（调用方跳过）。
fn read_text_window(path: &Path, window_bytes: Option<u64>) -> Option<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    let n = match window_bytes {
        Some(n) => {
            file.take(n).read_to_end(&mut buf).ok()?;
            n
        }
        None => {
            file.read_to_end(&mut buf).ok()?;
            u64::MAX
        }
    };
    if buf.len() as u64 > n {
        // 修剪到 UTF-8 字符边界，避免切断多字节序列。
        while !buf.is_empty() && (buf[buf.len() - 1] & 0xC0) == 0x80 {
            buf.pop();
        }
    }
    String::from_utf8(buf).ok()
}

/// 逐行扫描：每行至多一个命中（行模式语义），单文件超帽即停。
fn line_matches(re: &regex::Regex, text: &str, per_file_cap: usize) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if out.len() >= per_file_cap {
            break;
        }
        if re.is_match(line) {
            out.push((i + 1, line.to_string()));
        }
    }
    out
}

/// 全文扫描（跨行模式）：每个匹配记起始行；同行多命中只取首个。
fn multiline_matches(re: &regex::Regex, text: &str, per_file_cap: usize) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    for m in re.find_iter(text) {
        if out.len() >= per_file_cap {
            break;
        }
        let line = text[..m.start()].bytes().filter(|b| *b == b'\n').count() + 1;
        if out.last().is_some_and(|(l, _)| *l == line) {
            continue;
        }
        let line_text = text[m.start()..]
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        out.push((line, line_text));
    }
    out
}

/// 按 glob 模式在 `root` 下发现文件路径（尊重 `.gitignore`，经扫描缓存）。
pub fn glob_match(root: &Path, pattern: &str, max: usize) -> Result<Vec<PathBuf>, String> {
    // 保留旧默认（不含隐藏文件、尊重 .gitignore）——既有调用方行为不变。
    glob_match_opts(root, pattern, max, false, true)
}

/// 带扫描开关的 glob（H33）：`include_hidden` / `use_gitignore` 透传给文件扫描。
///
/// `pattern` 支持分号分隔的多模式（`src/**/*.ts; test/**/*.ts`，对齐 omp `find`
/// 的 `path` 语义）；任一模式命中即计入。结果按扫描顺序去重。
///
/// # Errors
/// 任一 glob 模式非法时返回错误文案。
pub fn glob_match_opts(
    root: &Path,
    pattern: &str,
    max: usize,
    include_hidden: bool,
    use_gitignore: bool,
) -> Result<Vec<PathBuf>, String> {
    let mut matchers = Vec::new();
    for raw in pattern.split(';') {
        let p = raw.trim();
        if p.is_empty() {
            continue;
        }
        // 目录/文件直接路径（无通配符）→ 精确匹配该相对路径。
        let glob = globset::GlobBuilder::new(p)
            .literal_separator(true)
            .build()
            .map_err(|e| e.to_string())?;
        matchers.push(glob.compile_matcher());
    }
    if matchers.is_empty() {
        return Ok(Vec::new());
    }
    let entries = crate::fs_cache::get_or_scan(root, include_hidden, use_gitignore);
    let mut out: Vec<PathBuf> = Vec::new();
    for e in entries.iter().filter(|e| e.is_file) {
        if out.len() >= max {
            break;
        }
        if matchers.iter().any(|m| {
            m.is_match(&e.rel_path) || e.rel_path.file_name().is_some_and(|n| m.is_match(n))
        }) {
            out.push(e.rel_path.clone());
        }
    }
    Ok(out)
}

/// 列出 `root` 下的条目（尊重 `.gitignore`）。
/// - `recursive = false`：仅直接子项（文件与目录，按名排序）。
/// - `recursive = true`：递归所有文件（跳过隐藏/gitignore）。
#[must_use]
pub fn list_files(root: &Path, recursive: bool, max: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !recursive {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                if out.len() >= max {
                    break;
                }
                let path = entry.path();
                let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                out.push(rel);
            }
            out.sort();
        }
        return out;
    }
    let entries = crate::fs_cache::get_or_scan(root, false, true);
    for e in entries.iter().filter(|e| e.is_file) {
        if out.len() >= max {
            break;
        }
        out.push(e.rel_path.clone());
    }
    out
}

/// 在行文本中把匹配正则的子串用 ANSI 红色加粗高亮（grep 结果渲染用）。
///
/// # Errors
/// 正则非法时返回错误。
pub fn highlight_match(line: &str, pattern: &str) -> Result<String, String> {
    let re = regex::Regex::new(pattern).map_err(|e| e.to_string())?;
    let out = re.replace_all(line, |c: &regex::Captures<'_>| {
        format!("\x1b[1;31m{}\x1b[0m", &c[0])
    });
    Ok(out.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-search-{name}-{}", nano()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn nano() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn grep_finds_pattern() {
        let dir = tmp("grep");
        fs::write(dir.join("a.rs"), "fn hello() {}\nfn world() {}\n").unwrap();
        let out = grep_opts(&dir, "hello", &GrepOptions::default()).unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].line, 1);
        assert!(!out.limit_reached && !out.timed_out);
    }

    #[test]
    fn glob_finds_by_extension() {
        let dir = tmp("glob");
        fs::write(dir.join("a.rs"), "x").unwrap();
        fs::write(dir.join("b.txt"), "y").unwrap();
        let files = glob_match(&dir, "**/*.rs", 10).unwrap();
        assert!(files.iter().any(|p| p.to_string_lossy() == "a.rs"));
        assert!(!files.iter().any(|p| p.to_string_lossy().ends_with("b.txt")));
    }

    #[test]
    fn grep_opts_case_insensitivity() {
        let dir = tmp("grep-case");
        fs::write(dir.join("a.txt"), "Hello\nworld\n").unwrap();
        let cs = grep_opts(&dir, "hello", &GrepOptions::default()).unwrap();
        assert_eq!(cs.hits.len(), 0, "默认区分大小写");
        let ci = grep_opts(
            &dir,
            "hello",
            &GrepOptions {
                ignore_case: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ci.hits.len(), 1);
        assert_eq!(ci.hits[0].line, 1);
        assert!(ci.hits[0].path.is_absolute(), "命中路径应为绝对路径");
    }

    #[test]
    fn grep_opts_gitignore_toggle() {
        let dir = tmp("grep-gi");
        fs::create_dir_all(dir.join(".git")).unwrap(); // ignore crate 仅在仓库内尊重 .gitignore
        fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.join("ignored.txt"), "needle\n").unwrap();
        fs::write(dir.join("kept.txt"), "needle\n").unwrap();
        let respect = grep_opts(&dir, "needle", &GrepOptions::default()).unwrap();
        assert_eq!(respect.hits.len(), 1, "默认尊重 gitignore");
        let no_gi = grep_opts(
            &dir,
            "needle",
            &GrepOptions {
                git_ignore: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(no_gi.hits.len(), 2);
    }

    #[test]
    fn grep_opts_per_file_and_total_caps() {
        let dir = tmp("grep-caps");
        fs::write(dir.join("hot.txt"), "x\nx\nx\nx\nx\n").unwrap();
        let capped = grep_opts(
            &dir,
            "x",
            &GrepOptions {
                max_per_file: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(capped.hits.len(), 2, "单文件帽生效");
        assert!(!capped.limit_reached, "单文件帽不等于总量帽");
        fs::write(dir.join("b.txt"), "x\n").unwrap();
        let total = grep_opts(
            &dir,
            "x",
            &GrepOptions {
                max_total: 3,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(total.hits.len(), 3);
        assert!(total.limit_reached, "触总量帽应置位");
    }

    #[test]
    fn grep_opts_deadline_times_out() {
        let dir = tmp("grep-deadline");
        fs::write(dir.join("a.txt"), "needle\n").unwrap();
        let out = grep_opts(
            &dir,
            "needle",
            &GrepOptions {
                deadline: Some(Instant::now()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(out.timed_out, "过期 deadline 应置 timed_out");
        assert!(out.hits.is_empty());
    }

    #[test]
    fn grep_opts_oversized_window_partial() {
        let dir = tmp("grep-window");
        let big = format!("needle\n{}\nneedle-tail\n", "p".repeat(2048));
        fs::write(dir.join("big.txt"), big).unwrap();
        let out = grep_opts(
            &dir,
            "needle",
            &GrepOptions {
                max_file_bytes: 1024,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(out.oversized_windowed, 1, "超大文件应计入窗口统计");
        assert_eq!(out.hits.len(), 1, "只搜到窗口内的首个命中");
        assert_eq!(out.hits[0].line, 1);
    }

    #[test]
    fn grep_opts_multiline_pattern() {
        let dir = tmp("grep-multi");
        fs::write(dir.join("a.txt"), "foo\nbar\nbaz\n").unwrap();
        let out = grep_opts(&dir, "foo\\nbar", &GrepOptions::default()).unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].line, 1, "跨行命中记起始行");
    }

    #[test]
    fn grep_opts_single_file_root() {
        let dir = tmp("grep-file-root");
        fs::write(dir.join("a.txt"), "needle\nother needle\n").unwrap();
        let out = grep_opts(&dir.join("a.txt"), "needle", &GrepOptions::default()).unwrap();
        assert_eq!(out.hits.len(), 2, "文件根直接可搜");
    }

    #[test]
    fn list_files_top_level_and_recursive() {
        let dir = tmp("list");
        fs::write(dir.join("a.rs"), "x").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/b.rs"), "y").unwrap();
        let top = list_files(&dir, false, 10);
        assert!(top.iter().any(|p| p.to_string_lossy() == "a.rs"));
        assert!(top.iter().any(|p| p.to_string_lossy() == "sub"));
        let rec = list_files(&dir, true, 10);
        assert!(rec.iter().any(|p| p.to_string_lossy() == "a.rs"));
        assert!(rec.iter().any(|p| p.to_string_lossy() == "sub/b.rs"));
    }
}
