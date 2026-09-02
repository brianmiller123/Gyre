//! 基准门：TTSR 匹配延迟（对标报告 §六.2 热路径基准）。
//!
//! 真实热路径函数（`agent_ttsr::TtsrManager`，crates/ttsr/src/matcher.rs，agent run-loop
//! 经 `TtsrCoordinator` 调用）：
//! - `TtsrManager::check_delta(stream, delta)`：matcher.rs:83
//!   （流式增量正则匹配——每收到一个文本/思考增量块调用一次，缓冲累积后逐规则门控匹配）
//! - `TtsrManager::check_tool_call(tool, path, digest)`：matcher.rs:99
//!   （工具载荷 digest 线性匹配——工具执行后的 `MessageEnd` 快照匹配）
//!
//! 规则集经 `parse_rule`（crates/ttsr/src/rule.rs:206）确定性构造，仿 `.gyre/rules/*.md`
//! 典型形态（text / tool:NAME 作用域 + 正则条件，Rust regex 线性时间匹配）。
//! 输入确定性：全部在 bench 内构造，不依赖外部文件或网络。

use std::hint::black_box;

use agent_ttsr::{Rule, TtsrManager, parse_rule};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

/// 典型 TTSR 规则集：8 条（text / tool 作用域混布，模拟真实装配）。
fn typical_rules() -> Vec<Rule> {
    let specs: &[(&str, &str)] = &[
        // (规则名, frontmatter + 正文)
        (
            "no-box-leak",
            "---\ncondition: [\"(?i)Box::leak\"]\nscope: [text, tool:write_file]\n---\n\
             禁止在生产代码路径使用 Box::leak；请改用 Arc<str>。",
        ),
        (
            "no-unwrap",
            "---\ncondition: [\"(?i)\\bunwrap\\(\\)\"]\nscope: [text, tool:write_file]\n---\n\
             禁止直接 unwrap；请用 ? 或显式错误处理。",
        ),
        (
            "no-todo",
            "---\ncondition: [\"(?i)\\btodo!?\\b\"]\nscope: [text, tool:write_file]\n---\n\
             不要在提交前留下 TODO 占位；请完成实现。",
        ),
        (
            "no-panic",
            "---\ncondition: [\"(?i)\\bpanic!?\\([^)]*\\)\"]\nscope: [text, tool:write_file]\n---\n\
             禁止 panic 路径；请用 Result 传播错误。",
        ),
        (
            "no-debugger",
            "---\ncondition: [\"(?i)\\bdebugger\\b\"]\nscope: [text, tool:write_file]\n---\n\
             不要遗留 debugger 语句。",
        ),
        (
            "no-println",
            "---\ncondition: [\"(?i)\\bprintln!?\\([^)]*\\)\"]\nscope: [text]\n---\n\
             不要在库代码里 println 调试；请用 tracing。",
        ),
        (
            "no-ffi-unsafe",
            "---\ncondition: [\"(?i)\\bunsafe\\s*(\\{|fn|impl)\"]\nscope: [text]\n---\n\
             尽量避免 unsafe；如必须请加安全说明注释。",
        ),
        (
            "no-raw-secret",
            "---\ncondition: [\"(?i)(api[_-]?key|password|token)\\s*[:=]\\s*[\\\"']\"]\n\
             scope: [tool:write_file, tool:run_command]\n---\n\
             不要硬编码密钥；请用环境变量注入。",
        ),
    ];
    specs
        .iter()
        .map(|(name, md)| parse_rule(name, md).expect("TTSR 基准规则构造失败"))
        .collect()
}

/// `check_delta：模拟流式输出` 80 个增量块（每块 ~120 字符，含 1 次命中）。
fn bench_check_delta(c: &mut Criterion) {
    let rules = typical_rules();
    let chunks: Vec<String> = (0..80)
        .map(|i| {
            if i % 10 == 4 {
                format!("assistant 正在生成代码……注意：不得使用 Box::leak 或 unwrap。第 {i} 块。")
            } else {
                format!("assistant 流式输出第 {i} 块：实现 `handle_{i}`，处理边界条件。")
            }
        })
        .collect();
    c.bench_function("ttsr/check_delta_80_chunks", |b| {
        b.iter_batched(
            // 每轮新匹配器：缓冲/注入状态不跨迭代泄漏（准备不计时）。
            || TtsrManager::new(rules.clone(), &[]),
            |mut m| {
                let mut hits = Vec::new();
                for chunk in &chunks {
                    hits.extend(m.check_delta("text", chunk));
                }
                black_box(hits);
            },
            BatchSize::SmallInput,
        );
    });
}

/// `check_tool_call：模拟典型工具调用快照匹配（写` / 读 / 命令三类载荷）。
fn bench_check_tool_call(c: &mut Criterion) {
    let rules = typical_rules();
    let calls: Vec<(String, Option<String>, String)> = vec![
        (
            "write_file".into(),
            Some("src/main.rs".into()),
            "pub fn run() { Box::leak(vec![1, 2, 3]); }".into(),
        ),
        (
            "write_file".into(),
            Some("src/lib.rs".into()),
            "let x = parse().unwrap();".into(),
        ),
        ("grep".into(), Some("src".into()), "TODO".into()),
        (
            "run_command".into(),
            Some(".".into()),
            "cargo test --all".into(),
        ),
        (
            "read_file".into(),
            Some("Cargo.toml".into()),
            "cargo build".into(),
        ),
        (
            "write_file".into(),
            Some("src/api.rs".into()),
            "let key = \"sk-abc123\";".into(),
        ),
    ];
    c.bench_function("ttsr/check_tool_call_6_calls", |b| {
        b.iter_batched(
            || TtsrManager::new(rules.clone(), &[]),
            |mut m| {
                let mut hits = Vec::new();
                for (tool, path, digest) in &calls {
                    hits.extend(m.check_tool_call(tool, path.as_deref(), digest));
                }
                black_box(hits);
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_check_delta, bench_check_tool_call);
criterion_main!(benches);
