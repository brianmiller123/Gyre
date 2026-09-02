//! 基准门：tiktoken 计数吞吐（对标报告 §六.2 热路径基准）。
//!
//! 真实热路径函数：`agent_context::token::TokenCounter`（crates/context/src/token.rs）：
//! - `TokenCounter::count_text`：token.rs:56（默认 cl100k 编码）
//! - `TokenCounter::count_text_for`：token.rs:51（按 model 族选 BPE，run-loop 用量统计
//!   与压缩估算走此路径：gpt-4o 系列 → `o200k_base`）
//! - `TokenCounter::count_context_for`：token.rs:68（完整上下文 system + messages 计数）
//!
//! 位置说明：grep 证实 crates/core 内无 tiktoken 封装（`Usage` 仅做加法记账），计数实现在
//! agent-context；基准文件按任务约定置于 crates/core/benches/，经 dev-dependency 引用
//! agent-context（core→context 的 dev 边成环为 Cargo 允许的 dev-dependency 环，不参与
//! 常规依赖图）。
//!
//! 输入确定性：8KB 代码文本在 bench 内构造（代码片段重复拼接），无外部依赖。

use std::hint::black_box;

use agent_context::token::TokenCounter;
use agent_core::{ProviderMessage, UserContent};
use criterion::{Criterion, criterion_group, criterion_main};

/// 基准文本：拼接至 ≥ 8 KiB 的 Rust 风格代码（确定性内容）。
fn build_8kb_code() -> String {
    const SNIPPET: &str = r#"/// 处理事件：解析参数并分派到对应 handler。
pub fn handle_event(event: &str, state: &mut State) -> Result<(), Error> {
    let parts: Vec<&str> = event.split_whitespace().collect();
    if parts.is_empty() {
        return Err(Error::Empty);
    }
    match parts[0] {
        "start" => start_session(state),
        "pause" => pause_session(state),
        "stop" => stop_session(state),
        _ => return Err(Error::Unknown(parts[0].to_string())),
    }
    Ok(())
}
"#;
    let mut code = String::with_capacity(8 * 1024 + SNIPPET.len());
    while code.len() < 8 * 1024 {
        code.push_str(SNIPPET);
    }
    code
}

fn bench_token_count(c: &mut Criterion) {
    // 词表加载只做一次（tiktoken BPE 词表构建不在基准测量范围内）。
    let counter = TokenCounter::openai().expect(
        "tiktoken 词表加载失败：bench 环境应可加载内嵌词表（失败回退见 TokenCounter::openai）",
    );
    let code = build_8kb_code();
    debug_assert!(code.len() >= 8 * 1024);

    // 默认编码路径（cl100k_base）：历史默认 / 非 OpenAI provider 近似。
    c.bench_function("tiktoken/count_text_8kb_cl100k", |b| {
        b.iter(|| black_box(counter.count_text(&code)));
    });

    // o200k_base 路径（gpt-4o 家族）：run-loop 用量统计的真实入口。
    c.bench_function("tiktoken/count_text_for_8kb_gpt4o", |b| {
        b.iter(|| black_box(counter.count_text_for(&code, "gpt-4o")));
    });

    // 完整上下文计数：system + 单条用户消息（每轮 build_provider_context 的用量路径）。
    let system = vec!["系统提示（基准）：你是代码助手，遵循仓库纪律。".to_string()];
    let msgs = vec![ProviderMessage::User {
        content: vec![UserContent::Text { text: code }],
    }];
    c.bench_function("tiktoken/count_context_8kb_gpt4o", |b| {
        b.iter(|| black_box(counter.count_context_for(&system, &msgs, "gpt-4o")));
    });
}

criterion_group!(benches, bench_token_count);
criterion_main!(benches);
