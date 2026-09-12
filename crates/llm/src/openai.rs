//! `OpenAI` Chat Completions 适配器（覆盖最广，含兼容网关 / 本地 vLLM）。
//!
//! 流程：构造 `/chat/completions` 请求体（`stream:true` + `include_usage`）→
//! `reqwest` 流式响应 → 逐行解析 SSE `data:` 帧 → 增量映射为 [`AssistantEvent`]。

use std::pin::Pin;

use agent_core::{
    Api, AssistantEvent, AssistantEventStream, CompletionRequest, ContentBlock, LlmError,
    LlmProvider, ProviderCallContext, ProviderMessage, StopReason, ToolChoice, ToolChoiceDirective,
    Usage, UserContent,
};
use async_stream::stream;
use futures::StreamExt;
use serde::Deserialize;

/// `OpenAI` Chat Completions 适配器。
pub struct OpenAiCompletionsAdapter {
    client: reqwest::Client,
}

impl OpenAiCompletionsAdapter {
    /// 构造（复用外部 `reqwest::Client` 以共享连接池）。
    #[must_use]
    pub const fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

/// 本适配器实现的是 Chat Completions wire；`Api::OpenAiResponses`（`/v1/responses`）
/// **不得**列入——名义支持会把 Responses 请求静默降级为 chat wire，Responses 专属能力
/// （reasoning items / encrypted replay）全失且报错误导排障。Responses wire 由
/// [`crate::OpenAiResponsesAdapter`] 单独承载。
/// `OllamaChat` 经其 OpenAI 兼容端点（`{base}/v1/chat/completions`）同构，保留。
const SUPPORTED: &[Api] = &[Api::OpenAiCompletions, Api::OllamaChat];

#[async_trait::async_trait]
impl LlmProvider for OpenAiCompletionsAdapter {
    fn id(&self) -> &'static str {
        "openai-completions"
    }
    fn supports(&self) -> &[Api] {
        SUPPORTED
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        ctx: &ProviderCallContext,
    ) -> Result<AssistantEventStream, LlmError> {
        let base = ctx
            .base_url
            .as_deref()
            .unwrap_or("https://api.openai.com/v1")
            .trim_end_matches('/');
        let url = format!("{base}/chat/completions");

        let body = build_body_with_quirks(&request, ctx.quirks);
        let model_id = request.model.id.clone();

        // H24：`auth = none`（或 key 为空）时不发内置鉴权头，而不是发 `Bearer `。
        let mut http = self.client.post(&url);
        if let Some(key) = ctx.builtin_api_key() {
            http = http.bearer_auth(key);
        }
        let resp = crate::apply_custom_headers(http, &ctx.headers)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        if !resp.status().is_success() {
            // 429 → RateLimit（含 Retry-After），供执行循环退避重试（Phase 0）。
            return Err(crate::http_error(resp).await);
        }
        Ok(parse_sse_stream(resp, model_id))
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 请求体构建
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
fn build_body(req: &CompletionRequest) -> serde_json::Value {
    build_body_with_quirks(req, agent_core::ProviderQuirks::default())
}

/// H24：带兼容开关的请求体构建（`build_body` 为其默认开关版本，保持既有调用点）。
fn build_body_with_quirks(
    req: &CompletionRequest,
    quirks: agent_core::ProviderQuirks,
) -> serde_json::Value {
    let mut messages: Vec<serde_json::Value> = Vec::new();

    for sys in &req.system {
        messages.push(serde_json::json!({ "role": "system", "content": sys }));
    }

    for msg in &req.messages {
        match msg {
            ProviderMessage::System(s) => {
                messages.push(serde_json::json!({ "role": "system", "content": s }));
            }
            ProviderMessage::User { content } => {
                // 用户消息支持 vision：含图像时走 content 数组（image_url data URI），
                // 纯文本时退回紧凑字符串形式以保持与旧模型兼容。
                let has_image = content
                    .iter()
                    .any(|c| matches!(c, UserContent::Image { .. }));
                if has_image {
                    let parts: Vec<serde_json::Value> = content
                        .iter()
                        .filter_map(|c| match c {
                            UserContent::Text { text } if !text.is_empty() => {
                                Some(serde_json::json!({ "type": "text", "text": text }))
                            }
                            UserContent::Image { mime, data } => Some(serde_json::json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{mime};base64,{data}") }
                            })),
                            UserContent::Text { .. } => None,
                        })
                        .collect();
                    messages.push(serde_json::json!({ "role": "user", "content": parts }));
                } else {
                    let text = join_user_text(content);
                    messages.push(serde_json::json!({ "role": "user", "content": text }));
                }
            }
            ProviderMessage::Assistant { content } => {
                let text: String = content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("");
                let tool_calls: Vec<serde_json::Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall { id, name, arguments, .. } => Some(serde_json::json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string())
                            }
                        })),
                        _ => None,
                    })
                    .collect();
                let mut entry = serde_json::json!({ "role": "assistant" });
                entry["content"] = if text.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(text)
                };
                if !tool_calls.is_empty() {
                    entry["tool_calls"] = serde_json::Value::Array(tool_calls);
                }
                messages.push(entry);
            }
            ProviderMessage::Tool {
                tool_call_id,
                content,
                images,
                ..
            } => {
                // OpenAI tool role 仅支持文本；图像降级为占位提示（Anthropic 端可真实多模态）。
                let final_content = if images.is_empty() {
                    content.clone()
                } else {
                    format!(
                        "{content}\n[附 {} 张图像，当前 provider 的 tool role 不支持工具结果多模态]",
                        images.len()
                    )
                };
                messages.push(serde_json::json!({ "role": "tool", "tool_call_id": tool_call_id, "content": final_content }));
            }
        }
    }

    // H24：`max_tokens_field` 决定 wire 字段名（部分网关只认 `max_completion_tokens`）。
    let mut body = serde_json::json!({
        "model": req.model.id,
        "messages": messages,
        "stream": true,
    });
    let max_tokens_field = quirks.max_tokens_field.unwrap_or_default();
    body[max_tokens_field.as_str()] = serde_json::json!(req.max_tokens);
    // H24：`omit_stream_options` 时省略 `stream_options`（部分兼容网关会 400）。
    if !quirks.omit_stream_options {
        body["stream_options"] = serde_json::json!({ "include_usage": true });
    }

    if !req.tools.is_empty() {
        let tools: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": crate::transform::normalize_tool_schema(&t.schema)
                    }
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
        if let Some(tc) = &req.tool_choice {
            if !quirks.omit_tool_choice {
                body["tool_choice"] = map_tool_choice(tc);
            }
        }
    }

    if let Some(temp) = req.temperature {
        if !quirks.omit_temperature {
            body["temperature"] = serde_json::json!(temp);
        }
    }
    // ThinkingConfig：OpenAI 推理模型映射为 reasoning_effort（按预算档位）
    if let Some(thinking) = &req.thinking {
        let effort = if thinking.budget_tokens >= 32_000 {
            "high"
        } else if thinking.budget_tokens >= 12_000 {
            "medium"
        } else {
            "low"
        };
        if !quirks.omit_reasoning {
            body["reasoning_effort"] = serde_json::json!(effort);
        }
    }
    // per-model 额外请求体字段（如 vLLM chat_template_kwargs）合并到顶层
    crate::merge_extra_body(&mut body, req.model.extra_body.as_ref());
    body
}

fn join_user_text(content: &[UserContent]) -> String {
    content
        .iter()
        .map(|c| match c {
            UserContent::Text { text } => text.as_str(),
            UserContent::Image { .. } => "[image]",
        })
        .collect::<Vec<_>>()
        .join("")
}

fn map_tool_choice(directive: &ToolChoiceDirective) -> serde_json::Value {
    match directive {
        ToolChoiceDirective::Hard(ToolChoice::Auto) => serde_json::json!("auto"),
        ToolChoiceDirective::Hard(ToolChoice::None) => serde_json::json!("none"),
        ToolChoiceDirective::Hard(ToolChoice::Any | ToolChoice::Required) => {
            serde_json::json!("required")
        }
        ToolChoiceDirective::Hard(ToolChoice::Function { name }) => {
            serde_json::json!({ "type": "function", "function": { "name": name } })
        }
        // 软需求由循环处理（提醒 + 必要时升级），Provider 端保持 auto 以保护前缀缓存。
        ToolChoiceDirective::Soft(_) => serde_json::json!("auto"),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// SSE 流解析
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct ToolCallAccum {
    id: Option<String>,
    name: Option<String>,
    args: String,
    started: bool,
}

#[derive(Deserialize)]
struct ChunkValue {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsageChunk>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FnDelta>,
}

#[derive(Deserialize)]
struct FnDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct UsageChunk {
    prompt_tokens: u64,
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

/// `OpenAI` 流式 `usage.prompt_tokens_details`（缓存计量）。
#[derive(Deserialize, Default)]
struct PromptTokensDetails {
    /// 缓存命中读取 token（`cached_tokens`）。
    #[serde(default)]
    cached_tokens: u64,
}

fn parse_sse_stream(resp: reqwest::Response, model_id: String) -> AssistantEventStream {
    let s = stream! {
        yield AssistantEvent::MessageStart;

        let mut bytes_stream = resp.bytes_stream();
        // 字节缓冲：以 `\n` 切行，确保跨 chunk 的多字节 UTF-8 字符不被拆断丢弃。
        let mut buf: Vec<u8> = Vec::new();
        let mut text_buf = String::new();
        let mut tool_calls: Vec<ToolCallAccum> = Vec::new();
        let mut finish: Option<String> = None;
        let mut usage_acc = Usage::default();

        loop {
            // 按 chunk 的空闲读超时：只要上游持续吐 token，每次读到新 chunk 即顺延计时；
            // 仅当真正静默超过阈值（上游挂起/网络中断）才判超时。替代 reqwest
            // ClientBuilder::timeout 的整条请求总超时——后者会误杀仍在正常输出的慢速
            // LLM 长流，导致收不到 `data: [DONE]` 终止帧而误判「未收到结束标记」。
            let chunk_res = match tokio::time::timeout(
                crate::STREAM_IDLE_TIMEOUT,
                bytes_stream.next(),
            )
            .await
            {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(_) => {
                    // P0-自愈激活：流瞬时中断时，若 finish 已收或存在已完成工具调用（参数合法
                    // JSON），发 MessageEnd（finish 未收则标记 stream_interrupted 瞬时错误），
                    // 供 agent 瞬时恢复保留并执行已完成工具；否则保持原兜底行为（发 Error）。
                    if let Some(msg) = finalize_stream_interrupt(
                        &model_id,
                        &text_buf,
                        &tool_calls,
                        &finish,
                        &usage_acc,
                    ) {
                        yield AssistantEvent::MessageEnd(msg);
                        return;
                    }
                    yield AssistantEvent::Error(LlmError::StreamInterrupted(format!(
                        "流空闲超过 {} 秒未收到数据，判定上游静默",
                        crate::STREAM_IDLE_TIMEOUT.as_secs()
                    )));
                    break;
                }
            };
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    if let Some(msg) = finalize_stream_interrupt(
                        &model_id,
                        &text_buf,
                        &tool_calls,
                        &finish,
                        &usage_acc,
                    ) {
                        yield AssistantEvent::MessageEnd(msg);
                        return;
                    }
                    yield AssistantEvent::Error(LlmError::StreamInterrupted(e.to_string()));
                    break;
                }
            };
            buf.extend_from_slice(chunk.as_ref());
            // 处理完整行
            while let Some(line_bytes) = crate::drain_line(&mut buf) {
                let line = String::from_utf8_lossy(&line_bytes).trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" {
                    let msg = build_message(&model_id, &text_buf, &tool_calls, &finish, &usage_acc);
                    yield AssistantEvent::MessageEnd(msg);
                    return;
                }
                let Ok(value) = serde_json::from_str::<ChunkValue>(data) else { continue };
                for choice in &value.choices {
                    if let Some(text) = choice.delta.content.clone() {
                        text_buf.push_str(&text);
                        yield AssistantEvent::TextDelta(text);
                    }
                    if let Some(tcs) = choice.delta.tool_calls.as_ref() {
                        for tc in tcs {
                            let idx = tc.index;
                            while tool_calls.len() <= idx {
                                tool_calls.push(ToolCallAccum::default());
                            }
                            let acc = &mut tool_calls[idx];
                            if let Some(id) = tc.id.clone() {
                                acc.id = Some(id);
                            }
                            if let Some(name) = tc.function.as_ref().and_then(|f| f.name.clone()) {
                                acc.name = Some(name);
                            }
                            let should_start = !acc.started && acc.id.is_some() && acc.name.is_some();
                            if should_start {
                                acc.started = true;
                            }
                            let start_evt = should_start.then(|| {
                                (acc.id.clone().unwrap_or_default(), acc.name.clone().unwrap_or_default())
                            });
                            let delta_evt = tc
                                .function
                                .as_ref()
                                .and_then(|f| f.arguments.as_ref())
                                .filter(|a| !a.is_empty())
                                .map(|a| {
                                    acc.args.push_str(a);
                                    (acc.id.clone().unwrap_or_default(), a.clone())
                                });
                            if let Some((id, name)) = start_evt {
                                yield AssistantEvent::ToolCallStart { id, name };
                            }
                            if let Some((id, partial)) = delta_evt {
                                yield AssistantEvent::ToolCallDelta { id, partial_json: partial };
                            }
                        }
                    }
                    if let Some(fr) = choice.finish_reason.clone() {
                        finish = Some(fr);
                    }
                }
                if let Some(u) = value.usage.as_ref() {
                    usage_acc.input_tokens = u.prompt_tokens;
                    usage_acc.output_tokens = u.completion_tokens;
                    // P0-4：prompt_tokens_details.cached_tokens → cache_read（OpenAI 兼容端点）。
                    if let Some(d) = u.prompt_tokens_details.as_ref() {
                        usage_acc.cache_read_tokens = d.cached_tokens;
                    }
                    yield AssistantEvent::Usage(usage_acc.clone());
                }
            }
            // 防御无换行的超长行撑爆内存：drain_line 抽干完整行后 buf 仅余未完结尾段。
            if crate::line_buffer_too_long(&buf) {
                // P0-自愈激活：超长行中断时同样尝试保留已完成工具；无内容则改走 break
                // fallthrough（而非 return 丢弃累积内容），让流自然结束 MessageEnd 兜底。
                if let Some(msg) = finalize_stream_interrupt(
                    &model_id,
                    &text_buf,
                    &tool_calls,
                    &finish,
                    &usage_acc,
                ) {
                    yield AssistantEvent::MessageEnd(msg);
                    return;
                }
                yield AssistantEvent::Error(LlmError::StreamInterrupted(
                    "SSE 行超过最大长度上限".into(),
                ));
                break;
            }
        }
        // 流自然结束（未见 [DONE]）
        let msg = build_message(&model_id, &text_buf, &tool_calls, &finish, &usage_acc);
        yield AssistantEvent::MessageEnd(msg);
    };
    Box::pin(s) as Pin<Box<dyn futures::Stream<Item = AssistantEvent> + Send>>
}

fn build_message(
    model_id: &str,
    text_buf: &str,
    tool_calls: &[ToolCallAccum],
    finish: &Option<String>,
    usage: &Usage,
) -> agent_core::AssistantMessage {
    let mut content = Vec::new();
    if !text_buf.is_empty() {
        content.push(ContentBlock::Text {
            text: text_buf.to_string(),
        });
    }
    for (i, tc) in tool_calls.iter().enumerate() {
        let id = tc.id.clone().unwrap_or_else(|| format!("call_{i}"));
        let name = tc.name.clone().unwrap_or_default();
        let arguments = if tc.args.is_empty() {
            serde_json::Value::Object(Default::default())
        } else {
            serde_json::from_str(&tc.args)
                .unwrap_or_else(|_| serde_json::Value::String(tc.args.clone()))
        };
        content.push(ContentBlock::ToolCall {
            id,
            name,
            arguments,
            signature: None,
        });
    }
    // P2-P：content_filter → StopReason::Error + sensitive 详情（移植 replay-policy.ts）。
    // replay 时 build_provider_context 据此过滤，避免 refusal 文本反复喂回模型。
    let (stop_reason, stop_details) = match finish.as_deref() {
        Some("length") => (Some(StopReason::Length), None),
        Some("tool_calls" | "function_call") => (Some(StopReason::ToolUse), None),
        Some("content_filter") => (
            Some(StopReason::Error),
            Some(agent_core::StopDetails::new("sensitive")),
        ),
        Some(_) => (Some(StopReason::Stop), None),
        None => (None, None),
    };
    agent_core::AssistantMessage {
        content,
        usage: usage.clone(),
        model: model_id.to_string(),
        stop_reason,
        stop_details,
    }
}

/// 流瞬时中断兜底终态（移植 oh-my-pi `retainCompletedToolCalls`）。
///
/// finish 已收（响应实际完成）或存在已完成工具调用（参数解析为合法 JSON）时返回
/// [`Some`] 消息供 [`MessageEnd`](AssistantEvent::MessageEnd) 承载——finish 未收时标记
/// `stream_interrupted` 瞬时错误，供 agent 循环的瞬时恢复改写为 ToolUse、执行已完成工具
/// 而非整轮废弃；否则返回 [`None`]（无内容可救，调用方发 [`Error`](AssistantEvent::Error)
/// 走兜底重试 / 续写）。
fn finalize_stream_interrupt(
    model_id: &str,
    text_buf: &str,
    tool_calls: &[ToolCallAccum],
    finish: &Option<String>,
    usage: &Usage,
) -> Option<agent_core::AssistantMessage> {
    let recoverable = finish.is_some()
        || tool_calls.iter().any(|tc| {
            tc.id.is_some()
                && tc.name.is_some()
                && serde_json::from_str::<serde_json::Value>(&tc.args).is_ok()
        });
    if !recoverable {
        return None;
    }
    let mut msg = build_message(model_id, text_buf, tool_calls, finish, usage);
    // finish 已收 → 保留其 stop_reason（含 content_filter→sensitive，不覆盖为瞬时类）；
    // finish 未收 → 标记瞬时错误触发 agent 瞬时恢复。
    if finish.is_none() {
        crate::mark_transient_stream_error(&mut msg);
    }
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ContentBlock, ProviderMessage, ToolSpec};

    #[test]
    fn supports_excludes_unimplemented_responses_wire() {
        // 伪适配回归：本适配器只做 chat wire，SUPPORTED 不得名义包含 Responses——
        // 否则 openai-responses 模型被静默降级为 chat wire（Responses 由
        // `OpenAiResponsesAdapter` 承载，见 openai_responses.rs）。
        assert_eq!(
            SUPPORTED,
            &[Api::OpenAiCompletions, Api::OllamaChat],
            "SUPPORTED 应恰为已实现的两种 wire"
        );
    }

    #[test]
    fn body_includes_system_and_user() {
        let req = CompletionRequest {
            model: agent_core::Model::with_defaults(
                "gpt-4o-mini",
                "openai",
                Api::OpenAiCompletions,
            ),
            system: vec!["You are helpful.".into()],
            messages: vec![ProviderMessage::User {
                content: vec![agent_core::UserContent::Text { text: "hi".into() }],
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: 16,
            temperature: Some(0.0),
            thinking: None,
            cache_key: None,
            stable_prefix_len: 0,
        };
        let body = build_body(&req);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "hi");
        assert_eq!(body["stream"], true);
        assert_eq!(body["temperature"], 0.0);
    }

    #[test]
    fn body_serializes_tools_and_tool_choice() {
        let req = CompletionRequest {
            model: agent_core::Model::with_defaults("m", "openai", Api::OpenAiCompletions),
            system: vec![],
            messages: vec![],
            tools: vec![ToolSpec::new(
                "read_file",
                "read",
                serde_json::json!({"type": "object"}),
            )],
            tool_choice: Some(ToolChoiceDirective::Hard(ToolChoice::Auto)),
            max_tokens: 16,
            temperature: None,
            thinking: None,
            cache_key: None,
            stable_prefix_len: 0,
        };
        let body = build_body(&req);
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn build_message_parses_tool_call_args() {
        let tc = ToolCallAccum {
            id: Some("call_1".into()),
            name: Some("read_file".into()),
            args: r#"{"path":"a.rs"}"#.into(),
            ..Default::default()
        };
        let msg = build_message(
            "m",
            "",
            &[tc],
            &Some("tool_calls".into()),
            &Usage::default(),
        );
        let block = &msg.content[0];
        let ContentBlock::ToolCall {
            name, arguments, ..
        } = block
        else {
            panic!("应为 ToolCall");
        };
        assert_eq!(*name, "read_file");
        assert_eq!(arguments["path"], "a.rs");
        assert_eq!(msg.stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn finalize_stream_interrupt_retains_completed_toolcall() {
        // P0-自愈激活：finish 未收 + 已完成工具调用（参数合法 JSON）→ 标记瞬时错误，
        // 供 agent 瞬时恢复保留并执行已完成工具（而非整轮废弃）。
        let tc = ToolCallAccum {
            id: Some("call_1".into()),
            name: Some("read_file".into()),
            args: r#"{"path":"a.rs"}"#.into(),
            ..Default::default()
        };
        let msg = finalize_stream_interrupt("m", "", &[tc], &None, &Usage::default())
            .expect("已完成工具调用应可恢复");
        assert_eq!(msg.stop_reason, Some(StopReason::Error));
        assert!(
            msg.stop_details
                .as_ref()
                .is_some_and(agent_core::StopDetails::is_transient_stream_error)
        );
    }

    #[test]
    fn finalize_stream_interrupt_finish_received_keeps_stop_reason() {
        // finish 已收（响应实际完成）→ 不标记瞬时错误，保留 finish 推导的 stop_reason。
        let tc = ToolCallAccum {
            id: Some("call_1".into()),
            name: Some("read_file".into()),
            args: r#"{"path":"a.rs"}"#.into(),
            ..Default::default()
        };
        let msg = finalize_stream_interrupt(
            "m",
            "",
            &[tc],
            &Some("tool_calls".into()),
            &Usage::default(),
        )
        .expect("finish 已收应返回消息");
        assert_eq!(msg.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(msg.stop_details, None);
    }

    #[test]
    fn finalize_stream_interrupt_partial_args_not_recoverable() {
        // 参数不合法 JSON（流中断于工具参数中途）→ 不可恢复，返回 None。
        let tc = ToolCallAccum {
            id: Some("call_1".into()),
            name: Some("read_file".into()),
            args: r#"{"path":"a.rs"#.into(), // 残缺 JSON
            ..Default::default()
        };
        assert!(finalize_stream_interrupt("m", "", &[tc], &None, &Usage::default()).is_none());
    }

    #[test]
    fn finalize_stream_interrupt_empty_not_recoverable() {
        // 无内容、无 finish → 不可恢复，返回 None（调用方发 Error 走兜底）。
        assert!(finalize_stream_interrupt("m", "", &[], &None, &Usage::default()).is_none());
    }

    /// H24：兼容开关逐项生效（默认全关 → 标准 wire 不变）。
    #[test]
    fn quirks_omit_parameters_on_demand() {
        use agent_core::ProviderQuirks;
        let mut req = CompletionRequest {
            model: agent_core::Model::with_defaults("m", "openai", Api::OpenAiCompletions),
            system: vec![],
            messages: vec![],
            tools: vec![ToolSpec::new(
                "t",
                "d",
                serde_json::json!({"type": "object"}),
            )],
            tool_choice: Some(ToolChoiceDirective::Hard(ToolChoice::Auto)),
            max_tokens: 16,
            temperature: Some(0.5),
            thinking: Some(agent_core::ThinkingConfig::new(16_000)),
            cache_key: None,
            stable_prefix_len: 0,
        };
        // 默认：标准字段齐全。
        let base = build_body_with_quirks(&req, ProviderQuirks::default());
        assert_eq!(base["stream_options"]["include_usage"], true);
        assert!(base["temperature"].is_number());
        assert_eq!(base["tool_choice"], "auto");
        assert_eq!(base["reasoning_effort"], "medium");

        // 全开：四个字段都被省略（网关不认时用）。
        let q = ProviderQuirks {
            omit_temperature: true,
            omit_stream_options: true,
            omit_tool_choice: true,
            omit_reasoning: true,
            max_tokens_field: None,
        };
        assert!(!q.is_empty());
        let trimmed = build_body_with_quirks(&req, q);
        assert!(trimmed.get("stream_options").is_none());
        assert!(trimmed.get("temperature").is_none());
        assert!(trimmed.get("tool_choice").is_none());
        assert!(trimmed.get("reasoning_effort").is_none());
        // 核心字段不受影响（兼容开关只删可选参数）。
        assert_eq!(trimmed["model"], "m");
        assert_eq!(trimmed["stream"], true);
        assert_eq!(trimmed["max_tokens"], 16);

        // H24：`max_tokens_field` 切换 wire 字段名（部分网关只认 max_completion_tokens）。
        let switched = build_body_with_quirks(
            &req,
            ProviderQuirks {
                max_tokens_field: Some(agent_core::MaxTokensField::MaxCompletionTokens),
                ..Default::default()
            },
        );
        assert_eq!(switched["max_completion_tokens"], 16);
        assert!(switched.get("max_tokens").is_none(), "旧字段名不应同时出现");
        assert!(trimmed["tools"].is_array());
        // `build_body` 保持旧语义（默认开关）。
        assert_eq!(build_body(&req)["stream_options"]["include_usage"], true);
        let _ = &mut req;
    }

    /// H24：自定义头的展开与非法头跳过（真实 reqwest 请求构建）。
    #[test]
    fn custom_headers_apply_and_skip_invalid() {
        let client = reqwest::Client::new();
        let req = crate::apply_custom_headers(
            client.post("http://127.0.0.1:9/v1/chat/completions"),
            &[
                ("HTTP-Referer".into(), "https://my.app".into()),
                ("X-Api-Version".into(), "2024-10-01".into()),
                ("bad header".into(), "x".into()), // 非法名 → 跳过
            ],
        )
        .build()
        .expect("请求应可构建");
        assert_eq!(req.headers()["HTTP-Referer"], "https://my.app");
        assert_eq!(req.headers()["X-Api-Version"], "2024-10-01");
        // 非法头未进入请求（且不影响其它头）。
        assert!(req.headers().get("bad header").is_none());
    }
    #[test]
    fn body_merges_extra_body_into_top_level() {
        let mut model =
            agent_core::Model::with_defaults("Qwen/Qwen3", "vllm", Api::OpenAiCompletions);
        model.extra_body = Some(serde_json::json!({
            "chat_template_kwargs": { "thinking": true }
        }));
        let req = CompletionRequest {
            model,
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 16,
            temperature: None,
            thinking: None,
            cache_key: None,
            stable_prefix_len: 0,
        };
        let body = build_body(&req);
        // extra_body 的键应出现在请求体顶层
        assert_eq!(body["chat_template_kwargs"]["thinking"], true);
        // 标准字段不受影响
        assert_eq!(body["model"], "Qwen/Qwen3");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn body_without_extra_body_is_unchanged() {
        let req = CompletionRequest {
            model: agent_core::Model::with_defaults("gpt-4o", "openai", Api::OpenAiCompletions),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 16,
            temperature: None,
            thinking: None,
            cache_key: None,
            stable_prefix_len: 0,
        };
        let body = build_body(&req);
        // 无 extra_body 时不引入意外字段
        assert!(body.get("chat_template_kwargs").is_none());
    }
}

// Provider 自荐注册统一集中在 plugin.rs（collect_providers 用共享 client 构造）。
