//! GLM（智谱 / Z.ai）官方 API Provider。
//!
//! 忠实移植 Zoo-Code [`zai.ts`](../../third/Zoo-Code/src/api/providers/zai.ts) 与
//! [`zai-format.ts`](../../third/Zoo-Code/src/api/transform/zai-format.ts) 的 GLM 特化处理：
//!
//! 1. **thinking 开关**：GLM 思考模型（glm-4.7+ / glm-5+）思考**默认开启**，必须显式
//!    `thinking:{type:"enabled"|"disabled"}` 控制（[`is_glm_thinking_model`]）。
//! 2. **reasoning_effort 档位**：GLM 自有阶梯 none/minimal/low/medium/high/xhigh/max，
//!    默认 high、深度 max（[`normalize_glm_reasoning_effort`]）。
//! 3. **reasoning_content 字段**：流式 delta 含 `reasoning_content`（非 OpenAI 标准），
//!    提取为 thinking 事件，并回填到 [`ContentBlock::Thinking`] 以支持多轮 preserveReasoning。
//! 4. **Z.ai 消息格式**：assistant 的思考内容序列化为 `reasoning_content` 回传；tool 结果后
//!    的纯文本（如 environment_details）合并进最后一条 tool 消息，避免 user 消息导致
//!    GLM 丢弃 reasoning_content（[`convert_to_zai_format`]）。
//! 5. **缓存用量**：`prompt_tokens_details.cached_tokens`（GLM 支持 prompt 缓存）。
//! 6. **max_tokens**：用标准 `max_tokens`（GLM 不使用 DeepSeek 的 `max_completion_tokens`）。

use std::pin::Pin;

use agent_core::{
    Api, AssistantEvent, AssistantEventStream, CompletionRequest, ContentBlock, LlmError, LlmProvider,
    ProviderCallContext, ProviderMessage, StopReason, ToolChoice, ToolChoiceDirective, Usage, UserContent,
};
use async_stream::stream;
use futures::StreamExt;
use serde::Deserialize;

/// GLM 国际版（Z.ai）默认 base URL。
pub const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/paas/v4";

/// GLM Provider（智谱 / Z.ai）。
pub struct GlmProvider {
    client: reqwest::Client,
}

impl GlmProvider {
    /// 构造（复用外部 reqwest::Client）。
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

const SUPPORTED: &[Api] = &[Api::Zai];

#[async_trait::async_trait]
impl LlmProvider for GlmProvider {
    fn id(&self) -> &'static str {
        "glm"
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

        let body = build_body(&request);
        let model_id = request.model.id.clone();

        let resp = self
            .client
            .post(&url)
            .bearer_auth(ctx.api_key.as_deref().unwrap_or_default())
            .json(&body)
            .send()
            .await
            .map_err(|e| map_transport_error(e, "GLM"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = crate::read_error_body(resp).await;
            return Err(map_glm_error(status.as_u16(), &text));
        }
        Ok(parse_glm_stream(resp, model_id))
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 判定逻辑（移植 zai.ts）
// ──────────────────────────────────────────────────────────────────────────────

/// 是否为 GLM 思考模型（思考默认开启，需显式 thinking toggle）。
///
/// 参考 `zai.ts`：`isThinkingModel = Array.isArray(info.supportsReasoningEffort)`。
/// GLM 思考模型含 4.7 系列（flash/flashx）、5 系列（含 5-turbo）。
#[must_use]
pub fn is_glm_thinking_model(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    // 兼容 glm-4.7 / glm-4-7 两种命名；glm-5 覆盖 glm-5、glm-5.1、glm-5.2、glm-5-turbo。
    id.starts_with("glm-4.7") || id.starts_with("glm-4-7") || id.starts_with("glm-5")
}

/// 归一化 reasoning_effort 为 GLM 取值。
///
/// GLM 自有阶梯 none/minimal/low/medium/high/xhigh/max。GLM-5.2 默认 high，
/// 深度推理（budget ≥ 32_000）映射为 max。
#[must_use]
pub fn normalize_glm_reasoning_effort(budget_tokens: usize) -> &'static str {
    if budget_tokens >= 32_000 {
        "max"
    } else {
        "high"
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 请求体构建（特化）
// ──────────────────────────────────────────────────────────────────────────────

fn build_body(req: &CompletionRequest) -> serde_json::Value {
    let thinking_model = is_glm_thinking_model(&req.model.id);
    // Z.ai 格式：preserveReasoning（glm-5.1/5.2），合并 tool 结果后的文本避免 reasoning 丢失。
    let zai_messages = convert_to_zai_format(&req.system, &req.messages, thinking_model);

    let mut body = serde_json::json!({
        "model": req.model.id,
        "messages": zai_messages,
        "stream": true,
        "stream_options": { "include_usage": true },
        // GLM 用标准 max_tokens（非 DeepSeek 的 max_completion_tokens）。
        "max_tokens": req.max_tokens,
    });

    if let Some(temp) = req.temperature {
        body["temperature"] = serde_json::json!(temp);
    }

    // thinking 开关（GLM 思考模型特化）：
    // - 思考模型：思考默认开启，须显式 enabled/disabled 控制。
    //   * thinking config 为 Some → enabled + reasoning_effort
    //   * thinking config 为 None → disabled（不发 reasoning_effort）
    // - 非思考模型：完全不发 thinking 字段（走标准 OpenAI 行为）。
    if thinking_model {
        if let Some(thinking) = &req.thinking {
            body["thinking"] = serde_json::json!({ "type": "enabled" });
            body["reasoning_effort"] = serde_json::json!(normalize_glm_reasoning_effort(thinking.budget_tokens));
        } else {
            body["thinking"] = serde_json::json!({ "type": "disabled" });
        }
    }

    if !req.tools.is_empty() {
        let tools: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": crate::transform::normalize_tool_schema(&t.schema)
                }
            }))
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
        if let Some(tc) = &req.tool_choice {
            body["tool_choice"] = map_tool_choice(tc);
        }
        // GLM 默认允许并行工具调用
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
        // 软需求由循环处理（提醒 + 必要时升级），Provider 端保持 auto 以保护前缀缓存。
        ToolChoiceDirective::Soft(_) => serde_json::json!("auto"),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Z.ai 消息格式（移植 zai-format.ts：preserveReasoning + mergeToolResultText）
// ──────────────────────────────────────────────────────────────────────────────

/// 将 system + messages 转为 GLM Z.ai 格式。
///
/// 关键差异（vs 标准 OpenAI）：
/// - assistant 的 [`ContentBlock::Thinking`] 序列化为顶层 `reasoning_content` 回传（interleaved
///   thinking 的 preserveReasoning 所必需）。
/// - 思考模型下，tool 结果后紧跟的纯文本 user 消息合并进最后一条 tool 消息，避免 GLM 见到
///   user 消息而丢弃全部 reasoning_content。
pub fn convert_to_zai_format(
    system: &[String],
    messages: &[ProviderMessage],
    thinking_model: bool,
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();

    for sys in system {
        out.push(serde_json::json!({ "role": "system", "content": sys }));
    }

    for msg in messages {
        match msg {
            ProviderMessage::System(s) => {
                out.push(serde_json::json!({ "role": "system", "content": s }));
            }
            ProviderMessage::User { content } => {
                let has_image = content.iter().any(|c| matches!(c, UserContent::Image { .. }));
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
                    out.push(serde_json::json!({ "role": "user", "content": parts }));
                } else {
                    let text = join_user_text(content);
                    if text.is_empty() {
                        continue;
                    }
                    // preserveReasoning：思考模型下，紧跟在 tool 消息后的纯文本 user 消息，
                    // 合并进最后一条 tool 消息（避免 GLM 丢弃 reasoning_content）。
                    if thinking_model {
                        if let Some(last) = out.last_mut() {
                            if last.get("role").and_then(|r| r.as_str()) == Some("tool") {
                                let existing = last["content"].as_str().unwrap_or("").to_string();
                                last["content"] =
                                    serde_json::Value::String(format!("{existing}\n\n{text}"));
                                continue;
                            }
                        }
                    }
                    out.push(serde_json::json!({ "role": "user", "content": text }));
                }
            }
            ProviderMessage::Assistant { content } => {
                let text: String = content.iter().filter_map(|b| b.as_text()).collect::<Vec<_>>().join("");
                // reasoning_content：聚合 Thinking 块（preserveReasoning 回传）。
                let reasoning: String = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Thinking { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
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
                if !reasoning.is_empty() {
                    entry["reasoning_content"] = serde_json::Value::String(reasoning);
                }
                out.push(entry);
            }
            ProviderMessage::Tool {
                tool_call_id, content, ..
            } => {
                out.push(serde_json::json!({ "role": "tool", "tool_call_id": tool_call_id, "content": content }));
            }
        }
    }

    out
}

fn join_user_text(content: &[UserContent]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            UserContent::Text { text } => Some(text.as_str()),
            UserContent::Image { .. } => Some("[image]"),
        })
        .collect::<Vec<_>>()
        .join("")
}

// ──────────────────────────────────────────────────────────────────────────────
// reasoning_content 提取（复用 deepseek 的 extract 语义）
// ──────────────────────────────────────────────────────────────────────────────

/// 从流式 delta 提取 reasoning 文本。
///
/// 优先 `reasoning_content`（GLM / DeepSeek-R1 风格），回退 `reasoning`（OpenRouter 风格）。
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
// SSE 流解析（特化：reasoning_content + 缓存用量 + Thinking 回填）
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
    usage: Option<GlmUsage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: GlmDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// GLM delta：含标准 content/tool_calls + reasoning_content。
#[derive(Deserialize, Default)]
struct GlmDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<GlmToolCall>>,
}

#[derive(Deserialize)]
struct GlmToolCall {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<GlmFn>,
}

#[derive(Deserialize)]
struct GlmFn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// GLM 用量：含 prompt_tokens_details（缓存计量）。
#[derive(Deserialize, Default)]
struct GlmUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<GlmPromptDetails>,
}

/// GLM 缓存详情。
#[derive(Deserialize, Default)]
struct GlmPromptDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

fn parse_glm_stream(resp: reqwest::Response, model_id: String) -> AssistantEventStream {
    let s = stream! {
        yield AssistantEvent::MessageStart;
        let mut bytes_stream = resp.bytes_stream();
        // 字节缓冲：以 `\n` 切行，确保跨 chunk 的多字节 UTF-8 字符不被拆断丢弃。
        let mut buf: Vec<u8> = Vec::new();
        let mut text_buf = String::new();
        let mut reasoning_buf = String::new();
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
                        "GLM 流空闲超过 {} 秒未收到数据，判定上游静默",
                        crate::STREAM_IDLE_TIMEOUT.as_secs()
                    )));
                    break;
                }
            };
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    yield AssistantEvent::Error(LlmError::StreamInterrupted(format!("GLM 流中断: {e}")));
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
                    let msg = build_glm_message(&model_id, &text_buf, &reasoning_buf, &tool_calls, &finish, &usage_acc);
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
                    // reasoning_content（GLM 独有）：提取为 ThinkingDelta，并累积回填 Thinking 块。
                    let delta_json = serde_json::json!({
                        "reasoning_content": choice.delta.reasoning_content,
                        "reasoning": <Option<String>>::None,
                    });
                    if let Some(rc) = extract_reasoning_from_delta(&delta_json) {
                        reasoning_buf.push_str(&rc);
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

                // 用量（GLM 缓存计量）
                if let Some(u) = value.usage.as_ref() {
                    usage_acc.input_tokens = u.prompt_tokens;
                    usage_acc.output_tokens = u.completion_tokens;
                    if let Some(details) = &u.prompt_tokens_details {
                        usage_acc.cache_read_tokens = details.cached_tokens.unwrap_or(0);
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
        // 流自然结束（未见 [DONE]）
        let msg = build_glm_message(&model_id, &text_buf, &reasoning_buf, &tool_calls, &finish, &usage_acc);
        yield AssistantEvent::MessageEnd(msg);
    };
    Box::pin(s) as Pin<Box<dyn futures::Stream<Item = AssistantEvent> + Send>>
}

fn build_glm_message(
    model_id: &str,
    text_buf: &str,
    reasoning_buf: &str,
    tool_calls: &[ToolCallAccum],
    finish: &Option<String>,
    usage: &Usage,
) -> agent_core::AssistantMessage {
    let mut content = Vec::new();
    // reasoning 回填 Thinking 块（preserveReasoning：多轮 tool 调用续接时回传 reasoning_content）。
    if !reasoning_buf.is_empty() {
        content.push(ContentBlock::Thinking {
            text: reasoning_buf.to_string(),
            signature: None,
        });
    }
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
    // P2-P：content_filter → Error + sensitive（GLM/Z.ai OpenAI 兼容，含 content_filter）。
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
// 错误处理（移植 openai-error-handler，GLM 特化）
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

/// GLM HTTP 错误映射。
fn map_glm_error(status: u16, body: &str) -> LlmError {
    match status {
        401 | 403 => LlmError::Auth(format!("GLM 鉴权失败（{status}）: {body}")),
        429 => LlmError::RateLimit { retry_after_ms: 5000 },
        s if (500..600).contains(&s) => LlmError::Http {
            status,
            body: format!("GLM 服务端错误: {body}"),
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
    use agent_core::{Model, ProviderMessage, ThinkingConfig};

    #[test]
    fn thinking_model_detection() {
        assert!(is_glm_thinking_model("glm-4.7"));
        assert!(is_glm_thinking_model("glm-4.7-flash"));
        assert!(is_glm_thinking_model("glm-4.7-flashx"));
        assert!(is_glm_thinking_model("GLM-5"));
        assert!(is_glm_thinking_model("glm-5.1"));
        assert!(is_glm_thinking_model("glm-5.2"));
        assert!(is_glm_thinking_model("glm-5-turbo"));
        assert!(!is_glm_thinking_model("glm-4.6"));
        assert!(!is_glm_thinking_model("glm-4.5"));
        assert!(!is_glm_thinking_model("gpt-4o"));
    }

    #[test]
    fn reasoning_effort_normalization() {
        assert_eq!(normalize_glm_reasoning_effort(1000), "high");
        assert_eq!(normalize_glm_reasoning_effort(31_999), "high");
        assert_eq!(normalize_glm_reasoning_effort(32_000), "max");
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
    fn build_body_thinking_enabled_sends_toggle_and_effort() {
        let req = CompletionRequest {
            model: Model::with_defaults("glm-5.2", "zai", Api::Zai),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 4096,
            temperature: Some(0.6),
            thinking: Some(ThinkingConfig::new(1000)),
            cache_key: None,
        };
        let body = build_body(&req);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["max_tokens"], 4096);
        // temperature 为 f32，经 serde_json 序列化为 f64 会引入精度，按数值比较。
        assert!((body["temperature"].as_f64().unwrap_or(0.0) - 0.6).abs() < 1e-5);
    }

    #[test]
    fn build_body_thinking_disabled_explicit() {
        let req = CompletionRequest {
            model: Model::with_defaults("glm-5.2", "zai", Api::Zai),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            thinking: None,
            cache_key: None,
        };
        let body = build_body(&req);
        // GLM 思考模型默认开启思考，禁用时须显式 disabled 且不发 reasoning_effort。
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn build_body_max_effort() {
        let req = CompletionRequest {
            model: Model::with_defaults("glm-5.2", "zai", Api::Zai),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            thinking: Some(ThinkingConfig::new(40_000)),
            cache_key: None,
        };
        let body = build_body(&req);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "max");
    }

    #[test]
    fn build_body_non_thinking_model_omits_toggle() {
        let req = CompletionRequest {
            model: Model::with_defaults("glm-4.6", "zai", Api::Zai),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            thinking: Some(ThinkingConfig::new(1000)),
            cache_key: None,
        };
        let body = build_body(&req);
        // 非思考模型不识别 thinking 字段。
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn zai_format_serializes_reasoning_content() {
        let messages = vec![ProviderMessage::Assistant {
            content: vec![
                ContentBlock::Thinking { text: "先分析需求".into(), signature: None },
                ContentBlock::Text { text: "我来实现".into() },
                ContentBlock::ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path":"a.rs"}),
                },
            ],
        }];
        let out = convert_to_zai_format(&[], &messages, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[0]["reasoning_content"], "先分析需求");
        assert_eq!(out[0]["content"], "我来实现");
        assert_eq!(out[0]["tool_calls"][0]["id"], "call_1");
        assert!(out[0]["tool_calls"][0]["function"]["arguments"].is_string());
    }

    #[test]
    fn zai_format_merges_post_tool_text_when_thinking() {
        // preserveReasoning：tool 结果后的纯文本 user 消息合并进最后 tool 消息。
        let messages = vec![
            ProviderMessage::Assistant {
                content: vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path":"x"}),
                }],
            },
            ProviderMessage::Tool {
                tool_call_id: "c1".into(),
                content: "file body".into(),
                is_error: false,
                images: vec![],
            },
            ProviderMessage::User {
                content: vec![UserContent::Text { text: "environment_details".into() }],
            },
        ];
        let out = convert_to_zai_format(&[], &messages, true);
        // 三条 → 两条（user 文本并入最后 tool 消息，不产生独立 user 消息）。
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[1]["role"], "tool");
        let merged = out[1]["content"].as_str().unwrap();
        assert!(merged.contains("file body"));
        assert!(merged.contains("environment_details"));
    }

    #[test]
    fn zai_format_keeps_post_tool_user_when_not_thinking() {
        // 非思考模型不合并：保留独立 user 消息。
        let messages = vec![
            ProviderMessage::Tool {
                tool_call_id: "c1".into(),
                content: "body".into(),
                is_error: false,
                images: vec![],
            },
            ProviderMessage::User {
                content: vec![UserContent::Text { text: "note".into() }],
            },
        ];
        let out = convert_to_zai_format(&[], &messages, false);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["role"], "user");
    }

    #[test]
    fn build_message_backfills_thinking_block() {
        let mut tc = ToolCallAccum::default();
        tc.id = Some("call_1".into());
        tc.name = Some("read_file".into());
        tc.args = r#"{"path":"a.rs"}"#.into();
        let msg = build_glm_message(
            "glm-5.2",
            "done",
            "推理过程",
            &[tc],
            &Some("tool_calls".into()),
            &Usage::default(),
        );
        // 顺序：Thinking → Text → ToolCall
        assert!(matches!(msg.content[0], ContentBlock::Thinking { .. }));
        assert!(matches!(msg.content[1], ContentBlock::Text { .. }));
        let ContentBlock::Thinking { text, .. } = &msg.content[0] else {
            panic!("应为 Thinking");
        };
        assert_eq!(text, "推理过程");
        assert_eq!(msg.stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn glm_error_mapping() {
        assert!(matches!(map_glm_error(401, "bad key"), LlmError::Auth(_)));
        assert!(matches!(map_glm_error(429, "slow down"), LlmError::RateLimit { .. }));
        match map_glm_error(500, "server") {
            LlmError::Http { status, .. } => assert_eq!(status, 500),
            _ => panic!("应为 Http"),
        }
    }

    #[test]
    fn zai_format_system_and_user_basic() {
        let messages = vec![ProviderMessage::User {
            content: vec![UserContent::Text { text: "hi".into() }],
        }];
        let out = convert_to_zai_format(&["be helpful".into()], &messages, true);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[0]["content"], "be helpful");
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[1]["content"], "hi");
    }
}
