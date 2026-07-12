//! Anthropic Messages API 适配器（Claude）。
//!
//! 流式 SSE：按 data 帧 `type` 字段分发（message_start / content_block_start /
//! content_block_delta / message_delta / message_stop），累积文本与工具调用。

use std::pin::Pin;

use agent_core::{
    Api, AssistantEvent, AssistantEventStream, CompletionRequest, ContentBlock, LlmError, LlmProvider,
    ProviderCallContext, ProviderMessage, StopReason, ToolChoice, ToolChoiceDirective, Usage, UserContent,
};
use async_stream::stream;
use futures::StreamExt;
use serde::Deserialize;

use crate::transform::{
    anthropic_apply_cache, anthropic_system_blocks, normalize_tool_schema, CacheStrategy,
};

/// Anthropic Messages 适配器。
pub struct AnthropicMessagesAdapter {
    client: reqwest::Client,
}

impl AnthropicMessagesAdapter {
    /// 构造。
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

const SUPPORTED: &[Api] = &[Api::AnthropicMessages];

#[async_trait::async_trait]
impl LlmProvider for AnthropicMessagesAdapter {
    fn id(&self) -> &'static str {
        "anthropic-messages"
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
            .unwrap_or("https://api.anthropic.com")
            .trim_end_matches('/');
        let url = format!("{base}/v1/messages");

        let body = build_body(&request);
        let model_id = request.model.id.clone();

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", ctx.api_key.as_deref().unwrap_or_default())
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let text = crate::read_error_body(resp).await;
            return Err(LlmError::Http {
                status: status.as_u16(),
                body: text,
            });
        }
        Ok(parse_stream(resp, model_id))
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 请求体
// ──────────────────────────────────────────────────────────────────────────────

fn build_body(req: &CompletionRequest) -> serde_json::Value {
    // system：合并 request.system 与 ProviderMessage::System
    let mut system_parts: Vec<String> = req.system.clone();
    let mut messages: Vec<serde_json::Value> = Vec::new();
    for m in &req.messages {
        match m {
            ProviderMessage::System(s) => system_parts.push(s.clone()),
            ProviderMessage::User { content } => {
                let blocks: Vec<serde_json::Value> = content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text { text } => Some(serde_json::json!({"type":"text","text":text})),
                        UserContent::Image { mime, data } => {
                            Some(serde_json::json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}}))
                        }
                    })
                    .collect();
                messages.push(serde_json::json!({"role":"user","content":blocks}));
            }
            ProviderMessage::Assistant { content } => {
                let blocks: Vec<serde_json::Value> = content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => serde_json::json!({"type":"text","text":text}),
                        ContentBlock::ToolCall { id, name, arguments } => serde_json::json!({
                            "type":"tool_use","id":id,"name":name,"input":arguments
                        }),
                        ContentBlock::Thinking { text, .. } => serde_json::json!({"type":"thinking","thinking":text}),
                    })
                    .collect();
                messages.push(serde_json::json!({"role":"assistant","content":blocks}));
            }
            ProviderMessage::Tool { tool_call_id, content, is_error, images } => {
                // Anthropic tool_result 支持多模态：有图像时 content 为 text + image block 数组。
                let content_val = if images.is_empty() {
                    serde_json::Value::String(content.clone())
                } else {
                    let mut blocks = vec![serde_json::json!({"type":"text","text":content})];
                    for img in images {
                        blocks.push(serde_json::json!({
                            "type":"image","source":{"type":"base64","media_type":img.mime,"data":img.data}
                        }));
                    }
                    serde_json::Value::Array(blocks)
                };
                messages.push(serde_json::json!({
                    "role":"user",
                    "content":[{"type":"tool_result","tool_use_id":tool_call_id,"content":content_val,"is_error":is_error}]
                }));
            }
        }
    }

    let mut body = serde_json::json!({
        "model": req.model.id,
        "max_tokens": req.max_tokens,
        "messages": messages,
        "stream": true,
    });
    if !system_parts.is_empty() {
        // system 段：统一经 transform 层构造（带 cache_control）。
        body["system"] = anthropic_system_blocks(&system_parts, CacheStrategy::MultiPoint);
    }
    if !req.tools.is_empty() {
        let tools: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| serde_json::json!({"name":t.name,"description":t.description,"input_schema":normalize_tool_schema(&t.schema)}))
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
        if let Some(tc) = &req.tool_choice {
            if let Some(a) = anthropic_tool_choice(tc) {
                body["tool_choice"] = a;
            }
        }
    }
    if let Some(temp) = req.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    // ThinkingConfig：Anthropic 映射为 thinking.budget_tokens
    if let Some(thinking) = &req.thinking {
        body["thinking"] = serde_json::json!({
            "type": "enabled",
            "budget_tokens": thinking.budget_tokens
        });
    }
    // 统一缓存层：多点 cache_control（system + tools 末尾 + 倒数第二条消息），最大化前缀缓存命中。
    anthropic_apply_cache(&mut body, CacheStrategy::MultiPoint);
    // per-model 额外请求体字段（如 vLLM chat_template_kwargs）合并到顶层
    crate::merge_extra_body(&mut body, req.model.extra_body.as_ref());
    body
}

fn anthropic_tool_choice(d: &ToolChoiceDirective) -> Option<serde_json::Value> {
    match d {
        ToolChoiceDirective::Hard(ToolChoice::Auto) => Some(serde_json::json!({"type":"auto"})),
        ToolChoiceDirective::Hard(ToolChoice::Any) | ToolChoiceDirective::Hard(ToolChoice::Required) => {
            Some(serde_json::json!({"type":"any"}))
        }
        ToolChoiceDirective::Hard(ToolChoice::None) => None,
        ToolChoiceDirective::Hard(ToolChoice::Function { name }) => {
            Some(serde_json::json!({"type":"tool","name":name}))
        }
        ToolChoiceDirective::Soft(_) => Some(serde_json::json!({"type":"auto"})),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// SSE 解析
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct BlockState {
    is_tool: bool,
    text: String,
    tool_id: Option<String>,
    tool_name: Option<String>,
    tool_args: String,
    started: bool,
}

#[derive(Deserialize)]
struct Ev {
    #[serde(rename = "type")]
    t: String,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    message: Option<MessageField>,
    #[serde(default)]
    content_block: Option<Cb>,
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    usage: Option<UsageChunk>,
}

#[derive(Deserialize)]
struct MessageField {
    #[serde(default)]
    usage: Option<UsageChunk>,
}

#[derive(Deserialize)]
struct Cb {
    #[serde(rename = "type")]
    t: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(rename = "type", default)]
    t: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct UsageChunk {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

fn parse_stream(resp: reqwest::Response, model_id: String) -> AssistantEventStream {
    let s = stream! {
        yield AssistantEvent::MessageStart;
        let mut bytes = resp.bytes_stream();
        // 字节缓冲：以 `\n` 切行，确保跨 chunk 的多字节 UTF-8 字符不被拆断丢弃。
        let mut buf: Vec<u8> = Vec::new();
        let mut blocks: Vec<BlockState> = Vec::new();
        let mut usage = Usage::default();
        let mut stop: Option<String> = None;

        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => { yield AssistantEvent::Error(LlmError::StreamInterrupted(e.to_string())); break; }
            };
            buf.extend_from_slice(chunk.as_ref());
            loop {
                let Some(line_bytes) = crate::drain_line(&mut buf) else { break };
                let line = String::from_utf8_lossy(&line_bytes).trim().to_string();
                if line.is_empty() { continue; }
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" { break; }
                let Ok(ev) = serde_json::from_str::<Ev>(data) else { continue };
                match ev.t.as_str() {
                    "message_start" => {
                        if let Some(u) = ev.message.as_ref().and_then(|m| m.usage.as_ref()) {
                            usage.input_tokens = u.input_tokens.unwrap_or(0);
                        }
                    }
                    "content_block_start" => {
                        if let (Some(idx), Some(cb)) = (ev.index, ev.content_block) {
                            while blocks.len() <= idx { blocks.push(BlockState::default()); }
                            let b = &mut blocks[idx];
                            if cb.t == "tool_use" {
                                b.is_tool = true;
                                b.tool_id = cb.id.clone();
                                b.tool_name = cb.name.clone();
                            }
                        }
                    }
                    "content_block_delta" => {
                        if let (Some(idx), Some(delta)) = (ev.index, ev.delta) {
                            while blocks.len() <= idx { blocks.push(BlockState::default()); }
                            let b = &mut blocks[idx];
                            match delta.t.as_deref() {
                                Some("text_delta") => {
                                    if let Some(t) = delta.text { b.text.push_str(&t); yield AssistantEvent::TextDelta(t); }
                                }
                                Some("input_json_delta") => {
                                    if let Some(pj) = delta.partial_json {
                                        b.tool_args.push_str(&pj);
                                        let id = b.tool_id.clone().unwrap_or_default();
                                        if !b.started && b.tool_id.is_some() && b.tool_name.is_some() {
                                            b.started = true;
                                            yield AssistantEvent::ToolCallStart {
                                                id: id.clone(),
                                                name: b.tool_name.clone().unwrap_or_default(),
                                            };
                                        }
                                        yield AssistantEvent::ToolCallDelta { id, partial_json: pj };
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(d) = &ev.delta {
                            if let Some(sr) = &d.stop_reason { stop = Some(sr.clone()); }
                        }
                        if let Some(u) = ev.usage { usage.output_tokens = u.output_tokens.unwrap_or(usage.output_tokens); }
                        yield AssistantEvent::Usage(usage.clone());
                    }
                    "message_stop" => {
                        let msg = build_message(&model_id, &blocks, &stop, &usage);
                        yield AssistantEvent::MessageEnd(msg);
                        return;
                    }
                    _ => {}
                }
            }
            // 防御无换行的超长行撑爆内存：drain_line 抽干完整行后 buf 仅余未完结尾段。
            if crate::line_buffer_too_long(&buf) {
                yield AssistantEvent::Error(LlmError::StreamInterrupted(
                    "SSE 行超过最大长度上限".into(),
                ));
                return;
            }
        }
        let msg = build_message(&model_id, &blocks, &stop, &usage);
        yield AssistantEvent::MessageEnd(msg);
    };
    Box::pin(s) as Pin<Box<dyn futures::Stream<Item = AssistantEvent> + Send>>
}

fn build_message(model_id: &str, blocks: &[BlockState], stop: &Option<String>, usage: &Usage) -> agent_core::AssistantMessage {
    let mut content = Vec::new();
    for b in blocks {
        if b.is_tool {
            let args = if b.tool_args.is_empty() {
                serde_json::Value::Object(Default::default())
            } else {
                serde_json::from_str(&b.tool_args).unwrap_or_else(|_| serde_json::Value::String(b.tool_args.clone()))
            };
            content.push(ContentBlock::ToolCall {
                id: b.tool_id.clone().unwrap_or_else(|| "tool".into()),
                name: b.tool_name.clone().unwrap_or_default(),
                arguments: args,
            });
        } else if !b.text.is_empty() {
            content.push(ContentBlock::Text { text: b.text.clone() });
        }
    }
    let stop_reason = stop.as_deref().map(|s| match s {
        "max_tokens" => StopReason::Length,
        "tool_use" | "tool_calls" => StopReason::ToolUse,
        _ => StopReason::Stop,
    });
    agent_core::AssistantMessage {
        content,
        usage: usage.clone(),
        model: model_id.to_string(),
        stop_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{Model, ProviderMessage};

    #[test]
    fn body_has_system_and_tools() {
        let req = CompletionRequest {
            model: Model::with_defaults("claude-3", "anthropic", Api::AnthropicMessages),
            system: vec!["be helpful".into()],
            messages: vec![ProviderMessage::User {
                content: vec![UserContent::Text { text: "hi".into() }],
            }],
            tools: vec![agent_core::ToolSpec::new("read_file", "r", serde_json::json!({"type":"object"}))],
            tool_choice: Some(ToolChoiceDirective::Hard(ToolChoice::Auto)),
            max_tokens: 16,
            temperature: None,
            thinking: None,
            cache_key: None,
        };
        let body = build_body(&req);
        // system 现为带 cache_control 的块数组
        assert_eq!(body["system"][0]["text"], "be helpful");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tool_choice"]["type"], "auto");
        assert_eq!(body["stream"], true);
    }
}
