//! 基准门：grep / glob 吞吐（对标报告 §六.2 热路径基准）。
//!
//! 真实热路径函数（crates/tools/src/search.rs 的 `GrepTool` / `GlobTool` 经
//! `spawn_blocking` 委托到 agent-search）：
//! - `agent_search::grep_opts(root, pattern, opts)`：crates/search/src/lib.rs
//!   （ignore 并行遍历 + 逐行正则匹配；工具调用参数 `GrepOptions::default()`）
//! - `agent_search::glob_match(root, pattern, max)`：crates/search/src/lib.rs:105
//!   （globset 匹配 + `fs_cache` 扫描缓存；工具调用参数 `(root, pattern, 100)`）
//! - 内存行匹配内核 `agent_search::highlight_match(line, pattern)`：
//!   crates/search/src/lib.rs:137（grep 结果渲染复用的同一正则匹配内核）
//!
//! 数据确定性：夹具全部在 bench 内构造——`tempfile` 临时目录小仓库与内存行列表，
//! 不依赖外部文件或网络。

use std::hint::black_box;
use std::path::Path;

use agent_search::{glob_match, grep_opts, highlight_match};
use criterion::{Criterion, criterion_group, criterion_main};

/// 小仓库规模：200 个文件（约 8k 行），模拟一次轻量工作区遍历。
const FILE_COUNT: usize = 200;
/// 每文件行数。
const LINES_PER_FILE: usize = 40;
/// grep 基准模式：约 10% 行命中。
const GREP_PATTERN: &str = "TODO";
/// glob 基准模式：发现全部 .rs 文件。
const GLOB_PATTERN: &str = "**/*.rs";

/// xorshift64*：确定性伪随机（夹具内容可复现，无外部依赖）。
const fn rng(seed: &mut u64) -> u64 {
    *seed ^= *seed >> 12;
    *seed ^= *seed << 25;
    *seed ^= *seed >> 27;
    (*seed).wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// 构造确定性小仓库：`src/`、`src/util/`、`tests/`、`docs/` 四层目录，200 个 .rs 文件。
fn build_fixture(root: &Path) {
    let mut seed = 0x5EED_1234;
    for i in 0..FILE_COUNT {
        let dir = match i % 4 {
            0 => "src",
            1 => "src/util",
            2 => "tests",
            _ => "docs",
        };
        let dir_path = root.join(dir);
        std::fs::create_dir_all(&dir_path).expect("基准夹具目录创建失败");
        let mut content = String::with_capacity(LINES_PER_FILE * 64);
        for line in 0..LINES_PER_FILE {
            // 每 ~10 行插入一个 TODO 命中（grep 命中率约 10%）。
            if line % 10 == 3 {
                content.push_str("// TODO(基准): 待办事项 checkpoint\n");
            } else {
                let n = rng(&mut seed) % 1_000;
                content.push_str(&format!(
                    "pub fn handler_{n}(x: usize) -> usize {{ x + {line} }}\n"
                ));
            }
        }
        std::fs::write(dir_path.join(format!("file_{i:03}.rs")), content)
            .expect("基准夹具写入失败");
    }
}

/// grep：临时小仓库全量遍历 + 逐行正则匹配吞吐。
fn bench_grep(c: &mut Criterion) {
    let dir = tempfile::TempDir::new().expect("基准临时目录创建失败");
    build_fixture(dir.path());
    c.bench_function("grep/todo_200_files", |b| {
        b.iter(|| {
            let outcome = grep_opts(
                dir.path(),
                GREP_PATTERN,
                &agent_search::GrepOptions::default(),
            )
            .expect("grep 基准正则合法");
            black_box(outcome.hits);
        });
    });
}

/// glob：临时小仓库 glob 匹配吞吐。
///
/// 注意：`glob_match` 经 `fs_cache`（TTL 1s）复用扫描结果，首轮之后各迭代测量的是
/// 缓存命中下的纯 globset 匹配——正是 run-loop 稳态的热路径形态（目录遍历成本被缓存）。
fn bench_glob(c: &mut Criterion) {
    let dir = tempfile::TempDir::new().expect("基准临时目录创建失败");
    build_fixture(dir.path());
    c.bench_function("glob_match/rs_200_files", |b| {
        b.iter(|| {
            let files =
                glob_match(dir.path(), GLOB_PATTERN, FILE_COUNT).expect("glob 基准模式合法");
            black_box(files);
        });
    });
}

/// 内存行匹配内核：预构造 10k 行字符串（模拟小仓库行集），逐行调用公开匹配函数。
///
/// 说明：`highlight_match` 每次调用重新编译正则（grep 内循环复用同一 Regex），
/// 此处衡量「按行调用公开匹配函数」的吞吐上限，命中行附加 ANSI 高亮成本。
fn bench_in_memory_lines(c: &mut Criterion) {
    let lines: Vec<String> = (0..10_000)
        .map(|i| {
            if i % 10 == 3 {
                format!("// TODO(基准): 第 {i} 行待办")
            } else {
                format!("pub fn handler_{i}(x: usize) -> usize {{ x + {i} }}")
            }
        })
        .collect();
    c.bench_function("line_match/todo_10k_lines", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for line in &lines {
                let out = highlight_match(line, GREP_PATTERN).expect("grep 基准正则合法");
                total += out.len();
            }
            black_box(total);
        });
    });
}

criterion_group!(benches, bench_grep, bench_glob, bench_in_memory_lines);
criterion_main!(benches);
