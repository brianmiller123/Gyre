//! DeepSeek 独立 Provider 路由模块。
//!
//! DeepSeek 的 API 基于但区别于标准 OpenAI Completions，本模块忠实移植 Zoo-Code
//! [`deepseek.ts`](../../third/Zoo-Code/src/api/providers/deepseek.ts) 的特化处理：
//!
//! 1. **reasoning_content 字段**：流式 delta 含 `reasoning_content`（非 OpenAI 标准），
//!    提取为 thinking 事件（[`extract_reasoning_from_delta`]）。
//! 2. **R1 消息格式**：DeepSeek-R1 不支持连续同角色消息，需合并（[`convert_to_r1_format`]）。
//! 3. **thinking 模式参数**：`thinking: {type:"enabled"}` + `reasoning_effort: "high"|"max"`
//!    （非标准 reasoning_effort 取值；[`normalize_reasoning_effort`]）。
//! 4. **温度限制**：thinking 模式下**不发送 temperature**（DeepSeek-R1 不支持）。
//! 5. **缓存用量**：`prompt_tokens_details.cached_tokens` / `cache_miss_tokens`（DeepSeek 特有）。
//! 6. **max_completion_tokens**：用 `max_completion_tokens` 而非 `max_tokens`。

use std::pin::Pin;

use agent_core::{
    Api, AssistantEvent, AssistantEventStream, CompletionRequest, ContentBlock, LlmError, LlmProvider,
    ProviderCallContext, ProviderMessage, StopReason, ThinkingConfig, ToolChoice, ToolChoiceDirective, Usage,
};
use async_stream::stream;
use futures::StreamExt;
use serde::Deserialize;

/// DeepSeek 默认 base URL。
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

/// 支持 thinking toggle 的 V4 模型集合。
const V4_THINKING_MODELS: &[&str] = &["deepseek-v4-flash", "deepseek-v4-pro"];

/// DeepSeek Provider。
pub struct DeepSeekProvider {
    client: reqwest::Client,
}

impl DeepSeekProvider {
    /// 构造（复用外部 reqwest::Client）。
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

const SUPPORTED: &[Api] = &[Api::DeepSeek];

#[async_trait::async_trait]
impl LlmProvider for DeepSeekProvider {
    fn id(&self) -> &'static str {
        "deepseek"
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
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/');
        let url = format!("{base}/chat/completions");
        let is_thinking = is_thinking_model(&request.model.id, request.thinking.as_ref());

        let body = build_body(&request, is_thinking);
        let model_id = request.model.id.clone();

        let resp = self
            .client
            .post(&url)
            .bearer_auth(ctx.api_key.as_deref().unwrap_or_default())
            .json(&body)
            .send()
            .await
            .map_err(|e| map_transport_error(e, "DeepSeek"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = crate::read_error_body(resp).await;
            return Err(map_deepseek_error(status.as_u16(), &text));
        }
        Ok(parse_deepseek_stream(resp, model_id))
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 判定逻辑（移植 deepseek.ts）
// ──────────────────────────────────────────────────────────────────────────────

/// 是否为 DeepSeek thinking 模型。
///
/// `deepseek-reasoner` 永远启用思考（官方 reasoner 内建思考，自动输出 reasoning_content）；
/// V4 系列需 `thinking:{type:"enabled"}` toggle（由 ThinkingConfig 触发）。
fn is_thinking_model(model_id: &str, thinking: Option<&ThinkingConfig>) -> bool {
    if model_id == "deepseek-reasoner" {
        return true;
    }
    // V4 模型：仅当显式启用 thinking 时触发
    V4_THINKING_MODELS.contains(&model_id) && thinking.is_some()
}

/// 归一化 reasoning_effort 为 DeepSeek 取值（"high" | "max"）。
///
/// DeepSeek 将 low/medium 映射为 high，xhigh 映射为 max。
#[must_use]
pub fn normalize_reasoning_effort(budget_tokens: usize) -> &'static str {
    // 按 budget 分档映射到 DeepSeek 的 high/max
    if budget_tokens >= 32_000 {
        "max"
    } else {
        "high"
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 请求体构建（特化）
// ──────────────────────────────────────────────────────────────────────────────

fn build_body(req: &CompletionRequest, is_thinking: bool) -> serde_json::Value {
    // R1 格式：合并连续同角色消息（DeepSeek-R1 不支持连续同角色）
    let r1_messages = convert_to_r1_format(&req.system, &req.messages, is_thinking);

    let mut body = serde_json::json!({
        "model": req.model.id,
        "messages": r1_messages,
        "stream": true,
        "stream_options": { "include_usage": true },
        // DeepSeek 用 max_completion_tokens 而非 max_tokens
        "max_completion_tokens": req.max_tokens,
    });

    // 温度：thinking 模式下不发（DeepSeek-R1 不支持）
    if !is_thinking {
        if let Some(temp) = req.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
    }

    // thinking 模式参数：
    // - deepseek-reasoner：官方 reasoner 自动思考，**不发** thinking/reasoning_effort 字段
    // - V4 模型：需 thinking:{type:"enabled"} toggle（由 ThinkingConfig 触发）
    if is_thinking && V4_THINKING_MODELS.contains(&req.model.id.as_str()) {
        body["thinking"] = serde_json::json!({ "type": "enabled" });
        if let Some(thinking) = &req.thinking {
            let effort = normalize_reasoning_effort(thinking.budget_tokens);
            body["reasoning_effort"] = serde_json::json!(effort);
        }
    }

    if !req.tools.is_empty() {
        let tools: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| serde_json::json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.schema}}))
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
        if let Some(tc) = &req.tool_choice {
            body["tool_choice"] = map_tool_choice(tc);
        }
        // DeepSeek 默认允许并行工具调用
        body["parallel_tool_calls"] = serde_json::json!(true);
    }

    // per-model 额外请求体字段（如 vLLM chat_template_kwargs）合并到顶层
    crate::merge_extra_body(&mut body, req.model.extra_body.as_ref());
    body
}

fn map_tool_choice(directive: &ToolChoiceDirective) -> serde_json::Value {
    match directive {
        ToolChoiceDirective::Hard(ToolChoice::Auto) => serde_json::json!("auto"),
        ToolChoiceDirective::Hard(ToolChoice::None) => serde_json::json!("none"),
        ToolChoiceDirective::Hard(ToolChoice::Any) | ToolChoiceDirective::Hard(ToolChoice::Required) => {
            serde_json::json!("required")
        }
        ToolChoiceDirective::Hard(ToolChoice::Function { name }) => {
            serde_json::json!({ "type": "function", "function": { "name": name } })
        }
        ToolChoiceDirective::Soft(_) => serde_json::json!("auto"),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// R1 消息格式（合并连续同角色）
// ──────────────────────────────────────────────────────────────────────────────

/// 将 system + messages 转为 DeepSeek R1 格式：合并连续同角色消息。
///
/// DeepSeek-R1 不支持连续同角色消息。对于 thinking 模型，`merge_tool_result_text`
/// 将 tool_result 后的文本合并到最后一条 tool 消息（避免创建 user 消息导致 reasoning 丢失）。
pub fn convert_to_r1_format(
    system: &[String],
    messages: &[ProviderMessage],
    merge_tool_result_text: bool,
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut last_role: Option<String> = None;

    // system 作为首条 user 消息
    if !system.is_empty() {
        let sys_text = system.join("\n\n");
        out.push(serde_json::json!({"role":"user","content":sys_text}));
        last_role = Some("user".to_string());
    }

    for m in messages {
        match m {
            ProviderMessage::System(s) => {
                // system 合并到 user（或新 user）
                push_or_merge(&mut out, &mut last_role, "user", s, merge_tool_result_text);
            }
            ProviderMessage::User { content } => {
                let text: String = content
                    .iter()
                    .filter_map(|c| match c {
                        agent_core::UserContent::Text { text } => Some(text.as_str()),
                        agent_core::UserContent::Image { .. } => Some("[image]"),
                    })
                    .collect::<Vec<_>>()
                    .join("");
                push_or_merge(&mut out, &mut last_role, "user", &text, merge_tool_result_text);
            }
            ProviderMessage::Assistant { content } => {
                let mut entry = serde_json::json!({"role":"assistant"});
                let text: String = content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("");
                if !text.is_empty() {
                    entry["content"] = serde_json::Value::String(text);
                }
                let tool_calls: Vec<serde_json::Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall { id, name, arguments, .. } => Some(serde_json::json!({
                            "id": id, "type":"function","function":{"name":name,"arguments":serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string())}
                        })),
                        _ => None,
                    })
                    .collect();
                if !tool_calls.is_empty() {
                    entry["tool_calls"] = serde_json::Value::Array(tool_calls);
                }
                // 重置 last_role：assistant 总是新条目
                out.push(entry);
                last_role = Some("assistant".to_string());
            }
            ProviderMessage::Tool {
                tool_call_id, content, ..
            } => {
                out.push(serde_json::json!({"role":"tool","tool_call_id":tool_call_id,"content":content}));
                last_role = Some("tool".to_string());
            }
        }
    }

    out
}

/// 推送或合并：若上一条同 role 则合并文本（R1 要求无连续同 role）。
fn push_or_merge(
    out: &mut Vec<serde_json::Value>,
    last_role: &mut Option<String>,
    role: &str,
    text: &str,
    _merge_tool_result_text: bool,
) {
    if last_role.as_deref() == Some(role) {
        if let Some(last) = out.last_mut() {
            let existing = last["content"].as_str().unwrap_or("").to_string();
            last["content"] = serde_json::Value::String(format!("{existing}\n{text}"));
            return;
        }
    }
    out.push(serde_json::json!({"role":role,"content":text}));
    *last_role = Some(role.to_string());
}

// ──────────────────────────────────────────────────────────────────────────────
// reasoning_content 提取（移植 extract-reasoning.ts）
// ──────────────────────────────────────────────────────────────────────────────

/// 从流式 delta 提取 reasoning 文本。
///
/// 优先 `reasoning_content`（DeepSeek-R1 / QwQ 风格），回退 `reasoning`（OpenRouter 风格）。
#[must_use]
pub fn extract_reasoning_from_delta(delta: &serde_json::Value) -> Option<String> {
    if let Some(rc) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
        if !rc.is_empty() {
            return Some(rc.to_string());
        }
    }
    if let Some(r) = delta.get("reasoning").and_then(|v| v.as_str()) {
        if !r.is_empty() {
            return Some(r.to_string());
        }
    }
    None
}

// ──────────────────────────────────────────────────────────────────────────────
// SSE 流解析（特化：reasoning_content + 缓存用量）
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
    usage: Option<DeepSeekUsage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: DeepSeekDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// DeepSeek delta：含标准 content/tool_calls + reasoning_content。
#[derive(Deserialize, Default)]
struct DeepSeekDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<DeepSeekToolCall>>,
}

#[derive(Deserialize)]
struct DeepSeekToolCall {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeepSeekFn>,
}

#[derive(Deserialize)]
struct DeepSeekFn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// DeepSeek 用量：含 prompt_tokens_details（缓存计量）。
#[derive(Deserialize, Default)]
struct DeepSeekUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<DeepSeekPromptDetails>,
}

/// DeepSeek 缓存详情。
#[derive(Deserialize, Default)]
struct DeepSeekPromptDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    cache_miss_tokens: Option<u64>,
}

fn parse_deepseek_stream(resp: reqwest::Response, model_id: String) -> AssistantEventStream {
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
            // 仅当真正静默超过阈值（上游挂起/网络中断）才判超时（替代整条请求总超时，
            // 避免慢速 LLM 长流被误杀、收不到 `[DONE]` 而误判「未收到结束标记」）。
            let chunk_res = match tokio::time::timeout(
                crate::STREAM_IDLE_TIMEOUT,
                bytes_stream.next(),
            )
            .await
            {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(_) => {
                    yield AssistantEvent::Error(LlmError::StreamInterrupted(format!(
                        "DeepSeek 流空闲超过 {} 秒未收到数据，判定上游静默",
                        crate::STREAM_IDLE_TIMEOUT.as_secs()
                    )));
                    break;
                }
            };
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    yield AssistantEvent::Error(LlmError::StreamInterrupted(format!("DeepSeek 流中断: {e}")));
                    break;
                }
            };
            buf.extend_from_slice(chunk.as_ref());
            loop {
                let Some(line_bytes) = crate::drain_line(&mut buf) else { break };
                let line = String::from_utf8_lossy(&line_bytes).trim().to_string();
                if line.is_empty() { continue; }
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" {
                    let msg = build_deepseek_message(&model_id, &text_buf, &tool_calls, &finish, &usage_acc);
                    yield AssistantEvent::MessageEnd(msg);
                    return;
                }
                let Ok(value) = serde_json::from_str::<ChunkValue>(data) else { continue };

                for choice in &value.choices {
                    // 文本内容
                    if let Some(text) = choice.delta.content.clone() {
                        if !text.is_empty() {
                            text_buf.push_str(&text);
                            yield AssistantEvent::TextDelta(text);
                        }
                    }
                    // reasoning_content（DeepSeek 独有）：复用提取逻辑
                    let delta_json = serde_json::json!({
                        "reasoning_content": choice.delta.reasoning_content,
                        "reasoning": <Option<String>>::None,
                    });
                    if let Some(rc) = extract_reasoning_from_delta(&delta_json) {
                        yield AssistantEvent::ThinkingDelta(rc);
                    }
                    // 工具调用
                    if let Some(tcs) = choice.delta.tool_calls.as_ref() {
                        for tc in tcs {
                            while tool_calls.len() <= tc.index {
                                tool_calls.push(ToolCallAccum::default());
                            }
                            let acc = &mut tool_calls[tc.index];
                            if let Some(id) = tc.id.clone() { acc.id = Some(id); }
                            if let Some(name) = tc.function.as_ref().and_then(|f| f.name.clone()) {
                                acc.name = Some(name);
                            }
                            let should_start = !acc.started && acc.id.is_some() && acc.name.is_some();
                            let start_evt = should_start.then(|| {
                                acc.started = true;
                                (acc.id.clone().unwrap_or_default(), acc.name.clone().unwrap_or_default())
                            });
                            let delta_evt = tc.function.as_ref()
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

                // 用量（DeepSeek 缓存计量）
                if let Some(u) = value.usage.as_ref() {
                    usage_acc.input_tokens = u.prompt_tokens;
                    usage_acc.output_tokens = u.completion_tokens;
                    if let Some(details) = &u.prompt_tokens_details {
                        usage_acc.cache_read_tokens = details.cached_tokens.unwrap_or(0);
                        usage_acc.cache_write_tokens = details.cache_miss_tokens.unwrap_or(0);
                    }
                    yield AssistantEvent::Usage(usage_acc.clone());
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
        let msg = build_deepseek_message(&model_id, &text_buf, &tool_calls, &finish, &usage_acc);
        yield AssistantEvent::MessageEnd(msg);
    };
    Box::pin(s) as Pin<Box<dyn futures::Stream<Item = AssistantEvent> + Send>>
}

fn build_deepseek_message(
    model_id: &str,
    text_buf: &str,
    tool_calls: &[ToolCallAccum],
    finish: &Option<String>,
    usage: &Usage,
) -> agent_core::AssistantMessage {
    let mut content = Vec::new();
    if !text_buf.is_empty() {
        content.push(ContentBlock::Text { text: text_buf.to_string() });
    }
    for (i, tc) in tool_calls.iter().enumerate() {
        let id = tc.id.clone().unwrap_or_else(|| format!("call_{i}"));
        let name = tc.name.clone().unwrap_or_default();
        let arguments = if tc.args.is_empty() {
            serde_json::Value::Object(Default::default())
        } else {
            serde_json::from_str(&tc.args).unwrap_or_else(|_| serde_json::Value::String(tc.args.clone()))
        };
        content.push(ContentBlock::ToolCall { id, name, arguments });
    }
    // P2-P：content_filter → Error + sensitive（DeepSeek OpenAI 兼容，含 content_filter）。
    let (stop_reason, stop_details) = match finish.as_deref() {
        Some("length") => (Some(StopReason::Length), None),
        Some("tool_calls") | Some("function_call") => (Some(StopReason::ToolUse), None),
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

// ──────────────────────────────────────────────────────────────────────────────
// 错误处理（移植 openai-error-handler，DeepSeek 特化）
// ──────────────────────────────────────────────────────────────────────────────

fn map_transport_error(e: reqwest::Error, provider: &str) -> LlmError {
    if e.is_timeout() {
        LlmError::Transport(format!("{provider} 请求超时: {e}"))
    } else if e.is_connect() {
        LlmError::Transport(format!("{provider} 连接失败: {e}"))
    } else {
        LlmError::Transport(format!("{provider} 网络错误: {e}"))
    }
}

/// DeepSeek HTTP 错误映射。
fn map_deepseek_error(status: u16, body: &str) -> LlmError {
    match status {
        401 | 403 => LlmError::Auth(format!("DeepSeek 鉴权失败（{status}）: {body}")),
        429 => {
            // 尝试解析 retry-after
            LlmError::RateLimit { retry_after_ms: 5000 }
        }
        s if (500..600).contains(&s) => LlmError::Http {
            status,
            body: format!("DeepSeek 服务端错误: {body}"),
        },
        _ => LlmError::Http { status, body: body.to_string() },
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 测试
// ──────────────────────────────────────────────────────────────────────────────

// Provider 自荐注册统一集中在 plugin.rs（collect_providers 用共享 client 构造）。

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{Model, ProviderMessage, UserContent};

    #[test]
    fn reasoning_effort_normalization() {
        assert_eq!(normalize_reasoning_effort(1000), "high");
        assert_eq!(normalize_reasoning_effort(40_000), "max");
    }

    #[test]
    fn extract_reasoning_prefers_reasoning_content() {
        let delta = serde_json::json!({"reasoning_content":"thinking...","reasoning":"fallback"});
        assert_eq!(extract_reasoning_from_delta(&delta), Some("thinking...".into()));
    }

    #[test]
    fn extract_reasoning_falls_back_to_reasoning() {
        let delta = serde_json::json!({"reasoning":"fallback"});
        assert_eq!(extract_reasoning_from_delta(&delta), Some("fallback".into()));
    }

    #[test]
    fn extract_reasoning_none_when_empty() {
        let delta = serde_json::json!({"reasoning_content":"","reasoning":""});
        assert_eq!(extract_reasoning_from_delta(&delta), None);
    }

    #[test]
    fn r1_format_merges_consecutive_user() {
        let messages = vec![
            ProviderMessage::User { content: vec![UserContent::Text { text: "hello".into() }] },
            ProviderMessage::User { content: vec![UserContent::Text { text: "world".into() }] },
        ];
        // 不带 system，验证两条连续 user 合并为 1 条
        let r1 = convert_to_r1_format(&[], &messages, false);
        assert_eq!(r1.len(), 1);
        let content = r1[0]["content"].as_str().unwrap();
        assert!(content.contains("hello"));
        assert!(content.contains("world"));
    }

    #[test]
    fn r1_format_preserves_alternating_roles() {
        let messages = vec![
            ProviderMessage::User { content: vec![UserContent::Text { text: "q".into() }] },
            ProviderMessage::Assistant { content: vec![ContentBlock::Text { text: "a".into() }] },
            ProviderMessage::User { content: vec![UserContent::Text { text: "q2".into() }] },
        ];
        let r1 = convert_to_r1_format(&[], &messages, false);
        assert_eq!(r1.len(), 3);
        assert_eq!(r1[0]["role"], "user");
        assert_eq!(r1[1]["role"], "assistant");
        assert_eq!(r1[2]["role"], "user");
    }

    #[test]
    fn build_body_non_thinking_sends_temperature() {
        let req = CompletionRequest {
            model: Model::with_defaults("deepseek-chat", "deepseek", Api::DeepSeek),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 1024,
            temperature: Some(0.5),
            thinking: None,
            cache_key: None,
        };
        let body = build_body(&req, false);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["max_completion_tokens"], 1024);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn build_body_thinking_omits_temperature() {
        let req = CompletionRequest {
            model: Model::with_defaults("deepseek-reasoner", "deepseek", Api::DeepSeek),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 2048,
            temperature: Some(0.7),
            thinking: Some(ThinkingConfig::new(40_000)),
            cache_key: None,
        };
        let body = build_body(&req, true);
        // deepseek-reasoner 自动思考：不发 temperature、不发 thinking/reasoning_effort 字段
        assert!(body.get("temperature").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn build_body_v4_thinking_toggle() {
        let req = CompletionRequest {
            model: Model::with_defaults("deepseek-v4-pro", "deepseek", Api::DeepSeek),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            thinking: Some(ThinkingConfig::new(1000)),
            cache_key: None,
        };
        let body = build_body(&req, true);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn deepseek_error_mapping() {
        assert!(matches!(map_deepseek_error(401, "bad key"), LlmError::Auth(_)));
        assert!(matches!(map_deepseek_error(429, "slow down"), LlmError::RateLimit { .. }));
        match map_deepseek_error(500, "server") {
            LlmError::Http { status, .. } => assert_eq!(status, 500),
            _ => panic!("应为 Http"),
        }
    }

    #[test]
    fn tool_call_serializes_id_and_string_arguments() {
        use agent_core::{ContentBlock, ProviderMessage, UserContent};
        let req = CompletionRequest {
            model: Model::with_defaults("deepseek-chat", "deepseek", Api::DeepSeek),
            system: vec![],
            messages: vec![
                ProviderMessage::User { content: vec![UserContent::Text { text: "hi".into() }] },
                ProviderMessage::Assistant {
                    content: vec![
                        ContentBlock::Text { text: "calling".into() },
                        ContentBlock::ToolCall {
                            id: "call_1".into(),
                            name: "list_files".into(),
                            arguments: serde_json::json!({"path":"."}),
                        },
                    ],
                },
                ProviderMessage::Tool { tool_call_id: "call_1".into(), content: "result".into(), is_error: false, images: vec![] },
            ],
            tools: vec![],
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            thinking: None,
            cache_key: None,
        };
        let body = build_body(&req, false);
        let msgs = body["messages"].as_array().unwrap();
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").expect("应有 assistant");
        let tc = &assistant["tool_calls"][0];
        assert_eq!(tc["id"], "call_1", "tool_call 必须含 id");
        assert!(tc["function"]["arguments"].is_string(), "arguments 必须为 JSON 字符串");
        assert_eq!(tc["function"]["name"], "list_files");
    }
}
