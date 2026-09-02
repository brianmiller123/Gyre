//! Google Generative AI（Gemini）流式适配器。
//!
//! 端点：`POST {base}/v1beta/models/{model}:streamGenerateContent?alt=sse`，鉴权 header
//! `x-goog-api-key`。请求映射要点：
//!
//! 1. **systemInstruction**：`request.system` 与 `ProviderMessage::System` 合并为
//!    `systemInstruction.parts[0].text`（`\n\n` 连接）。
//! 2. **contents**：user → `role:"user"`（text part / inlineData part）；assistant →
//!    `role:"model"`（text part / functionCall part，thinking 块不回传）；tool 结果 →
//!    `role:"user"` 的 `functionResponse` part（name 经 assistant 历史回查 id→name）。
//! 3. **thinkingConfig**：仅对思考能力模型（id 含 `gemini-2.5`/`gemini-3`，
//!    [`is_gemini_thinking_model`]）发送
//!    `generationConfig.thinkingConfig={includeThoughts,thinkingBudget}`；其余模型忽略
//!    （发送会被 API 拒绝），见 doc 注明。
//! 4. **SSE 解析**：`candidates[0].content.parts[]` 的 text 增量（`thought:true` →
//!    thinking 增量）、`functionCall` → 工具调用事件（Gemini 单帧完整给出 args，
//!    非增量拼装）；`finishReason` → [`StopReason`]；`usageMetadata` → [`Usage`]。
//! 5. **用量口径**：`output_tokens = candidatesTokenCount + thoughtsTokenCount`
//!    （思考 token 按输出计费，对齐 oh-my-pi 所用 Vercel AI SDK google provider 的
//!    completionTokens 口径；[`Usage`] 暂无独立 reasoning 字段故并入输出）。

use std::collections::HashMap;
use std::pin::Pin;

use agent_core::{
    Api, AssistantEvent, AssistantEventStream, CompletionRequest, ContentBlock, LlmError,
    LlmProvider, ProviderCallContext, ProviderMessage, StopReason, ToolSpec, Usage, UserContent,
};
use async_stream::stream;
use futures::StreamExt;
use serde::Deserialize;

/// Gemini 默认 base URL（Google AI Studio / Generative Language API）。
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// Google Generative AI（Gemini）Provider。
pub struct GeminiProvider {
    client: reqwest::Client,
}

impl GeminiProvider {
    /// 构造（复用外部 `reqwest::Client`）。
    #[must_use]
    pub const fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

const SUPPORTED: &[Api] = &[Api::GoogleGenerativeAi];

#[async_trait::async_trait]
impl LlmProvider for GeminiProvider {
    fn id(&self) -> &'static str {
        "google-generative-ai"
    }
    fn supports(&self) -> &[Api] {
        SUPPORTED
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        ctx: &ProviderCallContext,
    ) -> Result<AssistantEventStream, LlmError> {
        let url = build_url(ctx.base_url.as_deref(), &request.model.id);
        let body = build_body(&request);
        let model_id = request.model.id.clone();

        let resp = self
            .client
            .post(&url)
            .header("x-goog-api-key", ctx.api_key.as_deref().unwrap_or_default())
            .json(&body)
            .send()
            .await
            .map_err(|e| map_transport_error(e, "Gemini"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = crate::read_error_body(resp).await;
            return Err(map_gemini_error(status.as_u16(), &text));
        }
        Ok(parse_gemini_stream(resp, model_id))
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 判定与 URL 构造
// ──────────────────────────────────────────────────────────────────────────────

/// 是否为 Gemini 思考能力模型（支持 thinkingConfig）。
///
/// Gemini 2.5 系列（pro/flash/flash-lite）与 Gemini 3 系列支持
/// `generationConfig.thinkingConfig`；2.0 及更早模型发送该字段会被 API 拒绝，
/// 故 thinking 配置对其静默忽略。
#[must_use]
pub fn is_gemini_thinking_model(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    let id = id.strip_prefix("models/").unwrap_or(id.as_str());
    id.contains("gemini-2.5") || id.contains("gemini-3")
}

/// 构造流式端点 URL。
///
/// `{base}/v1beta/models/{model}:streamGenerateContent?alt=sse`；容忍 base 已带
/// `/v1beta` 尾缀与尾斜杠，容忍 model id 已带 `models/` 前缀（均不重复拼接）。
#[must_use]
pub fn build_url(base: Option<&str>, model_id: &str) -> String {
    let base = base
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/')
        .trim_end_matches("/v1beta");
    let model = model_id.strip_prefix("models/").unwrap_or(model_id);
    format!("{base}/v1beta/models/{model}:streamGenerateContent?alt=sse")
}

// ──────────────────────────────────────────────────────────────────────────────
// 请求体构建
// ──────────────────────────────────────────────────────────────────────────────

fn build_body(req: &CompletionRequest) -> serde_json::Value {
    // system：合并 request.system 与 ProviderMessage::System（同 anthropic 适配器）。
    let mut system_parts: Vec<String> = req.system.clone();
    // tool_call_id → 工具名回查表：Gemini functionResponse 需要函数名而非调用 id。
    let mut tool_names: HashMap<&str, &str> = HashMap::new();
    for m in &req.messages {
        match m {
            ProviderMessage::System(s) => system_parts.push(s.clone()),
            ProviderMessage::Assistant { content } => {
                for b in content {
                    if let ContentBlock::ToolCall { id, name, .. } = b {
                        tool_names.insert(id.as_str(), name.as_str());
                    }
                }
            }
            _ => {}
        }
    }

    let mut contents: Vec<serde_json::Value> = Vec::new();
    for m in &req.messages {
        match m {
            ProviderMessage::System(_) => {} // 已并入 systemInstruction
            ProviderMessage::User { content } => {
                let parts: Vec<serde_json::Value> = content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text { text } if !text.is_empty() => {
                            Some(serde_json::json!({ "text": text }))
                        }
                        UserContent::Image { mime, data } => Some(serde_json::json!({
                            "inlineData": { "mimeType": mime, "data": data }
                        })),
                        UserContent::Text { .. } => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    contents.push(serde_json::json!({ "role": "user", "parts": parts }));
                }
            }
            ProviderMessage::Assistant { content } => {
                let parts: Vec<serde_json::Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            Some(serde_json::json!({ "text": text }))
                        }
                        // 工具调用 → functionCall part（args 为完整 JSON 对象直传）。
                        ContentBlock::ToolCall { name, arguments, .. } => {
                            Some(serde_json::json!({ "functionCall": { "name": name, "args": arguments } }))
                        }
                        // thinking 块跳过：Gemini 无 preserveReasoning 等价物，回传会被拒。
                        ContentBlock::Thinking { .. } => None,
                        ContentBlock::Text { .. } => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    contents.push(serde_json::json!({ "role": "model", "parts": parts }));
                }
            }
            ProviderMessage::Tool {
                tool_call_id,
                content,
                ..
            } => {
                let name = tool_names
                    .get(tool_call_id.as_str())
                    .copied()
                    .unwrap_or(tool_call_id);
                contents.push(serde_json::json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": { "name": name, "response": { "result": content } }
                    }]
                }));
            }
        }
    }

    let mut generation = serde_json::json!({ "maxOutputTokens": req.max_tokens });
    if let Some(temp) = req.temperature {
        generation["temperature"] = serde_json::json!(temp);
    }
    // thinkingConfig：仅思考能力模型发送（其余模型 API 不识别该字段，静默忽略）。
    if let Some(thinking) = &req.thinking {
        if is_gemini_thinking_model(&req.model.id) {
            generation["thinkingConfig"] = serde_json::json!({
                "includeThoughts": true,
                "thinkingBudget": thinking.budget_tokens,
            });
        }
    }

    let mut body = serde_json::json!({
        "contents": contents,
        "generationConfig": generation,
    });
    if !system_parts.is_empty() {
        body["systemInstruction"] =
            serde_json::json!({ "parts": [{ "text": system_parts.join("\n\n") }] });
    }
    if !req.tools.is_empty() {
        let decls: Vec<serde_json::Value> = req.tools.iter().map(tool_declaration).collect();
        body["tools"] = serde_json::json!([{ "functionDeclarations": decls }]);
    }
    // per-model 额外请求体字段合并到顶层（如 safetySettings 等透传配置）。
    crate::merge_extra_body(&mut body, req.model.extra_body.as_ref());
    body
}

/// 工具规格 → Gemini functionDeclaration（schema 直传：Gemini 接受 `OpenAPI` 风格子集）。
fn tool_declaration(t: &ToolSpec) -> serde_json::Value {
    serde_json::json!({
        "name": t.name,
        "description": t.description,
        "parameters": t.schema,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// SSE 帧（wire 类型，字段名对齐 REST JSON 的 camelCase，容忍 snake_case 别名）
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GeminiChunk {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default, rename = "usageMetadata", alias = "usage_metadata")]
    usage_metadata: Option<GeminiUsageMeta>,
    /// 流中错误帧（配额耗尽 / 服务端中途失败时以普通 data 帧下发）。
    #[serde(default)]
    error: Option<GeminiErrorFrame>,
    /// prompt 级拦截（安全策略在生成前拒绝）。
    #[serde(default, rename = "promptFeedback", alias = "prompt_feedback")]
    prompt_feedback: Option<GeminiPromptFeedback>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiContent>,
    #[serde(default, rename = "finishReason", alias = "finish_reason")]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct GeminiContent {
    #[serde(default)]
    parts: Vec<GeminiPart>,
}

#[derive(Deserialize, Default)]
struct GeminiPart {
    #[serde(default)]
    text: Option<String>,
    /// true 表示该 text part 为思考摘要（includeThoughts 开启时出现）。
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default, rename = "functionCall", alias = "function_call")]
    function_call: Option<GeminiFunctionCall>,
}

#[derive(Deserialize)]
struct GeminiFunctionCall {
    #[serde(default)]
    name: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct GeminiUsageMeta {
    #[serde(default, rename = "promptTokenCount", alias = "prompt_token_count")]
    prompt_token_count: Option<u64>,
    #[serde(
        default,
        rename = "candidatesTokenCount",
        alias = "candidates_token_count"
    )]
    candidates_token_count: Option<u64>,
    #[serde(default, rename = "thoughtsTokenCount", alias = "thoughts_token_count")]
    thoughts_token_count: Option<u64>,
}

#[derive(Deserialize, Default)]
struct GeminiErrorFrame {
    #[serde(default)]
    code: u16,
    #[serde(default)]
    message: String,
    #[serde(default)]
    status: String,
}

#[derive(Deserialize, Default)]
struct GeminiPromptFeedback {
    #[serde(default, rename = "blockReason", alias = "block_reason")]
    block_reason: Option<String>,
}

// ──────────────────────────────────────────────────────────────────────────────
// SSE 流解析（状态机：可离线 fixture 驱动测试）
// ──────────────────────────────────────────────────────────────────────────────

/// Gemini functionCall 单帧完整给出（无增量拼装），args 直接持有 JSON 对象。
#[derive(Default)]
struct ToolCallAccum {
    id: Option<String>,
    name: Option<String>,
    args: serde_json::Value,
}

/// 单条流的累积状态（供 [`parse_gemini_stream`] 与 fixture 测试复用）。
struct GeminiStreamState {
    model_id: String,
    text_buf: String,
    thought_buf: String,
    tool_calls: Vec<ToolCallAccum>,
    finish: Option<String>,
    usage: Usage,
}

impl GeminiStreamState {
    fn new(model_id: String) -> Self {
        Self {
            model_id,
            text_buf: String::new(),
            thought_buf: String::new(),
            tool_calls: Vec::new(),
            finish: None,
            usage: Usage::default(),
        }
    }

    /// 处理一个 SSE data 帧（已解析的 chunk），返回本帧应发的增量事件。
    fn ingest_chunk(&mut self, chunk: GeminiChunk) -> Vec<AssistantEvent> {
        let mut events = Vec::new();
        // prompt 被安全策略拦截：refusal 类终态，立即报错终止。
        if let Some(reason) = chunk
            .prompt_feedback
            .as_ref()
            .and_then(|f| f.block_reason.as_deref())
            .filter(|r| !r.is_empty())
        {
            events.push(AssistantEvent::Error(LlmError::Unsupported(format!(
                "Gemini prompt 被拦截（blockReason={reason}）"
            ))));
            return events;
        }
        // 流中错误帧（配额/服务端中途失败）：按 HTTP 错误归类上报。
        if let Some(e) = chunk.error {
            let frame = if e.status.is_empty() {
                e.message.clone()
            } else {
                format!("{}: {}", e.status, e.message)
            };
            events.push(AssistantEvent::Error(LlmError::Http {
                status: e.code,
                body: frame,
            }));
            return events;
        }
        for candidate in &chunk.candidates {
            if let Some(content) = &candidate.content {
                for part in &content.parts {
                    if let Some(fc) = &part.function_call {
                        // Gemini 不提供调用 id：按到达序合成（回传时 functionResponse
                        // 以函数名匹配，id 仅用于本侧事件流关联）。
                        let id = format!("call_{}", self.tool_calls.len());
                        let args = fc.args.clone().unwrap_or_else(|| serde_json::json!({}));
                        self.tool_calls.push(ToolCallAccum {
                            id: Some(id.clone()),
                            name: Some(fc.name.clone()),
                            args: args.clone(),
                        });
                        events.push(AssistantEvent::ToolCallStart {
                            id: id.clone(),
                            name: fc.name.clone(),
                        });
                        let partial = serde_json::to_string(&args).unwrap_or_default();
                        if !partial.is_empty() {
                            events.push(AssistantEvent::ToolCallDelta {
                                id,
                                partial_json: partial,
                            });
                        }
                    } else if let Some(text) = part.text.as_deref().filter(|t| !t.is_empty()) {
                        if part.thought == Some(true) {
                            self.thought_buf.push_str(text);
                            events.push(AssistantEvent::ThinkingDelta(text.to_string()));
                        } else {
                            self.text_buf.push_str(text);
                            events.push(AssistantEvent::TextDelta(text.to_string()));
                        }
                    }
                }
            }
            if let Some(fr) = candidate.finish_reason.as_deref().filter(|f| !f.is_empty()) {
                self.finish = Some(fr.to_string());
            }
        }
        if let Some(u) = &chunk.usage_metadata {
            if let Some(p) = u.prompt_token_count {
                self.usage.input_tokens = p;
            }
            // 思考 token 按输出计费：candidatesTokenCount + thoughtsTokenCount。
            self.usage.output_tokens =
                u.candidates_token_count.unwrap_or(0) + u.thoughts_token_count.unwrap_or(0);
            events.push(AssistantEvent::Usage(self.usage.clone()));
        }
        events
    }

    /// 聚合为完整助手消息。
    fn build_message(&self) -> agent_core::AssistantMessage {
        let mut content = Vec::new();
        if !self.thought_buf.is_empty() {
            content.push(ContentBlock::Thinking {
                text: self.thought_buf.clone(),
                signature: None,
            });
        }
        if !self.text_buf.is_empty() {
            content.push(ContentBlock::Text {
                text: self.text_buf.clone(),
            });
        }
        for tc in &self.tool_calls {
            content.push(ContentBlock::ToolCall {
                id: tc.id.clone().unwrap_or_default(),
                name: tc.name.clone().unwrap_or_default(),
                arguments: tc.args.clone(),
            });
        }
        let (stop_reason, stop_details) =
            map_stop(self.finish.as_deref(), !self.tool_calls.is_empty());
        agent_core::AssistantMessage {
            content,
            usage: self.usage.clone(),
            model: self.model_id.clone(),
            stop_reason,
            stop_details,
        }
    }
}

/// finishReason → (`StopReason`, `StopDetails`)。
///
/// 模型发起函数调用时 Gemini 的 finishReason 通常仍为 `STOP`——有工具调用即判
/// [`StopReason::ToolUse`]（对齐 openai 适配器的 `tool_calls` 语义）。
fn map_stop(
    finish: Option<&str>,
    has_tool_calls: bool,
) -> (Option<StopReason>, Option<agent_core::StopDetails>) {
    if has_tool_calls {
        return (Some(StopReason::ToolUse), None);
    }
    match finish {
        Some("STOP") => (Some(StopReason::Stop), None),
        Some("MAX_TOKENS") => (Some(StopReason::Length), None),
        // SAFETY 族 → sensitive（内容安全拦截）；RECITATION → refusal（复述版权内容）。
        // 两者均 refusal-like：不应作为对话重放（StopDetails::is_refusal_like）。
        Some("SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII" | "IMAGE_SAFETY") => (
            Some(StopReason::Error),
            Some(agent_core::StopDetails::new("sensitive")),
        ),
        Some("RECITATION") => (
            Some(StopReason::Error),
            Some(agent_core::StopDetails::new("refusal")),
        ),
        Some(other) => (
            Some(StopReason::Error),
            Some(agent_core::StopDetails::new(format!("gemini_{other}"))),
        ),
        None => (None, None),
    }
}

fn parse_gemini_stream(resp: reqwest::Response, model_id: String) -> AssistantEventStream {
    let s = stream! {
        yield AssistantEvent::MessageStart;
        let mut bytes_stream = resp.bytes_stream();
        // 字节缓冲：以 `\n` 切行，确保跨 chunk 的多字节 UTF-8 字符不被拆断丢弃。
        let mut buf: Vec<u8> = Vec::new();
        let mut st = GeminiStreamState::new(model_id);

        loop {
            // 按 chunk 的空闲读超时：只要上游持续吐 token，每次读到新 chunk 即顺延计时；
            // 仅当真正静默超过阈值（上游挂起/网络中断）才判超时（替代整条请求总超时，
            // 避免慢速 LLM 长流被误杀；Gemini SSE 无 [DONE] 终止帧，依赖流自然关闭）。
            let chunk_res = match tokio::time::timeout(
                crate::STREAM_IDLE_TIMEOUT,
                bytes_stream.next(),
            )
            .await
            {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(_) => {
                    // P0-自愈激活：流瞬时中断时，若 finish 已收或存在已完成工具调用，发
                    // MessageEnd（finish 未收则标记 stream_interrupted 瞬时错误），供 agent
                    // 瞬时恢复保留并执行已完成工具；否则保持原兜底（发 Error）。
                    if let Some(msg) = finalize_stream_interrupt(&st) {
                        yield AssistantEvent::MessageEnd(msg);
                        return;
                    }
                    yield AssistantEvent::Error(LlmError::StreamInterrupted(format!(
                        "Gemini 流空闲超过 {} 秒未收到数据，判定上游静默",
                        crate::STREAM_IDLE_TIMEOUT.as_secs()
                    )));
                    break;
                }
            };
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    if let Some(msg) = finalize_stream_interrupt(&st) {
                        yield AssistantEvent::MessageEnd(msg);
                        return;
                    }
                    yield AssistantEvent::Error(LlmError::StreamInterrupted(format!("Gemini 流中断: {e}")));
                    break;
                }
            };
            buf.extend_from_slice(chunk.as_ref());
            while let Some(line_bytes) = crate::drain_line(&mut buf) {
                let line = String::from_utf8_lossy(&line_bytes).trim().to_string();
                if line.is_empty() { continue; }
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                // Gemini 官方 SSE 无 [DONE]；容忍兼容网关补发的终止帧。
                if data == "[DONE]" {
                    yield AssistantEvent::MessageEnd(st.build_message());
                    return;
                }
                let Ok(chunk) = serde_json::from_str::<GeminiChunk>(data) else { continue };
                for ev in st.ingest_chunk(chunk) {
                    // 致命帧（error / prompt 拦截）：发出 Error 后仍以 MessageEnd 收口
                    // （保留已累积内容，对齐 anthropic 适配器的 Error+MessageEnd 惯例）。
                    let fatal = matches!(ev, AssistantEvent::Error(_));
                    yield ev;
                    if fatal {
                        yield AssistantEvent::MessageEnd(st.build_message());
                        return;
                    }
                }
            }
            // 防御无换行的超长行撑爆内存：drain_line 抽干完整行后 buf 仅余未完结尾段。
            if crate::line_buffer_too_long(&buf) {
                if let Some(msg) = finalize_stream_interrupt(&st) {
                    yield AssistantEvent::MessageEnd(msg);
                    return;
                }
                yield AssistantEvent::Error(LlmError::StreamInterrupted(
                    "SSE 行超过最大长度上限".into(),
                ));
                break;
            }
        }
        // 流自然结束（未见 [DONE]）。
        yield AssistantEvent::MessageEnd(st.build_message());
    };
    Box::pin(s) as Pin<Box<dyn futures::Stream<Item = AssistantEvent> + Send>>
}

/// 流瞬时中断兜底终态（移植 oh-my-pi `retainCompletedToolCalls`）。
///
/// finish 已收（响应实际完成）或存在已完成工具调用（Gemini functionCall 单帧完整
/// 给出，有 name 即视为完成）时返回 [`Some`] 消息供
/// [`MessageEnd`](AssistantEvent::MessageEnd) 承载——finish 未收时标记
/// `stream_interrupted` 瞬时错误，供 agent 循环瞬时恢复改写为 ToolUse、执行已完成
/// 工具而非整轮废弃；否则返回 [`None`]（调用方发 [`Error`](AssistantEvent::Error)）。
fn finalize_stream_interrupt(st: &GeminiStreamState) -> Option<agent_core::AssistantMessage> {
    let recoverable = st.finish.is_some() || st.tool_calls.iter().any(|tc| tc.name.is_some());
    if !recoverable {
        return None;
    }
    let mut msg = st.build_message();
    if st.finish.is_none() {
        crate::mark_transient_stream_error(&mut msg);
    }
    Some(msg)
}

// ──────────────────────────────────────────────────────────────────────────────
// 错误处理
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

/// Gemini HTTP 错误映射（分类学对齐 glm/deepseek 适配器）。
fn map_gemini_error(status: u16, body: &str) -> LlmError {
    match status {
        401 | 403 => LlmError::Auth(format!("Gemini 鉴权失败（{status}）: {body}")),
        429 => LlmError::RateLimit {
            retry_after_ms: 5000,
        },
        s if (500..600).contains(&s) => LlmError::Http {
            status,
            body: format!("Gemini 服务端错误: {body}"),
        },
        _ => LlmError::Http {
            status,
            body: body.to_string(),
        },
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 测试
// ──────────────────────────────────────────────────────────────────────────────

// Provider 自荐注册统一集中在 plugin.rs（collect_providers 用共享 client 构造）。

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{Model, ThinkingConfig};

    fn req(model_id: &str) -> CompletionRequest {
        CompletionRequest {
            model: Model::with_defaults(model_id, "google", Api::GoogleGenerativeAi),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            thinking: None,
            cache_key: None,
            stable_prefix_len: 0,
        }
    }

    #[test]
    fn thinking_model_detection() {
        assert!(is_gemini_thinking_model("gemini-2.5-pro"));
        assert!(is_gemini_thinking_model("gemini-2.5-flash"));
        assert!(is_gemini_thinking_model("gemini-2.5-flash-lite"));
        assert!(is_gemini_thinking_model("Gemini-3-Pro-Preview"));
        assert!(is_gemini_thinking_model("models/gemini-2.5-pro"));
        assert!(!is_gemini_thinking_model("gemini-2.0-flash"));
        assert!(!is_gemini_thinking_model("gemini-1.5-pro"));
        assert!(!is_gemini_thinking_model("gpt-4o"));
    }

    #[test]
    fn url_uses_default_base_and_model_path() {
        let url = build_url(None, "gemini-2.5-pro");
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn url_tolerates_custom_base_variants() {
        // 自定义 base + 尾斜杠。
        let url = build_url(Some("https://gw.example.com/"), "gemini-3-pro");
        assert_eq!(
            url,
            "https://gw.example.com/v1beta/models/gemini-3-pro:streamGenerateContent?alt=sse"
        );
        // base 已带 /v1beta：不重复拼接。
        let url = build_url(Some("https://gw.example.com/v1beta"), "gemini-3-pro");
        assert_eq!(
            url,
            "https://gw.example.com/v1beta/models/gemini-3-pro:streamGenerateContent?alt=sse"
        );
        // model id 已带 models/ 前缀：不重复拼接。
        let url = build_url(None, "models/gemini-2.5-flash");
        assert!(url.contains("/v1beta/models/gemini-2.5-flash:streamGenerateContent"));
        assert!(!url.contains("models/models/"));
    }

    #[test]
    fn build_body_maps_system_contents_tools_thinking() {
        let mut r = req("gemini-2.5-pro");
        r.system = vec!["be helpful".into()];
        r.messages = vec![ProviderMessage::User {
            content: vec![UserContent::Text { text: "hi".into() }],
        }];
        r.tools = vec![agent_core::ToolSpec::new(
            "read_file",
            "读取文件",
            serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
        )];
        r.temperature = Some(0.2);
        r.thinking = Some(ThinkingConfig::new(2048));
        let body = build_body(&r);

        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be helpful");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
        let decl = &body["tools"][0]["functionDeclarations"][0];
        assert_eq!(decl["name"], "read_file");
        assert_eq!(decl["description"], "读取文件");
        // schema 直传。
        assert_eq!(decl["parameters"]["type"], "object");
        assert_eq!(decl["parameters"]["properties"]["path"]["type"], "string");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 1024);
        assert!(
            (body["generationConfig"]["temperature"]
                .as_f64()
                .unwrap_or(0.0)
                - 0.2)
                .abs()
                < 1e-5
        );
        let tc = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(tc["includeThoughts"], true);
        assert_eq!(tc["thinkingBudget"], 2048);
    }

    #[test]
    fn build_body_ignores_thinking_for_non_thinking_model() {
        // 2.0 模型不识别 thinkingConfig：配置存在也应静默忽略。
        let mut r = req("gemini-2.0-flash");
        r.thinking = Some(ThinkingConfig::new(2048));
        let body = build_body(&r);
        assert!(body["generationConfig"].get("thinkingConfig").is_none());
    }

    #[test]
    fn build_body_maps_assistant_toolcall_and_tool_result() {
        let mut r = req("gemini-2.5-pro");
        r.messages = vec![
            ProviderMessage::Assistant {
                content: vec![
                    ContentBlock::Thinking {
                        text: "先分析".into(),
                        signature: None,
                    },
                    ContentBlock::ToolCall {
                        id: "call_0".into(),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path":"a.rs"}),
                    },
                ],
            },
            ProviderMessage::Tool {
                tool_call_id: "call_0".into(),
                content: "file body".into(),
                is_error: false,
                images: vec![],
            },
        ];
        let body = build_body(&r);
        // assistant 轮：thinking 跳过，仅 functionCall part。
        assert_eq!(body["contents"][0]["role"], "model");
        assert_eq!(
            body["contents"][0]["parts"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(
            body["contents"][0]["parts"][0]["functionCall"]["name"],
            "read_file"
        );
        assert_eq!(
            body["contents"][0]["parts"][0]["functionCall"]["args"]["path"],
            "a.rs"
        );
        // tool 结果轮：functionResponse 的 name 经历史回查（call_0 → read_file）。
        assert_eq!(body["contents"][1]["role"], "user");
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["name"],
            "read_file"
        );
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["response"]["result"],
            "file body"
        );
    }

    #[test]
    fn build_body_maps_user_image_to_inline_data() {
        let mut r = req("gemini-2.5-pro");
        r.messages = vec![ProviderMessage::User {
            content: vec![
                UserContent::Text {
                    text: "看图".into(),
                },
                UserContent::Image {
                    mime: "image/png".into(),
                    data: "aGk=".into(),
                },
            ],
        }];
        let body = build_body(&r);
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["inlineData"]["data"], "aGk=");
    }

    /// fixture 驱动的 SSE 帧解析：喂入 data 帧文本，返回全部事件 + 终态消息。
    fn run_fixture(
        model_id: &str,
        frames: &[&str],
    ) -> (Vec<AssistantEvent>, agent_core::AssistantMessage) {
        let mut st = GeminiStreamState::new(model_id.to_string());
        let mut events = Vec::new();
        for f in frames {
            let chunk = serde_json::from_str::<GeminiChunk>(f).expect("fixture 应可解析");
            events.extend(st.ingest_chunk(chunk));
        }
        (events, st.build_message())
    }

    #[test]
    fn sse_text_and_thinking_deltas() {
        let (events, msg) = run_fixture(
            "gemini-2.5-pro",
            &[
                r#"{"candidates":[{"content":{"parts":[{"text":"思考中","thought":true}]}}]}"#,
                r#"{"candidates":[{"content":{"parts":[{"text":"你"}]}}]}"#,
                r#"{"candidates":[{"content":{"parts":[{"text":"好"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":3,"thoughtsTokenCount":7}}"#,
            ],
        );
        // 事件序：thinking 增量 → 文本增量 ×2 → Usage。
        assert!(matches!(&events[0], AssistantEvent::ThinkingDelta(t) if t == "思考中"));
        assert!(matches!(&events[1], AssistantEvent::TextDelta(t) if t == "你"));
        assert!(matches!(&events[2], AssistantEvent::TextDelta(t) if t == "好"));
        assert!(
            matches!(&events[3], AssistantEvent::Usage(u) if u.input_tokens == 10 && u.output_tokens == 10)
        );
        // 终态消息：Thinking 块 + Text 块，STOP + thoughts 计入输出。
        assert!(matches!(&msg.content[0], ContentBlock::Thinking { text, .. } if text == "思考中"));
        assert!(matches!(&msg.content[1], ContentBlock::Text { text } if text == "你好"));
        assert_eq!(msg.stop_reason, Some(StopReason::Stop));
        assert_eq!(msg.usage.input_tokens, 10);
        assert_eq!(msg.usage.output_tokens, 10); // candidates 3 + thoughts 7
    }

    #[test]
    fn sse_function_call_emits_tool_events() {
        let (events, msg) = run_fixture(
            "gemini-2.5-pro",
            &[
                r#"{"candidates":[{"content":{"parts":[{"text":"读取文件"}]}}]}"#,
                r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"path":"a.rs"}}}]}}]}"#,
                r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":8}}"#,
            ],
        );
        // functionCall → Start + 单帧完整 Delta（args 直接为 JSON 对象）。
        assert!(
            matches!(&events[1], AssistantEvent::ToolCallStart { name, .. } if name == "read_file")
        );
        let AssistantEvent::ToolCallDelta { partial_json, .. } = &events[2] else {
            panic!("应为 ToolCallDelta");
        };
        let args: serde_json::Value = serde_json::from_str(partial_json).unwrap();
        assert_eq!(args["path"], "a.rs");
        // 有工具调用即 ToolUse（即便 finishReason 为 STOP）。
        assert_eq!(msg.stop_reason, Some(StopReason::ToolUse));
        let (id, name, arguments) = msg.tool_calls()[0];
        assert_eq!(name, "read_file");
        assert_eq!(arguments["path"], "a.rs");
        assert!(!id.is_empty(), "合成调用 id 不应为空");
    }

    #[test]
    fn sse_finish_reason_mapping() {
        let (_, msg) = run_fixture(
            "gemini-2.5-pro",
            &[
                r#"{"candidates":[{"content":{"parts":[{"text":"截断"}]},"finishReason":"MAX_TOKENS"}]}"#,
            ],
        );
        assert_eq!(msg.stop_reason, Some(StopReason::Length));

        let (_, msg) = run_fixture(
            "gemini-2.5-pro",
            &[
                r#"{"candidates":[{"content":{"parts":[{"text":"不能说"}]},"finishReason":"SAFETY"}]}"#,
            ],
        );
        assert_eq!(msg.stop_reason, Some(StopReason::Error));
        assert!(
            msg.stop_details
                .as_ref()
                .is_some_and(agent_core::StopDetails::is_refusal_like)
        );

        let (_, msg) = run_fixture(
            "gemini-2.5-pro",
            &[
                r#"{"candidates":[{"content":{"parts":[{"text":"复述"}]},"finishReason":"RECITATION"}]}"#,
            ],
        );
        assert_eq!(msg.stop_reason, Some(StopReason::Error));
        assert!(
            msg.stop_details
                .as_ref()
                .is_some_and(agent_core::StopDetails::is_refusal_like)
        );

        let (_, msg) = run_fixture(
            "gemini-2.5-pro",
            &[r#"{"candidates":[{"finishReason":"MALFORMED_FUNCTION_CALL"}]}"#],
        );
        assert_eq!(msg.stop_reason, Some(StopReason::Error));
    }

    #[test]
    fn sse_stream_error_frame_maps_to_error_event() {
        let (events, _) = run_fixture(
            "gemini-2.5-pro",
            &[
                r#"{"error":{"code":429,"message":"Resource exhausted","status":"RESOURCE_EXHAUSTED"}}"#,
            ],
        );
        assert!(matches!(
            &events[0],
            AssistantEvent::Error(LlmError::Http { status: 429, .. })
        ));
    }

    #[test]
    fn sse_blocked_prompt_maps_to_error_event() {
        let (events, _) = run_fixture(
            "gemini-2.5-pro",
            &[r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#],
        );
        assert!(matches!(
            &events[0],
            AssistantEvent::Error(LlmError::Unsupported(_))
        ));
    }

    #[test]
    fn finalize_stream_interrupt_retains_completed_toolcall() {
        // P0-自愈：finish 未收 + 已完成工具调用 → 标记瞬时错误供恢复。
        let mut st = GeminiStreamState::new("gemini-2.5-pro".into());
        st.ingest_chunk(
            serde_json::from_str(r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"path":"a.rs"}}}]}}]}"#)
                .unwrap(),
        );
        let msg = finalize_stream_interrupt(&st).expect("已完成工具调用应可恢复");
        assert_eq!(msg.stop_reason, Some(StopReason::Error));
        assert!(
            msg.stop_details
                .as_ref()
                .is_some_and(agent_core::StopDetails::is_transient_stream_error)
        );
    }

    #[test]
    fn finalize_stream_interrupt_none_when_empty() {
        let st = GeminiStreamState::new("gemini-2.5-pro".into());
        assert!(finalize_stream_interrupt(&st).is_none());
    }

    #[test]
    fn gemini_error_mapping() {
        assert!(matches!(
            map_gemini_error(401, "bad key"),
            LlmError::Auth(_)
        ));
        assert!(matches!(
            map_gemini_error(403, "forbidden"),
            LlmError::Auth(_)
        ));
        assert!(matches!(
            map_gemini_error(429, "slow down"),
            LlmError::RateLimit { .. }
        ));
        match map_gemini_error(500, "server") {
            LlmError::Http { status, .. } => assert_eq!(status, 500),
            _ => panic!("应为 Http"),
        }
        match map_gemini_error(404, "model not found") {
            LlmError::Http { status, .. } => assert_eq!(status, 404),
            _ => panic!("应为 Http"),
        }
    }
}
