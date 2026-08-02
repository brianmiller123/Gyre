//! 基准门：run-loop 压缩耗时（对标报告 §六.2 热路径基准）。
//!
//! 真实热路径函数（`agent_context::compaction::Compactor`，crates/context/src/compaction.rs）：
//! - `Compactor::summarize(log, keep_recent, provider)`：compaction.rs:443
//!   （旧消息经 `message_to_summary_line` 序列化为文本行 + 摘要消息拼接；provider 在
//!   run-loop 中为 LLM 调用，基准用固定摘要 provider，只测序列化部分）
//! - `Compactor::snapcompact(log, keep_recent, opts)`：compaction.rs:468
//!   （normalize + paginate + `agent_snapcompact::render_frame` 渲染 PNG 帧；帧形状固定
//!   1568² / 8×16 单元格，单帧行数上限 98——基准按 ~92 行构造单页小帧，控制渲染时长）
//!
//! 输入确定性：消息日志在 bench 内构造（user / assistant / tool_call / tool_result 混合），
//! 不依赖外部文件或网络。

use std::future::Future;
use std::hint::black_box;
use std::pin::Pin;

use agent_core::{
    AgentMessage, AssistantMessage, ContentBlock, ToolResult, ToolResultMessage, Usage,
};
use agent_context::compaction::{Compactor, SnapcompactOptions, SummaryProvider};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use tokio::runtime::Runtime;

/// 固定摘要 provider：不调 LLM，返回确定性摘要——基准聚焦消息序列化本身。
struct FixedSummary;

impl SummaryProvider for FixedSummary {
    fn summarize(
        &self,
        _old: &[String],
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async {
            Ok("基准摘要：已完成 3 项，进行中 2 项，下一步 1 项。".to_string())
        })
    }
}

fn assistant(text: &str) -> AgentMessage {
    AgentMessage::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text { text: text.into() }],
        usage: Usage::default(),
        model: "bench-model".into(),
        stop_reason: None,
        stop_details: None,
    })
}

fn tool_call(id: &str, name: &str) -> AgentMessage {
    AgentMessage::Assistant(AssistantMessage {
        content: vec![ContentBlock::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "bench-model".into(),
        stop_reason: None,
        stop_details: None,
    })
}

fn tool_result(id: &str, text: &str) -> AgentMessage {
    AgentMessage::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        result: ToolResult::text(text),
    })
}

/// 构造 N 条真实形态的对话日志（交替 user / assistant / 工具调用与结果）。
/// 各消息单行文本，保证 `message_to_summary_line` 每消息渲染恰好一行（行数可预测）。
fn build_log(n: usize) -> Vec<AgentMessage> {
    let mut log = Vec::with_capacity(n + n / 4);
    for i in 0..n {
        match i % 4 {
            0 => log.push(AgentMessage::user_text(format!("问题 {i}：如何实现 {i} 号功能？"))),
            1 => log.push(assistant(&format!(
                "方案：实现 {i} 号功能，注意边界条件与性能，随后自测验证。"
            ))),
            2 => {
                let id = format!("call-{i}");
                let tool = if i % 2 == 0 { "read_file" } else { "grep" };
                log.push(tool_call(&id, tool));
                log.push(tool_result(&id, &format!("命中 {i} 行，含关键信息 TODO：检查点 {i}。")));
            }
            _ => log.push(assistant(&format!("已确认第 {i} 项完成，下一步继续推进。"))),
        }
    }
    log
}

/// summarize：旧消息序列化 + 摘要消息拼接（keep_recent 窗口保留在日志中）。
fn bench_summarize(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime 创建失败");
    let log = build_log(120);
    c.bench_function("compaction/summarize_120_msgs", |b| {
        b.iter_batched(
            // 每轮复制日志（不计时；调用方按值持有日志，复制即真实调用前置成本）。
            || log.clone(),
            |log| {
                let out = rt
                    .block_on(Compactor::summarize(log, 20, &FixedSummary))
                    .expect("summarize 不应失败");
                black_box(out);
            },
            BatchSize::SmallInput,
        );
    });
}

/// snapcompact：旧消息序列化 + 分页 + PNG 帧渲染（~92 行 → 单页小帧，控制时长）。
fn bench_snapcompact(c: &mut Criterion) {
    let log = build_log(90);
    let opts = SnapcompactOptions {
        max_frames: 4,
        // 默认帧 1568² / 16px 行高 = 98 行；单页小帧把渲染时长控制在毫秒级。
        max_lines: 98,
        max_cols: 196,
    };
    c.bench_function("compaction/snapcompact_90_msgs", |b| {
        b.iter_batched(
            || log.clone(),
            |log| {
                let out = Compactor::snapcompact(log, 20, &opts).expect("snapcompact 不应失败");
                black_box(out);
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_summarize, bench_snapcompact);
criterion_main!(benches);
